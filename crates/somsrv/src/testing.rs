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

    // The tests run in parallel against one daemon, started by whichever build a
    // test happens to use: a client of a newer build must not stop the daemon of
    // an older one that the other tests are using. Children inherit this.
    // SAFETY: set under the lock above, before any child of this call exists.
    unsafe { std::env::set_var(crate::daemon::KEEP_RUNNING_DAEMON_ENV, "1") };

    // The crate is `<root>/crates/somsrv`. No `..` in the path: ConPTY
    // refuses to start a process from one.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("the crate is two levels below the workspace root");
    let target_dir = workspace_root.join("target").join("test-bins");
    let started = Instant::now();
    loop {
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
            .args(["build", "-p", "somsrv", "-p", "somsrp", "--features", "somsrv/test-support", "--target-dir"])
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
