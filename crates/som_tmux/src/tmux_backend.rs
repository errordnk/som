//! Unix-only replacement for the from-scratch RELAY/HOLDER/`redraw.rs`
//! transparent-proxy architecture (`crate::relay`/`crate::server`) — see
//! `SOM_MUX_PLAN.md`'s "som-tmux v2" section for the full rationale and
//! `project_som_tmux` memory's "Обновление 32" for the short version: that
//! architecture diffed a parsed screen grid and replayed only visible
//! content, which structurally cannot carry terminal-mode negotiation
//! (DECCKM, Device Attributes queries) or, per two separately-confirmed
//! bugs, even some raw byte shapes over certain transports — every one of
//! which a real, installed `tmux` already handles correctly, since it's a
//! transparent PTY multiplexer, not a content-diffing one.
//!
//! Deliberately Unix-only: `tmux` has no native Windows port with real PTY
//! support. The Windows-local profile (`dnk`, `pwsh.exe`) keeps using
//! `crate::relay`/`crate::server` unchanged — see `main.rs`'s dispatch.
//!
//! This module replaces BOTH the old HOLDER and RELAY roles with a much
//! smaller pair of responsibilities:
//! - "ensure a session exists" (`ensure_session`, what the old HOLDER's
//!   `Session::spawn` did) — ~10 lines instead of an entire `session.rs`.
//! - "proxy bytes between Som's own PTY and `tmux attach-session`"
//!   (`attach_and_proxy`, what the old RELAY's `connect_or_become_holder`
//!   + read/write loops did) — no protocol framing at all, since there's
//!   no data to frame: it's a raw byte pipe now, same as Som's own plain
//!   (non-tmux) terminal already is for an ordinary shell.

use anyhow::Context;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// The tmux config this crate writes and passes via `-f` to every `tmux`
/// invocation it makes — never the user's own `~/.tmux.conf` (Som owns all
/// tab/split/keybinding UI itself; a SECOND, real tmux's own keybindings/
/// status-bar competing with that would be actively wrong, not just
/// redundant). Written once per HOLDER-role invocation (see
/// `ensure_session`) to a fixed, well-known path rather than a tempfile —
/// deliberately shared across every pane/profile on this machine (there's
/// nothing pane-specific in it), so re-writing it on every single pane
/// creation is wasted work, not a correctness requirement; it's rewritten
/// unconditionally anyway (cheap, and self-healing if a user or a future
/// version of this file ever changes its contents).
fn minimal_config_path() -> std::path::PathBuf {
    paths::config_dir().join("som-tmux.conf")
}

/// `prefix None` disables ALL of tmux's own keybindings that go through
/// its prefix key (the vast majority of them) — Som has its own tab/split
/// keybindings and must never compete with tmux's (e.g. tmux's own
/// Ctrl+B-prefixed split/kill/detach bindings would otherwise still work
/// underneath whatever Som itself binds those same keys to). `status off`
/// removes tmux's own status bar — Som's tab bar is the only tab-like UI
/// the user should ever see. `history-limit` maps Som's own
/// `TerminalSettings::max_scroll_history_lines` onto tmux's equivalent
/// option (see `ensure_session`'s `scrollback` parameter) — written as
/// part of this same file rather than a separate `set-option` call after
/// `new-session`, so a session's scrollback is correct from its very
/// first line of output, not just after a follow-up command runs.
fn write_minimal_config(scrollback: Option<usize>) -> anyhow::Result<std::path::PathBuf> {
    let path = minimal_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {parent:?}"))?;
    }
    let history_limit = scrollback.unwrap_or(10_000);
    let contents = format!(
        "set -g prefix None\n\
         set -g status off\n\
         set -g history-limit {history_limit}\n\
         set -g mouse off\n"
    );
    std::fs::write(&path, contents).with_context(|| format!("failed to write tmux config to {path:?}"))?;
    Ok(path)
}

/// Runs `tmux -f <config> <args...>` to completion and returns whether it
/// succeeded — used for the fire-and-forget administrative commands
/// (`has-session`/`new-session`/`kill-session`), none of which need their
/// stdout/stderr inspected by callers here (a failed `has-session` just
/// means "no session yet", not an error to propagate).
///
/// `setsid()` (via `pre_exec`) is required here, not optional — without
/// it, `tmux new-session -d ...` (run from a RELAY that is ITSELF sitting
/// inside a pty, since Som spawned it into one) reliably hung instead of
/// returning immediately as `-d`/detached is documented to do: confirmed
/// directly (a plain shell invocation of the exact same command returned
/// in ~0.03s, while the identical command run as this process's own child
/// without `setsid()` never returned at all, even though the tmux SESSION
/// it created was itself fully alive and working). tmux's own client
/// process apparently doesn't fully detach from a controlling terminal it
/// inherits, so it has to be given a new session (in the POSIX sense, not
/// the tmux sense) explicitly before it execs.
fn run_tmux(config_path: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("tmux");
    command.arg("-f").arg(config_path).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            // SAFETY: `setsid()` is async-signal-safe and the only thing
            // done here between fork and exec.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.status()
}

/// Ensures a detached tmux session named `pane_id` exists, creating one
/// running `program`/`args` at `cols`x`rows` if it doesn't — this is the
/// ENTIRE replacement for the old HOLDER's job (`server::run` +
/// `Session::spawn` + the redraw-forwarding machinery). Idempotent: safe
/// to call every time a pane is (re)attached to, not just the first time,
/// since `has-session` short-circuits to a no-op when the session already
/// survived a prior Som close — that survival, unchanged from the old
/// design's goal, is the entire point of this module existing.
pub fn ensure_session(pane_id: &str, program: &str, args: &[String], cwd: Option<&str>, cols: u16, rows: u16, scrollback: Option<usize>) -> anyhow::Result<()> {
    let config_path = write_minimal_config(scrollback)?;

    if run_tmux(&config_path, &["has-session", "-t", pane_id]).map(|status| status.success()).unwrap_or(false) {
        return Ok(()); // already running — exactly the survive-restart case this exists for
    }

    let mut new_session_args: Vec<String> = vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        pane_id.to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ];
    if let Some(cwd) = cwd {
        new_session_args.push("-c".to_string());
        new_session_args.push(cwd.to_string());
    }
    new_session_args.push(program.to_string());
    new_session_args.extend(args.iter().cloned());

    let arg_refs: Vec<&str> = new_session_args.iter().map(String::as_str).collect();
    let status = run_tmux(&config_path, &arg_refs).with_context(|| format!("failed to spawn tmux for pane {pane_id:?}"))?;
    anyhow::ensure!(status.success(), "tmux new-session exited with {status:?} for pane {pane_id:?}");
    Ok(())
}

/// Runs as the RELAY: ensures the session exists (see `ensure_session`),
/// then execs `tmux attach-session` as a child of THIS process and
/// transparently shuttles bytes between Som's own PTY (this process's own
/// stdin/stdout — exactly like a plain, non-tmux shell already is for
/// Som) and that child's own pty. No message framing, no protocol — tmux's
/// own terminal handling is what makes DECCKM/DA-queries/arbitrary escape
/// sequences just work, which is the entire reason this module exists
/// instead of the old redraw-diffing design.
///
/// The attach-session child needs a REAL pty, not a plain pipe — confirmed
/// the hard way: `tmux attach-session` with `Stdio::piped()` for its
/// stdin/stdout immediately exits with "open terminal failed: not a
/// terminal" (tmux checks `isatty()`), which left it a zombie and this
/// RELAY hanging forever waiting on a child that had already died. A
/// plain (non-tmux) Som terminal never hits this because Som's own PTY
/// layer (`alacritty_terminal::tty`) already gives the real shell a real
/// pty; this module has to make its own for the SAME reason, one layer
/// further down, since it's proxying rather than being that real shell
/// itself.
///
/// A lone NUL byte on this process's own stdin is still
/// `TerminalView::on_removed`'s explicit "the user closed this tab, kill
/// the real shell for good" signal (unchanged convention from the old
/// design) — checked here, before forwarding, and translated into
/// `tmux kill-session` rather than the old `RelayInput::Close`/
/// `session.kill()`.
pub fn run(pane_id: &str, program: &str, args: &[String], cwd: Option<&str>, scrollback: Option<usize>) -> anyhow::Result<()> {
    let (cols, rows) = crate::term_size::current_size().unwrap_or((80, 24));
    ensure_session(pane_id, program, args, cwd, cols, rows, scrollback)?;

    let config_path = minimal_config_path();

    let pty = nix::pty::openpty(
        Some(&nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }),
        None,
    )
    .context("failed to open a pty for the tmux attach-session child")?;

    let mut command = Command::new("tmux");
    command.arg("-f").arg(&config_path).args(["attach-session", "-t", pane_id]);
    // SAFETY: `dup`/`setsid`/`ioctl(TIOCSCTTY)` are all async-signal-safe,
    // and this is the only thing done between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        let slave_fd = std::os::fd::AsRawFd::as_raw_fd(&pty.slave);
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Makes the slave side this new session's controlling
            // terminal — without this, tmux's own `isatty()` check on its
            // stdin/stdout passes, but it never receives a SIGWINCH on
            // resize and some terminal-state ioctls behave as if there
            // were no controlling terminal at all.
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(slave_fd, 0) == -1 || libc::dup2(slave_fd, 1) == -1 || libc::dup2(slave_fd, 2) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The slave fd only needs to exist long enough for `dup2` inside
    // `pre_exec` (which runs in the forked child, so this parent-side
    // `Stdio::null()` doesn't fight over the same fd) — the actual
    // stdin/stdout/stderr this child ends up using are the `dup2`'d
    // copies set up above, not these.
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = command.spawn().with_context(|| format!("failed to spawn tmux attach-session for pane {pane_id:?}"))?;

    // Close this process's own copy of the slave fd now that the child
    // has it (inherited via `dup2` in `pre_exec`, so it doesn't need this
    // parent-side copy to keep working) — standard pty practice: a parent
    // holding the slave open too can prevent the child from ever seeing a
    // clean EOF-equivalent condition on its own end, since some other
    // process (this one) would still count as a potential future writer.
    drop(pty.slave);

    // The master fd is what this process actually reads/writes — cloning
    // it (rather than sharing one `File` across the reader thread and the
    // main thread below) mirrors `pipe::unix::PipeConnection`'s own
    // reasoning (see that module's doc comment): a reader blocked on one
    // fd must not block a concurrent writer on the other direction.
    let master_read: std::fs::File = pty.master.try_clone().context("failed to clone pty master fd for reading")?.into();
    let mut master_write: std::fs::File = pty.master.into();

    // Copies the attach-session child's pty output straight to this
    // process's own stdout (Som's PTY) on a dedicated thread — the main
    // thread below handles the other direction (Som's PTY input -> the
    // pty master, and on to tmux) so the NUL-byte-as-close-signal check
    // can live inline with reading Som's own input, mirroring where
    // `crate::relay::run`'s equivalent check used to live.
    let reader_thread = std::thread::spawn(move || -> std::io::Result<()> {
        let mut master_read = master_read;
        let mut buf = [0u8; 4096];
        let mut stdout = std::io::stdout();
        loop {
            let read = match master_read.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                // A pty master read fails with EIO once its slave side has
                // no more open references (the attach-session child
                // exited) — the real-world equivalent of EOF for a pty,
                // not a genuine error to propagate.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(err) => return Err(err),
            };
            stdout.write_all(&buf[..read])?;
            stdout.flush()?;
        }
    });

    // Polls this process's own stdout (Som's PTY, resized whenever Som
    // resizes the pane/window) for size changes and applies them to the
    // attach-session child's pty via `TIOCSWINSZ` — same poll-based
    // approach `crate::relay::run`'s equivalent thread uses (see
    // `crate::term_size`'s module doc comment for why polling rather than
    // an event subscription), just applied directly to a real pty's
    // window size instead of forwarding a `RelayInput::Resize` message
    // (there's no message protocol left to forward one over). `TIOCSWINSZ`
    // itself is what delivers the real `SIGWINCH` to the attach-session
    // child's own process group — nothing here sends a signal directly.
    // TEMPORARILY DISABLED while diagnosing a real corruption bug (see
    // `project_som_tmux` memory "Обновление 33"/34): typed text through
    // this pipeline showed up on the REAL remote screen (confirmed via
    // `tmux capture-pane`, not just Som's own parsing) as garbled
    // fragments looking like broken DA-query-response escape sequences.
    // Suspected (not yet confirmed) that this thread's periodic
    // `TIOCSWINSZ` was triggering bash's readline to re-query the
    // terminal (cursor position/DA) more often than expected, interacting
    // badly with concurrent stdin forwarding. Re-enable once the real
    // cause is isolated — resize is still a real, needed feature, just
    // not proven innocent yet.
    #[allow(unused_variables)]
    let resize_thread_disabled = (cols, rows, master_write.try_clone());

    let mut buf = [0u8; 4096];
    loop {
        let read = match std::io::stdin().read(&mut buf) {
            Ok(0) => break, // EOF — Som closed its side
            Ok(n) => n,
            Err(_) => break,
        };
        if buf[..read].contains(&0u8) {
            run_tmux(&config_path, &["kill-session", "-t", pane_id]).ok();
            break;
        }
        if master_write.write_all(&buf[..read]).is_err() {
            break; // attach-session child gone
        }
        master_write.flush().ok();
    }

    drop(master_write);
    child.kill().ok();
    // Best-effort — the child exiting/being killed ends the reader
    // thread's own read loop (EIO).
    drop(reader_thread);
    drop(resize_thread_disabled);
    Ok(())
}
