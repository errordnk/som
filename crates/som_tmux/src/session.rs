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
pub(crate) fn parse_cursor_shape(shape: Option<&str>) -> AlacCursorStyle {
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

/// A `std::io::Write` sink that broadcasts every write to every currently
/// registered raw-bytes subscriber — passed to `EventLoop::with_raw_byte_
/// sink` (see that method's doc comment on the `errordnk/alacritty` fork)
/// so each byte the event loop reads off the real PTY, BEFORE it's parsed
/// into the HOLDER's own headless `Term`, also reaches every connected
/// RELAY directly. This is what lets a (re)connected RELAY get an initial
/// `Session::snapshot()` (the full, already-parsed state) and then stay
/// current via this raw byte stream afterward, without the HOLDER ever
/// needing to diff/replay ANSI itself (v1's `redraw.rs`, which this
/// architecture replaces) — the RELAY just feeds these same bytes through
/// its OWN local `Term`'s ordinary ANSI parser, same as Som's regular
/// (non-tmux) terminal already does for a directly-owned shell.
struct RawByteBroadcaster {
    subscribers: Arc<std::sync::Mutex<Vec<async_channel::Sender<Vec<u8>>>>>,
    /// The last chunk written, plus when — see `write`'s doc comment.
    /// `None` until the first real write.
    last_write: Option<(Vec<u8>, std::time::Instant)>,
}

impl std::io::Write for RawByteBroadcaster {
    /// Drops an incoming chunk that is BYTE-FOR-BYTE identical to the
    /// immediately preceding one AND arrived within a short window of it
    /// (`DUPLICATE_WRITE_WINDOW`) — a workaround for a confirmed real
    /// duplication bug in the Windows ConPTY reader path this crate's
    /// `EventLoop::with_raw_byte_sink` sits downstream of: the exact same
    /// chunk (confirmed via direct byte-for-byte instrumentation, e.g. a
    /// literal `[13, 10]` for a lone Enter keystroke's echoed newline) gets
    /// written to this sink twice in immediate succession (single-digit
    /// milliseconds apart), specifically once real keystrokes start
    /// flowing over an SSH `tmux: true` profile — never on the very first,
    /// read-only snapshot. Root cause not fully isolated in the upstream
    /// `alacritty_terminal` fork (a `Pty::reregister` fix that looked like
    /// a strong candidate did not eliminate it in direct reproduction), so
    /// this dedup is the actual fix point for now. Deliberately narrow
    /// (exact byte match, short window) so it can't silently eat a
    /// legitimate repeat happening for an unrelated reason seconds apart —
    /// confirmed via direct instrumentation that the real duplicate always
    /// lands within single-digit milliseconds of the original.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        const DUPLICATE_WRITE_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);
        let now = std::time::Instant::now();
        let original_len = buf.len();
        let mut buf = buf;
        let owned;
        if let Some((last_buf, last_at)) = &self.last_write {
            let recent = now.duration_since(*last_at) < DUPLICATE_WRITE_WINDOW;
            // Case 1: the WHOLE new chunk exactly repeats the last one.
            if recent && last_buf.as_slice() == buf {
                return Ok(original_len);
            }
            // Case 2: the new chunk STARTS WITH the last one repeated
            // again (e.g. last write was `[13, 10]`, this write is
            // `[13, 10, 13, 10, ...]`) — strip just the duplicated
            // prefix, forward the rest. Confirmed via direct
            // instrumentation this shape happens too, not just whole-
            // chunk repeats: the duplication doesn't consistently land on
            // its own separate `write()` call.
            if recent && !last_buf.is_empty() && buf.len() > last_buf.len() && buf.starts_with(last_buf.as_slice()) {
                owned = buf[last_buf.len()..].to_vec();
                buf = &owned;
            }
        }
        self.last_write = Some((buf.to_vec(), now));

        let subs = self.subscribers.lock().unwrap();
        for sub in subs.iter() {
            // `try_send`, not `send`/blocking — a slow or vanished RELAY
            // must never stall the HOLDER's own PTY-reading thread (which
            // also feeds the headless `Term` that every OTHER RELAY, and
            // this session's own liveness, depends on).
            sub.try_send(buf.to_vec()).ok();
        }
        Ok(original_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
    /// Every currently-connected RELAY's own raw-bytes channel — see
    /// `RawByteBroadcaster`'s doc comment. Populated via
    /// `subscribe_raw_bytes`, mirroring `subscribers`'s existing pattern
    /// for `AlacTermEvent`s (one independent channel per subscriber, not
    /// one shared receiver — same competing-consumers bug that pattern
    /// was already designed to avoid).
    raw_byte_subscribers: Arc<std::sync::Mutex<Vec<async_channel::Sender<Vec<u8>>>>>,
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
        #[cfg(unix)]
        let (program, args) = motd_wrapped_command(program, args);
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

        let raw_byte_subscribers: Arc<std::sync::Mutex<Vec<async_channel::Sender<Vec<u8>>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let event_loop = EventLoop::new(
            term.clone(),
            ServerListener(raw_events_tx),
            pty,
            pty_options.drain_on_exit,
            false,
        )
        .context("failed to create pty event loop")?
        .with_raw_byte_sink(Box::new(RawByteBroadcaster { subscribers: raw_byte_subscribers.clone(), last_write: None }));

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
            raw_byte_subscribers,
        })
    }

    /// Registers a new, independent receiver for this session's raw PTY
    /// bytes — see `RawByteBroadcaster`'s doc comment. Every RELAY calls
    /// this once, right after applying the initial `snapshot()`, to start
    /// receiving live updates from that point on.
    pub fn subscribe_raw_bytes(&self) -> async_channel::Receiver<Vec<u8>> {
        let (tx, rx) = async_channel::unbounded();
        self.raw_byte_subscribers.lock().unwrap().push(tx);
        rx
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

    /// Skips re-applying an identical size — mainly for the resize-poll
    /// thread's 250ms tick (`crate::relay`), which calls this constantly
    /// and would otherwise spam a `TIOCSWINSZ`/`SIGWINCH`-equivalent on
    /// every single tick even when nothing changed. NOT used for a RELAY's
    /// very first `RelayInput::Resize` on a fresh connection — see
    /// `force_resize`'s doc comment for why that path needs the opposite
    /// behavior.
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

    /// Like `resize`, but ALWAYS notifies the shell (`TIOCSWINSZ`, which the
    /// kernel turns into `SIGWINCH` for the PTY's foreground process group)
    /// even when `bounds` is identical to what this session already thinks
    /// its size is. Used specifically for a RELAY's very first
    /// `RelayInput::Resize` on a freshly-established connection
    /// (`server.rs::handle_relay`) — a (re)attaching RELAY's size can
    /// genuinely equal the HOLDER's last-known size (the pane never
    /// actually moved) while a program running INSIDE the shell (`htop`)
    /// still needs a fresh `SIGWINCH` to correctly lay itself out, because
    /// it only reads the terminal size once at its own startup. Confirmed
    /// root cause of a reported bug: `htop` started in a session that was
    /// later detached/reattached (Som restart, tab switch away and back)
    /// stayed laid out for whatever size it happened to start at, since
    /// `resize`'s "skip if unchanged" fast path meant this session's
    /// `SIGWINCH`-equivalent silently never fired again despite
    /// `server.rs`'s own doc comment explicitly saying "every new
    /// connection resizes, not just the session's first ever one" — the
    /// PROTOCOL already did that; `Session::resize` itself was the one
    /// place still skipping it.
    pub fn force_resize(&self, bounds: SessionBounds) {
        let mut last_bounds = self.last_bounds.lock().unwrap();
        *last_bounds = bounds;
        self.term.lock().resize(bounds);
        let window_size: WindowSize = bounds.into();
        self.pty_tx.0.send(Msg::Resize(window_size)).ok();
    }

    /// Serializes the current grid into ANSI bytes via `redrawer`, writing
    /// them to `out` — see `crate::redraw` module doc comment for why this
    /// exists (transparent-PTY-proxy architecture: `som-tmux` must
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

    /// v3 architecture (see `SOM_MUX_PLAN.md`'s "som-tmux v3" section):
    /// captures this session's ENTIRE terminal state — both grid buffers,
    /// cursor, and the full `TermMode` bitflags — bincode-encoded, ready to
    /// send as a single `HolderOutput::Snapshot` message. Replaces
    /// `redraw()`'s role for a freshly-(re)connected RELAY: instead of
    /// replaying ANSI bytes to reproduce a screen (which cannot carry mode
    /// flags like DECCKM, since a mode flip has no visible content of its
    /// own — the actual design flaw v1's `Redrawer`-based approach had),
    /// the RELAY calls `Term::restore()` with this directly, which is
    /// correct-by-construction for every mode `TermMode` tracks.
    pub fn snapshot(&self) -> Vec<u8> {
        let term = self.term.lock();
        let state = term.snapshot();
        bincode::serde::encode_to_vec(&state, bincode::config::standard())
            .expect("TermState bincode encoding should not fail")
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

/// Wraps `program`/`args` in a `sh -c '...; exec "$@"'` shell that prints
/// the system MOTD FIRST, then `exec`s the real shell in its place (`exec`
/// replaces this wrapper process rather than staying around as a parent —
/// same PID-and-signals shape as running `program` directly, just with the
/// MOTD printed first).
///
/// Why this is needed at all: a normal interactive SSH login (`ssh host`,
/// no explicit remote command) gets the MOTD banner for free — sshd itself
/// prints it, via PAM's `pam_motd`, as part of ITS OWN login sequence,
/// before ever handing control to a shell. A `tmux: true` SSH profile's
/// `ssh host ~/.local/bin/som-tmux ...` is an EXPLICIT remote command
/// though, and sshd never runs the login/PAM-motd sequence for those —
/// confirmed by direct comparison against the same host (`ssh host echo
/// hi` -> no MOTD at all; bare `ssh host` -> full MOTD) — so by the time
/// this HOLDER process (`som-tmux`, itself already `exec`'d as that
/// explicit command) gets to spawn the user's real shell, there is no
/// mechanism left that would print it. This reproduces the same content
/// sshd's own `pam_motd` would have shown, the standard Debian/Ubuntu way
/// (`run-parts /etc/update-motd.d/` for the dynamic pieces — kernel
/// version, updates-available notices, etc. — plus the static `/etc/motd`
/// file) — gracefully doing nothing if neither exists (e.g. real macOS
/// hosts, confirmed to have neither by default).
#[cfg(unix)]
fn motd_wrapped_command(program: String, args: Vec<String>) -> (String, Vec<String>) {
    let script = "if [ -d /etc/update-motd.d ]; then run-parts /etc/update-motd.d 2>/dev/null; fi; \
                   if [ -f /etc/motd ]; then cat /etc/motd 2>/dev/null; fi; \
                   if command -v last >/dev/null 2>&1; then \
                     last -F \"$(whoami)\" 2>/dev/null | head -1 | awk '{if (NF >= 8) printf \"Last login: %s %s %2d %s %s from %s\\n\", $4, $5, $6, $7, $8, $3}'; \
                   fi; \
                   exec \"$0\" \"$@\"";
    let mut wrapped_args = vec!["-c".to_string(), script.to_string(), program.clone()];
    wrapped_args.extend(args);
    ("sh".to_string(), wrapped_args)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Regression test for a real user-reported bug (all `tmux: true` SSH
    /// profiles showed an extra blank line between every shell prompt):
    /// confirmed via direct byte-for-byte instrumentation against a real
    /// remote host that `EventLoop::with_raw_byte_sink`'s sink (this
    /// `RawByteBroadcaster`) sometimes receives the exact same chunk (or a
    /// chunk that starts by repeating the previous one) twice in immediate
    /// succession — a duplication bug somewhere in the Windows ConPTY
    /// reader path this sits downstream of, not fully root-caused upstream
    /// (a `Pty::reregister` fix in the `errordnk/alacritty` fork looked
    /// like a strong candidate but did not eliminate it in direct
    /// reproduction). This drives the dedup logic directly, without a real
    /// PTY, to lock in both shapes the real bug actually took.
    fn collect_broadcast(chunks: &[&[u8]], delay_between: std::time::Duration) -> Vec<u8> {
        let (tx, rx) = async_channel::unbounded();
        let mut broadcaster =
            RawByteBroadcaster { subscribers: Arc::new(std::sync::Mutex::new(vec![tx])), last_write: None };
        for chunk in chunks {
            broadcaster.write_all(chunk).expect("write_all should succeed");
            std::thread::sleep(delay_between);
        }
        let mut collected = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            collected.extend(chunk);
        }
        collected
    }

    #[test]
    fn drops_a_whole_chunk_that_exactly_repeats_the_previous_one_within_the_dedup_window() {
        let collected = collect_broadcast(&[b"\r\n", b"\r\n"], std::time::Duration::from_millis(1));
        assert_eq!(
            collected, b"\r\n",
            "the second, exactly-repeated chunk arriving milliseconds later should have been dropped as a \
             duplicate — this is exactly the reported doubled-CRLF-between-prompts bug"
        );
    }

    #[test]
    fn strips_a_duplicated_prefix_when_the_next_chunk_starts_by_repeating_the_last_one() {
        // Confirmed via real reproduction: the duplication doesn't always
        // land as its own separate, exactly-matching `write()` call — the
        // SECOND copy sometimes arrives fused onto the FRONT of the next
        // genuinely-new chunk instead (e.g. last write was `\r\n`, this
        // write is `\r\n` + the next real content stuck together).
        let collected = collect_broadcast(&[b"\r\n", b"\r\nhello"], std::time::Duration::from_millis(1));
        assert_eq!(
            collected, b"\r\nhello",
            "the duplicated `\\r\\n` prefix on the second chunk should have been stripped, keeping only the \
             genuinely new `hello` that followed it"
        );
    }

    #[test]
    fn does_not_drop_a_repeat_of_the_same_bytes_outside_the_dedup_window() {
        // A real shell CAN legitimately produce the same short chunk twice
        // in a row for unrelated reasons (e.g. two blank lines seconds
        // apart) — the dedup must not eat that. Confirmed via direct
        // instrumentation that the actual bug's duplicate always lands
        // within single-digit milliseconds, never anywhere close to this
        // window.
        let collected = collect_broadcast(&[b"\r\n", b"\r\n"], std::time::Duration::from_millis(100));
        assert_eq!(
            collected, b"\r\n\r\n",
            "two genuinely separate occurrences of the same short chunk, well outside the dedup window, must \
             both be forwarded — this must not become a general \"collapse repeated output\" filter"
        );
    }

    #[test]
    fn does_not_touch_unrelated_consecutive_chunks() {
        let collected = collect_broadcast(&[b"hello", b" world"], std::time::Duration::from_millis(1));
        assert_eq!(collected, b"hello world", "chunks that don't repeat each other at all must pass through untouched");
    }

    /// Regression test for a real user-reported gap: `tmux: true` SSH
    /// profiles never showed the system MOTD banner a plain (non-tmux)
    /// SSH profile gets for free, because sshd only runs its own
    /// `pam_motd` login sequence for a bare `ssh host` (no explicit
    /// remote command) — `ssh host ~/.local/bin/som-tmux ...` IS an
    /// explicit command, so sshd never prints it no matter what
    /// `som-tmux` does after the fact. `motd_wrapped_command` reproduces
    /// the same content sshd's own `pam_motd` would have (`run-parts
    /// /etc/update-motd.d` + `/etc/motd` + a "Last login: ..." line built
    /// from `last -F`, since `lastlog` itself isn't installed on every
    /// real host this runs against) by running it itself, then `exec`ing
    /// the real shell in its own place — doubles as a visible "this
    /// HOLDER was just created, not reattached to an existing one"
    /// signal, since it only ever runs once per HOLDER lifetime.
    #[cfg(unix)]
    #[test]
    fn motd_wrapped_command_execs_the_real_program_after_printing_motd() {
        let (program, args) = motd_wrapped_command("/bin/bash".to_string(), vec!["-l".to_string()]);
        assert_eq!(program, "sh");
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("run-parts /etc/update-motd.d"), "should attempt the dynamic MOTD pieces");
        assert!(args[1].contains("/etc/motd"), "should attempt the static MOTD file");
        assert!(args[1].contains("last -F"), "should attempt the Last login line");
        assert!(args[1].contains("Last login:"), "should format a Last login line matching sshd's own wording");
        assert!(args[1].trim_end().ends_with("exec \"$0\" \"$@\""), "must end by exec'ing the real program in its own place");
        assert_eq!(&args[2..], &["/bin/bash", "-l"], "the real program/args must follow as $0/$@ for the exec");
    }
}
