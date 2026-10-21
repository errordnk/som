//! Helpers for the tests of other crates that run the real `somsrv` and
//! `somsrp` as processes (a relay as Som runs it, `somsrp` streaming a file
//! to a real daemon). Behind the `test-support` feature, never in Som.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a build may wait for a `somsrv` of an earlier test run to exit
/// (a daemon with nothing to do shuts itself down after 60 seconds), which
/// holds its binary and so keeps the linker from replacing it.
const DAEMON_EXIT_PATIENCE: Duration = Duration::from_secs(90);

/// The debug binary `name` (`somsrv`, `somsrp`), built now.
///
/// Built, not found: the binary a test starts as a process is what the test
/// is about, and one left in `target/debug` by some earlier build made such
/// tests pass or fail by the day it was last built (a `somsrp` four days old
/// failed four tests for nothing anyone could see). It is built into its own
/// directory, `target/test-bins`: a `cargo build` run from a test with the
/// features of a different package set rebuilds half the dependency tree and
/// fights the outer `cargo test` for the files of the target directory it is
/// run from; a directory of its own has neither problem, and a second build
/// of what is current costs next to nothing. Both binaries are built
/// together, as `somsrp` starts the `somsrv` next to it.
///
/// Panics with cargo's own output when the build fails, so a test that
/// cannot have its binary never passes and never silently skips.
pub fn debug_binary(name: &str) -> PathBuf {
    static ONE_BUILD_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _guard = ONE_BUILD_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // The crate is `<root>/crates/somsrv`. No `..` in the path: ConPTY
    // refuses to start a process from one.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("the crate is two levels below the workspace root");
    let target_dir = workspace_root.join("target").join("test-bins");
    let started = Instant::now();
    loop {
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
            .args(["build", "-p", "somsrv", "-p", "somsrp", "--target-dir"])
            .arg(&target_dir)
            .current_dir(&workspace_root)
            .output()
            .expect("cargo can be run to build the binaries a test runs");
        if output.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let held_by_a_running_daemon = stderr.contains("failed to remove file") || stderr.contains("Access is denied");
        if held_by_a_running_daemon && started.elapsed() < DAEMON_EXIT_PATIENCE {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        panic!("building somsrv and somsrp for a real-process test failed:\n{stderr}");
    }
    let file_name = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let path = target_dir.join("debug").join(file_name);
    assert!(path.is_file(), "{path:?} was not produced by the build");
    path
}

/// What `ssh -L 127.0.0.1:<port>:<daemon socket>` is to the tab: a loopback
/// address whose connections are carried to the daemon of this machine, one
/// daemon connection per accepted one. Returns the address to connect to.
/// (The real thing is a byte pump; this one moves whole messages, which for
/// the length-prefixed protocol comes to the same.)
///
/// The forward counts the connections it carried, so a test can tell that its
/// traffic went through it and not around it.
pub struct LocalForward {
    pub address: std::net::SocketAddr,
    pub connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

pub fn spawn_tcp_forward_to_local_daemon(daemon_binary: &Path) -> LocalForward {
    use crate::tcp::TcpConnection;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port for the forward");
    let address = listener.local_addr().expect("the forward's address");
    let daemon_binary = daemon_binary.to_path_buf();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            counted.fetch_add(1, Ordering::Relaxed);
            let daemon_binary = daemon_binary.clone();
            std::thread::spawn(move || {
                let Ok(from_client) = TcpConnection::from_stream(stream).map(Arc::new) else { return };
                let Ok(to_daemon) = crate::daemon::connect_or_spawn(&daemon_binary).map(Arc::new) else { return };
                let (client, daemon) = (from_client.clone(), to_daemon.clone());
                std::thread::spawn(move || {
                    while let Ok(message) = daemon.read_message() {
                        if client.write_message(&message).is_err() {
                            return;
                        }
                    }
                });
                while let Ok(message) = from_client.read_message() {
                    if to_daemon.write_message(&message).is_err() {
                        return;
                    }
                }
            });
        }
    });
    LocalForward { address, connections }
}
