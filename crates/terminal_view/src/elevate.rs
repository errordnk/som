//! Elevation helpers behind `TabProfile::admin` — a local profile that must
//! run with administrator/root rights.
//!
//! An elevated process can't be a child of this (non-elevated) process's
//! PTY, so such a profile can't open as an ordinary in-process tab. It
//! opens as its own elevated Som window instead: this process re-launches
//! its own executable with `--admin-profile <name>` through the platform's
//! elevation mechanism —
//!   * Windows: `ShellExecuteW` with the `runas` verb (UAC prompt);
//!   * macOS: `osascript` with administrator privileges;
//!   * Linux/FreeBSD: `pkexec` (polkit), forwarding the display variables.
//!
//! See `workspace::som_admin` for what that child process does differently.

use anyhow::Result;

/// Outcome of asking the OS to start the elevated window.
#[derive(Debug, PartialEq, Eq)]
pub enum ElevatedLaunch {
    /// The elevation helper was started (or, on Windows, UAC was accepted).
    Started,
    /// The user dismissed the UAC prompt. Not an error — they chose not to.
    Cancelled,
}

/// `true` if opening a profile with `admin: profile_admin` has to go
/// through a separate elevated window: the profile asks for admin, and this
/// process is neither already elevated nor itself an admin-launched child
/// (the latter would otherwise relaunch itself forever).
pub fn needs_elevated_window(profile_admin: bool) -> bool {
    profile_admin && !workspace::som_admin::is_admin_session() && !is_process_elevated()
}

#[cfg(unix)]
pub fn is_process_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(target_os = "windows")]
pub fn is_process_elevated() -> bool {
    use util::ResultExt as _;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).log_err().is_none() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .log_err()
        .is_some();
        CloseHandle(token).log_err();
        queried && elevation.TokenIsElevated != 0
    }
}

/// Starts a new, elevated Som window for the profile named `profile_name`.
#[cfg(target_os = "windows")]
pub fn launch_elevated_window(profile_name: &str) -> Result<ElevatedLaunch> {
    use anyhow::Context as _;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, w};

    // `ShellExecuteW` reports "access denied" (5) when the user declines
    // the UAC prompt; anything above 32 is success, per its docs.
    const SE_ERR_ACCESSDENIED: isize = 5;

    let exe = std::env::current_exe().context("locating the running Som executable")?;
    let params = format!("--admin-profile {}", quote_windows_arg(profile_name));
    let result = unsafe {
        ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), &HSTRING::from(params), None, SW_SHOWNORMAL)
    };
    match result.0 as isize {
        code if code > 32 => Ok(ElevatedLaunch::Started),
        SE_ERR_ACCESSDENIED => Ok(ElevatedLaunch::Cancelled),
        code => anyhow::bail!(
            "ShellExecuteW(runas) failed with code {code}: {}",
            std::io::Error::last_os_error()
        ),
    }
}

/// Starts a new Som window running as root for the profile named
/// `profile_name`. The process keeps the invoking user's `settings.json`
/// (`--admin-config-dir`) but gets root's `HOME`, so everything it writes
/// (logs, caches, extracted artifacts) lands under root's home rather than
/// leaving root-owned files in the user's.
#[cfg(unix)]
pub fn launch_elevated_window(profile_name: &str) -> Result<ElevatedLaunch> {
    use anyhow::Context as _;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the running Som executable")?;
    let config_dir = paths::config_dir().clone();
    let som_args = [
        exe.to_string_lossy().into_owned(),
        "--admin-profile".to_owned(),
        profile_name.to_owned(),
        "--admin-config-dir".to_owned(),
        config_dir.to_string_lossy().into_owned(),
    ];

    let mut command = if cfg!(target_os = "macos") {
        // `do shell script ... with administrator privileges` waits for the
        // command, so background it and drop its output.
        let shell_command = format!(
            "env HOME=\"$(eval echo ~root)\" {} >/dev/null 2>&1 &",
            som_args.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ")
        );
        let script = format!(
            "do shell script \"{}\" with administrator privileges",
            applescript_escape(&shell_command)
        );
        let mut command = Command::new("osascript");
        command.arg("-e").arg(script);
        command
    } else {
        // `pkexec` resets the environment, so forward what a GUI process
        // needs to reach the user's display server.
        let mut command = Command::new("pkexec");
        command.arg("env");
        for var in ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
            if let Some(value) = std::env::var_os(var) {
                let mut assignment = std::ffi::OsString::from(format!("{var}="));
                assignment.push(value);
                command.arg(assignment);
            }
        }
        command.arg("HOME=/root").args(&som_args);
        command
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the privilege-elevation helper (pkexec / osascript)")?;

    // The helper blocks on the password prompt (and, for `pkexec`, for the
    // whole lifetime of the root window), so it's reaped off-thread. A
    // non-zero exit here is almost always the user dismissing the prompt.
    let profile_name = profile_name.to_owned();
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => log::info!(
            "elevation helper for admin profile {profile_name:?} exited with {status} (prompt dismissed or not authorized)"
        ),
        Err(err) => log::warn!("waiting for elevation helper failed: {err}"),
    });
    Ok(ElevatedLaunch::Started)
}

/// Single-quotes `arg` for a POSIX shell.
#[cfg(unix)]
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Escapes `text` for use inside an AppleScript double-quoted string.
#[cfg(unix)]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', r"\\").replace('"', "\\\"")
}

/// Quotes one argument per the Windows command-line rules
/// (`CommandLineToArgvW`): wrap in double quotes, escape embedded quotes,
/// and double any backslashes that precede a quote or the closing quote.
#[cfg(target_os = "windows")]
fn quote_windows_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn shell_quote_survives_single_quotes() {
        assert_eq!(super::shell_quote("it's"), r#"'it'\''s'"#);
    }

    #[cfg(unix)]
    #[test]
    fn applescript_escape_handles_quotes_and_backslashes() {
        assert_eq!(super::applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[cfg(target_os = "windows")]
    mod windows_quoting {
        use super::super::quote_windows_arg;

        #[test]
        fn plain_name_is_just_wrapped_in_quotes() {
            assert_eq!(quote_windows_arg("PowerShell Admin"), "\"PowerShell Admin\"");
        }

        #[test]
        fn embedded_quote_is_escaped() {
            assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        }

        #[test]
        fn trailing_backslash_is_doubled_so_it_cannot_escape_the_closing_quote() {
            assert_eq!(quote_windows_arg(r"dir\"), r#""dir\\""#);
        }

        #[test]
        fn backslash_before_a_quote_is_doubled_plus_one_escape() {
            assert_eq!(quote_windows_arg(r#"a\"b"#), r#""a\\\"b""#);
        }

        #[test]
        fn interior_backslashes_not_followed_by_a_quote_are_untouched() {
            assert_eq!(quote_windows_arg(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        }
    }
}
