//! The same length-prefixed message connection as `PipeConnection`, over a TCP
//! stream: how Som talks to the daemon of a remote host. The tab's own ssh
//! connection forwards a loopback port to that daemon (`ssh -L`, see
//! `protocol::ssh_forward_spec`), so what Som connects to here is the local
//! end of that forward. No second ssh connection, no process of its own.

use anyhow::{Context as _, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

pub struct TcpConnection {
    // Separate halves behind separate locks, as in `pipe::unix`: a thread
    // blocked in `read_message` must not keep another from writing.
    read_half: Mutex<TcpStream>,
    write_half: Mutex<TcpStream>,
}

impl TcpConnection {
    pub fn connect(address: SocketAddr, timeout: Duration) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&address, timeout).with_context(|| format!("cannot connect to {address}"))?;
        stream.set_nodelay(true).context("cannot set TCP_NODELAY")?;
        let read_half = stream.try_clone().context("cannot clone the connection for independent read and write halves")?;
        Ok(Self { read_half: Mutex::new(read_half), write_half: Mutex::new(stream) })
    }

    /// Wraps a stream accepted from a listener (the test stand-in for ssh).
    pub fn from_stream(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true).context("cannot set TCP_NODELAY")?;
        let read_half = stream.try_clone().context("cannot clone the connection for independent read and write halves")?;
        Ok(Self { read_half: Mutex::new(read_half), write_half: Mutex::new(stream) })
    }

    /// Limits how long `read_message` waits (`None`: forever). The forward can
    /// accept a connection whose far end then never answers (no daemon behind
    /// it), so the handshake has to be bounded.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        let read_half = self.read_half.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        read_half.set_read_timeout(timeout).context("cannot set the read timeout")
    }

    pub fn read_message(&self) -> Result<Vec<u8>> {
        let mut read_half = self.read_half.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut len_bytes = [0u8; 4];
        read_half.read_exact(&mut len_bytes).context("connection closed or read failed (length prefix)")?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut buf = vec![0u8; len];
        read_half.read_exact(&mut buf).context("connection closed or read failed (payload)")?;
        Ok(buf)
    }

    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        let mut write_half = self.write_half.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // One write, so the prefix and the payload leave together.
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(payload);
        write_half.write_all(&framed).context("connection closed or write failed")?;
        write_half.flush().context("connection closed or flush failed")?;
        Ok(())
    }
}

/// Carries messages both ways between a TCP client and a daemon connection
/// until either side hangs up.
pub fn pump(client: std::sync::Arc<TcpConnection>, daemon: std::sync::Arc<crate::pipe::PipeConnection>) {
    let (to_client, from_daemon) = (client.clone(), daemon.clone());
    std::thread::spawn(move || {
        while let Ok(message) = from_daemon.read_message() {
            if to_client.write_message(&message).is_err() {
                return;
            }
        }
    });
    while let Ok(message) = client.read_message() {
        if daemon.write_message(&message).is_err() {
            break;
        }
    }
    // The client is gone: end the daemon's side too, or its handler (and the
    // thread above) would wait on a connection nobody uses any more.
    daemon.shutdown();
}

/// A loopback TCP listener that hands each connection on to the daemon of
/// this machine: the way Som reaches a daemon it has no socket path to (in
/// WSL). Stops listening when dropped.
pub struct LoopbackListener {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LoopbackListener {
    /// Listens on `127.0.0.1:port` and serves `Srv` connections through to
    /// the daemon socket at `daemon_socket`. Anything whose first message is
    /// not the `Srv` connection kind is dropped unanswered.
    pub fn start(port: u16, daemon_socket: String) -> Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
        listener.set_nonblocking(true).context("cannot make the listener non-blocking")?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let daemon_socket = daemon_socket.clone();
                        std::thread::spawn(move || {
                            if let Err(err) = serve_srv_connection(stream, &daemon_socket) {
                                log::debug!("a connection on the loopback listener ended: {err:#}");
                            }
                        });
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(err) => {
                        log::warn!("the loopback listener failed: {err}");
                        return;
                    },
                }
            }
        });
        Ok(Self { stop })
    }
}

impl Drop for LoopbackListener {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn serve_srv_connection(stream: TcpStream, daemon_socket: &str) -> Result<()> {
    // An accepted stream may inherit the listener's non-blocking mode.
    stream.set_nonblocking(false).context("cannot make the connection blocking")?;
    let client = std::sync::Arc::new(TcpConnection::from_stream(stream)?);
    client.set_read_timeout(Some(Duration::from_secs(10)))?;
    let first = client.read_message()?;
    anyhow::ensure!(first == [crate::protocol::ConnectionKind::Srv as u8], "the first message is not the Srv connection kind");
    client.set_read_timeout(None)?;
    let daemon = std::sync::Arc::new(crate::pipe::PipeConnection::connect(daemon_socket)?);
    daemon.write_message(&first)?;
    pump(client, daemon);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_keep_their_boundaries_and_content() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = TcpConnection::from_stream(stream).unwrap();
            // Echoes three messages, one of them empty and one of them large.
            for _ in 0..3 {
                let message = connection.read_message().unwrap();
                connection.write_message(&message).unwrap();
            }
        });
        let client = TcpConnection::connect(address, Duration::from_secs(5)).unwrap();
        let large: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
        for message in [b"first".to_vec(), Vec::new(), large] {
            client.write_message(&message).unwrap();
            assert_eq!(client.read_message().unwrap(), message);
        }
        server.join().unwrap();
        assert!(client.read_message().is_err(), "a closed connection is an error, not an empty message");
    }

    #[test]
    fn a_peer_that_never_answers_is_given_up_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpConnection::connect(listener.local_addr().unwrap(), Duration::from_secs(5)).unwrap();
        client.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        assert!(client.read_message().is_err());
    }
}
