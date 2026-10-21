//! The link with real processes: a relay (what an ssh or wsl tab runs on the
//! remote machine) is started with piped stdio standing in for the pty. On
//! "Som's" side a stream extractor takes the frames out of the relay's output
//! and a `SrvLink` writes into its input. A second connection straight to the
//! daemon plays the remote `somsrp`.

use somsrv::link::{Extractor, LinkChannel, SrvLink};
use somsrv::pipe::PipeConnection;
use somsrv::protocol::{ConnectionKind, ContentMetadata, ContentType, HandshakeInfo, SrvRequest, SrvResponse};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fails the test instead of hanging when `body` does not finish.
fn within(limit: Duration, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        body();
        done.send(()).ok();
    });
    assert!(finished.recv_timeout(limit).is_ok(), "no result in {limit:?} (a reply never came through the link, or the body panicked)");
}

struct Tab {
    relay: std::process::Child,
    link: Arc<SrvLink>,
    /// What the terminal parser would have been given: the relay's output
    /// without the frames.
    shown: Arc<Mutex<Vec<u8>>>,
    stdin: Arc<Mutex<std::process::ChildStdin>>,
}

impl Tab {
    fn start() -> Tab {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let pane_id = format!("link-test-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let mut relay = std::process::Command::new(env!("CARGO_BIN_EXE_somsrv"))
            .args(["link-test", pane_id.as_str(), shell])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("starting the relay");
        let stdin = Arc::new(Mutex::new(relay.stdin.take().unwrap()));
        let mut stdout = relay.stdout.take().unwrap();

        let writer = stdin.clone();
        let link = SrvLink::new(Box::new(move |bytes| {
            let mut stdin = writer.lock().unwrap();
            stdin.write_all(bytes)?;
            stdin.flush()
        }));

        let shown = Arc::new(Mutex::new(Vec::new()));
        let (reader_link, reader_shown) = (link.clone(), shown.clone());
        std::thread::spawn(move || {
            let mut extractor = Extractor::default();
            let mut buf = [0u8; 4096];
            loop {
                let Ok(read) = stdout.read(&mut buf) else { return };
                if read == 0 {
                    return;
                }
                let (mut passed, mut bodies) = (Vec::new(), Vec::new());
                extractor.feed(&buf[..read], &mut passed, &mut bodies);
                reader_shown.lock().unwrap().extend_from_slice(&passed);
                for body in &bodies {
                    reader_link.on_frame(body);
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        while !link.is_ready() {
            assert!(Instant::now() < deadline, "the relay never said hello");
            std::thread::sleep(Duration::from_millis(20));
        }
        Tab { relay, link, shown, stdin }
    }

    /// A connection to the daemon behind the link, handshaken.
    fn channel(&self) -> LinkChannel {
        let channel = self.link.open_channel().expect("opening a channel");
        channel.write_message(&[ConnectionKind::Srv as u8]).unwrap();
        channel.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
        let reply = channel.read_message().expect("the handshake reply through the link");
        assert!(matches!(serde_json::from_slice::<SrvResponse>(&reply).unwrap(), SrvResponse::Handshake(_)));
        channel
    }

    /// The tab is closed: a NUL byte is the relay's signal.
    fn close(mut self) {
        self.stdin.lock().unwrap().write_all(&[0]).unwrap();
        self.relay.wait().unwrap();
    }
}

fn player() -> PipeConnection {
    let connection = somsrv::daemon::connect_or_spawn(std::path::Path::new(env!("CARGO_BIN_EXE_somsrv"))).expect("reaching the daemon");
    ConnectionKind::Srv.write_to(&connection).unwrap();
    connection.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
    connection.read_message().unwrap();
    connection
}

fn unique_ids() -> (u32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    (((now & 0xFF_FFFF) as u32).max(1), (((now >> 24) & 0xFF_FFFF) as u32).max(1))
}

fn put(player: &PipeConnection, session_id: u32, file_id: u32, offset: u64, data: &[u8], total_size: u64) {
    let request = SrvRequest::PutChunk {
        session_id,
        file_id,
        offset,
        data: data.to_vec(),
        total_size,
        content_type: ContentType::Markdown,
        metadata: ContentMetadata::Markdown { base_dir: String::new() },
    };
    player.write_message(&serde_json::to_vec(&request).unwrap()).expect("the player announcing");
}

/// Subscribes through the link and returns once the daemon has the
/// subscription (a later request on the same connection is answered only
/// after it).
fn subscribe(channel: &LinkChannel, session_id: u32, file_id: u32) {
    channel.write_message(&serde_json::to_vec(&SrvRequest::SubscribeProgress { session_id, file_id }).unwrap()).unwrap();
    channel.write_message(&serde_json::to_vec(&SrvRequest::ListSessions { client_id: None }).unwrap()).unwrap();
    loop {
        let message = channel.read_message().expect("the subscription through the link");
        if matches!(serde_json::from_slice::<SrvResponse>(&message), Ok(SrvResponse::Sessions(_))) {
            return;
        }
    }
}

/// Reads the pushes of a subscription until `total` bytes have arrived, and
/// returns them in file order.
fn collect(channel: &LinkChannel, session_id: u32, file_id: u32, total: usize) -> Vec<u8> {
    let mut file = vec![0u8; total];
    let mut have = vec![false; total];
    while have.iter().any(|&b| !b) {
        let message = channel.read_message().expect("the transfer through the link");
        if let Ok(SrvResponse::Progress { session_id: s, file_id: f, chunk_offset, chunk_data, .. }) = serde_json::from_slice::<SrvResponse>(&message)
            && (s, f) == (session_id, file_id)
        {
            let start = chunk_offset as usize;
            file[start..start + chunk_data.len()].copy_from_slice(&chunk_data);
            have[start..start + chunk_data.len()].fill(true);
        }
    }
    file
}

#[test]
fn what_a_remote_player_announces_reaches_som_through_the_pty_of_the_tab() {
    within(Duration::from_secs(120), || {
        let tab = Tab::start();
        let (session_id, file_id) = unique_ids();
        let player = player();

        let channel = tab.channel();
        subscribe(&channel, session_id, file_id);

        // A real transfer: pieces of random data, a long run of zeros (what the
        // terminal side's duplicate-read workaround would eat if it could see
        // equal pieces), and text.
        let mut data: Vec<u8> = Vec::new();
        let mut state = 0x1234_5678u32;
        for _ in 0..(1 << 20) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            data.push(state as u8);
        }
        data.extend(std::iter::repeat_n(0u8, 400_000));
        data.extend_from_slice("# a document of the other machine\n".repeat(2000).as_bytes());
        let total = data.len() as u64;
        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + 256 * 1024).min(data.len());
            put(&player, session_id, file_id, offset as u64, &data[offset..end], total);
            offset = end;
        }
        let received = collect(&channel, session_id, file_id, data.len());
        assert!(received == data, "the {} bytes did not arrive unchanged", data.len());

        // The way back: a request made behind the link reaches the player.
        let clicker = tab.channel();
        clicker.write_message(&serde_json::to_vec(&SrvRequest::RequestByteRange { session_id, file_id, offset: 3, len: 5 }).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "the request made through the link never reached the player");
            let message = player.read_message().expect("the player's connection");
            if let Ok(SrvRequest::RequestByteRange { session_id: s, file_id: f, offset, len }) = serde_json::from_slice::<SrvRequest>(&message)
                && (s, f) == (session_id, file_id)
            {
                assert_eq!((offset, len), (3, 5));
                break;
            }
        }

        // The terminal parser is given nothing of the link.
        let shown = tab.shown.lock().unwrap().clone();
        assert!(!shown.windows(5).any(|window| window == b"\x1b_SRV"), "a frame reached the terminal parser");

        drop(clicker);
        drop(channel);
        tab.close();
    });
}

#[test]
fn a_link_that_lost_a_frame_starts_over_and_works_again() {
    within(Duration::from_secs(120), || {
        let tab = Tab::start();
        let first = tab.channel();

        // A frame that is not the next one: as if some had been lost on the way.
        // Written straight into the relay's input as Som's link would have.
        let mut encoder_probe = tab.stdin.lock().unwrap();
        let garbage = b"\x1b_SRV;AAAAAAAAAAAAAAAAAAAAAAAAAAAA\x1b\\";
        encoder_probe.write_all(garbage).unwrap();
        encoder_probe.flush().unwrap();
        drop(encoder_probe);

        // The relay answers a broken stream with a reset, which ends the old
        // channel on this side...
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "the broken frame was never answered with a reset");
            if first.read_message().is_err() {
                break;
            }
        }
        // ...and a new channel works.
        let again = tab.channel();
        drop(again);
        tab.close();
    });
}
