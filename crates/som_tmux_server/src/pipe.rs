//! Overlapped (async-at-the-Win32-level) wrapper over Win32 named pipes.
//!
//! Uses `FILE_FLAG_OVERLAPPED` deliberately, NOT synchronous I/O — a
//! synchronous named pipe handle serializes ALL I/O through the handle,
//! meaning a blocking `ReadFile` pending on one thread will make a
//! concurrent `WriteFile` from a different thread on the SAME handle block
//! until the read completes. This is documented Windows behavior (see
//! "Synchronous and Overlapped Pipe I/O" on Microsoft Learn), not a logic
//! bug — and it's exactly the shape `server.rs` needs (one thread reads
//! incoming `ClientMessage`s while another thread streams `GridUpdate`s out
//! on the same connection). Found via a real deadlock in manual testing: the
//! second `GridUpdate` write hung forever whenever the reader thread was
//! blocked waiting on the next client message. Each `read`/`write` call here
//! gets its own `OVERLAPPED` with its own manual-reset event (per Microsoft's
//! guidance — reusing one event across concurrent operations makes it
//! impossible to tell which operation actually completed).

use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::PCWSTR;

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// An `OVERLAPPED` struct plus the event handle it points at, bundled so the
/// event is always created fresh per-operation (see module doc comment for
/// why sharing one event across concurrent reads/writes would be wrong) and
/// cleaned up automatically.
struct Overlapped {
    inner: OVERLAPPED,
    event: HANDLE,
}

impl Overlapped {
    fn new() -> Result<Self> {
        let event = unsafe { CreateEventW(None, true, false, None) }?;
        let mut inner = OVERLAPPED::default();
        inner.hEvent = event;
        Ok(Self { inner, event })
    }
}

impl Drop for Overlapped {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.event).ok();
        }
    }
}

/// One end of a connected named pipe instance, using overlapped I/O so reads
/// and writes on this handle can proceed concurrently from different
/// threads. `HANDLE` itself is `Copy`/safe to share; each call here uses its
/// own `Overlapped` so concurrent calls don't stomp on each other's state.
pub struct PipeConnection {
    handle: HANDLE,
}

// SAFETY: overlapped ReadFile/WriteFile calls each carry their own OVERLAPPED
// (and thus their own completion event), so concurrent calls from different
// threads on this HANDLE don't share mutable state — the OS tracks each
// operation independently via its OVERLAPPED pointer.
unsafe impl Send for PipeConnection {}
unsafe impl Sync for PipeConnection {}

impl PipeConnection {
    /// Blocks until a client connects to a fresh instance of `pipe_name`,
    /// then returns the connected pipe. Callers loop this to keep accepting
    /// new clients (Windows lets multiple separate `CreateNamedPipeW` calls
    /// share one pipe name; the OS fans out incoming connections across
    /// whichever instances are currently waiting).
    ///
    /// Deliberately does NOT pass `FILE_FLAG_FIRST_PIPE_INSTANCE` — that flag
    /// means "fail unless I am the very first instance of this pipe name",
    /// which is wrong for a loop that creates a fresh instance per accepted
    /// client: every call after the first would hit `ERROR_ACCESS_DENIED`
    /// (this was hit in testing — the accept loop busy-looped on that error
    /// thousands of times/sec since nothing throttled retries). Plain
    /// `PIPE_UNLIMITED_INSTANCES` on every `CreateNamedPipeW` call already
    /// allows any number of instances to coexist under the same name.
    pub fn accept(pipe_name: &str) -> Result<Self> {
        let wide_name = to_wide(pipe_name);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide_name.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
                    PIPE_ACCESS_DUPLEX.0 | windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED.0,
                ),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                1 << 16,
                1 << 16,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("CreateNamedPipeW failed: {}", io::Error::last_os_error());
        }

        let mut overlapped = Overlapped::new()?;
        let connected = unsafe { ConnectNamedPipe(handle, Some(&mut overlapped.inner)) };
        if connected.is_err() {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            if err == ERROR_PIPE_CONNECTED {
                // Client connected between CreateNamedPipeW and ConnectNamedPipe —
                // already connected, nothing to wait for.
            } else if err == ERROR_IO_PENDING {
                let mut ignored = 0u32;
                let ok =
                    unsafe { GetOverlappedResult(handle, &overlapped.inner, &mut ignored, true) };
                if ok.is_err() {
                    unsafe { CloseHandle(handle).ok() };
                    bail!("ConnectNamedPipe (overlapped) failed: {}", io::Error::last_os_error());
                }
            } else {
                unsafe { CloseHandle(handle).ok() };
                bail!("ConnectNamedPipe failed: {:?}", err);
            }
        }

        Ok(Self { handle })
    }

    /// Connects as a client to an already-running server's pipe. Returns an
    /// error (not a panic) if nothing is listening — callers treat that as
    /// "server not alive, spawn a new one" rather than a hard failure.
    pub fn connect(pipe_name: &str) -> Result<Self> {
        let wide_name = to_wide(pipe_name);
        let handle = unsafe {
            windows::Win32::Storage::FileSystem::CreateFileW(
                PCWSTR(wide_name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED,
                None,
            )
        }?;
        if handle == INVALID_HANDLE_VALUE {
            bail!("CreateFileW failed: {}", io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Reads exactly one length-prefixed message: a 4-byte little-endian
    /// length, followed by that many bytes of payload.
    pub fn read_message(&self) -> Result<Vec<u8>> {
        let mut len_bytes = [0u8; 4];
        self.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut buf = vec![0u8; len];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        let len = (payload.len() as u32).to_le_bytes();
        self.write_all(&len)?;
        self.write_all(payload)?;
        Ok(())
    }

    fn read_exact(&self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            let overlapped = Overlapped::new()?;
            let mut read = 0u32;
            let ok = unsafe { ReadFile(self.handle, Some(buf), Some(&mut read), Some(&overlapped.inner as *const _ as *mut _)) };
            if ok.is_err() {
                let err = unsafe { windows::Win32::Foundation::GetLastError() };
                if err == ERROR_IO_PENDING {
                    unsafe { WaitForSingleObject(overlapped.event, INFINITE) };
                    let mut actually_read = 0u32;
                    let completed = unsafe {
                        GetOverlappedResult(self.handle, &overlapped.inner, &mut actually_read, false)
                    };
                    if completed.is_err() || actually_read == 0 {
                        bail!("pipe closed or overlapped ReadFile failed");
                    }
                    read = actually_read;
                } else {
                    bail!("pipe closed or ReadFile failed: {err:?}");
                }
            }
            if read == 0 {
                bail!("pipe closed (zero-byte read)");
            }
            buf = &mut buf[read as usize..];
        }
        Ok(())
    }

    fn write_all(&self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            let overlapped = Overlapped::new()?;
            let mut written = 0u32;
            let ok = unsafe { WriteFile(self.handle, Some(buf), Some(&mut written), Some(&overlapped.inner as *const _ as *mut _)) };
            if ok.is_err() {
                let err = unsafe { windows::Win32::Foundation::GetLastError() };
                if err == ERROR_IO_PENDING {
                    unsafe { WaitForSingleObject(overlapped.event, INFINITE) };
                    let mut actually_written = 0u32;
                    let completed = unsafe {
                        GetOverlappedResult(self.handle, &overlapped.inner, &mut actually_written, false)
                    };
                    if completed.is_err() || actually_written == 0 {
                        bail!("pipe closed or overlapped WriteFile failed");
                    }
                    written = actually_written;
                } else {
                    bail!("pipe closed or WriteFile failed: {err:?}");
                }
            }
            if written == 0 {
                bail!("pipe closed (zero-byte write)");
            }
            buf = &buf[written as usize..];
        }
        Ok(())
    }
}

impl Drop for PipeConnection {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle).ok();
        }
    }
}
