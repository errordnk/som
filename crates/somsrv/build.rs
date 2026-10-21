//! Gives every build of `somsrv` an id: the time it was built. It travels in
//! the handshake and in `--version`, and is written into the binary as text
//! (`protocol::BUILD_MARK`) so that whoever holds the file - Som with the build
//! it embeds, a relay with its own - can tell which of two builds is the newer
//! without running it. No `rerun-if` lines: the script runs again whenever
//! anything in the package changed, which is when a new id is wanted.

fn main() {
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
    println!("cargo:rustc-env=SOMSRV_BUILD_ID={seconds}");
}
