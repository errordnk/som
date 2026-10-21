//! The forward Som puts on a tab's ssh command, against a real host:
//! `SOM_LIVE_HOST=deb cargo test -p somsrv --test ssh_forward_live -- --ignored`.
//! The host needs a `somsrv` daemon that publishes the socket alias (any
//! daemon of this version started after the alias was added).

use somsrv::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse, ssh_forward_spec};
use somsrv::tcp::TcpConnection;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn live_forward_over_ssh_reaches_the_daemon_of_the_remote_host() {
    let host = std::env::var("SOM_LIVE_HOST").expect("set SOM_LIVE_HOST to an ssh host that runs a somsrv daemon");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ssh = if cfg!(windows) { r"C:\Windows\System32\OpenSSH\ssh.exe" } else { "ssh" };
    let mut forward = std::process::Command::new(ssh)
        .args(["-N", "-T", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes", "-L", &ssh_forward_spec(port), &host])
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("starting ssh");

    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(30);
    let connection = loop {
        assert!(Instant::now() < deadline, "the forward never came up");
        if let Ok(connection) = TcpConnection::connect(address, Duration::from_secs(2)) {
            break connection;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    connection.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    connection.write_message(&[ConnectionKind::Srv as u8]).unwrap();
    connection.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
    let reply = connection.read_message().expect("the remote daemon answers through the forward");
    let reply = serde_json::from_slice::<SrvResponse>(&reply).unwrap();
    forward.kill().unwrap();
    forward.wait().unwrap();
    assert!(matches!(reply, SrvResponse::Handshake(_)), "{reply:?}");
}
