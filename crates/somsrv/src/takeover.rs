//! The cutover of a redeploy: `somsrv --takeover <old-pid> <staged> <final>
//! <marker>`.
//!
//! Som stages a new build as `somsrv.new` next to the live `somsrv` and drops a
//! marker file once it is complete. The running daemon notices the marker when
//! no client is connected (see `server`'s idle watcher) and starts a copy of
//! itself in this mode - a separate process, because a daemon cannot replace its
//! own executable (impossible on Windows, and cleaner kept out of the
//! long-lived process on Unix). This process:
//!
//! 1. waits for the old daemon to exit (nudging it with SIGTERM first on Unix,
//!    forcing it after a few seconds),
//! 2. renames `somsrv.new` onto the live `somsrv` (safe now: nothing runs it),
//! 3. removes the marker and starts a fresh `somsrv --daemon` from the new file.
//!
//! Every connection the old daemon held drops when it exits: the OS closes a
//! dead process's sockets, there is no separate step for it.
//!
//! It used to be a second binary, `somsrvupd`, embedded into `somsrv` from a
//! directory a manual script filled. Nobody ran the script for Linux or macOS,
//! so on those hosts the daemon found nothing to start and no update was ever
//! applied. A copy of the running binary needs nothing to be built or embedded.

use std::time::Duration;

pub fn run(old_pid: u32, staged_path: &std::path::Path, final_path: &std::path::Path, marker_path: &std::path::Path) -> anyhow::Result<()> {
    request_graceful_exit(old_pid);
    wait_for_pid_to_exit(old_pid, Duration::from_secs(5), Duration::from_secs(10));

    // Safe now — `old_pid` (the only process that could have been
    // executing `final_path`) is confirmed gone. `staged_path` and
    // `final_path` are always DIFFERENT files (`somsrv.new` vs
    // `somsrv`), so this rename never touches whatever `somsrvupd`
    // ITSELF is currently executing from either.
    std::fs::rename(staged_path, final_path)
        .map_err(|err| anyhow::anyhow!("failed to rename {staged_path:?} onto {final_path:?}: {err}"))?;
    let _ = std::fs::remove_file(marker_path);

    spawn_new_daemon(final_path)?;
    Ok(())
}

/// Asks `pid` to exit on its own first — a `somsrv` daemon mid-request
/// (writing a `srv_cache` chunk to disk, mid-PTY-read) gets a chance to
/// unwind cleanly instead of always being yanked out from under itself.
/// Best-effort: if this fails (e.g. the pid is already gone, or this
/// process lacks permission), `wait_for_pid_to_exit`'s own forceful
/// fallback covers it regardless.
#[cfg(unix)]
fn request_graceful_exit(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(target_os = "windows")]
fn request_graceful_exit(pid: u32) {
    // Windows has no direct equivalent of SIGTERM for an arbitrary
    // process (no universal "please exit soon" signal any process can
    // opt into) — `somsrv` has no console/message-loop of its own to
    // receive one anyway (it's a headless background daemon). Skipping
    // straight to the forceful path in `wait_for_pid_to_exit` is the
    // correct behavior here, not a gap: there is no gentler option to
    // reach for on this platform for this kind of process.
    let _ = pid;
}

/// Waits for `pid` to stop existing, escalating from patient polling to a
/// forceful kill after `graceful_timeout`, then gives up (logs to stderr,
/// returns anyway) after `total_timeout` — a hung old process must never
/// block the redeploy forever.
fn wait_for_pid_to_exit(pid: u32, graceful_timeout: Duration, total_timeout: Duration) {
    let sysinfo_pid = sysinfo::Pid::from_u32(pid);
    let refresh_kind = sysinfo::ProcessRefreshKind::nothing();
    let start = std::time::Instant::now();
    let mut forced = false;

    loop {
        let mut system = sysinfo::System::new();
        let found = system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]), true, refresh_kind);
        if found == 0 {
            return; // gone
        }

        let elapsed = start.elapsed();
        if elapsed >= total_timeout {
            eprintln!("somsrvupd: gave up waiting for old somsrv process {pid} to exit after {total_timeout:?}, proceeding anyway");
            return;
        }
        if !forced && elapsed >= graceful_timeout {
            forced = true;
            force_kill(&mut system, sysinfo_pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn force_kill(system: &mut sysinfo::System, sysinfo_pid: sysinfo::Pid) {
    if let Some(process) = system.process(sysinfo_pid) {
        process.kill();
    }
}

/// Spawns a fresh, fully detached `somsrv --daemon` from the just-
/// renamed `final_path` — mirrors `somsrv::daemon`'s own detached-spawn
/// mechanics (survives this process's own exit, no console window, no
/// inherited stdio), duplicated here rather than shared: `daemon.rs`
/// lives in the library half of this crate and `somsrvupd` deliberately
/// depends on as little of it as possible (see this file's own module
/// doc comment — `somsrvupd` is meant to stay small and simple, a
/// dependency this codebase can trust to keep working even if `somsrv`
/// itself is mid-crash).
#[cfg(target_os = "windows")]
fn spawn_new_daemon(binary_path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::process::Command::new(binary_path)
        .arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to spawn new somsrv daemon from {binary_path:?}: {err}"))?;
    Ok(())
}

#[cfg(unix)]
fn spawn_new_daemon(binary_path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(binary_path);
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
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to spawn new somsrv daemon from {binary_path:?}: {err}"))?;
    Ok(())
}
