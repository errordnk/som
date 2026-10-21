//! The relay holds back an ESC typed into its tab for a moment, in case it is the
//! start of one of the link's frames. When nothing follows it is the Escape
//! key, and it must reach the shell: vim, readline's vi mode and every program
//! that waits for a lone ESC depend on it.

#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn a_lone_escape_typed_into_the_tab_reaches_the_shell() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The "shell" reads one raw byte and prints it in hex.
        let script = "stty raw -echo; od -An -tx1 -N1; sleep 30";
        let pane_id = format!("escape-key-{}", std::process::id());
        let mut relay = std::process::Command::new(env!("CARGO_BIN_EXE_somsrv"))
            .args(["escape-key", pane_id.as_str(), "sh", "-c", script])
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

        // Let the shell get to its read, then press Escape - alone, nothing after it.
        std::thread::sleep(Duration::from_secs(3));
        let stdin = relay.stdin.as_mut().unwrap();
        stdin.write_all(&[0x1b]).unwrap();
        stdin.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut reached = false;
        while Instant::now() < deadline {
            if seen.lock().unwrap().windows(2).any(|window| window == b"1b") {
                reached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        relay.stdin.as_mut().unwrap().write_all(&[0]).ok();
        relay.wait().ok();
        done.send(reached).ok();
    });
    let reached = finished.recv_timeout(Duration::from_secs(120)).expect("no result in time");
    assert!(reached, "a lone ESC typed into the tab never reached the shell");
}
