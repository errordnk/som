//! Spawn-if-not-running helper for the shared `somsrv` daemon, shared by
//! every kind of client that needs one running before it can connect:
//! `crate::relay` (the RELAY a `tmux: true` PTY profile spawns into,
//! re-spawning ITSELF in `--daemon` mode via its own `current_exe()`),
//! `somsrp` (the payload sender, which needs a NEARBY prebuilt `somsrv`
//! binary rather than its own `current_exe()` — it isn't `somsrv`
//! itself), and Som's own `rich_content_srv_channel` (the progress
//! subscriber, same "find a nearby binary" situation as `somsrp`).
//!
//! Kept here (in the library half of this crate, unlike `relay`/`server`/
//! `srv_cache`, which stay private to the `somsrv` binary — see this
//! crate's `lib.rs` doc comment) specifically so it can be called from
//! OUTSIDE this crate without duplicating the detached-process-spawn
//! logic three times over.

use crate::pipe::PipeConnection;
use crate::protocol::daemon_socket_path;
use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_RETRY_ATTEMPTS: u32 = 20;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The `somsrv(.exe)` binary expected next to THIS process's own
/// executable — the deploy convention every client that isn't `somsrv`
/// itself relies on to find it: Som proper (`terminal_view::
/// terminal_panel`, deploying it next to `som.exe` on a remote host, and
/// expecting it locally next to its own `som.exe`), `somsrp` (lives in
/// the same `target/<profile>` directory as `somsrv` in a dev build,
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
/// `target/<profile>/somsrv(.exe)` bin target sits. Without this
/// fallback, every headless test exercising this side-channel (real
/// `somsrv` + real `somsrp` child process) would silently fail to find
/// a `somsrv` that's very much been built — confirmed the hard way as
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
    let binary_name = if cfg!(target_os = "windows") { "somsrv.exe" } else { "somsrv" };
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
    anyhow::bail!("could not connect to somsrv daemon at {socket_path:?} after spawning {daemon_binary_path:?}")
}

/// Spawns a fully detached `somsrv --daemon` process so it outlives
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
    spawn_detached_with_args(daemon_binary_path, &["--daemon"])
}

/// The redeploy marker file's name, sitting next to `somsrv(.exe)` at
/// `~/.local/bin/` — see `check_and_apply_pending_redeploy`'s own doc
/// comment for the full protocol this is part of. Written by Som's
/// `terminal_panel::ensure_remote_binary_deployed` ONLY after `somsrv.
/// new` has been fully uploaded and made executable — its mere presence
/// is the one-and-only signal a complete redeploy is waiting to be
/// applied, so nothing on the `somsrv` side needs to separately verify
/// `.new`'s integrity.
const REDEPLOY_MARKER_NAME: &str = "somsrv.redeploy-pending";

/// The staged new binary's filename, sitting next to `somsrv(.exe)` —
/// what Som's `scp_to_remote` uploads to, and what the takeover renames
/// into place once it's safe to do so.
#[cfg(not(target_os = "windows"))]
const REDEPLOY_STAGED_NAME_UNIX: &str = "somsrv.new";
#[cfg(target_os = "windows")]
const REDEPLOY_STAGED_NAME_WINDOWS: &str = "somsrv.new.exe";

/// Where the redeploy marker is: next to this daemon's own binary.
fn redeploy_marker_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(REDEPLOY_MARKER_NAME))
}

/// Whether Som has staged a new build for this daemon to switch to.
pub fn redeploy_pending() -> bool {
    redeploy_marker_path().is_some_and(|marker| marker.is_file())
}

/// If a redeploy is staged, hands the cutover to a copy of this very binary
/// started as `somsrv --takeover` (see `crate::takeover`) and returns `true`:
/// the caller must then exit at once so the takeover can replace the binary.
/// Returns `false` when nothing is staged or the hand-off failed (the marker is
/// put back, so a later attempt gets another chance).
///
/// Called by the idle watcher in `server` - only when no client is connected.
/// It used to be called on every new connection, which dropped the tabs of a Som
/// that was running and had just connected, and never ran at all on a host where
/// the updater it needed was not embedded.
///
/// The marker's `remove_file` is the atomic claim: only a caller whose removal
/// succeeds goes on, so two of them can never both start a takeover.
pub fn check_and_apply_pending_redeploy() -> bool {
    let Some(marker_path) = redeploy_marker_path() else { return false };
    let Some(parent) = marker_path.parent().map(Path::to_path_buf) else { return false };
    if std::fs::remove_file(&marker_path).is_err() {
        return false; // no marker, or another caller already took it
    }

    log::info!("redeploy marker found at {marker_path:?} - starting the takeover");
    let staged_path = staged_binary_path(&parent);
    let final_path = final_binary_path(&parent);
    match spawn_takeover(std::process::id(), &staged_path, &final_path, &marker_path) {
        Ok(()) => true,
        Err(err) => {
            log::error!("failed to start the takeover, putting the marker back for a later attempt: {err:#}");
            if let Err(err) = std::fs::write(&marker_path, b"") {
                log::error!("and the marker could not be put back: {err}");
            }
            false
        }
    }
}

/// Starts a copy of this binary under the OS temp directory as
/// `somsrv --takeover <old-pid> <staged> <final> <marker>`, detached. A copy,
/// not the binary itself: the takeover replaces that file, which cannot be done
/// while it runs (Windows) and is better not done from the process being
/// replaced (Unix).
fn spawn_takeover(old_pid: u32, staged_path: &Path, final_path: &Path, marker_path: &Path) -> anyhow::Result<()> {
    let suffix = if cfg!(target_os = "windows") { ".exe" } else { "" };
    let temp_dir = std::env::temp_dir();
    // What earlier takeovers left (a running copy cannot be deleted on Windows).
    if let Ok(entries) = std::fs::read_dir(&temp_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("somsrv-takeover-") {
                std::fs::remove_file(entry.path()).ok();
            }
        }
    }
    let this_binary = std::env::current_exe().context("cannot tell where this somsrv is")?;
    let copy = temp_dir.join(format!("somsrv-takeover-{old_pid}{suffix}"));
    std::fs::copy(&this_binary, &copy).with_context(|| format!("failed to copy {this_binary:?} to {copy:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).with_context(|| format!("failed to make {copy:?} executable"))?;
    }
    spawn_detached_with_args(
        &copy,
        &[
            "--takeover",
            &old_pid.to_string(),
            &staged_path.to_string_lossy(),
            &final_path.to_string_lossy(),
            &marker_path.to_string_lossy(),
        ],
    )
}

/// Full path to the staged `.new` binary next to `parent` (the directory
/// `somsrv(.exe)` itself lives in) — platform-specific filename (see
/// `REDEPLOY_STAGED_NAME_UNIX`/`_WINDOWS`'s own doc comment).
fn staged_binary_path(parent: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        parent.join(REDEPLOY_STAGED_NAME_WINDOWS)
    }
    #[cfg(not(target_os = "windows"))]
    {
        parent.join(REDEPLOY_STAGED_NAME_UNIX)
    }
}

/// Full path `.new` gets renamed TO — the ordinary `somsrv(.exe)` name,
/// matching `binary_path_next_to_current_exe`'s own naming convention.
fn final_binary_path(parent: &Path) -> PathBuf {
    let binary_name = if cfg!(target_os = "windows") { "somsrv.exe" } else { "somsrv" };
    parent.join(binary_name)
}

/// Like `spawn_detached`, but with caller-provided `args` instead of the
/// hardcoded `["--daemon"]` — used for spawning the takeover (a different
/// mode of this binary, with its own argv), sharing the same
/// detached-process mechanics (survive the spawning process's own exit,
/// no console window, no inherited stdio) `spawn_detached` already has.
#[cfg(target_os = "windows")]
fn spawn_detached_with_args(binary_path: &Path, args: &[&str]) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::process::Command::new(binary_path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
fn spawn_detached_with_args(binary_path: &Path, args: &[&str]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(binary_path);
    command.args(args);
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
