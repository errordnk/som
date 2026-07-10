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

/// Puts this process's OWN stdin into raw mode (`cfmakeraw`: disables
/// canonical/line-buffered input, local echo, and signal-generating
/// control characters) when it's a real tty — a no-op otherwise (e.g. a
/// local/WSL RELAY, whose stdin is a plain Windows-ConPTY-backed PTY slave
/// with no separate `CONSOLE_MODE` layer in front of it, same reasoning as
/// this function's Windows sibling).
///
/// Needed specifically for an SSH `tmux: true` profile's RELAY once it's
/// given a real remote pty (`-tt`, forced so the real pane size can
/// propagate — see `wrap_remote_command_args`'s doc comment): a freshly
/// allocated pty defaults to COOKED mode (`ICANON`/`ECHO` on) same as any
/// other Unix tty, unlike the plain (non-tty) pipe stdin this RELAY used
/// to have before `-tt`. Left in cooked mode, the kernel's own tty driver
/// line-buffers everything this process reads off its stdin BEFORE it ever
/// reaches the code that forwards bytes to the HOLDER — confirmed root
/// cause of three real reported symptoms once `-tt` shipped: `htop` (and
/// any other program needing single-keystroke input) needing an extra
/// Enter to register a `q`, arrow keys/F-keys never reaching the remote
/// shell at all (ICANON's own line editor intercepts them instead of
/// passing the raw escape sequence through), and a lost first character on
/// backspace-heavy edits (the kernel's own line editor, not the real
/// shell's readline, absorbing it).
#[cfg(unix)]
pub fn enable_raw_input_mode() {
    use std::os::fd::BorrowedFd;

    // SAFETY: fd 0 (stdin) is valid for the lifetime of this call — a
    // `BorrowedFd` here doesn't take ownership or close anything.
    let stdin = unsafe { BorrowedFd::borrow_raw(0) };
    let Ok(mut termios) = nix::sys::termios::tcgetattr(stdin) else {
        // Not a tty (e.g. this RELAY's stdin is a plain pipe, as it always
        // was before -tt, or as WSL/local profiles' still are) — nothing
        // to do.
        return;
    };
    nix::sys::termios::cfmakeraw(&mut termios);
    let _ = nix::sys::termios::tcsetattr(stdin, nix::sys::termios::SetArg::TCSANOW, &termios);
}
