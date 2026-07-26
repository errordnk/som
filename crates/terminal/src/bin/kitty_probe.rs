//! A standalone helper binary (an ordinary executable via Cargo's default
//! `src/bin/*.rs` auto-discovery — NOT a `#[test]`) used only by
//! `terminal.rs`'s `test_kitty_query_response_reaches_a_real_child_process`.
//!
//! Why this needs to be a real separate process at all: the bug class this
//! exists to catch/prevent (KITTY_GRAPHICS_PLAN.md Stage 7 — confirmed via
//! extensive manual live testing with a real `yazi` process and a
//! hand-built probe on 2026-07-24/25) is specifically about whether a
//! *distinct child process*, reading its OWN stdin exactly the way a real
//! Kitty-graphics-aware program does, actually receives the `a=q` query
//! response Som's `Terminal` writes back into the PTY. That can only be
//! observed by spawning a real second process connected to the same real
//! OS PTY the terminal-under-test is connected to — an in-process mock or
//! a `write_output`-only unit test (already covered elsewhere, e.g.
//! `terminal.rs`'s `test_kitty_graphics_query_does_not_reach_image_store`)
//! proves the protocol logic is correct but says nothing about whether the
//! bytes actually reach a second process across the real PTY on this
//! platform.
//!
//! **Raw mode is mandatory here, not optional** — this was the actual root
//! cause of a false-positive "Som doesn't deliver terminal responses at
//! all" diagnosis during initial manual investigation (2026-07-25): a
//! first version of this binary read `std::io::stdin()` in the OS's
//! default COOKED/canonical mode, where the kernel's own line-discipline
//! buffers input until a newline arrives — since a real terminal response
//! (`\x1b_Gi=31;OK\x1b\`) has no trailing `\n`, it sat in the kernel's line
//! buffer forever and was never delivered to a canonical-mode `read()`,
//! on EITHER Windows or Unix. Real Kitty-graphics clients (yazi, kitty
//! itself) always switch to raw mode before probing for exactly this
//! reason. Confirmed via a separate GPUI/Som-independent isolation
//! harness (`alacritty_terminal`'s own `Term`/`EventLoop` directly, no
//! Som code involved at all) that raw-mode reading correctly receives the
//! response on both platforms once this fix was made.
use std::io::{Read, Write};
use std::time::{Duration, Instant};

#[cfg(unix)]
fn enable_raw_mode() -> libc::termios {
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut original);
        let mut raw = original;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        original
    }
}

#[cfg(unix)]
fn restore_mode(original: libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
    }
}

#[cfg(windows)]
fn enable_raw_mode() -> u32 {
    use windows::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE).expect("failed to get stdin handle");
        let mut original = Default::default();
        GetConsoleMode(handle, &mut original).expect("failed to get console mode");
        // ENABLE_VIRTUAL_TERMINAL_INPUT is required for the console to
        // deliver raw VT/escape byte sequences straight through to this
        // process's stdin read buffer, rather than translating them into
        // synthesized INPUT_RECORD key events (the console's default
        // behavior for anything that looks like a special key sequence) —
        // without it, a terminal's own escape-sequence RESPONSE (as
        // opposed to literal user keystrokes) may never surface as plain
        // bytes to a `ReadFile`/`std::io::Read` caller at all.
        let raw = (original.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0;
        SetConsoleMode(handle, windows::Win32::System::Console::CONSOLE_MODE(raw))
            .expect("failed to set raw console mode");
        original.0
    }
}

#[cfg(windows)]
fn restore_mode(original: u32) {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        if let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) {
            let _ = SetConsoleMode(handle, CONSOLE_MODE(original));
        }
    }
}

fn main() {
    let original_mode = enable_raw_mode();

    // If invoked with `--da1`, send a plain DA1 query instead of a Kitty
    // graphics query — used to isolate whether ANY `Terminal::write_to_pty`
    // response fails to reach a real child process on this platform (a
    // `write_to_pty` mechanism problem) versus something specific to the
    // Kitty graphics response shape (an APC-specific problem).
    let use_da1 = std::env::args().any(|a| a == "--da1");
    let query: &[u8] = if use_da1 {
        b"\x1b[0c"
    } else {
        // Same control-data shape a real Kitty-graphics-aware program's
        // capability probe uses (id=31, s=/v=/t=/f= extra keys included,
        // since real clients like yazi send these too — parse_command
        // must ignore them harmlessly, already covered by
        // kitty_graphics.rs's own tests; this binary only cares about the
        // RESPONSE, not re-testing parsing).
        b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
    };
    print!("{}", String::from_utf8_lossy(query));
    std::io::stdout().flush().expect("stdout flush must succeed");

    // Read whatever comes back on our OWN stdin (exactly what a real
    // Kitty-graphics client does) via a background thread + channel, so a
    // stalled blocking read can never hang this binary forever — matches
    // real clients' own bounded-wait behavior (e.g. yazi's own 1s DA1
    // read timeout) closely enough for this test's purpose.
    let stop_marker: &[u8] = if use_da1 { b"c" } else { b"\x1b_Gi=31;OK\x1b\\" };
    let stop_marker = stop_marker.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    let matched = if stop_marker.len() == 1 {
                        buf.last() == stop_marker.first()
                    } else {
                        buf.windows(stop_marker.len()).any(|w| w == stop_marker.as_slice())
                    };
                    if matched || buf.len() >= 4096 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    let start = Instant::now();
    let result = rx.recv_timeout(Duration::from_secs(5));

    restore_mode(original_mode);

    match result {
        Ok(buf) => {
            let text = String::from_utf8_lossy(&buf);
            eprintln!("KITTY_PROBE_RESULT: RECEIVED {} bytes in {:?}\r", buf.len(), start.elapsed());
            let ok = if use_da1 { text.contains("\x1b[?") } else { text.contains("\x1b_Gi=31;OK") };
            eprintln!("KITTY_PROBE_RESULT: CONTAINS_OK={ok}\r");
        }
        Err(_) => {
            eprintln!("KITTY_PROBE_RESULT: TIMED_OUT after {:?}\r", start.elapsed());
            eprintln!("KITTY_PROBE_RESULT: CONTAINS_OK=false\r");
        }
    }
}
