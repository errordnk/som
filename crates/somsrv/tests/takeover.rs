//! The cutover of a redeploy with real processes: the old "daemon" (a plain
//! sleeping process) goes away, the staged file takes the place of the live
//! one, the marker is removed, and a new daemon is started from the new file.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn write_script(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
}

#[test]
fn the_takeover_replaces_the_binary_after_the_old_daemon_is_gone_and_starts_the_new_one() {
    let dir = std::env::temp_dir().join(format!("somsrv-takeover-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let live = dir.join("somsrv");
    let staged = dir.join("somsrv.new");
    let marker = dir.join("somsrv.redeploy-pending");
    let started = dir.join("new-daemon-started");

    write_script(&live, "#!/bin/sh\necho old\n");
    // The new build, when started as the daemon, leaves a trace.
    write_script(&staged, &format!("#!/bin/sh\necho \"$@\" > '{}'\n", started.display()));
    std::fs::write(&marker, b"").unwrap();

    // The old daemon: a process that would run for ever.
    let mut old_daemon = std::process::Command::new("sleep").arg("600").spawn().unwrap();
    let old_pid = old_daemon.id();
    // Its exit is collected here, or it would stay a zombie that still "exists".
    let reaper = std::thread::spawn(move || old_daemon.wait());

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_somsrv"))
        .args(["--takeover", &old_pid.to_string()])
        .arg(&staged)
        .arg(&live)
        .arg(&marker)
        .status()
        .expect("running the takeover");
    assert!(status.success(), "the takeover failed: {status:?}");
    reaper.join().unwrap().ok();

    assert!(!process_exists(old_pid), "the old daemon is still running");
    assert_eq!(std::fs::read_to_string(&live).unwrap(), std::fs::read_to_string(dir.join("somsrv")).unwrap());
    assert!(std::fs::read_to_string(&live).unwrap().contains("new-daemon-started"), "the staged build is not in the live place");
    assert!(!staged.exists(), "the staged file should have been renamed away");
    assert!(!marker.exists(), "the marker should be gone");

    // The new daemon was started from the live path with --daemon.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_to_string(&started).expect("the new daemon was never started").trim(), "--daemon");
    std::fs::remove_dir_all(&dir).ok();
}
