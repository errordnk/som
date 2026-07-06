//! Client-side connection management for `som-tmux`: connecting to an
//! already-running `som-tmux-server` for a profile, or spawning one
//! detached (so it outlives Som) when none is listening yet.
//!
//! Windows-only for now, matching the server's current transport (a named
//! pipe) — see `SOM_MUX_PLAN.md`'s "Where the server actually runs" section
//! for the WSL/SSH transports this doesn't cover yet.

use anyhow::{Context as _, Result, bail};
use som_tmux_server::pipe::PipeConnection;
use som_tmux_server::protocol::pipe_name;
use std::time::Duration;

const CONNECT_RETRY_ATTEMPTS: u32 = 20;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Connects to the profile's `som-tmux-server`, spawning it (detached) if
/// nothing answers yet. Blocking — call this from a background task (e.g.
/// `cx.background_spawn`), never directly on the UI thread.
///
/// Serialized per-profile with a named OS mutex (see `spawn_lock`) around
/// the whole "did I fail to connect, then spawn" decision — without it, two
/// callers racing on the SAME profile (e.g. `restore_som_tabs` restoring one
/// tmux tab while a health-check reconnect is running for another, or two
/// Som windows/processes touching the same profile) can both observe "no
/// server answering" at the same moment and both spawn one. Windows' named
/// pipes allow multiple same-named instances (`PIPE_UNLIMITED_INSTANCES`),
/// so this doesn't fail loudly — it silently produces two live
/// `som-tmux-server.exe` processes for one profile, splitting later clients
/// between them at random and making sessions "disappear" depending on which
/// one a given `Attach` happens to land on. Found exactly this way: a real
/// `som-tmux-server-dnk.log` had two processes' log lines interleaved
/// mid-write, and `tasklist` showed two `som-tmux-server.exe` for the same
/// profile.
pub fn connect_or_spawn(profile_name: &str) -> Result<PipeConnection> {
    let name = pipe_name(profile_name);
    if let Ok(connection) = PipeConnection::connect(&name) {
        return Ok(connection);
    }

    let _guard = spawn_lock(profile_name)?;
    // Re-check now that the lock is held — whoever held it before this
    // caller may well have just spawned the server this caller needs, in
    // which case connecting directly beats spawning a redundant second one.
    //
    // This retries a few times (short delay, no fresh spawn attempt) rather
    // than falling straight through to `spawn_detached_server` on the first
    // failure: a server another thread just spawned and successfully
    // connected to (releasing this lock right after) needs a brief moment
    // to loop back to its accept call and be ready for the NEXT client —
    // one successful connection doesn't mean the server can immediately
    // accept a second one. Without this, two callers racing on the same
    // profile could both hold the lock in sequence, both see "nothing
    // answering" in this narrow window, and both spawn their own server —
    // found by testing: 8 threads racing on one profile produced 2 live
    // server processes even with the lock in place, until this retry was
    // added.
    const POST_LOCK_RETRY_ATTEMPTS: u32 = 10;
    const POST_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);
    for attempt in 0..POST_LOCK_RETRY_ATTEMPTS {
        if let Ok(connection) = PipeConnection::connect(&name) {
            return Ok(connection);
        }
        if attempt + 1 < POST_LOCK_RETRY_ATTEMPTS {
            std::thread::sleep(POST_LOCK_RETRY_DELAY);
        }
    }

    spawn_detached_server(profile_name)?;

    for attempt in 0..CONNECT_RETRY_ATTEMPTS {
        match PipeConnection::connect(&name) {
            Ok(connection) => return Ok(connection),
            Err(_) if attempt + 1 < CONNECT_RETRY_ATTEMPTS => {
                std::thread::sleep(CONNECT_RETRY_DELAY);
            }
            Err(err) => return Err(err),
        }
    }
    bail!("could not connect to som-tmux-server for profile {profile_name:?} after spawning it")
}

/// Spawns `som-tmux-server.exe <profile>` as a fully detached process that
/// survives Som exiting/crashing — the entire point of som-tmux (see
/// `project_som_tmux` memory / `SOM_MUX_PLAN.md`).
///
/// Uses `DETACHED_PROCESS` rather than the `CREATE_NO_WINDOW` flag
/// `util::command` uses elsewhere in this codebase — the two are mutually
/// exclusive per Microsoft's docs (undefined behavior if both are set), and
/// `CREATE_NO_WINDOW` alone doesn't address the actual requirement here
/// (surviving the parent exiting).
///
/// Deliberately does NOT add `CREATE_NEW_PROCESS_GROUP` or
/// `CREATE_BREAKAWAY_FROM_JOB` — adding either of those caused
/// `CreateProcessW` to fail with `ERROR_ACCESS_DENIED` (confirmed by manual
/// testing: `DETACHED_PROCESS` alone works, adding the other two together
/// broke it). This repo doesn't put its own process in a Windows Job Object
/// anywhere (confirmed via research), so breakaway isn't needed in
/// practice; if that ever changes, revisit this rather than blindly
/// re-adding the flag.
#[cfg(target_os = "windows")]
fn spawn_detached_server(profile_name: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let server_path = server_binary_path()?;

    std::process::Command::new(server_path)
        .arg(profile_name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .context("failed to spawn som-tmux-server")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn server_binary_path() -> Result<std::path::PathBuf> {
    let exe_dir = std::env::current_exe()
        .context("failed to determine Som's own executable path")?
        .parent()
        .context("Som's executable path has no parent directory")?
        .to_path_buf();
    let candidate = exe_dir.join("som-tmux-server.exe");
    if !candidate.is_file() {
        bail!("som-tmux-server.exe not found next to Som's own executable at {candidate:?}");
    }
    Ok(candidate)
}

/// RAII handle for a named Windows mutex — held only for the duration of
/// `connect_or_spawn`'s "should I spawn a server for this profile?" check,
/// released (via `Drop`) as soon as that function returns either way. Not a
/// long-lived lock on the server itself, just enough to make the
/// check-then-spawn decision atomic across every caller of
/// `connect_or_spawn` for the same profile, in-process or across processes.
#[cfg(target_os = "windows")]
struct SpawnLock(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for SpawnLock {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Named per-profile so unrelated profiles never contend with each other —
/// only callers racing on the SAME profile's server need to be serialized.
/// A plain (non-`Global\`) name is session-scoped, which is the right scope
/// here: this only needs to coordinate processes that could plausibly reach
/// the same named pipe, and named pipes without `Global\` are session-scoped
/// too (see `som_tmux_server::protocol::pipe_name`).
#[cfg(target_os = "windows")]
fn spawn_lock(profile_name: &str) -> Result<SpawnLock> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Threading::{CreateMutexW, INFINITE, WaitForSingleObject};
    use windows::core::PCWSTR;

    let name: Vec<u16> = std::ffi::OsStr::new(&format!("som-tmux-spawn-lock-{profile_name}"))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .context("failed to create som-tmux spawn-lock mutex")?;
    unsafe { WaitForSingleObject(handle, INFINITE) };
    Ok(SpawnLock(handle))
}

#[cfg(test)]
mod spawn_race_tests {
    //! Real (non-mocked) regression test for the double-spawn bug: several
    //! threads calling `connect_or_spawn` for the SAME never-before-seen
    //! profile at once used to each observe "nothing answers" and each spawn
    //! their own `som-tmux-server.exe` — Windows named pipes tolerate
    //! multiple same-named instances, so this didn't fail loudly, it just
    //! quietly left two live server processes for one profile with clients
    //! split between them at random. See `connect_or_spawn`'s doc comment for
    //! how this was actually found in a real `~/.config/som/logs/
    //! som-tmux-server-dnk.log` (interleaved writes from two processes).
    //!
    //! Requires `som-tmux-server.exe` next to the test binary — same
    //! constraint as `som_tmux_session`'s `reconnect_tests`, see that
    //! module's doc comment for the exact command to run this with.
    //! Deliberately no keystrokes/UI involved (see `feedback_autoit_focus_check`
    //! memory) — pure process/pipe-level verification.

    use super::*;
    use std::sync::Arc;

    #[test]
    #[ignore = "requires som-tmux-server.exe next to the test binary; see module doc comment"]
    fn concurrent_connect_or_spawn_yields_exactly_one_server() {
        let profile = format!("test-spawn-race-{}", uuid::Uuid::new_v4());
        let profile = Arc::new(profile);

        // Each thread creates a real session (not just a bare connect) and
        // keeps its connection open — a connection that never creates a
        // session doesn't stop the server from self-terminating the moment
        // ANY connection ends (see `som_tmux_server::server`'s "0 sessions ->
        // exit"), which made an earlier version of this test pass for the
        // wrong reason (no server left to count, rather than exactly one
        // surviving). Real panes always create a session before this
        // function would even be relevant to them, so this matches actual
        // usage.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let profile = profile.clone();
                std::thread::spawn(move || -> Result<PipeConnection> {
                    let connection = connect_or_spawn(&profile)?;
                    let payload = serde_json::to_vec(&som_tmux_server::protocol::ClientMessage::NewSession {
                        program: "C:\\Windows\\System32\\cmd.exe".into(),
                        args: vec![],
                        cwd: None,
                        cols: 80,
                        rows: 24,
                    })?;
                    connection.write_message(&payload)?;
                    let _ = connection.read_message()?; // wait for SessionCreated
                    Ok(connection)
                })
            })
            .collect();

        let connections: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for connection in &connections {
            assert!(connection.is_ok(), "every concurrent caller should still get a working connection");
        }

        // Give any wrongly-spawned second server a moment to have started
        // and registered itself, then check exactly one process is running
        // for this profile's server binary with this profile as an argument.
        std::thread::sleep(Duration::from_millis(500));
        let count = count_processes_with_arg("som-tmux-server.exe", &profile);
        drop(connections); // hold every connection (and its session) open up to here, on purpose
        assert_eq!(count, 1, "exactly one som-tmux-server.exe should exist for this profile, found {count}");
    }

    /// Counts every running `image_name` process system-wide via
    /// `tasklist` — NOT matched by command line (`tasklist` doesn't expose
    /// it, and `wmic`/`Get-CimInstance`-based approaches either don't exist
    /// on this Windows version or silently returned empty output with exit
    /// code 0 when launched via `std::process::Command`, despite identical
    /// scripts working fine typed directly into an interactive shell — never
    /// root-caused). Correct for this test specifically only because each
    /// run kills every stray `som-tmux-server.exe` first and uses a random
    /// UUID profile name that nothing else on the system would be running —
    /// NOT a generally reusable "count processes for this profile" helper.
    #[cfg(target_os = "windows")]
    fn count_processes_with_arg(image_name: &str, _arg: &str) -> usize {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {image_name}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("failed to run tasklist to count processes");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with(&format!("\"{image_name}\"")))
            .count()
    }
}
