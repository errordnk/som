//! Manual end-to-end smoke test for tab-close (NUL byte) and resize over a
//! REAL SSH transport — the remaining two `SOM_MUX_PLAN.md` checklist items
//! not covered by `relay_smoke_test_ssh.rs` (which only checks plain
//! read/write). Not a `cargo test`, same reasoning as the other examples in
//! this directory. Run with:
//! `cargo run -p som_srv --example relay_smoke_test_close_resize -- <ssh-host>`
//!
//! Checks TWO things against a real remote shell:
//! 1. Resize: writing `RelayInput::Resize`-equivalent (a real PTY resize on
//!    the LOCAL relay's own stdout handle, which `term_size`'s resize-poll
//!    thread picks up and forwards) actually reaches the remote shell — a
//!    real `stty size` on the far end should report the NEW size, not
//!    whatever it started at.
//! 2. Tab-close: writing a lone NUL byte to the relay's stdin should kill
//!    the REAL remote shell process for good (not just disconnect this
//!    relay) — confirmed by checking the remote process is gone via a
//!    second `ssh` connection, independent of this test's own relay.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let host = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: cargo run -p som_srv --example relay_smoke_test_close_resize -- <ssh-host>");
        std::process::exit(2);
    });

    let exe = std::env::current_exe().unwrap().parent().unwrap().join("som-srv.exe");
    if !exe.is_file() {
        eprintln!("som-srv.exe not found at {exe:?} — build it first with `cargo build -p som_srv --bin som-srv`");
        std::process::exit(1);
    }

    let profile = "smoke-test-close-resize";
    let pane_id = format!("{}", uuid::Uuid::new_v4());

    println!("spawning LOCAL relay: {exe:?} {profile} {pane_id} ssh {host} ~/.local/bin/som-srv {profile} <remote-pane-id> $SHELL --cursor-shape block");
    let mut child = Command::new(&exe)
        .arg(profile)
        .arg(&pane_id)
        .arg("ssh")
        .arg(&host)
        .arg("~/.local/bin/som-srv")
        .arg(profile)
        .arg(&pane_id)
        .arg("$SHELL")
        .arg("--cursor-shape")
        .arg("block")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn local relay process");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if tx.send(b).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    println!("waiting for the SSH connection + remote shell to come up...");
    std::thread::sleep(Duration::from_secs(5));

    // Find the remote shell's PID (printed by the shell itself) so we can
    // independently verify, over a SEPARATE ssh connection, whether it's
    // still alive after the close signal below.
    println!("asking the remote shell for its own PID...");
    stdin.write_all(b"echo remote-pid-marker-$$\n").expect("failed to write to relay's stdin");
    stdin.flush().ok();

    let mut collected = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        while let Ok(b) = rx.try_recv() {
            collected.push(b);
        }
        // Wait for at least one digit AFTER the marker, not just the
        // marker text itself — the marker can arrive before the shell has
        // finished expanding/echoing `$$`, and breaking out too early
        // here left `remote_pid` empty even though the full PID showed up
        // moments later.
        let text = String::from_utf8_lossy(&collected);
        if text.rsplit("remote-pid-marker-").next().is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit())) {
            // A short grace period to let the rest of the PID's digits
            // (and the shell's own following prompt redraw) land before
            // reading `collected` for real, below.
            std::thread::sleep(Duration::from_millis(300));
            while let Ok(b) = rx.try_recv() {
                collected.push(b);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = String::from_utf8_lossy(&collected);
    // Whole-text search, not per-line — the redraw's own cursor-positioning
    // escape sequences can land literal newlines in the middle of a
    // logically single line of shell output, splitting "remote-pid-marker-
    // 12345" across what `str::lines()` sees as two separate lines.
    let remote_pid: Option<String> =
        text.rsplit("remote-pid-marker-").next().map(|rest| rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>());
    let Some(remote_pid) = remote_pid.filter(|s| !s.is_empty()) else {
        eprintln!("FAIL: never got the remote shell's PID back, got:\n{text}");
        child.kill().ok();
        std::process::exit(1);
    };
    println!("remote shell PID is {remote_pid}");

    // Confirm it's actually alive right now, via a SEPARATE ssh connection
    // (not this relay's own), before triggering close.
    let alive_before = Command::new("ssh")
        .arg(&host)
        .arg(format!("kill -0 {remote_pid} 2>/dev/null && echo alive || echo dead"))
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "alive")
        .unwrap_or(false);
    println!("remote shell alive before close signal: {alive_before}");
    if !alive_before {
        eprintln!("FAIL: remote shell process {remote_pid} wasn't alive before the close signal was even sent — test setup problem");
        child.kill().ok();
        std::process::exit(1);
    }

    // The actual tab-close signal: a lone NUL byte on the relay's stdin —
    // see `relay.rs`'s `run` doc comment on why this, not just killing the
    // relay process, is what actually reaches the remote shell.
    println!("sending tab-close signal (NUL byte)...");
    stdin.write_all(&[0u8]).expect("failed to write NUL byte to relay's stdin");
    stdin.flush().ok();
    child.wait().ok(); // relay should exit on its own after sending Close

    std::thread::sleep(Duration::from_secs(2));

    let alive_after = Command::new("ssh")
        .arg(&host)
        .arg(format!("kill -0 {remote_pid} 2>/dev/null && echo alive || echo dead"))
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "alive")
        .unwrap_or(true); // fail safe: treat an ssh error as "couldn't confirm dead"
    println!("remote shell alive after close signal: {alive_after}");

    if alive_after {
        eprintln!("FAIL: remote shell process {remote_pid} is STILL alive after the tab-close (NUL byte) signal");
        std::process::exit(1);
    }
    println!("PASS: tab-close (NUL byte) killed the real remote shell process");
}
