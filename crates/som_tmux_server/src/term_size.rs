//! Reads the RELAY process's own terminal size (cols/rows) directly from
//! its stdout handle — used to detect when Som resizes the ConPTY/PTY this
//! process is sitting in, so the RELAY can tell the HOLDER to resize its
//! session to match. See `project_som_tmux` memory ("Обновление 20", the
//! "`RelayInput::Resize` — CONFIRMED, concrete gap" note): the HOLDER's
//! session used to be stuck at a hardcoded 80x24 forever because nothing
//! ever told it about Som's actual pane size, which is the root cause of
//! the "terminal doesn't fill the pane" symptom (and, less obviously, of
//! screen artifacts after the shell scrolls — CUP/scroll escape codes
//! generated against the wrong screen width land in the wrong place once
//! Som's `TerminalElement`, which DOES know the real pane size, interprets
//! them).
//!
//! No Win32/POSIX event exists in this codebase's dependency set to be
//! notified of a resize directly, so this is deliberately a poll: cheap
//! enough (a single syscall) to check every quarter second from a
//! dedicated thread without meaningfully loading anything, and simple
//! enough not to need a platform-specific event loop just for this one
//! signal.

/// Current size of the terminal this process's stdout is attached to, or
/// `None` if it can't be determined (e.g. stdout isn't actually a console/
/// tty — shouldn't normally happen for a RELAY, which Som always spawns
/// with a real PTY, but this is a poll loop, not a hard requirement, so a
/// transient failure just means "skip this tick" rather than crashing).
#[cfg(windows)]
pub fn current_size() -> Option<(u16, u16)> {
    use windows::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE};

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        GetConsoleScreenBufferInfo(handle, &mut info).ok()?;
        // `srWindow` is the actually-visible window into the (usually much
        // taller) scrollback screen buffer — `dwSize` alone would report
        // the buffer's full height, not what's currently on screen.
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
        Some((cols, rows))
    }
}

#[cfg(unix)]
pub fn current_size() -> Option<(u16, u16)> {
    #[repr(C)]
    #[derive(Default)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    let mut size = Winsize::default();
    // SAFETY: `size` is a valid, correctly-sized buffer for TIOCGWINSZ, and
    // stdout's fd is always valid for the lifetime of this call.
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result != 0 || size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    Some((size.ws_col, size.ws_row))
}
