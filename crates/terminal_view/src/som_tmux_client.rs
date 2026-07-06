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
pub fn connect_or_spawn(profile_name: &str) -> Result<PipeConnection> {
    let name = pipe_name(profile_name);
    if let Ok(connection) = PipeConnection::connect(&name) {
        return Ok(connection);
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
