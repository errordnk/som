//! The bridge: Som's rich-content channel to a `somsrv` daemon on ANOTHER
//! machine, carried over one `ssh host somsrv --srv-bridge` process (or
//! `wsl -- somsrv --srv-bridge`).
//!
//! Why it exists: `somsrp` (and anything else that speaks `SrvRequest`) on a
//! remote host talks to the daemon of THAT host - the pictures, videos,
//! files and radio streams it announces live there, with that host's disk and
//! that host's IP. Som, on the user's machine, used to ask only its own local
//! daemon, which has never heard of them: the placeholder grid stayed black.
//! The bridge lets Som ask the remote daemon instead.
//!
//! Wire format over the bridge process's stdin/stdout: frames of
//! `[channel: u32 LE][len: u32 LE][payload]`. A channel is one connection to
//! the daemon (Som opens many: a long-lived subscription per placement,
//! one-shot requests for seeks and clicks). `len == CLOSE` carries no
//! payload and closes the channel from either side. The payload is exactly
//! one message of the daemon's own framing (what `PipeConnection::
//! read_message` returns), so the bridge never looks inside it: the
//! `ConnectionKind` byte, the handshake and every `SrvRequest` /
//! `SrvResponse` pass through untouched.
//!
//! No pty: a pty would mangle binary frames (this is also why the bridge is
//! a second ssh process and not a side channel of the tab's own session).

use crate::pipe::PipeConnection;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// The `len` value of a frame that closes its channel.
const CLOSE: u32 = u32::MAX;

/// A frame longer than this is a broken stream, not a message.
const MAX_PAYLOAD: u32 = 1 << 30;

fn write_frame(out: &Mutex<impl Write>, channel: u32, payload: Option<&[u8]>) -> Result<()> {
    let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    out.write_all(&channel.to_le_bytes())?;
    match payload {
        Some(payload) => {
            out.write_all(&(payload.len() as u32).to_le_bytes())?;
            out.write_all(payload)?;
        },
        None => out.write_all(&CLOSE.to_le_bytes())?,
    }
    out.flush()?;
    Ok(())
}

fn read_frame(input: &mut impl Read) -> Result<(u32, Option<Vec<u8>>)> {
    let mut header = [0u8; 8];
    input.read_exact(&mut header).context("the bridge stream ended")?;
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if len == CLOSE {
        return Ok((channel, None));
    }
    anyhow::ensure!(len <= MAX_PAYLOAD, "bridge frame of {len} bytes");
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload).context("the bridge stream ended inside a frame")?;
    Ok((channel, Some(payload)))
}

// ---- the remote end: `somsrv --srv-bridge` -------------------------------

/// Serves the bridge on this process's stdin/stdout until stdin ends (ssh
/// went away), connecting each new channel to the daemon of this machine.
pub fn run_stdio_bridge() -> Result<()> {
    let daemon_binary = std::env::current_exe().context("cannot tell where somsrv itself is")?;
    serve(std::io::stdin().lock(), std::io::stdout(), move || crate::daemon::connect_or_spawn(&daemon_binary))
}

fn serve<R, W, C>(mut input: R, output: W, connect: C) -> Result<()>
where
    R: Read,
    W: Write + Send + 'static,
    C: Fn() -> Result<PipeConnection>,
{
    let output = Arc::new(Mutex::new(output));
    let channels: Arc<Mutex<HashMap<u32, Arc<PipeConnection>>>> = Arc::default();

    let result = loop {
        let (channel, payload) = match read_frame(&mut input) {
            Ok(frame) => frame,
            Err(err) => break Err(err),
        };
        let Some(payload) = payload else {
            let closed = channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel);
            if let Some(connection) = closed {
                connection.shutdown();
            }
            continue;
        };

        let existing = channels.lock().unwrap_or_else(|p| p.into_inner()).get(&channel).cloned();
        let connection = match existing {
            Some(connection) => connection,
            None => match connect() {
                Ok(connection) => {
                    let connection = Arc::new(connection);
                    channels.lock().unwrap_or_else(|p| p.into_inner()).insert(channel, connection.clone());
                    spawn_daemon_reader(channel, connection.clone(), channels.clone(), output.clone());
                    connection
                },
                Err(err) => {
                    log::warn!("bridge channel {channel}: cannot reach the daemon: {err:#}");
                    if let Err(err) = write_frame(&*output, channel, None) {
                        break Err(err);
                    }
                    continue;
                },
            },
        };
        if let Err(err) = connection.write_message(&payload) {
            log::debug!("bridge channel {channel}: the daemon went away: {err:#}");
            channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel);
            connection.shutdown();
            if let Err(err) = write_frame(&*output, channel, None) {
                break Err(err);
            }
        }
    };

    for (_, connection) in channels.lock().unwrap_or_else(|p| p.into_inner()).drain() {
        connection.shutdown();
    }
    result
}

/// Pumps everything the daemon sends on `channel` to the other side of the
/// bridge, and closes the channel there when the daemon hangs up.
fn spawn_daemon_reader<W: Write + Send + 'static>(
    channel: u32,
    connection: Arc<PipeConnection>,
    channels: Arc<Mutex<HashMap<u32, Arc<PipeConnection>>>>,
    output: Arc<Mutex<W>>,
) {
    std::thread::spawn(move || {
        loop {
            let message = match connection.read_message() {
                Ok(message) => message,
                Err(_) => break,
            };
            if write_frame(&*output, channel, Some(&message)).is_err() {
                break;
            }
        }
        let still_open = channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel).is_some();
        if still_open && let Err(err) = write_frame(&*output, channel, None) {
            log::debug!("bridge channel {channel}: cannot report the close: {err:#}");
        }
    });
}

// ---- the local end: what Som holds ---------------------------------------

type ChannelSenders = Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>;

/// A running bridge process. Shared by every placement of every tab that
/// talks to the same remote host.
pub struct RemoteBridge {
    writer: Mutex<Box<dyn Write + Send>>,
    channels: ChannelSenders,
    next_channel: AtomicU32,
    alive: AtomicBool,
    child: Mutex<Option<std::process::Child>>,
}

impl RemoteBridge {
    /// Starts `program args...` (the bridge command: ssh, or wsl, ending in
    /// `somsrv --srv-bridge`) and speaks the bridge protocol over its pipes.
    pub fn spawn(program: &str, args: &[String]) -> Result<Arc<Self>> {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().with_context(|| format!("cannot start the bridge command {program:?}"))?;
        let stdin = child.stdin.take().context("the bridge command has no stdin")?;
        let stdout = child.stdout.take().context("the bridge command has no stdout")?;
        Ok(Self::over_streams(stdout, stdin, Some(child)))
    }

    /// The bridge over any pair of streams (`child` is killed when the bridge
    /// is dropped). `spawn` is this over a process; tests use it over pipes.
    pub fn over_streams(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<std::process::Child>,
    ) -> Arc<Self> {
        let bridge = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            channels: Mutex::default(),
            next_channel: AtomicU32::new(1),
            alive: AtomicBool::new(true),
            child: Mutex::new(child),
        });
        let weak = Arc::downgrade(&bridge);
        std::thread::spawn(move || {
            let mut reader = reader;
            loop {
                let frame = read_frame(&mut reader);
                let Some(bridge) = weak.upgrade() else { return };
                match frame {
                    Ok((channel, Some(payload))) => {
                        let sender = bridge.channels.lock().unwrap_or_else(|p| p.into_inner()).get(&channel).cloned();
                        if let Some(sender) = sender {
                            // A reader that has gone away has nothing left to hear.
                            sender.send(payload).ok();
                        }
                    },
                    Ok((channel, None)) => {
                        bridge.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&channel);
                    },
                    Err(err) => {
                        log::debug!("the bridge is over: {err:#}");
                        bridge.alive.store(false, Ordering::Release);
                        bridge.channels.lock().unwrap_or_else(|p| p.into_inner()).clear();
                        return;
                    },
                }
            }
        });
        bridge
    }

    /// False once the other side hung up: the bridge cannot be reused, a new
    /// one has to be spawned.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// A new connection to the remote daemon.
    pub fn open_channel(self: &Arc<Self>) -> Result<BridgeChannel> {
        anyhow::ensure!(self.is_alive(), "the bridge to the remote host is down");
        let id = self.next_channel.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.channels.lock().unwrap_or_else(|p| p.into_inner()).insert(id, sender);
        // The reader thread may have declared the bridge dead between the
        // check above and the insert; a channel nobody will ever feed would
        // block its reader forever.
        if !self.is_alive() {
            self.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
            anyhow::bail!("the bridge to the remote host is down");
        }
        Ok(BridgeChannel { id, bridge: self.clone(), incoming: Mutex::new(receiver) })
    }
}

impl Drop for RemoteBridge {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if let Err(err) = child.kill() {
                log::debug!("the bridge process had already ended: {err}");
            }
            child.wait().ok();
        }
    }
}

/// One connection to the remote daemon through the bridge: the same two
/// calls as `PipeConnection`, and the same messages.
pub struct BridgeChannel {
    id: u32,
    bridge: Arc<RemoteBridge>,
    incoming: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl BridgeChannel {
    pub fn read_message(&self) -> Result<Vec<u8>> {
        let incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
        incoming.recv().map_err(|_| anyhow::anyhow!("the remote daemon closed the channel"))
    }

    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        anyhow::ensure!(self.bridge.is_alive(), "the bridge to the remote host is down");
        write_frame(&self.bridge.writer, self.id, Some(payload))
    }
}

impl Drop for BridgeChannel {
    fn drop(&mut self) {
        self.bridge.channels.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
        if self.bridge.is_alive() && let Err(err) = write_frame(&self.bridge.writer, self.id, None) {
            log::debug!("cannot close bridge channel {}: {err:#}", self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two in-memory byte pipes, one per direction.
    struct PipeEnd {
        receiver: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }
    impl Read for PipeEnd {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.receiver.recv() {
                    Ok(chunk) => self.pending = chunk,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }
    struct PipeSink(mpsc::Sender<Vec<u8>>);
    impl Write for PipeSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.send(buf.to_vec()).map_err(|_| std::io::ErrorKind::BrokenPipe)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn pipe() -> (PipeSink, PipeEnd) {
        let (sender, receiver) = mpsc::channel();
        (PipeSink(sender), PipeEnd { receiver, pending: Vec::new() })
    }

    #[test]
    fn frames_round_trip_and_a_close_frame_has_no_payload() {
        let (sink, mut end) = pipe();
        let sink = Mutex::new(sink);
        write_frame(&sink, 7, Some(b"hello")).unwrap();
        write_frame(&sink, 9, None).unwrap();
        write_frame(&sink, 7, Some(b"")).unwrap();
        assert_eq!(read_frame(&mut end).unwrap(), (7, Some(b"hello".to_vec())));
        assert_eq!(read_frame(&mut end).unwrap(), (9, None));
        assert_eq!(read_frame(&mut end).unwrap(), (7, Some(Vec::new())), "an empty message is not a close");
    }

    #[test]
    fn a_dead_stream_marks_the_bridge_down_and_wakes_every_reader() {
        let (to_bridge, from_remote) = pipe();
        let (to_remote, _remote_input) = pipe();
        let bridge = RemoteBridge::over_streams(from_remote, to_remote, None);
        let channel = bridge.open_channel().unwrap();
        let reader = std::thread::spawn(move || channel.read_message());
        drop(to_bridge); // the remote side hangs up
        assert!(reader.join().unwrap().is_err(), "a read on a dead bridge must fail, not wait forever");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while bridge.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!bridge.is_alive());
        assert!(bridge.open_channel().is_err(), "no channel can be opened on a dead bridge");
    }

    #[test]
    fn each_channel_gets_only_its_own_messages() {
        let (to_bridge, from_remote) = {
            let (sink, end) = pipe();
            (Mutex::new(sink), end)
        };
        let (to_remote, mut remote_input) = pipe();
        let bridge = RemoteBridge::over_streams(from_remote, to_remote, None);
        let first = bridge.open_channel().unwrap();
        let second = bridge.open_channel().unwrap();

        first.write_message(b"to-the-daemon").unwrap();
        assert_eq!(read_frame(&mut remote_input).unwrap(), (first.id, Some(b"to-the-daemon".to_vec())));

        write_frame(&to_bridge, second.id, Some(b"for-second")).unwrap();
        write_frame(&to_bridge, first.id, Some(b"for-first")).unwrap();
        assert_eq!(first.read_message().unwrap(), b"for-first");
        assert_eq!(second.read_message().unwrap(), b"for-second");

        // Dropping a channel tells the other side.
        let first_id = first.id;
        drop(first);
        assert_eq!(read_frame(&mut remote_input).unwrap(), (first_id, None));
    }
}
