//! Platform-specific IPC transport between a HOLDER and its RELAY(s) — a
//! named pipe on Windows, a Unix domain socket everywhere else. Both
//! implementations expose the same `PipeConnection` API
//! (`accept`/`connect`/`read_message`/`write_message`, length-prefixed
//! framing), so `crate::server`/`crate::relay` don't need any
//! `#[cfg(...)]` beyond the accept-loop shape itself (see each module's
//! doc comment for why that one part still differs: Windows named pipes
//! allow multiple instances under one name, so `PipeConnection::accept`
//! alone is enough; a Unix domain socket needs an actual `UnixListener`
//! kept alive across accepts).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Listener, PipeConnection, accept_on, bind};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Listener, PipeConnection, accept_on, bind};
