//! Measures how long `som-srv` takes to make a `RequestByteRange`'s
//! target bytes actually readable, against the SAME real, large (16GB+)
//! video file this session's seek-latency bug reports were made against
//! — not a small fixture (seeking on a small file already works fine;
//! the bug is specifically distance/file-size dependent, so a test on a
//! small file would not reproduce it at all).
//!
//! `#[ignore]`d: needs a real multi-GB file on disk at a fixed path, a
//! real `som-srv.exe` daemon process, and takes real wall-clock minutes
//! to stream — not appropriate for a normal `cargo test` run. Run
//! explicitly via `cargo test --release -p som_srv --test
//! seek_latency_bench -- --ignored --nocapture`.
//!
//! This drives the REAL wire protocol end to end, mirroring `somcat`'s
//! own connection shape EXACTLY (confirmed by reading `server.rs`'s
//! `handle_srv_request`): `RequestByteRange` is forwarded back down the
//! SAME connection that sent the first `PutChunk` for a given
//! `(session_id, file_id)` (`SrvCache::register_sender_route`'s implicit,
//! first-`PutChunk`-wins registration) — there is no separate
//! "responder" connection. So one thread here does both roles at once,
//! just like `somcat::srv_channel::SrvChannel` really does: a writer
//! sending sequential `PutChunk`s, and a background reader on the SAME
//! connection watching for an unsolicited `RequestByteRange` to answer.
//! A second, independent connection acts as the "receiver" (exactly what
//! `Terminal`'s `rich_content_srv_channel` does — `SubscribeProgress`,
//! then issue `RequestByteRange` and measure how long the `Progress`
//! push stream takes to report the seek target as readable).

use som_srv::pipe::PipeConnection;
use som_srv::protocol::{ConnectionKind, ContentMetadata, ContentType, HandshakeInfo, SrvRequest, SrvResponse, VideoCodec};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn large_fixture_path() -> Option<std::path::PathBuf> {
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for name in [
        "Ready.or.Not.2.Here.I.Come.2026.1080p.MA.WEB-DLRip.x264-HiDt_EniaHD.mkv",
        "The.Drama.2026.720p.iT.WEB-DLRip.x264_New-Team_il68k.mkv",
    ] {
        let path = repo_root.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn daemon_binary_path() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // deps/
    path.pop(); // release/ or debug/
    path.push(if cfg!(windows) { "som-srv.exe" } else { "som-srv" });
    path
}

fn connect_and_handshake(daemon_binary: &std::path::Path) -> PipeConnection {
    let connection = som_srv::daemon::connect_or_spawn(daemon_binary).expect("connecting to som-srv daemon");
    ConnectionKind::Srv.write_to(&connection).expect("writing connection kind");
    let payload = serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap();
    connection.write_message(&payload).expect("sending handshake");
    let message = connection.read_message().expect("reading handshake reply");
    match serde_json::from_slice::<SrvResponse>(&message).unwrap() {
        SrvResponse::Handshake(_) => {},
        other => panic!("expected Handshake reply, got {other:?}"),
    }
    connection
}

fn video_metadata() -> ContentMetadata {
    ContentMetadata::Video {
        width_px: 1920,
        height_px: 1080,
        fps_numerator: 24,
        fps_denominator: 1,
        codec: VideoCodec::H264,
        audio_stream_index: None,
        subtitle_stream_index: None,
        extension: "mp4".to_string(),
    }
}

fn send_request(connection: &PipeConnection, writer: &Mutex<()>, message: &SrvRequest) {
    let payload = serde_json::to_vec(message).unwrap();
    let _guard = writer.lock().unwrap_or_else(|p| p.into_inner());
    connection.write_message(&payload).expect("sending message");
}

/// One connection doing BOTH jobs `somcat` really does on its single
/// `SrvChannel`: sequentially `PutChunk`s the whole file from a
/// background thread, while this same function's caller-spawned reader
/// thread watches the SAME connection for an unsolicited
/// `RequestByteRange` (forwarded by the daemon per `server.rs`'s
/// `forward_srv_request`) and answers it out-of-order — exactly the real
/// seek-support path being measured.
fn spawn_sender_and_range_responder(
    path: std::path::PathBuf,
    daemon_binary: std::path::PathBuf,
    session_id: u32,
    file_id: u32,
    total_size: u64,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let connection = Arc::new(connect_and_handshake(&daemon_binary));
        let writer = Arc::new(Mutex::new(()));

        // Reader thread: answers RequestByteRange messages the daemon
        // forwards back down this connection, interleaved with the
        // writer thread's own sequential PutChunks — mirrors `somcat`'s
        // real background query-reader thread design (see project
        // memory on `write_lock` guarding concurrent writes to one
        // PipeConnection).
        let reader_connection = connection.clone();
        let reader_writer = writer.clone();
        let reader_path = path.clone();
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::open(&reader_path).expect("opening fixture for range responses");
            loop {
                if reader_stop.load(Ordering::Relaxed) {
                    return;
                }
                let message = match reader_connection.read_message() {
                    Ok(m) => m,
                    Err(_) => return,
                };
                let Ok(request) = serde_json::from_slice::<SrvRequest>(&message) else { continue };
                if let SrvRequest::RequestByteRange { session_id: rsid, file_id: rfid, offset, len } = request {
                    if rsid != session_id || rfid != file_id {
                        continue;
                    }
                    let end = (offset + len).min(total_size);
                    if end <= offset {
                        continue;
                    }
                    let mut buf = vec![0u8; (end - offset) as usize];
                    file.seek(SeekFrom::Start(offset)).expect("seeking for range response");
                    file.read_exact(&mut buf).expect("reading range response bytes");
                    send_request(
                        &reader_connection,
                        &reader_writer,
                        &SrvRequest::PutChunk {
                            session_id,
                            file_id,
                            offset,
                            data: buf,
                            total_size,
                            content_type: ContentType::Video,
                            metadata: video_metadata(),
                        },
                    );
                }
            }
        });

        // Writer: the ordinary sequential send, same shape as `somcat`'s
        // real `stream_file`.
        let mut file = std::fs::File::open(&path).expect("opening fixture for sending");
        const CHUNK_SIZE: usize = 256 * 1024;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut offset = 0u64;
        while offset < total_size {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            file.seek(SeekFrom::Start(offset)).expect("seeking sender file");
            let n = file.read(&mut buf).expect("reading fixture chunk");
            if n == 0 {
                break;
            }
            send_request(
                &connection,
                &writer,
                &SrvRequest::PutChunk {
                    session_id,
                    file_id,
                    offset,
                    data: buf[..n].to_vec(),
                    total_size,
                    content_type: ContentType::Video,
                    metadata: video_metadata(),
                },
            );
            offset += n as u64;
        }

        stop.store(true, Ordering::Relaxed);
        let _ = reader.join();
    })
}

/// Blocks on `SubscribeProgress` pushes until either watermark
/// (`contiguous_len` front, `tail_available_from` back) or a
/// `pending_ranges` entry covers `target_offset`, returning how long
/// that took from the moment this function was called. Mirrors exactly
/// what `GrowingFileStream::read` (crates/terminal/src/
/// rich_content_video_player.rs) itself checks — this is the same
/// three-way readability test, just observed from the wire instead of
/// from inside the decode thread.
fn wait_until_offset_readable(
    daemon_binary: &std::path::Path,
    session_id: u32,
    file_id: u32,
    target_offset: u64,
    timeout: Duration,
) -> Option<Duration> {
    let connection = connect_and_handshake(daemon_binary);
    let writer = Mutex::new(());
    send_request(&connection, &writer, &SrvRequest::SubscribeProgress { session_id, file_id });
    let start = Instant::now();
    while start.elapsed() < timeout {
        let Ok(message) = connection.read_message() else { return None };
        let Ok(response) = serde_json::from_slice::<SrvResponse>(&message) else { continue };
        if let SrvResponse::Progress {
            session_id: rsid, file_id: rfid, contiguous_len, tail_available_from, pending_ranges, total_size, ..
        } = response
        {
            if rsid != session_id || rfid != file_id {
                continue;
            }
            let readable = target_offset < contiguous_len
                || (total_size > 0 && target_offset >= tail_available_from)
                || pending_ranges.iter().any(|&(s, e)| target_offset >= s && target_offset < e);
            if readable {
                return Some(start.elapsed());
            }
        }
    }
    None
}

#[test]
#[ignore]
fn measure_seek_latency_at_increasing_distances_on_a_large_file() {
    let Some(path) = large_fixture_path() else {
        eprintln!("skipping: no large fixture file present at the repo root");
        return;
    };
    let total_size = std::fs::metadata(&path).unwrap().len();
    println!("fixture: {} ({:.2} GB)", path.display(), total_size as f64 / 1e9);

    let daemon_binary = daemon_binary_path();
    assert!(daemon_binary.is_file(), "som-srv.exe not found at {}", daemon_binary.display());

    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let session_id = (now_ms & 0xFF_FFFF).max(1) as u32;
    let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1) as u32;

    let stop = Arc::new(AtomicBool::new(false));
    let _sender = spawn_sender_and_range_responder(path.clone(), daemon_binary.clone(), session_id, file_id, total_size, stop.clone());

    // Give the sender a moment to establish the transfer (first PutChunk
    // creates the cache entry + registers the sender route) before
    // issuing any seeks.
    std::thread::sleep(Duration::from_millis(500));

    // Seek targets at increasing distance from the front — this is
    // exactly the shape of the reported bug ("chem dal'she seek, tem
    // dol'she reaktsiya"): a near-front target should resolve almost
    // instantly (the sequential sender gets there on its own quickly),
    // while a near-end target exercises whether the explicit
    // `RequestByteRange` responder path actually shortcuts the wait, or
    // whether (the bug) the reader is still effectively waiting for
    // sequential delivery.
    let fractions = [0.05_f64, 0.25, 0.50, 0.75, 0.95];
    for fraction in fractions {
        let target_offset = (total_size as f64 * fraction) as u64;
        let connection = connect_and_handshake(&daemon_binary);
        let writer = Mutex::new(());
        send_request(&connection, &writer, &SrvRequest::RequestByteRange { session_id, file_id, offset: target_offset, len: 4 * 1024 * 1024 });
        drop(connection);

        let elapsed = wait_until_offset_readable(&daemon_binary, session_id, file_id, target_offset, Duration::from_secs(120));
        match elapsed {
            Some(d) => println!("fraction={fraction:.2} offset={target_offset} -> readable after {:.3}s", d.as_secs_f64()),
            None => println!("fraction={fraction:.2} offset={target_offset} -> NOT readable within 120s timeout"),
        }
    }

    stop.store(true, Ordering::Relaxed);
}
