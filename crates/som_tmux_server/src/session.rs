use crate::bounds::SessionBounds;
use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config;
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle};
use anyhow::{Context as _, Result};
use std::sync::Arc;
use uuid::Uuid;

/// Mirrors Som's own `terminal::terminal_settings::CursorShape -> AlacCursorStyle`
/// conversion (`crates/terminal/src/terminal_settings.rs`) — duplicated
/// here rather than depending on that crate (which pulls in GPUI and the
/// rest of Som's editor-side settings machinery, entirely unwanted in a
/// small standalone server binary) for the 4 variants that actually exist.
/// Parses the plain string Som passes via `--cursor-shape` (see
/// `main.rs`'s `Args` doc comment) rather than a shared enum type.
fn parse_cursor_shape(shape: Option<&str>) -> AlacCursorStyle {
    let shape = match shape {
        Some("underline") => AlacCursorShape::Underline,
        Some("bar") => AlacCursorShape::Beam,
        Some("hollow") => AlacCursorShape::HollowBlock,
        _ => AlacCursorShape::Block, // "block", unrecognized, or None
    };
    AlacCursorStyle { shape, blinking: false }
}

/// Mirrors `terminal::ZedListener` (`crates/terminal/src/terminal.rs`) —
/// alacritty's `EventLoop` needs somewhere to forward parse events. The only
/// one the server cares about is `Wakeup` ("the grid changed, redraw") —
/// alacritty's IO thread parses PTY bytes directly into `Term` itself and
/// never exposes the raw bytes, only this "something changed" signal, so
/// that's what drives sending updated snapshots to attached clients.
///
/// Uses `async_channel` (not `futures::channel::mpsc`) specifically because
/// `server.rs`'s connection threads are blocking (thread-per-connection over
/// synchronous Win32 named pipe calls, not an async runtime) and need
/// `Receiver::recv_blocking()` to bridge into that world without pulling in
/// a whole async executor just for this one channel.
#[derive(Clone)]
struct ServerListener(async_channel::Sender<AlacTermEvent>);

impl EventListener for ServerListener {
    fn send_event(&self, event: AlacTermEvent) {
        log::debug!("ServerListener received event: {event:?}");
        self.0.try_send(event).ok();
    }
}

/// One session = one pane's PTY + its own `alacritty_terminal::Term`. The
/// server keeps parsing PTY output into this `Term` continuously (whether or
/// not a client is currently attached), so a (re)attaching client can be
/// handed the current screen content immediately instead of replaying raw
/// bytes across the gap.
pub struct Session {
    pub id: Uuid,
    term: Arc<FairMutex<Term<ServerListener>>>,
    pty_tx: Notifier,
    /// A `Mutex` (not a plain field) purely so `resize` can take `&self`
    /// instead of `&mut self` — `Session` is shared via `Arc` across
    /// several threads (the shell-exit watcher, the redraw forwarder, the
    /// RELAY connection's read loop), none of which hold exclusive access,
    /// and resize is rare/cheap enough that a `Mutex` here isn't worth
    /// restructuring the rest of `Session` around a `Mutex<Session>`
    /// wrapper just to support this one method.
    last_bounds: Arc<std::sync::Mutex<SessionBounds>>,
    /// PID of the spawned shell/program, captured before the `Pty` handle is
    /// moved into `EventLoop` (which owns it from then on and doesn't expose
    /// it back out) — this is the only way to actually kill the process on
    /// `CloseSession`, since alacritty's `EventLoop`/`Notifier` only exposes
    /// writing bytes and resizing, not process control.
    pid: Option<sysinfo::Pid>,
    /// Every live subscriber's own private channel — see `subscribe()`'s
    /// doc comment for why this exists instead of one shared
    /// `events_rx` field every consumer read from directly (the bug that
    /// used to be here: `async_channel` is a competing-consumers queue, NOT
    /// a broadcast — each `Wakeup` went to exactly ONE of potentially
    /// several readers, not all of them). Also held by the broadcaster
    /// thread spawned in `spawn()` (a second `Arc` clone, not through
    /// `Session` itself, since that thread starts before `Session` exists).
    subscribers: Arc<std::sync::Mutex<Vec<async_channel::Sender<AlacTermEvent>>>>,
}

impl Session {
    /// `cursor_shape`/`scrollback` mirror Som's own `TerminalSettings` (see
    /// `main.rs`'s `Args` doc comment for why they arrive as a plain string/
    /// `usize` rather than shared setting types) — without these, the
    /// `Term` this HOLDER owns silently used alacritty's own defaults
    /// (block cursor, hardcoded default scrollback) regardless of whatever
    /// the user actually configured in Som's `settings.json`, which is
    /// exactly the "cursor isn't the shape I configured" bug this fixes.
    pub fn spawn(
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        bounds: SessionBounds,
        cursor_shape: Option<String>,
        scrollback: Option<usize>,
    ) -> Result<Self> {
        // Mirrors `util::shell::ShellKind::tty_escape_args` (not depended on
        // directly — it pulls in gpui_util/git2/etc., far too heavy for
        // this small standalone binary — just the one bit of logic this
        // needs, reimplemented locally): `cmd.exe` is the ONE shell that
        // must NOT have its arguments escaped (escaping produces too many
        // quotes for CMD to parse), every other shell (crucially including
        // PowerShell) needs proper escaping. This was hardcoded `false`
        // (the cmd.exe-only behavior) UNCONDITIONALLY before — for
        // PowerShell specifically, that meant `tty::windows::cmdline` built
        // its command line with UNESCAPED arguments. Found and fixed while
        // investigating a reported PSReadLine continuation-prompt (">>")
        // artifact, but ruled out as ITS cause by direct experiment (the
        // artifact still reproduced with this fix alone) — see `relay.rs`'s
        // `strip_cr_induced_lf` for the actual fix. Kept anyway: it's a
        // real correctness gap on its own (matches what Som's own regular
        // terminal already does for every shell it spawns).
        let escape_args = !program.eq_ignore_ascii_case("cmd.exe")
            && !std::path::Path::new(&program)
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"));
        let shell = tty::Shell::new(program, args);
        // Mirrors `terminal::insert_zed_terminal_env` — without an explicit
        // `TERM`, the shell/its line editor (confirmed cause: PowerShell's
        // PSReadLine) has to guess the terminal's capabilities from
        // whatever platform default it falls back to, which does not
        // necessarily match how this crate's own `redraw.rs`/`Term` actually
        // behaves. `TERM=xterm-256color` is exactly what Som's own regular
        // (non-tmux) terminal already sets for every shell it spawns.
        let mut env = std::collections::HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        let pty_options = tty::Options {
            shell: Some(shell),
            working_directory: cwd.map(std::path::PathBuf::from),
            drain_on_exit: true,
            env,
            #[cfg(not(windows))]
            child_signal_mask: None,
            #[cfg(windows)]
            escape_args,
        };

        let pty = tty::new(&pty_options, bounds.into(), 0).context("failed to spawn pty")?;
        let pid = process_pid(&pty);

        let term_config = Config {
            default_cursor_style: parse_cursor_shape(cursor_shape.as_deref()),
            scrolling_history: scrollback.unwrap_or(Config::default().scrolling_history),
            ..Config::default()
        };

        let (raw_events_tx, raw_events_rx) = async_channel::unbounded();
        let term = Term::new(term_config, &bounds, ServerListener(raw_events_tx.clone()));
        let term = Arc::new(FairMutex::new(term));

        let event_loop = EventLoop::new(
            term.clone(),
            ServerListener(raw_events_tx),
            pty,
            pty_options.drain_on_exit,
            false,
        )
        .context("failed to create pty event loop")?;

        let pty_tx = Notifier(event_loop.channel());
        // Mirrors terminal.rs's own "DANGER" comment: this detaches the IO
        // thread — nothing here ever joins it, it runs for the process's
        // whole lifetime (which is exactly what we want: the server outlives
        // any single client, so there's no "shutdown" moment to join at
        // other than process exit).
        let _io_thread = event_loop.spawn();

        // A permanent pump thread, running for the session's whole lifetime
        // regardless of whether any client is attached — NOT something that
        // only starts on `Attach`. This matters because programs routinely
        // send escape sequences that expect an immediate response from
        // "the terminal" (e.g. Device Attributes queries; PowerShell/ConPTY
        // sends one on startup) via `AlacTermEvent::PtyWrite`, and if nothing
        // ever answers those, the program can sit there waiting and produce
        // no further visible output at all — which is exactly the bug this
        // fixes (found via manual smoke-testing: `snapshot_text()` came back
        // as 1920 blank cells because PowerShell was blocked waiting on a
        // `[?6c` reply that nothing was sending). `terminal.rs` gets away
        // with handling `PtyWrite` lazily (in its own `process_event`,
        // driven by GPUI's render loop) because a GPUI window is always
        // "polling" while visible; the server has no such render loop, so it
        // needs its own always-on consumer.
        //
        // Also the ONLY place that reads `raw_events_rx` — a `Wakeup`/`Exit`
        // is broadcast to every current subscriber (see `subscribe()`'s doc
        // comment for the bug this replaced: several threads used to all
        // `recv_blocking()` the SAME single channel directly, which is a
        // competing-consumers queue, not a broadcast — a `Wakeup` meant for
        // the redraw-forwarder could just as easily be silently consumed by
        // the unrelated shell-exit-watcher thread instead, and the RELAY
        // would never learn the screen changed at all).
        let pump_pty_tx = Notifier(pty_tx.0.clone());
        let subscribers: Arc<std::sync::Mutex<Vec<async_channel::Sender<AlacTermEvent>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let pump_subscribers = subscribers.clone();
        let last_bounds = Arc::new(std::sync::Mutex::new(bounds));
        let pump_last_bounds = last_bounds.clone();
        std::thread::spawn(move || {
            while let Ok(event) = raw_events_rx.recv_blocking() {
                match event {
                    AlacTermEvent::PtyWrite(bytes) => pump_pty_tx.notify(bytes.into_bytes()),
                    // Answers a program's own terminal-size query (e.g. the
                    // `\x1b[18t` "report text area size" escape a full-
                    // screen editor like `micro` sends on startup) with this
                    // session's ACTUAL current bounds — mirrors
                    // `terminal::Terminal::process_event`'s identical
                    // handling of the same event for Som's own regular
                    // (non-tmux) terminal. Left unanswered (as this used to
                    // be, falling into the `_ => {}` catch-all below), a
                    // program that sizes its own layout off this reply
                    // rather than the PTY's actual `WindowSize` has no way
                    // to learn the real pane dimensions — confirmed root
                    // cause of a reported "`micro` doesn't fill the whole
                    // pane width" bug (unlike `htop`, which apparently sizes
                    // itself off the PTY dimensions directly and doesn't
                    // send this query at all, which is why fixing THIS
                    // session's resize-propagation earlier fixed htop but
                    // not micro).
                    AlacTermEvent::TextAreaSizeRequest(format) => {
                        let bounds = *pump_last_bounds.lock().unwrap();
                        let window_size: WindowSize = bounds.into();
                        pump_pty_tx.notify(format(window_size).into_bytes());
                    }
                    AlacTermEvent::Wakeup => {
                        let subs = pump_subscribers.lock().unwrap();
                        for sub in subs.iter() {
                            sub.try_send(AlacTermEvent::Wakeup).ok();
                        }
                    }
                    AlacTermEvent::Exit => {
                        let subs = pump_subscribers.lock().unwrap();
                        for sub in subs.iter() {
                            sub.try_send(AlacTermEvent::Exit).ok();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            id: Uuid::new_v4(),
            term,
            pty_tx,
            last_bounds,
            pid,
            subscribers,
        })
    }

    /// Registers a new, independent receiver for this session's `Wakeup`/
    /// `Exit` events — every call gets its OWN channel, and the pump thread
    /// started in `spawn()` broadcasts each event to ALL of them, not just
    /// whichever one happens to `recv` first. Each of the HOLDER's several
    /// consumers (the shell-exit watcher in `server.rs::run`, and one
    /// redraw-forwarder per connected RELAY in `spawn_forwarder`) must call
    /// this to get its own feed — sharing one `Receiver` between them was
    /// the actual root cause of a reported PowerShell PSReadLine
    /// continuation-prompt (">>") artifact: with a shared MPMC channel, a
    /// `Wakeup` meant for the redraw-forwarder could be silently consumed by
    /// the exit-watcher thread instead (which does nothing with a `true`
    /// result besides looping back around), so the RELAY never learned the
    /// screen had changed and the client-side terminal never advanced past
    /// its last-known state — indistinguishable from dropped input from the
    /// user's point of view.
    pub fn subscribe(&self) -> async_channel::Receiver<AlacTermEvent> {
        let (tx, rx) = async_channel::unbounded();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Kills the underlying process for real (used for an explicit
    /// `CloseSession` — as opposed to a client just disconnecting, which
    /// leaves the session alive so it can be re-attached to).
    pub fn kill(&self) -> bool {
        let Some(pid) = self.pid else { return false };
        let refresh_kind = sysinfo::ProcessRefreshKind::nothing();
        let mut system = sysinfo::System::new();
        if system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            refresh_kind,
        ) != 1
        {
            return false;
        }
        system.process(pid).is_some_and(|process| process.kill())
    }

    pub fn write(&self, bytes: Vec<u8>) {
        self.pty_tx.notify(bytes);
    }

    /// Direct text dump of the underlying `Term`'s grid, bypassing
    /// `crate::redraw` entirely — used by tests to distinguish "the real
    /// shell/ConPTY produced this content" from "this crate's own diff/
    /// redraw serialization corrupted it". Not used by production code
    /// (the HOLDER always goes through `redraw()`), kept as a test-only
    /// diagnostic tool since it was exactly what isolated a real bug (see
    /// `redraw.rs`'s `pressing_enter_at_a_wide_pane_size_...` test) to
    /// `relay.rs` instead of here.
    pub fn diag_grid_text(&self) -> String {
        let term = self.term.lock();
        term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect()
    }

    pub fn resize(&self, bounds: SessionBounds) {
        let mut last_bounds = self.last_bounds.lock().unwrap();
        if bounds == *last_bounds {
            return;
        }
        *last_bounds = bounds;
        self.term.lock().resize(bounds);
        let window_size: WindowSize = bounds.into();
        self.pty_tx.0.send(Msg::Resize(window_size)).ok();
    }

    /// Serializes the current grid into ANSI bytes via `redrawer`, writing
    /// them to `out` — see `crate::redraw` module doc comment for why this
    /// exists (transparent-PTY-proxy architecture: `som-tmux-server` must
    /// emit plain ANSI on its own stdout, not a structured protocol, so
    /// Som's own unmodified `TerminalElement` can parse it like any other
    /// shell). `redrawer` carries the diff state; pass the SAME `Redrawer`
    /// across repeated calls for incremental updates (only what changed
    /// gets emitted), or a freshly-`Redrawer::new()` one for a full
    /// redraw (a newly-(re)attached client needs to see the whole screen).
    pub fn redraw(&self, redrawer: &mut crate::redraw::Redrawer, out: &mut impl std::io::Write) -> std::io::Result<()> {
        let term = self.term.lock();
        redrawer.redraw(&term, out)
    }

    /// Waits for the next `Wakeup` on `events_rx` (i.e. the grid actually
    /// changed) — `events_rx` must be a receiver obtained from THIS
    /// session's own `subscribe()`, not shared with any other caller (see
    /// `subscribe()`'s doc comment for why: this used to take no receiver
    /// argument at all and read a single field shared across every
    /// consumer, which silently dropped events between competing threads).
    /// Only ever carries `Wakeup`/`Exit` (everything else, notably
    /// `PtyWrite`, is handled internally by the pump thread started in
    /// `spawn`, never forwarded here). Returns `false` on `Exit`/channel
    /// closed. Blocking (not async) — every consumer in this crate (the
    /// shell-exit watcher, the redraw forwarder) runs on a plain OS thread,
    /// not an async executor.
    pub fn next_change_blocking(&self, events_rx: &async_channel::Receiver<AlacTermEvent>) -> bool {
        loop {
            match events_rx.recv_blocking() {
                Ok(AlacTermEvent::Wakeup) => return true,
                Ok(AlacTermEvent::Exit) => return false,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }
}

/// Grabs the spawned process's PID before `pty` is moved into `EventLoop`
/// (which owns it exclusively from then on). Platform APIs differ: Unix's
/// `Pty::child()` is a plain `std::process::Child`; Windows's
/// `Pty::child_watcher()` wraps a job-object-based watcher (see
/// `terminal::pty_info::ProcessIdGetter` for the same pattern used
/// elsewhere in this repo).
#[cfg(unix)]
fn process_pid(pty: &tty::Pty) -> Option<sysinfo::Pid> {
    Some(sysinfo::Pid::from_u32(pty.child().id()))
}

#[cfg(windows)]
fn process_pid(pty: &tty::Pty) -> Option<sysinfo::Pid> {
    pty.child_watcher()
        .pid()
        .map(|pid| sysinfo::Pid::from_u32(u32::from(pid)))
}

// Color/attribute round-trip through a real PTY is covered by
// `crate::redraw`'s tests (`redraw_round_trips_colored_text_through_a_real_
// parser`), which exercises `Session::redraw` — the actual method used in
// production now that `Session::snapshot`/the old `GridSnapshot` protocol
// are gone. No separate test needed here for the same behavior.
