//! The bridge with the real processes: `somsrv --srv-bridge` is started the
//! way Som starts it on a remote host (just without ssh in front), and a
//! second connection plays the `somsrp` of that host. What the player
//! announces to the daemon must reach the subscriber that is on the far side
//! of the bridge, and a request from that side must reach the player.

use somsrv::bridge::{BridgeChannel, RemoteBridge};
use somsrv::pipe::PipeConnection;
use somsrv::protocol::{ConnectionKind, ContentMetadata, ContentType, HandshakeInfo, SrvRequest, SrvResponse};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Runs `body` on its own thread and fails the test when it is not done in
/// time: a bridge that swallows a reply leaves the reads below blocked for
/// good, and a test that hangs says nothing.
fn within(limit: Duration, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        body();
        done.send(()).ok();
    });
    assert!(finished.recv_timeout(limit).is_ok(), "no result in {limit:?}: a reply never came through the bridge (or the body panicked)");
}

fn somsrv_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_somsrv"))
}

fn send_over_bridge(channel: &BridgeChannel, request: &SrvRequest) {
    channel.write_message(&serde_json::to_vec(request).unwrap()).expect("writing through the bridge");
}

fn handshake_over_bridge(bridge: &Arc<RemoteBridge>) -> BridgeChannel {
    let channel = bridge.open_channel().expect("opening a channel");
    channel.write_message(&[ConnectionKind::Srv as u8]).expect("connection kind");
    send_over_bridge(&channel, &SrvRequest::Handshake(HandshakeInfo::current()));
    let reply = channel.read_message().expect("the handshake reply through the bridge");
    match serde_json::from_slice::<SrvResponse>(&reply).unwrap() {
        SrvResponse::Handshake(_) => channel,
        other => panic!("expected a Handshake through the bridge, got {other:?}"),
    }
}

/// A connection straight to the daemon of this machine: the remote `somsrp`.
fn player_connection() -> PipeConnection {
    let connection = somsrv::daemon::connect_or_spawn(&somsrv_binary()).expect("reaching the daemon");
    ConnectionKind::Srv.write_to(&connection).unwrap();
    connection.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
    let reply = connection.read_message().unwrap();
    assert!(matches!(serde_json::from_slice::<SrvResponse>(&reply).unwrap(), SrvResponse::Handshake(_)));
    connection
}

fn unique_ids() -> (u32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    (((now & 0xFF_FFFF) as u32).max(1), (((now >> 24) & 0xFF_FFFF) as u32).max(1))
}

fn announce(player: &PipeConnection, session_id: u32, file_id: u32, text: &[u8]) {
    let put = SrvRequest::PutChunk {
        session_id,
        file_id,
        offset: 0,
        data: text.to_vec(),
        total_size: text.len() as u64,
        content_type: ContentType::Markdown,
        metadata: ContentMetadata::Markdown { base_dir: String::new() },
    };
    player.write_message(&serde_json::to_vec(&put).unwrap()).expect("announcing to the daemon");
}

#[test]
fn what_a_remote_player_announces_reaches_the_subscriber_behind_the_bridge() {
    within(Duration::from_secs(60), announced_bytes_and_requests_cross_the_bridge);
}

fn announced_bytes_and_requests_cross_the_bridge() {
    let bridge = RemoteBridge::spawn(somsrv_binary().to_str().unwrap(), &["--srv-bridge".to_string()]).expect("starting the bridge process");
    let (session_id, file_id) = unique_ids();
    let player = player_connection();
    let text = b"# from the other machine\n";

    let subscriber = handshake_over_bridge(&bridge);
    send_over_bridge(&subscriber, &SrvRequest::SubscribeProgress { session_id, file_id });

    // The subscription has no acknowledgement, and the daemon hands a late
    // subscriber no bytes: announce again until the push with the bytes comes.
    let deadline = Instant::now() + Duration::from_secs(20);
    let reader = std::thread::spawn(move || {
        loop {
            let message = subscriber.read_message().expect("the subscription through the bridge");
            if let SrvResponse::Progress { chunk_data, total_size, .. } = serde_json::from_slice::<SrvResponse>(&message).unwrap()
                && !chunk_data.is_empty()
            {
                return (chunk_data, total_size, subscriber);
            }
        }
    });
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "nothing the player announced came through the bridge");
        announce(&player, session_id, file_id, text);
        std::thread::sleep(Duration::from_millis(100));
    }
    let (chunk_data, total_size, subscriber) = reader.join().unwrap();
    assert_eq!(chunk_data, text, "the bytes arrive unchanged");
    assert_eq!(total_size, text.len() as u64);

    // The way back: a request made behind the bridge (a seek, a click) is
    // forwarded by the daemon to the connection that announced the file.
    let clicker = handshake_over_bridge(&bridge);
    send_over_bridge(&clicker, &SrvRequest::RequestByteRange { session_id, file_id, offset: 3, len: 5 });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "the request made behind the bridge never reached the player");
        let message = player.read_message().expect("the player's connection");
        if let Ok(SrvRequest::RequestByteRange { session_id: s, file_id: f, offset, len }) = serde_json::from_slice::<SrvRequest>(&message)
            && (s, f) == (session_id, file_id)
        {
            assert_eq!((offset, len), (3, 5));
            break;
        }
    }

    // Closing the one-shot channel leaves the long-lived one alone.
    drop(clicker);
    announce(&player, session_id, file_id, text);
    drop(subscriber);
}

#[test]
fn a_bridge_whose_process_is_gone_is_reported_down() {
    // A command that is not a bridge: it ends at once without a word.
    let command = if cfg!(windows) { "cmd" } else { "true" };
    let args: Vec<String> = if cfg!(windows) { vec!["/c".into(), "exit".into()] } else { vec![] };
    let bridge = RemoteBridge::spawn(command, &args).expect("starting the stand-in");
    let deadline = Instant::now() + Duration::from_secs(10);
    while bridge.is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!bridge.is_alive(), "a bridge process that exited must be seen as down");
    assert!(bridge.open_channel().is_err());
}

/// Against a real host over real ssh, the way Som starts the bridge:
/// `SOM_BRIDGE_LIVE_HOST=deb cargo test -p somsrv --test bridge_end_to_end -- --ignored live`.
/// The host needs a `somsrv` with `--srv-bridge` at `~/.local/bin/somsrv`
/// (or wherever `SOM_BRIDGE_LIVE_SOMSRV` points).
#[test]
#[ignore]
fn live_bridge_over_ssh_reaches_the_daemon_of_the_remote_host() {
    let Ok(host) = std::env::var("SOM_BRIDGE_LIVE_HOST") else {
        panic!("set SOM_BRIDGE_LIVE_HOST to an ssh host that has somsrv at ~/.local/bin/somsrv");
    };
    let ssh = if cfg!(windows) { r"C:\Windows\System32\OpenSSH\ssh.exe" } else { "ssh" };
    let remote_somsrv = std::env::var("SOM_BRIDGE_LIVE_SOMSRV").unwrap_or_else(|_| "~/.local/bin/somsrv".to_string());
    let args: Vec<String> =
        ["-T", "-o", "BatchMode=yes", host.as_str(), remote_somsrv.as_str(), "--srv-bridge"].iter().map(|arg| arg.to_string()).collect();
    within(Duration::from_secs(60), move || {
        let bridge = RemoteBridge::spawn(ssh, &args).expect("starting ssh");
        let channel = handshake_over_bridge(&bridge);
        send_over_bridge(&channel, &SrvRequest::ListSessions { client_id: None });
        let reply = channel.read_message().expect("the reply of the remote daemon");
        assert!(matches!(serde_json::from_slice::<SrvResponse>(&reply).unwrap(), SrvResponse::Sessions(_)));
    });
}
