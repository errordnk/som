//! `somsrv`'s side of the protocol: the message types live in
//! `srp_client::protocol` (shared with every client) and are re-exported
//! here, so `somsrv::protocol::X` names the same type; what only the daemon's
//! own builds have (the build id) is here.

pub use srp_client::protocol::*;

/// Makes this test process, and every process it starts from now on, use
/// the test daemon (`INSTANCE_ENV=test`), and never replace a running one
/// (`KEEP_RUNNING_DAEMON_ENV`: the tests run in parallel, possibly on
/// different builds). Every test that reaches a daemon calls this first.
pub fn use_test_daemon() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: set once, before the caller starts any process or daemon
    // connection; the tests of a run all set the same values.
    ONCE.call_once(|| unsafe {
        std::env::set_var(INSTANCE_ENV, "test");
        std::env::set_var(crate::daemon::KEEP_RUNNING_DAEMON_ENV, "1");
    });
}

/// The handshake of this build of `somsrv` (and of Som, whose version it carries).
pub fn current_handshake() -> HandshakeInfo {
    HandshakeInfo::new(env!("CARGO_PKG_VERSION"), BUILD_ID)
}

/// The id of this build (see `build.rs`).
pub const BUILD_ID: u64 = parse_decimal(env!("SOMSRV_BUILD_ID"));

const fn parse_decimal(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let mut value = 0u64;
    let mut index = 0;
    while index < bytes.len() {
        value = value * 10 + (bytes[index] - b'0') as u64;
        index += 1;
    }
    value
}

const BUILD_MARK_PREFIX: &[u8] = b"SOMSRV-BUILD-ID:";

/// The id of this build as text inside the binary, where [`build_id_in`] finds
/// it. `#[used]` keeps it even though nothing reads it at run time.
#[used]
pub static BUILD_MARK: &str = concat!("SOMSRV-BUILD-ID:", env!("SOMSRV_BUILD_ID"), ":");

/// The id of the `somsrv` build whose file contents are `binary`, or `None` if
/// it is not a build that carries one.
pub fn build_id_in(binary: &[u8]) -> Option<u64> {
    let mut from = 0;
    while let Some(at) = binary[from..].windows(BUILD_MARK_PREFIX.len()).position(|window| window == BUILD_MARK_PREFIX) {
        let digits_start = from + at + BUILD_MARK_PREFIX.len();
        let digits = binary[digits_start..].iter().take_while(|byte| byte.is_ascii_digit()).count();
        // The needle is also a string in this very function: only the mark itself has
        // digits followed by a colon.
        if digits > 0 && binary.get(digits_start + digits) == Some(&b':') {
            return std::str::from_utf8(&binary[digits_start..digits_start + digits]).ok()?.parse().ok();
        }
        from = digits_start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_id_of_a_binary_is_found_in_its_bytes() {
        let mut binary = vec![0u8, 1, 2];
        binary.extend_from_slice(b"SOMSRV-BUILD-ID:"); // the needle alone, as in code
        binary.extend_from_slice(b"junk\0");
        binary.extend_from_slice(BUILD_MARK.as_bytes());
        binary.extend_from_slice(&[9, 9]);
        assert_eq!(build_id_in(&binary), Some(BUILD_ID));
        assert_eq!(build_id_in(b"no mark here SOMSRV-BUILD-ID:"), None);
        assert_eq!(build_id_in(b"SOMSRV-BUILD-ID:123"), None, "digits without the closing colon are not a mark");
        assert_eq!(build_id_in(b"xx SOMSRV-BUILD-ID:1700000000:yy"), Some(1_700_000_000));
    }

    #[test]
    fn a_handshake_from_an_older_build_without_an_id_still_reads_and_counts_as_id_zero() {
        let old: HandshakeInfo = serde_json::from_str(r#"{"version":"0.7.0","os":"Linux","arch":"Amd64"}"#).unwrap();
        assert_eq!((old.build_id, old.pid), (0, 0));
        let round_trip: HandshakeInfo = serde_json::from_str(&serde_json::to_string(&current_handshake()).unwrap()).unwrap();
        assert_eq!(round_trip.build_id, BUILD_ID);
        assert!(BUILD_ID > 1_700_000_000, "the build id is a plausible Unix time");
    }
}

#[cfg(test)]
mod instance_tests {
    use super::*;

    #[test]
    fn the_test_daemon_has_an_address_of_its_own() {
        use_test_daemon();
        assert_eq!(instance_suffix(), "-test");
        assert!(daemon_socket_path().contains("somsrv-test") || daemon_socket_path().ends_with("-test.sock"), "{}", daemon_socket_path());
    }

    /// A test that starts or reaches a daemon outside `testing::debug_binary`
    /// must ask for the test daemon first, or it runs against the user's
    /// (`tests/relay_sessions.rs` did, and its sessions went to the user's
    /// daemon; 2026-11-03).
    #[test]
    fn every_integration_test_that_reaches_a_daemon_uses_the_test_one() {
        let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(tests).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let reaches_a_daemon = text.contains("connect_or_spawn") || text.contains("CARGO_BIN_EXE_somsrv");
            if reaches_a_daemon && !text.contains("use_test_daemon()") {
                offenders.push(entry.path());
            }
        }
        assert!(offenders.is_empty(), "these reach a daemon without use_test_daemon(): {offenders:?}");
    }
}
