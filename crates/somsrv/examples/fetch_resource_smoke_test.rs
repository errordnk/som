//! Manual end-to-end smoke test for `SrvRequest::FetchResource` — proves
//! the whole pipe-connect -> `FetchResource` -> `http_fetch::fetch_and_
//! stream` -> `SrvCache::put_chunk` -> `SubscribeProgress` push pipeline
//! actually works over a real daemon connection, not just that
//! `http_fetch.rs`'s own unit tests (which talk to `SrvCache` directly,
//! no connection/serialization involved) pass. Mirrors `relay_smoke_
//! test.rs`'s own shape (spawn/connect against a REAL `somsrv.exe` next
//! to this example's own binary, not a `cargo test` unit test) since
//! `daemon::connect_or_spawn` needs a real on-disk binary to spawn if no
//! daemon is running yet.
//!
//! Run with: `cargo run -p somsrv --example fetch_resource_smoke_test`
//!
//! Exercises two cases:
//! 1. A local-path fetch (a temp markdown file) — expect `Progress`
//!    pushes whose accumulated bytes match the file exactly.
//! 2. A `FetchResource` for a target with no recognizable content type
//!    (an unmapped extension) — expect a `FetchFailed` reply instead of
//!    any `Progress` push.

use somsrv::daemon;
use somsrv::pipe::PipeConnection;
use somsrv::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse};
use std::time::Duration;

fn send(connection: &PipeConnection, message: &SrvRequest) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    connection.write_message(&payload)?;
    Ok(())
}

fn read(connection: &PipeConnection) -> anyhow::Result<SrvResponse> {
    let message = connection.read_message()?;
    Ok(serde_json::from_slice(&message)?)
}

fn connect_and_handshake() -> anyhow::Result<PipeConnection> {
    let binary_path = daemon::binary_path_next_to_current_exe()?;
    let connection = daemon::connect_or_spawn(&binary_path)?;
    ConnectionKind::Srv.write_to(&connection)?;
    send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
    match read(&connection)? {
        SrvResponse::Handshake(_) => Ok(connection),
        other => anyhow::bail!("expected Handshake as the first response from somsrv, got {other:?}"),
    }
}

/// Reads responses off `connection` until either a `Progress` push
/// carrying `contiguous_len == total_size` (transfer complete) or a
/// `FetchFailed` arrives, or `timeout` elapses — enough for a single-shot
/// smoke test without needing a real byte-accumulating consumer like
/// Som's own `SrvProgressState`.
fn wait_for_outcome(connection: &PipeConnection, timeout: Duration) -> anyhow::Result<SrvResponse> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timed out waiting for Progress/FetchFailed");
        }
        let response = read(connection)?;
        match &response {
            SrvResponse::Progress { contiguous_len, total_size, .. } if contiguous_len >= total_size => {
                return Ok(response);
            }
            SrvResponse::FetchFailed { .. } => return Ok(response),
            _ => continue,
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("FAIL: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // Case 1: local-path fetch of a real markdown file.
    let file_path = std::env::temp_dir().join(format!("somsrv_fetch_smoke_test_{}.md", std::process::id()));
    let file_contents = b"# fetch resource smoke test\n";
    std::fs::write(&file_path, file_contents)?;

    let connection = connect_and_handshake()?;
    let session_id = 0x5eed_0001;
    let file_id = 0x5eed_0002;
    send(&connection, &SrvRequest::SubscribeProgress { session_id, file_id })?;
    send(
        &connection,
        &SrvRequest::FetchResource { session_id, file_id, target: file_path.to_string_lossy().into_owned(), base_dir: None },
    )?;

    let outcome = wait_for_outcome(&connection, Duration::from_secs(5))?;
    std::fs::remove_file(&file_path).ok();
    match outcome {
        SrvResponse::Progress { total_size, .. } if total_size == file_contents.len() as u64 => {
            println!("PASS: local-path FetchResource delivered {total_size} bytes via Progress push");
        }
        other => anyhow::bail!("expected a completed Progress push with total_size={}, got {other:?}", file_contents.len()),
    }

    // Case 2: unrecognized extension -> FetchFailed, no Progress at all.
    let bad_path = std::env::temp_dir().join(format!("somsrv_fetch_smoke_test_bad_{}.xyz", std::process::id()));
    std::fs::write(&bad_path, b"whatever")?;

    let session_id = 0x5eed_0003;
    let file_id = 0x5eed_0004;
    send(&connection, &SrvRequest::SubscribeProgress { session_id, file_id })?;
    send(&connection, &SrvRequest::FetchResource { session_id, file_id, target: bad_path.to_string_lossy().into_owned(), base_dir: None })?;

    let outcome = wait_for_outcome(&connection, Duration::from_secs(5))?;
    std::fs::remove_file(&bad_path).ok();
    match outcome {
        SrvResponse::FetchFailed { reason, .. } => {
            println!("PASS: FetchResource for an unrecognized extension correctly failed: {reason}");
        }
        other => anyhow::bail!("expected FetchFailed for an unrecognized extension, got {other:?}"),
    }

    Ok(())
}
