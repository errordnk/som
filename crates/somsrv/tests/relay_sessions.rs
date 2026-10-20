//! Real-process tests of what a `tmux: true` tab relies on: a session
//! lives in the daemon, not in the relay Som runs, so it survives Som
//! closing and the next relay with the same pane id finds it there; and a
//! session ends only when the tab is closed.
//!
//! Nothing is simulated: every test runs the real `somsrv` binary the way
//! Som does (a relay with the profile, the pane id and the shell as its
//! arguments, whose stdin/stdout stand in for Som's PTY), which spawns the
//! real shared daemon, and looks at the daemon's own list of sessions
//! (`somsrv::admin::list_sessions`). They are not skipped when something is
//! missing - a test that quietly skips is how a broken feature goes
//! unnoticed - cargo builds the binary these tests run (`CARGO_BIN_EXE_*`).

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for the daemon to show what a test is waiting for.
const PATIENCE: Duration = Duration::from_secs(20);

fn shell_program() -> &'static str {
    if cfg!(windows) { "C:\\Windows\\System32\\cmd.exe" } else { "/bin/sh" }
}

/// A pane id no other test (or run) uses: the daemon is shared with
/// whatever else runs on the machine.
fn unique_pane_id(name: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("relay-test-{}-{}-{name}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A relay as Som runs it for a `tmux: true` tab.
struct Relay {
    child: Child,
}

impl Relay {
    fn start(pane_id: &str) -> Relay {
        let mut child = Command::new(env!("CARGO_BIN_EXE_somsrv"))
            .args(["relay-test", pane_id, shell_program(), "--cursor-shape", "block"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the somsrv binary starts as a relay");
        // What the relay paints for Som is read and thrown away, as a
        // full pipe would block it.
        let mut stdout = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            std::io::copy(&mut stdout, &mut std::io::sink()).ok();
        });
        Relay { child }
    }

    /// What happens to a relay when Som quits: the process is gone.
    fn kill(mut self) {
        self.child.kill().expect("the relay can be killed");
        self.child.wait().expect("the relay is reaped");
    }

    /// What Som does when the user closes the tab: one NUL byte.
    fn close_tab(&mut self) {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        stdin.write_all(&[0u8]).expect("the NUL reaches the relay");
        stdin.flush().expect("the NUL is flushed");
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // A relay left running (a failed test) must not outlive it.
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Takes every session a test made out of the daemon when the test ends,
/// failed or not.
struct Sessions(Vec<String>);

impl Drop for Sessions {
    fn drop(&mut self) {
        for pane_id in &self.0 {
            if let Err(err) = somsrv::admin::kill_session(None, pane_id.clone()) {
                eprintln!("could not clean up the session {pane_id}: {err:#}");
            }
        }
    }
}

/// How many sessions the daemon holds for `pane_id` (it is down, or not up
/// yet: none).
fn sessions_for(pane_id: &str) -> usize {
    somsrv::admin::list_sessions(None)
        .map(|sessions| sessions.iter().filter(|session| session.pane_id == pane_id).count())
        .unwrap_or(0)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("gave up after {PATIENCE:?} waiting for: {what}");
}

#[test]
fn a_session_outlives_its_relay_and_the_next_relay_finds_it_there() {
    let pane_id = unique_pane_id("outlives");
    let _sessions = Sessions(vec![pane_id.clone()]);

    let first = Relay::start(&pane_id);
    wait_until("the session to be registered", || sessions_for(&pane_id) == 1);

    // Som quits: the relay dies, the session must not.
    first.kill();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(sessions_for(&pane_id), 1, "the session went with its relay - htop, mc and micro would all be lost");

    // Som starts again: its relay for the same pane id reattaches, it does not make another.
    let _second = Relay::start(&pane_id);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(sessions_for(&pane_id), 1, "the new relay made a second session instead of reattaching to the first");
}

#[test]
fn every_pane_of_a_tab_keeps_its_own_session_across_a_restart() {
    let pane_ids: Vec<String> = (0..3).map(|n| unique_pane_id(&format!("pane{n}"))).collect();
    let _sessions = Sessions(pane_ids.clone());

    let relays: Vec<Relay> = pane_ids.iter().map(|id| Relay::start(id)).collect();
    wait_until("all three sessions to be registered", || pane_ids.iter().all(|id| sessions_for(id) == 1));

    for relay in relays {
        relay.kill();
    }
    std::thread::sleep(Duration::from_secs(1));
    for id in &pane_ids {
        assert_eq!(sessions_for(id), 1, "the session {id} did not survive the relays");
    }

    // The restore: each pane connects with the pane id saved for it.
    let _relays: Vec<Relay> = pane_ids.iter().map(|id| Relay::start(id)).collect();
    std::thread::sleep(Duration::from_secs(2));
    for id in &pane_ids {
        assert_eq!(sessions_for(id), 1, "the pane {id} did not reattach to its session (a second one was made)");
    }
}

#[test]
fn closing_the_tab_ends_its_session_and_removes_it_from_the_daemon() {
    let pane_id = unique_pane_id("close");
    let _sessions = Sessions(vec![pane_id.clone()]);

    let mut relay = Relay::start(&pane_id);
    wait_until("the session to be registered", || sessions_for(&pane_id) == 1);

    relay.close_tab();
    wait_until("the closed tab's session to leave the daemon's registry", || sessions_for(&pane_id) == 0);
}
