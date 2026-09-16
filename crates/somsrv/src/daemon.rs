//! Spawn-if-not-running helper for the shared `somsrv` daemon, shared by
//! every kind of client that needs one running before it can connect:
//! `crate::relay` (the RELAY a `tmux: true` PTY profile spawns into,
//! re-spawning ITSELF in `--daemon` mode via its own `current_exe()`),
//! `somcat` (the payload sender, which needs a NEARBY prebuilt `somsrv`
//! binary rather than its own `current_exe()` — it isn't `somsrv`
//! itself), and Som's own `rich_content_srv_channel` (the progress
//! subscriber, same "find a nearby binary" situation as `somcat`).
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
/// expecting it locally next to its own `som.exe`), `somcat` (lives in
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
/// `somsrv` + real `somcat` child process) would silently fail to find
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
/// what Som's `scp_to_remote` uploads to, and what `somsrvupd` renames
/// into place once it's safe to do so.
#[cfg(not(target_os = "windows"))]
const REDEPLOY_STAGED_NAME_UNIX: &str = "somsrv.new";
#[cfg(target_os = "windows")]
const REDEPLOY_STAGED_NAME_WINDOWS: &str = "somsrv.new.exe";

/// `somsrvupd`'s own compiled bytes, embedded directly into `somsrv` at
/// BUILD time via `rust-embed` — mirrors exactly how Som's own `assets`
/// crate embeds a pre-built `somsrv` for each remote platform
/// (`#[include = "srv/..."]` in `assets.rs`). Reads from `embedded-upd/`
/// (a sibling of `src/` in this crate), which `scripts/update-srv-
/// binaries.sh` populates as a manual, pre-release step — `somsrvupd` must
/// be built and copied there BEFORE `somsrv` itself is built, the same
/// two-pass requirement `assets/srv/{platform}/` already has for
/// `somsrv` inside `som` itself. Re-extracted to disk FRESH on every
/// single redeploy (see `check_and_apply_pending_redeploy`) rather than
/// reused if a copy happens to already be sitting there — this
/// guarantees whatever `somsrvupd` binary actually runs is always the one
/// bundled with the CURRENTLY installed `somsrv`, never a stale
/// leftover from some earlier version that happened to survive on disk.
#[derive(rust_embed::RustEmbed)]
#[folder = "embedded-upd"]
struct EmbeddedUpd;

fn somsrvupd_bytes() -> anyhow::Result<std::borrow::Cow<'static, [u8]>> {
    let name = if cfg!(target_os = "windows") { "somsrvupd.exe" } else { "somsrvupd" };
    EmbeddedUpd::get(name).map(|file| file.data).ok_or_else(|| anyhow::anyhow!("somsrvupd not embedded for this platform (expected {name:?} in embedded-upd/)"))
}

/// Checks for `REDEPLOY_MARKER_NAME` next to `this_binary_path` and, if
/// present, hands the entire cutover off to a freshly-extracted `somsrvupd`
/// process — see that binary's own module doc comment (`bin/somsrvupd.rs`)
/// for the full rename-and-restart sequence, which necessarily runs in a
/// SEPARATE process: this running daemon can't safely replace/delete its
/// own executable file while it's still the one executing it, especially
/// on Windows, where the OS holds an exclusive lock on a running image's
/// file.
///
/// Called from `server::run`'s connection-accept loop itself (the single
/// sequential thread that owns `pipe::accept_on`), right before
/// dispatching each new connection to its own handler thread — this is
/// deliberately checked on every single new connection attempt, not once
/// at startup, since a redeploy can be staged by Som at any point while
/// this daemon has already been running for a while. Because the accept
/// loop is single-threaded, there is in practice never more than one
/// caller of this function alive at once WITHIN this process — but the
/// marker file's `std::fs::remove_file` is still the mechanism that
/// makes this safe even if that ever changed (or across a hypothetical
/// second daemon process briefly coexisting): it's atomic at the OS
/// level, so whichever caller's `remove_file` call actually succeeds is
/// the one — and only one — that proceeds to spawn `somsrvupd`; anyone
/// else sees the file already gone and returns `false` immediately, no
/// `Mutex`/`AtomicBool` needed. If spawning `somsrvupd` itself then fails
/// (rare — a filesystem/exec problem, not a race), the marker is WRITTEN
/// BACK so a later connection gets another chance rather than the
/// redeploy silently vanishing forever.
///
/// Returns `true` if a redeploy was found and handed off — the caller
/// must then close every existing connection and exit immediately
/// (WITHOUT accepting the connection that triggered this check; that
/// connection's own client will simply reconnect once the new daemon is
/// up, same as it already tolerates any other daemon restart). Returns
/// `false` if nothing needs to happen (no marker, or another thread
/// already claimed it), in which case the caller proceeds normally.
pub fn check_and_apply_pending_redeploy() -> bool {
    let Ok(this_binary_path) = std::env::current_exe() else { return false };
    let Some(parent) = this_binary_path.parent() else { return false };
    let marker_path = parent.join(REDEPLOY_MARKER_NAME);

    // The atomic hand-off point — see this function's own doc comment.
    // Only the thread whose `remove_file` call actually succeeds
    // continues past this point.
    if std::fs::remove_file(&marker_path).is_err() {
        return false; // no marker, or another thread/process already took it
    }

    log::info!("redeploy marker found at {marker_path:?} — extracting and spawning somsrvupd to take over");
    let staged_path = staged_binary_path(parent);
    let final_path = final_binary_path(parent);
    let this_pid = std::process::id();

    match extract_and_spawn_somsrvupd(this_pid, &staged_path, &final_path, &marker_path) {
        Ok(()) => true,
        Err(err) => {
            log::error!("failed to hand off the redeploy to somsrvupd, restoring the marker for a later attempt: {err:#}");
            // Best-effort: put the marker back so a LATER connection gets
            // another chance — this failure was in spawning the helper
            // (a filesystem/exec problem on THIS side), not anything
            // that would make retrying pointless the way a bad `.new`
            // download would.
            let _ = std::fs::write(&marker_path, b"");
            false
        }
    }
}

/// Extracts the embedded `somsrvupd` to a fresh copy under the OS temp
/// directory (`std::env::temp_dir()` — `/tmp` on Linux/macOS, both
/// commonly tmpfs-backed so this never actually touches a physical disk
/// there; `%TEMP%` on Windows, which has no equivalent in-memory
/// guarantee but is still the closest standard-API option available —
/// see this module's own doc comment for why a platform split beyond
/// this wasn't worth it: `memfd_create` is Linux-only, and the Windows/
/// macOS alternatives to it are either fragile or commonly flagged by
/// antivirus software for looking like process-hollowing malware), makes
/// it executable on Unix, spawns it detached with the four positional
/// args it expects (`bin/somsrvupd.rs`'s own `main` documents the exact
/// protocol), and deletes the temp copy immediately after a successful
/// spawn — the OS has already loaded the new `somsrvupd` process's own
/// image into memory by the time `spawn` returns, so the file on disk
/// has done its job and doesn't need to persist for the child's sake.
/// A fresh copy is written on EVERY redeploy (never reused even if a
/// leftover happens to still be sitting there) — see `somsrvupd_bytes`'s
/// own doc comment for why that freshness matters.
fn extract_and_spawn_somsrvupd(old_pid: u32, staged_path: &Path, final_path: &Path, marker_path: &Path) -> anyhow::Result<()> {
    let somsrvupd_path = std::env::temp_dir().join(if cfg!(target_os = "windows") { "somsrvupd.exe" } else { "somsrvupd" });
    let bytes = somsrvupd_bytes()?;
    std::fs::write(&somsrvupd_path, bytes.as_ref()).with_context(|| format!("failed to extract somsrvupd to {somsrvupd_path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&somsrvupd_path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to make {somsrvupd_path:?} executable"))?;
    }

    let spawn_result = spawn_detached_with_args(
        &somsrvupd_path,
        &[
            &old_pid.to_string(),
            &staged_path.to_string_lossy(),
            &final_path.to_string_lossy(),
            &marker_path.to_string_lossy(),
        ],
    );

    // Best-effort cleanup either way: on success the OS already has the
    // new process's image loaded, so the on-disk copy is no longer
    // needed; on failure there's nothing useful left to clean up FOR
    // (the caller's own error path already restores the redeploy marker
    // for a later attempt), but leaving a stray `somsrvupd(.exe)` in the
    // temp directory serves no purpose either way.
    let _ = std::fs::remove_file(&somsrvupd_path);

    spawn_result
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
/// hardcoded `["--daemon"]` — used for spawning `somsrvupd` (a different
/// binary entirely, with its own positional argv), sharing the same
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
