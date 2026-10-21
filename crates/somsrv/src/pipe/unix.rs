//! Unix domain socket implementation of `PipeConnection` — the Unix
//! counterpart to `windows.rs`'s named pipes. Much simpler: `UnixStream`
//! already supports independent concurrent reads/writes from different
//! threads via `try_clone()` (each clone is a separate file descriptor
//! pointing at the same underlying socket, and POSIX read/write on
//! different fds to the same socket don't serialize against each other the
//! way a single synchronous Windows pipe HANDLE does — see `windows.rs`'s
//! module doc comment for why THAT needed overlapped I/O at all). No
//! event/overlapped-structure machinery needed here.

use anyhow::{Context as _, Result};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

pub struct PipeConnection {
    // Two independent fds (via `try_clone`) behind their own mutexes, not
    // one fd behind one mutex — a reader blocked in `read_message` (which
    // is the common case: `crate::server`/`crate::relay` each have one
    // thread sitting in a blocking read waiting for the next message) must
    // not block a concurrent `write_message` from a different thread, the
    // same requirement `windows.rs`'s overlapped I/O exists to satisfy.
    // Each mutex here only ever has ONE caller in practice (one reader
    // thread, one writer thread per connection), so this isn't adding real
    // contention — it's just interior mutability to give `read_message`/
    // `write_message` a `&self` signature matching the Windows side.
    read_half: std::sync::Mutex<UnixStream>,
    write_half: std::sync::Mutex<UnixStream>,
}

pub type Listener = UnixListener;

/// Binds a fresh listener at `socket_path` — removes any stale socket file
/// left over from a previous run first (a Unix domain socket path that
/// already exists as a file, even a dead one, makes `bind` fail with
/// `EADDRINUSE`; a crashed/killed previous HOLDER can easily leave one
/// behind, so this is the normal case to handle, not a rare edge case).
pub fn bind(socket_path: &str) -> Result<Listener> {
    let _ = std::fs::remove_file(socket_path);
    UnixListener::bind(socket_path).with_context(|| format!("failed to bind unix socket at {socket_path:?}"))
}

/// Blocks until a client connects to `listener`, then returns the connected
/// socket. Unlike Windows named pipes (which allow multiple instances under
/// one name so several `accept` calls can each grab their own), a Unix
/// domain socket needs exactly one `UnixListener` bound to the path — the
/// caller keeps `listener` alive across repeated calls to this rather than
/// re-binding each time (see `crate::server::run`'s accept loop, which now
/// branches on platform for exactly this reason).
pub fn accept_on(listener: &Listener) -> Result<PipeConnection> {
    let (stream, _addr) = listener.accept().context("UnixListener::accept failed")?;
    let read_half = stream.try_clone().context("failed to clone accepted socket for independent read/write halves")?;
    Ok(PipeConnection { read_half: std::sync::Mutex::new(read_half), write_half: std::sync::Mutex::new(stream) })
}

impl PipeConnection {
    /// Connects as a client to an already-running server's socket. Returns
    /// an error (not a panic) if nothing is listening — callers treat that
    /// as "server not alive, spawn a new one" rather than a hard failure,
    /// same as the Windows side.
    pub fn connect(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).with_context(|| format!("failed to connect to unix socket at {socket_path:?}"))?;
        let read_half = stream.try_clone().context("failed to clone connected socket for independent read/write halves")?;
        Ok(Self { read_half: std::sync::Mutex::new(read_half), write_half: std::sync::Mutex::new(stream) })
    }

    /// Reads exactly one length-prefixed message: a 4-byte little-endian
    /// length, followed by that many bytes of payload. Matches the
    /// Windows side's wire format exactly — the two platforms'
    /// `PipeConnection`s never talk to each other directly (a HOLDER and
    /// its RELAY are always the same binary/platform), but keeping the
    /// framing identical means `crate::server`/`crate::relay` don't need
    /// any platform-specific logic above this layer.
    pub fn read_message(&self) -> Result<Vec<u8>> {
        let mut read_half = self.read_half.lock().unwrap();
        let mut len_bytes = [0u8; 4];
        read_half.read_exact(&mut len_bytes).context("pipe closed or read failed (length prefix)")?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut buf = vec![0u8; len];
        read_half.read_exact(&mut buf).context("pipe closed or read failed (payload)")?;
        Ok(buf)
    }

    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        let len = (payload.len() as u32).to_le_bytes();
        let mut write_half = self.write_half.lock().unwrap();
        write_half.write_all(&len).context("pipe closed or write failed (length prefix)")?;
        write_half.write_all(payload).context("pipe closed or write failed (payload)")?;
        write_half.flush().context("pipe closed or flush failed")?;
        Ok(())
    }
}
