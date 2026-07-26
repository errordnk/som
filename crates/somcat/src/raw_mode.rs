//! Cross-platform console raw-mode setup, deliberately hand-rolled instead
//! of depending on `crossterm` — see this crate's root doc comment for
//! why: `crossterm`'s own Windows implementation (confirmed by reading its
//! source directly, see `project_kitty_graphics_protocol` memory) never
//! sets `ENABLE_VIRTUAL_TERMINAL_INPUT`, without which the Windows console
//! does not deliver raw VT/escape byte sequences to a reading process's
//! stdin at all — exactly the bug this tool exists to route around.
//!
//! Returns an RAII guard so the original mode is always restored on drop,
//! even if the caller exits via an early `return`/`?` — mirrors the same
//! pattern `crates/terminal/src/bin/kitty_probe.rs` uses, just packaged as
//! a proper guard type here since `somcat` is a real user-facing tool, not
//! a throwaway test binary.

pub struct RawModeGuard {
    #[cfg(unix)]
    original: libc::termios,
    #[cfg(windows)]
    original: u32,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        restore(self);
    }
}

#[cfg(unix)]
pub fn enable() -> RawModeGuard {
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut original);
        let mut raw = original;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        RawModeGuard { original }
    }
}

#[cfg(unix)]
fn restore(guard: &RawModeGuard) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &guard.original);
    }
}

#[cfg(windows)]
pub fn enable() -> RawModeGuard {
    use windows::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let input_handle = GetStdHandle(STD_INPUT_HANDLE).expect("failed to get stdin handle");
        let mut original = Default::default();
        GetConsoleMode(input_handle, &mut original).expect("failed to get console input mode");

        // See this module's doc comment: ENABLE_VIRTUAL_TERMINAL_INPUT is
        // the one flag crossterm's own Windows raw-mode setup omits, and
        // is required for a terminal-response's raw escape bytes to reach
        // this process's stdin instead of being consumed/translated by
        // the console's own input parser.
        let raw = (original.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0;
        SetConsoleMode(input_handle, windows::Win32::System::Console::CONSOLE_MODE(raw))
            .expect("failed to set raw console input mode");

        // Also ensure the OUTPUT side interprets ANSI/VT escape sequences
        // (cursor movement, SGR colors) rather than treating them as
        // literal text — legacy `cmd.exe` consoles don't have this on by
        // default. Best-effort: some hosts (older Windows builds) don't
        // support this at all, which isn't fatal to Kitty graphics output
        // itself (that's parsed by whatever real terminal is hosting this
        // process, not by this console mode flag).
        if let Ok(output_handle) = GetStdHandle(STD_OUTPUT_HANDLE) {
            let mut out_mode = Default::default();
            if GetConsoleMode(output_handle, &mut out_mode).is_ok() {
                let _ = SetConsoleMode(
                    output_handle,
                    windows::Win32::System::Console::CONSOLE_MODE(
                        out_mode.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0,
                    ),
                );
            }
        }

        RawModeGuard { original: original.0 }
    }
}

#[cfg(windows)]
fn restore(guard: &RawModeGuard) {
    use windows::Win32::System::Console::{CONSOLE_MODE, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
    unsafe {
        if let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) {
            let _ = SetConsoleMode(handle, CONSOLE_MODE(guard.original));
        }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("somcat's raw_mode module only supports Unix and Windows");
