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

/// Makes sure the daemon already running on this machine (if one is) is not an
/// older build than `daemon_binary`: when it is, the daemon is stopped - with the
/// sessions it holds; the relays attached to it end as their connection goes -
/// and `true` is returned, so that the next `connect_or_spawn` starts the daemon
/// from the new file. A newer build wins at once, whoever is connected and
/// whatever sessions there are: keeping the machine on one build matters more
/// than what the old one held.
///
/// Called by the clients that are about to talk to the daemon (a relay, Som, a
/// `somsrp`) with the binary they come with; it is how a new `somsrv` that has
/// been put in place (by Som's deploy, by a rebuild) takes over from the old one
/// the moment anything of the new version connects.
///
/// `false` when no daemon is running, the running one is as new, or `daemon_binary`
/// carries no build id (so nothing can be said about it).
pub fn replace_if_stale(daemon_binary: &Path) -> anyhow::Result<bool> {
    if std::env::var_os(KEEP_RUNNING_DAEMON_ENV).is_some() {
        return Ok(false);
    }
    replace_if_stale_at(&daemon_socket_path(), daemon_binary)
}

/// When this environment variable is set, a running daemon is never replaced
/// by [`replace_if_stale`], whatever build it is. For whoever runs several
/// builds side by side (tests, a developer): the clients of the newest would
/// otherwise stop the daemon the others are using, each in turn.
pub const KEEP_RUNNING_DAEMON_ENV: &str = "SOMSRV_KEEP_DAEMON";

fn replace_if_stale_at(socket_path: &str, daemon_binary: &Path) -> anyhow::Result<bool> {
    let bytes = std::fs::read(daemon_binary).with_context(|| format!("cannot read {daemon_binary:?}"))?;
    let Some(expected) = crate::protocol::build_id_in(&bytes) else { return Ok(false) };
    let Some(running) = probe_running_daemon(socket_path) else { return Ok(false) };
    if running.build_id >= expected {
        return Ok(false);
    }
    log::info!("the daemon here is build {} and {daemon_binary:?} is build {expected}: replacing it", running.build_id);
    stop_daemon(running.pid);
    // Gone when nothing answers on its address any more.
    for _ in 0..CONNECT_RETRY_ATTEMPTS {
        if probe_running_daemon(socket_path).is_none() {
            return Ok(true);
        }
        std::thread::sleep(CONNECT_RETRY_DELAY);
    }
    anyhow::bail!("the old daemon still answers after it was stopped")
}

/// The handshake of the daemon on `socket_path`, if one answers; never starts one.
fn probe_running_daemon(socket_path: &str) -> Option<crate::protocol::HandshakeInfo> {
    use crate::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse};
    let connection = PipeConnection::connect(socket_path).ok()?;
    ConnectionKind::Srv.write_to(&connection).ok()?;
    connection.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).ok()?).ok()?;
    match serde_json::from_slice::<SrvResponse>(&connection.read_message().ok()?).ok()? {
        SrvResponse::Handshake(info) => Some(info),
        _ => None,
    }
}

/// Kills the daemon that answered: the process it named in its handshake, or, for
/// a build old enough not to name itself, this user's `somsrv` process started
/// with `--daemon` (a daemon is always started that way). A machine can have a
/// daemon per user, and one user's client has no business with another's.
fn stop_daemon(pid: u32) {
    let own_pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always).with_user(sysinfo::UpdateKind::Always),
    );
    if pid != 0 {
        match system.process(sysinfo::Pid::from_u32(pid)) {
            Some(process) if sysinfo::Pid::from_u32(pid) != own_pid => {
                log::info!("stopping the daemon, process {pid}");
                process.kill();
            },
            _ => log::debug!("the daemon, process {pid}, is already gone"),
        }
        return;
    }
    let Some(own_user) = system.process(own_pid).and_then(|process| process.user_id().cloned()) else {
        log::warn!("cannot tell which user this process runs as: not stopping any daemon");
        return;
    };
    let name = if cfg!(target_os = "windows") { "somsrv.exe" } else { "somsrv" };
    for (pid, process) in system.processes() {
        let is_daemon = process.name().to_string_lossy() == name && process.cmd().iter().any(|arg| arg == "--daemon");
        if is_daemon && *pid != own_pid && process.user_id() == Some(&own_user) {
            log::info!("stopping the daemon, process {pid}");
            process.kill();
        }
    }
}

/// Starts `binary_path` detached (a session of its own, no inherited stdio): it
/// survives the process that started it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse};

    /// A "daemon" on an address of its own that answers one handshake as build
    /// `build_id`, naming `pid` as the process behind it, then is gone. The receiver
    /// gets a message once it has answered.
    fn fake_daemon(socket: &str, build_id: u64, pid: u32) -> std::sync::mpsc::Receiver<()> {
        let listener = crate::pipe::bind(socket).expect("binding the fake daemon's address");
        let (served, served_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let connection = crate::pipe::accept_on(&listener).expect("accepting");
            ConnectionKind::read_from(&connection).expect("the connection kind");
            let request = serde_json::from_slice::<SrvRequest>(&connection.read_message().expect("the handshake")).expect("a request");
            assert!(matches!(request, SrvRequest::Handshake(_)));
            let info = HandshakeInfo { build_id, pid, ..HandshakeInfo::current() };
            connection.write_message(&serde_json::to_vec(&SrvResponse::Handshake(info)).unwrap()).expect("answering");
            served.send(()).ok();
        });
        // On Windows the pipe exists once the thread is waiting in `accept_on`.
        std::thread::sleep(Duration::from_millis(300));
        served_receiver
    }

    /// An address for a fake daemon: a pipe name on Windows, a socket in `scratch` (a
    /// directory of the test's own, removed when the test ends) elsewhere.
    fn unique_socket(scratch: &Path, name: &str) -> String {
        if cfg!(windows) {
            format!(r"\\.\pipe\somsrv-test-{name}-{}", std::process::id())
        } else {
            scratch.join(format!("{name}.sock")).to_string_lossy().into_owned()
        }
    }

    fn binary_of_build(scratch: &Path, build_id: u64) -> PathBuf {
        let path = scratch.join(format!("somsrv-build-{build_id}"));
        std::fs::write(&path, format!("some code SOMSRV-BUILD-ID:{build_id}: more code")).unwrap();
        path
    }

    fn sleeper() -> std::process::Child {
        let (program, args): (&str, &[&str]) = if cfg!(windows) { ("ping", &["-n", "600", "127.0.0.1"]) } else { ("sleep", &["600"]) };
        std::process::Command::new(program).args(args).stdout(std::process::Stdio::null()).spawn().expect("a process to stand for the daemon")
    }

    #[test]
    fn a_daemon_older_than_the_binary_is_stopped_and_only_that_process() {
        let scratch = tempfile::tempdir().unwrap();
        let socket = unique_socket(scratch.path(), "older");
        let mut old_daemon = sleeper();
        let mut bystander = sleeper();
        let served = fake_daemon(&socket, 1_000, old_daemon.id());

        let replaced = replace_if_stale_at(&socket, &binary_of_build(scratch.path(), 2_000)).expect("replacing");
        served.recv_timeout(Duration::from_secs(20)).expect("the fake daemon was never asked");
        assert!(replaced, "an older daemon must be replaced");
        assert!(old_daemon.wait().is_ok(), "the daemon was not stopped");
        assert!(bystander.try_wait().unwrap().is_none(), "a process that was not the daemon was stopped too");
        bystander.kill().unwrap();
        bystander.wait().ok();
    }

    #[test]
    fn a_daemon_as_new_or_newer_than_the_binary_is_left_alone() {
        let scratch = tempfile::tempdir().unwrap();
        for running_build in [2_000, 3_000] {
            let socket = unique_socket(scratch.path(), &format!("current-{running_build}"));
            let mut daemon = sleeper();
            let served = fake_daemon(&socket, running_build, daemon.id());
            assert!(!replace_if_stale_at(&socket, &binary_of_build(scratch.path(), 2_000)).expect("checking"));
            served.recv_timeout(Duration::from_secs(20)).expect("the fake daemon was never asked");
            assert!(daemon.try_wait().unwrap().is_none(), "the daemon was stopped though it was current");
            daemon.kill().unwrap();
            daemon.wait().ok();
        }
    }

    #[test]
    fn nothing_is_done_when_no_daemon_runs_or_the_binary_has_no_id() {
        let scratch = tempfile::tempdir().unwrap();
        assert!(!replace_if_stale_at(&unique_socket(scratch.path(), "none"), &binary_of_build(scratch.path(), 2_000)).expect("checking"));
        let plain = scratch.path().join("somsrv-without-an-id");
        std::fs::write(&plain, b"no id in here").unwrap();
        assert!(!replace_if_stale_at(&unique_socket(scratch.path(), "plain"), &plain).expect("checking"));
    }
}
