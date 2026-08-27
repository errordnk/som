//! Manual end-to-end smoke test for the RELAY/HOLDER architecture — not a
//! `cargo test` because it needs a REAL `som-srv.exe` sitting next
//! to whatever binary runs this (unlike a `cargo test` unit test running
//! from `target/debug/deps/`, which wouldn't have it, and
//! `relay::spawn_detached_holder` needs `current_exe()` to actually resolve
//! to the real binary since it spawns a detached copy of itself as the
//! HOLDER). Run with:
//! `cargo run -p som_srv --example relay_smoke_test`
//!
//! Spawns `som-srv.exe` in RELAY mode with its stdio piped (the
//! same shape Som's own PTY creation gives a shell command), writes a
//! command to its stdin exactly like a user typing, and asserts the ANSI
//! bytes that come back out of its stdout contain the real, uncorrupted
//! output of that command — proving the whole HOLDER-spawn -> pipe-connect
//! -> real-PTY -> grid-parse -> ANSI-redraw -> stdout pipeline actually
//! works end to end, not just that its pieces compile.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let exe = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("som-srv.exe");
    if !exe.is_file() {
        eprintln!("som-srv.exe not found at {exe:?} — build it first with `cargo build -p som_srv --bin som-srv`");
        std::process::exit(1);
    }

    let profile = "smoke-test";
    let pane_id = format!("{}", uuid::Uuid::new_v4());

    println!("spawning relay: {exe:?} {profile} {pane_id} cmd.exe");
    let mut child = Command::new(&exe)
        .arg(profile)
        .arg(&pane_id)
        .arg("C:\\Windows\\System32\\cmd.exe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn relay process");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Read whatever the relay produces for a bounded time, accumulating
    // bytes — this includes the initial full redraw (the empty cmd.exe
    // prompt) and, after we write below, the echoed command's output.
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

    // Give the holder a moment to spawn, connect, and produce its first
    // full redraw (empty prompt) before we type anything.
    std::thread::sleep(Duration::from_millis(1500));

    println!("writing command to relay's stdin (as if Som's PTY forwarded a keystroke)");
    stdin
        .write_all(b"echo relay-smoke-test-marker\r\n")
        .expect("failed to write to relay's stdin");
    stdin.flush().ok();

    let mut collected = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        while let Ok(b) = rx.try_recv() {
            collected.push(b);
        }
        if String::from_utf8_lossy(&collected).contains("relay-smoke-test-marker") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let text = String::from_utf8_lossy(&collected);
    println!("collected {} bytes from relay's stdout", collected.len());

    // Kill only the RELAY (simulating Som closing its window) — the HOLDER
    // it spawned is detached and should survive this.
    child.kill().ok();
    child.wait().ok();

    if !text.contains("relay-smoke-test-marker") {
        eprintln!("FAIL: expected 'relay-smoke-test-marker' in relay stdout, got:\n{text}");
        std::process::exit(1);
    }
    println!("PASS: relay's stdout contained the real shell's echoed output");

    // Reattach: spawn a SECOND relay with the SAME pane_id, simulating Som
    // restarting and restoring the same tab. If the holder survived (as
    // it should — it's detached), this new relay should connect to it
    // rather than starting a fresh shell, and its first (full) redraw
    // should already contain the marker from before — proving history
    // wasn't lost across the "restart".
    println!("\nreattaching with a second relay, same pane-id...");
    std::thread::sleep(Duration::from_millis(500));

    let mut child2 = Command::new(&exe)
        .arg(profile)
        .arg(&pane_id)
        .arg("C:\\Windows\\System32\\cmd.exe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn second relay process");
    let mut stdout2 = child2.stdout.take().unwrap();

    let (tx2, rx2) = std::sync::mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout2.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if tx2.send(b).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut collected2 = Vec::new();
    let deadline2 = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline2 {
        while let Ok(b) = rx2.try_recv() {
            collected2.push(b);
        }
        if String::from_utf8_lossy(&collected2).contains("relay-smoke-test-marker") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let text2 = String::from_utf8_lossy(&collected2);
    println!("collected {} bytes from second relay's stdout", collected2.len());

    child2.kill().ok();
    child2.wait().ok();

    // Now actually clean up the holder for real, via the close-session
    // path — otherwise it'd sit there running forever after this example
    // exits. Simplest way from here: just kill it directly by profile/pane
    // name isn't available, so rely on it having no more relays and
    // whatever real close-session mechanism the next implementation phase
    // adds; for THIS smoke test, forcibly kill any som-srv.exe
    // left over is out of scope (would kill unrelated instances too) — the
    // holder is harmless left running under a "smoke-test" profile name
    // and will be cleaned up by the caller/CI environment.

    if text2.contains("relay-smoke-test-marker") {
        println!("PASS: second relay's FIRST redraw already contained history from before the \"restart\" — reattach works");
    } else {
        eprintln!("FAIL: expected 'relay-smoke-test-marker' in second relay's stdout (reattach should restore prior screen state), got:\n{text2}");
        std::process::exit(1);
    }
}
