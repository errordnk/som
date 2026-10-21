//! The loopback port a WSL tab's daemon listens on for Som, against the real
//! daemon: an `Srv` connection is carried through and answered, anything else
//! is refused, and the port goes away with its owner.

use somsrv::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse, daemon_socket_path};
use somsrv::tcp::{LoopbackListener, TcpConnection};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn connect(port: u16) -> TcpConnection {
    TcpConnection::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).expect("connecting to the loopback listener")
}

/// Fails the test instead of hanging when `body` does not finish.
fn within(limit: Duration, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        body();
        done.send(()).ok();
    });
    assert!(finished.recv_timeout(limit).is_ok(), "no result in {limit:?}");
}

#[test]
fn an_srv_connection_is_carried_to_the_daemon_and_other_kinds_are_refused() {
    within(Duration::from_secs(60), || {
        // The daemon of this machine, started the way a relay would.
        somsrv::daemon::connect_or_spawn(std::path::Path::new(env!("CARGO_BIN_EXE_somsrv"))).expect("a daemon to forward to");
        let port = free_port();
        let listener = LoopbackListener::start(port, daemon_socket_path()).expect("listening");

        let client = connect(port);
        client.write_message(&[ConnectionKind::Srv as u8]).unwrap();
        client.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
        let reply = client.read_message().expect("the daemon answers through the listener");
        assert!(matches!(serde_json::from_slice::<SrvResponse>(&reply).unwrap(), SrvResponse::Handshake(_)));

        // A relay connection would let whoever reaches the port start a shell.
        let relay = connect(port);
        relay.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        relay.write_message(&[ConnectionKind::Relay as u8]).unwrap();
        assert!(relay.read_message().is_err(), "a Relay connection must be closed, not served");

        // Dropping the listener ends the port.
        drop(listener);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).is_err() {
                break;
            }
            assert!(Instant::now() < deadline, "the port is still open after its listener was dropped");
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

#[test]
fn a_relay_asks_its_daemon_for_the_port_and_takes_it_away_when_it_goes() {
    within(Duration::from_secs(90), || {
        let port = free_port();
        let pane_id = format!("loopback-test-{}", std::process::id());
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        // The relay as Som starts a WSL tab's: the flag names the port.
        let mut relay = std::process::Command::new(env!("CARGO_BIN_EXE_somsrv"))
            .args(["loopback-test", pane_id.as_str(), shell, "--srv-tcp-port", &port.to_string()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("starting the relay");

        let deadline = Instant::now() + Duration::from_secs(30);
        let client = loop {
            assert!(Instant::now() < deadline, "the daemon never listened on the port its relay named (an older somsrv daemon still running on this machine does not know the request: stop it and run again)");
            if let Ok(client) = TcpConnection::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)) {
                break client;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        client.write_message(&[ConnectionKind::Srv as u8]).unwrap();
        client.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
        let reply = client.read_message().expect("the daemon answers on the relay's port");
        assert!(matches!(serde_json::from_slice::<SrvResponse>(&reply).unwrap(), SrvResponse::Handshake(_)));

        // Closing the tab (a NUL byte is the relay's signal) ends the relay's
        // connection to the daemon, and with it the port.
        {
            use std::io::Write;
            relay.stdin.as_mut().unwrap().write_all(&[0]).unwrap();
        }
        relay.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).is_err() {
                break;
            }
            assert!(Instant::now() < deadline, "the port outlived the relay that asked for it");
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}
