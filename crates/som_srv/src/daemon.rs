//! Spawn-if-not-running helper for the shared `som-srv` daemon, shared by
//! every kind of client that needs one running before it can connect:
//! `crate::relay` (the RELAY a `tmux: true` PTY profile spawns into,
//! re-spawning ITSELF in `--daemon` mode via its own `current_exe()`),
//! `somcat` (the payload sender, which needs a NEARBY prebuilt `som-srv`
//! binary rather than its own `current_exe()` — it isn't `som-srv`
//! itself), and Som's own `rich_content_srv_channel` (the progress
//! subscriber, same "find a nearby binary" situation as `somcat`).
//!
//! Kept here (in the library half of this crate, unlike `relay`/`server`/
//! `srv_cache`, which stay private to the `som-srv` binary — see this
//! crate's `lib.rs` doc comment) specifically so it can be called from
//! OUTSIDE this crate without duplicating the detached-process-spawn
//! logic three times over.

use crate::pipe::PipeConnection;
use crate::protocol::daemon_socket_path;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_RETRY_ATTEMPTS: u32 = 20;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The `som-srv(.exe)` binary expected next to THIS process's own
/// executable — the deploy convention every client that isn't `som-srv`
/// itself relies on to find it: Som proper (`terminal_view::
/// terminal_panel`, deploying it next to `som.exe` on a remote host, and
/// expecting it locally next to its own `som.exe`), `somcat` (lives in
/// the same `target/<profile>` directory as `som-srv` in a dev build,
/// and is expected to be deployed the same way in a packaged build), and
/// `crates/terminal`'s `rich_content_srv_channel` (runs inside Som's own
/// process, so "next to current_exe" means the same thing as it does for
/// `terminal_panel`'s copy of this lookup — the two are meant to always
/// agree, which is exactly why this is one shared function instead of
/// two independently-maintained copies).
///
/// Also checks one directory up from `current_exe()`'s parent — needed
/// for `#[gpui::test]`/`cargo test` binaries specifically, which `cargo`
/// places in `target/<profile>/deps/`, one level deeper than the real
/// `target/<profile>/som-srv(.exe)` bin target sits. Without this
/// fallback, every headless test exercising this side-channel (real
/// `som-srv` + real `somcat` child process) would silently fail to find
/// a `som-srv` that's very much been built — confirmed the hard way as
/// `rich_content_placements()`/`rich_content_video_placements()` never
/// seeing a placement at all, tracing back to `spawn_progress_listener`'s
/// background thread failing to connect and giving up silently (that
/// failure mode is itself intentional, see its own doc comment — this
/// fixes the actual missing-binary bug, not the tolerance for it).
pub fn binary_path_next_to_current_exe() -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .map_err(|err| anyhow::anyhow!("failed to determine this process's own executable path: {err}"))?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("this process's executable path has no parent directory"))?
        .to_path_buf();
    let binary_name = if cfg!(target_os = "windows") { "som-srv.exe" } else { "som-srv" };
    for candidate_dir in [exe_dir.clone(), exe_dir.parent().map(Path::to_path_buf).unwrap_or_default()] {
        let candidate = candidate_dir.join(binary_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("{binary_name} not found next to this process's own executable at {exe_dir:?} (or its parent)")
}

/// Tries to connect to the daemon's fixed socket first; if nothing's
/// listening, spawns a detached instance of `daemon_binary_path` in
/// `--daemon` mode and retries until it comes up (or gives up after
/// [`CONNECT_RETRY_ATTEMPTS`]). Returns the raw, untagged connection —
/// callers still need to write their own [`crate::protocol::ConnectionKind`]
/// tag before anything else, since that tag (`Relay` vs `Srv`) is
/// caller-specific.
pub fn connect_or_spawn(daemon_binary_path: &Path) -> anyhow::Result<PipeConnection> {
    let socket_path = daemon_socket_path();

    if let Ok(connection) = PipeConnection::connect(&socket_path) {
        return Ok(connection);
    }

    spawn_detached(daemon_binary_path)?;

    for attempt in 0..CONNECT_RETRY_ATTEMPTS {
        match PipeConnection::connect(&socket_path) {
            Ok(connection) => return Ok(connection),
            Err(_) if attempt + 1 < CONNECT_RETRY_ATTEMPTS => std::thread::sleep(CONNECT_RETRY_DELAY),
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("could not connect to som-srv daemon at {socket_path:?} after spawning {daemon_binary_path:?}")
}

/// Spawns a fully detached `som-srv --daemon` process so it outlives
/// whichever short-lived client spawned it — see `crate::relay`'s own
/// `spawn_detached_daemon` (this function's predecessor, before it moved
/// here to be shared) for the full history behind the exact
/// flags/mechanism used on each platform.
#[cfg(target_os = "windows")]
fn spawn_detached(daemon_binary_path: &Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::process::Command::new(daemon_binary_path)
        .arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
fn spawn_detached(daemon_binary_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(daemon_binary_path);
    command.arg("--daemon");
    unsafe {
        // SAFETY: `setsid()` is async-signal-safe and the only thing done
        // here between fork and exec — no allocation, no locking, exactly
        // what `pre_exec`'s safety contract requires.
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}
