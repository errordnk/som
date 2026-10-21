//! What a shell echoes for a held backspace is the same few bytes over and over
//! within milliseconds. Every one of them has to reach the terminal: the
//! daemon once dropped a chunk identical to the one before it that came within
//! 50 ms (meant for a duplicate read that cannot be told from this), so about
//! every second erase was lost and the characters stayed on the screen.

#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn identical_chunks_in_quick_succession_all_reach_the_relays_output() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        const ECHOES: usize = 150;
        // The "shell": writes the erase echo of a backspace again and again, a few
        // milliseconds apart, so each lands in a read of its own. It waits first: what it
        // prints before the relay is attached is not the relay's to receive.
        let script = format!(r#"sleep 2; i=0; while [ $i -lt {ECHOES} ]; do printf '\b\033[K'; sleep 0.004; i=$((i+1)); done; sleep 30"#);
        let pane_id = format!("echo-burst-{}", std::process::id());
        let mut relay = std::process::Command::new(env!("CARGO_BIN_EXE_somsrv"))
            .args(["echo-burst", pane_id.as_str(), "sh", "-c", script.as_str()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("starting the relay");
        let mut stdout = relay.stdout.take().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reader_seen = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(read) = stdout.read(&mut buf) {
                if read == 0 {
                    return;
                }
                reader_seen.lock().unwrap().extend_from_slice(&buf[..read]);
            }
        });

        let erase: &[u8] = b"\x08\x1b[K";
        let count = |bytes: &[u8]| bytes.windows(erase.len()).filter(|window| *window == erase).count();
        let deadline = Instant::now() + Duration::from_secs(60);
        while count(&seen.lock().unwrap()) < ECHOES && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let got = count(&seen.lock().unwrap());
        // The tab is closed (a NUL byte is the relay's signal), which ends the shell too.
        relay.stdin.as_mut().unwrap().write_all(&[0]).ok();
        relay.wait().ok();
        done.send(got).ok();
    });
    let got = finished.recv_timeout(Duration::from_secs(120)).expect("the relay produced no result in time");
    assert_eq!(got, 150, "{got} of 150 erase echoes reached the terminal: identical chunks in quick succession were dropped");
}
