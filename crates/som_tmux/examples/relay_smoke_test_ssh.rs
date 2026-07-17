//! Manual end-to-end smoke test for the RELAY/HOLDER architecture over a
//! REAL SSH transport — the remote-profile counterpart to
//! `relay_smoke_test.rs` (which only covers the local Windows path). Not a
//! `cargo test` for the same reason as that file: needs a real `som-tmux.exe`
//! sitting next to whatever binary runs this. Run with:
//! `cargo run -p som_tmux --example relay_smoke_test_ssh -- <ssh-host>`
//!
//! Reproduces EXACTLY what `terminal_panel.rs`'s `wrap_remote_command_args`
//! builds for a real `tmux: true` SSH profile: the LOCAL `som-tmux.exe`'s
//! HOLDER spawns `ssh <host> ~/.local/bin/som-tmux <profile> <pane-id>
//! $SHELL --cursor-shape block` as its real PTY child — that remote
//! `som-tmux` then becomes ITS OWN local HOLDER/RELAY pair on the far side
//! (a Unix-socket pipe there), spawning the user's real login shell. This
//! is the one thing the crate's own unit tests (which spawn `Session`
//! directly, no real SSH hop) don't cover — the actual double-hop wire
//! path a real Som window uses for a real `tmux: true` remote profile.
//!
//! Requires: `~/.local/bin/som-tmux` already deployed and runnable on the
//! target host (see `terminal_panel.rs`'s `ensure_remote_binary_deployed`
//! for the real auto-deploy this bypasses — this test assumes it's already
//! done, to keep this test itself fast and deploy-failure-agnostic).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let host = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: cargo run -p som_tmux --example relay_smoke_test_ssh -- <ssh-host>");
        std::process::exit(2);
    });

    let exe = std::env::current_exe().unwrap().parent().unwrap().join("som-tmux.exe");
    if !exe.is_file() {
        eprintln!("som-tmux.exe not found at {exe:?} — build it first with `cargo build -p som_tmux --bin som-tmux`");
        std::process::exit(1);
    }

    let profile = "smoke-test-ssh";
    let pane_id = std::env::args().nth(2).unwrap_or_else(|| format!("{}", uuid::Uuid::new_v4()));

    // Exactly what `wrap_remote_command_args` builds for a real `tmux: true`
    // SSH profile — see that function's doc comment.
    println!("spawning LOCAL relay: {exe:?} {profile} {pane_id} ssh {host} ~/.local/bin/som-tmux {profile} <remote-pane-id> $SHELL --cursor-shape block");
    let mut child = Command::new(&exe)
        .arg(profile)
        .arg(&pane_id)
        .arg("ssh")
        .arg(&host)
        .arg("~/.local/bin/som-tmux")
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

    // SSH connection setup + remote shell startup is slower than the local
    // path's cmd.exe spawn — give it real time before typing anything.
    println!("waiting for the SSH connection + remote shell to come up...");
    std::thread::sleep(Duration::from_secs(5));

    println!("writing command to relay's stdin (as if Som's PTY forwarded a keystroke)");
    stdin.write_all(b"echo ssh-relay-smoke-test-marker\n").expect("failed to write to relay's stdin");
    stdin.flush().ok();

    let mut collected = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        while let Ok(b) = rx.try_recv() {
            collected.push(b);
        }
        if String::from_utf8_lossy(&collected).contains("ssh-relay-smoke-test-marker") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let text = String::from_utf8_lossy(&collected);
    println!("collected {} bytes from relay's stdout", collected.len());

    child.kill().ok();
    child.wait().ok();

    if !text.contains("ssh-relay-smoke-test-marker") {
        eprintln!("FAIL: expected 'ssh-relay-smoke-test-marker' in relay stdout, got:\n{text}");
        std::process::exit(1);
    }
    println!("PASS: relay's stdout contained the real remote shell's echoed output over SSH");

    // The actual regression check: no double-terminal-emulation garbage
    // (stray escape-sequence fragments, corrupted control codes) anywhere
    // in the stream — not just that the marker text happened to survive.
    // Checks for the exact kind of artifact a real prior bug produced
    // (garbled `1c0`/`☁`-style fragments) — a byte sequence that looks like
    // a botched/partially-consumed escape sequence rather than either a
    // real ANSI escape (starts with ESC, 0x1b) or plain printable text.
    let suspicious_fragment = text.contains("1c0") || text.contains("☁") || text.contains("[?1;2c") && !text.contains('\x1b');
    if suspicious_fragment {
        eprintln!("FAIL: found a corruption artifact in the relay's output:\n{text}");
        std::process::exit(1);
    }
    println!("PASS: no double-terminal-emulation corruption artifacts found in the output");
}
