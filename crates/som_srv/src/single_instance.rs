//! Guards against two `som-srv --daemon` processes running at once.
//!
//! Found live: Windows named pipes accept any number of independent
//! `CreateNamedPipeW` instances under the same name (`PIPE_UNLIMITED_
//! INSTANCES`, needed for the normal case of many clients connecting to
//! ONE daemon's accept loop — see `pipe::windows`'s own module doc
//! comment). That means `daemon::connect_or_spawn`'s "connect, and if
//! nothing answers, spawn a new daemon" race has no protection at the
//! transport level: two clients that both find nothing listening at the
//! same moment (e.g. two rich-content placements opening within the same
//! ~100ms window right after a fresh boot) can each spawn their own
//! `som-srv --daemon`, and BOTH will succeed at binding the pipe name —
//! there is no equivalent of `EADDRINUSE` for named pipes. The result:
//! two daemons silently coexist, each with its own empty session/cache
//! registry, and whichever one a given client's `PipeConnection::connect`
//! happens to land on (Windows fans out new connections across whichever
//! instances are currently in `ConnectNamedPipe`) becomes unpredictable
//! per-connection — confirmed live as widespread playback breakage after
//! a burst of preview activity, traced to exactly two `som-srv.exe`
//! processes running simultaneously.
//!
//! The fix is an OS-level mutual-exclusion primitive acquired BEFORE
//! `pipe::bind`, so the daemon can tell "am I actually the first/only
//! instance" atomically instead of inferring it from whether the pipe
//! connect succeeded (which is exactly the racy check that caused this).
//! A second `--daemon` invocation that loses the race exits immediately
//! (code 0, not an error — losing this race is the expected, correct
//! outcome for a spawn-if-not-running client, not a failure) rather than
//! also binding the pipe name.

/// Acquires the machine-wide (Windows) or per-uid (Unix, matching
/// `protocol::daemon_socket_path`'s own per-uid socket convention) daemon
/// lock. Returns `Ok(true)` if this process now holds it (the caller
/// should proceed to `pipe::bind` and run normally), `Ok(false)` if
/// another process already holds it (the caller should exit cleanly).
///
/// The returned guard must be kept alive (not dropped) for as long as
/// this process wants to keep holding the lock — dropping it (or process
/// exit, which the OS treats identically for both mechanisms below)
/// releases it immediately.
pub fn try_acquire_daemon_lock() -> anyhow::Result<Option<DaemonLockGuard>> {
    imp::try_acquire()
}

// Held only for its `Drop` impl (releasing the lock on scope exit) — never
// read directly, hence the field itself looking unused to the compiler.
#[allow(dead_code)]
pub struct DaemonLockGuard(imp::PlatformGuard);

#[cfg(windows)]
mod imp {
    use super::DaemonLockGuard;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    pub struct PlatformGuard(HANDLE);

    impl Drop for PlatformGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0).ok();
            }
        }
    }

    /// A named kernel mutex, NOT a named pipe — deliberately a different
    /// Win32 object/namespace than `protocol::daemon_socket_path`'s pipe,
    /// so this guard's own lifetime never interacts with pipe instance
    /// counting at all. `CreateMutexW` is atomic: if an object with this
    /// name already exists, `GetLastError()` reports `ERROR_ALREADY_
    /// EXISTS` even though the call itself still returns a valid (extra)
    /// handle to the SAME underlying mutex — that handle is closed
    /// immediately in the `false` branch rather than kept, since this
    /// process didn't win the race and must not act as if it holds the
    /// lock.
    pub fn try_acquire() -> anyhow::Result<Option<DaemonLockGuard>> {
        // No `Global\` prefix deliberately — that namespace needs
        // `SeCreateGlobalPrivilege`, which isn't guaranteed for every
        // session (e.g. some restricted/service contexts). The default,
        // session-local namespace already matches what's actually wanted
        // here: `daemon_socket_path`'s own pipe name has no per-session
        // scoping either, so a session-local mutex and a session-global
        // pipe name landing in the same practical scope (one interactive
        // user session on this machine) is the right pairing.
        let name: Vec<u16> = "som-srv-daemon-singleton\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
            .map_err(|err| anyhow::anyhow!("CreateMutexW failed: {err}"))?;
        if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle).ok() };
            return Ok(None);
        }
        Ok(Some(DaemonLockGuard(PlatformGuard(handle))))
    }
}

#[cfg(unix)]
mod imp {
    use super::DaemonLockGuard;
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    pub struct PlatformGuard(std::fs::File);

    /// An `flock`ed file next to the daemon's own socket path (`/tmp`,
    /// per-uid — mirrors `protocol::daemon_socket_path`'s own per-uid
    /// convention, since each OS account on a shared host runs its own
    /// independent daemon). `flock(LOCK_EX | LOCK_NB)` is atomic at the
    /// kernel level and, crucially, is released automatically on process
    /// exit/crash even without an explicit `unlock` — no stale-lock
    /// cleanup needed, unlike a plain "check if a PID file exists"
    /// scheme.
    pub fn try_acquire() -> anyhow::Result<Option<DaemonLockGuard>> {
        let path = format!("/tmp/som-srv-{}.lock", unsafe { libc::getuid() });
        let file = OpenOptions::new().create(true).write(true).open(&path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err.into());
        }
        Ok(Some(DaemonLockGuard(PlatformGuard(file))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second acquire attempt, while the first guard is still alive,
    /// must lose the race — this is the exact scenario that let two
    /// `som-srv --daemon` processes coexist before this module existed
    /// (see this module's own doc comment). Uses the SAME real OS
    /// primitive as production code (no mock), just within one process —
    /// both `CreateMutexW` and `flock` are documented to behave
    /// identically whether the second attempt comes from the same
    /// process or a different one, so this is a faithful reproduction,
    /// not an approximation.
    #[test]
    fn second_acquire_fails_while_first_guard_is_held() {
        let first = try_acquire_daemon_lock().expect("first acquire should not error");
        assert!(first.is_some(), "first acquire should win the race");

        let second = try_acquire_daemon_lock().expect("second acquire should not error");
        assert!(second.is_none(), "second acquire must lose the race while the first guard is alive");

        drop(first);

        let third = try_acquire_daemon_lock().expect("third acquire should not error");
        assert!(third.is_some(), "acquire should succeed again once the earlier guard is dropped");
    }
}
