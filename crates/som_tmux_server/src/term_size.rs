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

/// Disables Windows console's line-buffering/local-echo on this process's
/// OWN stdin (the RELAY's, sitting inside the ConPTY Som created for it) —
/// without this, `std::io::stdin().read()` on Windows goes through
/// `ReadConsoleW`, which (per-console, independent of any ConPTY layering)
/// defaults to `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_
/// INPUT` regardless of what's actually attached on the other end. That
/// means every keystroke sent to this process gets buffered until a
/// newline AND is echoed back locally by Windows itself, completely
/// independent of anything this crate's own code does with the bytes
/// afterward.
///
/// This is confirmed as the root cause of THREE user-reported symptoms that
/// all trace back to this single gap: (1) `htop` needing `q` + Enter instead
/// of a bare `q` (line-buffering: nothing reaches its `read()` until a
/// newline), (2) typed command text showing up in whatever the console's
/// last-active color happened to be — cyan, from the prompt right before it
/// — instead of PSReadLine's own syntax-highlight color (this is Windows's
/// OWN local echo painting the character, not a byte `crate::redraw`
/// generated at all), and (3) PSReadLine's live suggestions/autocomplete
/// never triggering (it never sees individual keystrokes to react to, only
/// a complete line at a time, same underlying cause as (1)).
///
/// A real (non-tmux) shell never hits this: it's the DIRECT child of the
/// ConPTY Som created, and ConPTY's own conout/conin pipes are plain
/// anonymous pipes (see `alacritty_terminal::tty::windows::conpty`), not a
/// `CONSOLE_MODE`-governed console input buffer — there's no `ReadConsoleW`
/// anywhere in that path. This crate's RELAY, in contrast, calls
/// `std::io::stdin()` directly on Windows, which — being an ordinary
/// process with an ordinary attached console (the one ConPTY gave it) —
/// goes through the full legacy console API with all its defaults intact.
///
/// Also turns ON `ENABLE_VIRTUAL_TERMINAL_INPUT` — without it, Windows
/// still intercepts special keys (arrows, Home/End, function keys) and
/// reports them ONLY through the separate `INPUT_RECORD`-based console API
/// (`ReadConsoleInputW`), never as bytes in the plain byte stream this
/// process's `std::io::stdin().read()` actually reads — turning it on is
/// what makes Windows translate them into the ANSI escape sequences
/// (`\x1b[A` for up-arrow, etc.) any Unix PTY already sends, so a plain
/// byte-stream read sees them at all. Confirmed root cause of a second wave
/// of symptoms that only surfaced once `ENABLE_LINE_INPUT` above was turned
/// off (cooked mode had been silently doing its own arrow-key/Backspace
/// line-editing before that): arrow keys not working, and Backspace
/// deleting the ENTIRE typed line instead of one character (Windows' own
/// cooked-mode line editor was intercepting Backspace as ITS OWN "erase
/// line" gesture rather than it ever reaching the shell as the single
/// `\x7f`/`\x08` byte a Unix PTY would send).
///
/// Called once, right at RELAY startup, before the stdin-forwarding loop
/// ever starts reading — this is a per-console-handle setting, not
/// per-`read()` call, so it only needs to happen once for the process's
/// entire lifetime.
#[cfg(windows)]
pub fn enable_raw_input_mode() {
    use windows::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle,
        STD_INPUT_HANDLE, SetConsoleMode,
    };

    unsafe {
        let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) else { return };
        let mut mode = Default::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return;
        }
        mode &= !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        mode |= ENABLE_VIRTUAL_TERMINAL_INPUT;
        SetConsoleMode(handle, mode).ok();
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

/// No-op on Unix — see the Windows version's doc comment for what this
/// fixes there. On Unix, this process's stdin is a real PTY slave (the one
/// Som created for it), and reading it is a plain `read(2)` on that PTY's
/// fd — there's no separate `CONSOLE_MODE`-governed input layer sitting in
/// front of it the way `ReadConsoleW` implies on Windows, so there's
/// nothing here to disable.
#[cfg(unix)]
pub fn enable_raw_input_mode() {}
