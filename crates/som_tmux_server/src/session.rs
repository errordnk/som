use crate::bounds::SessionBounds;
use som_tmux_server::protocol::{CursorPosition, GridSnapshot, ProtocolCell};
use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config;
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::CursorShape;
use anyhow::{Context as _, Result};
use std::sync::Arc;
use uuid::Uuid;

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
    last_bounds: SessionBounds,
    /// PID of the spawned shell/program, captured before the `Pty` handle is
    /// moved into `EventLoop` (which owns it from then on and doesn't expose
    /// it back out) — this is the only way to actually kill the process on
    /// `CloseSession`, since alacritty's `EventLoop`/`Notifier` only exposes
    /// writing bytes and resizing, not process control.
    pid: Option<sysinfo::Pid>,
    /// `Wakeup` notifications only (see `spawn`'s internal pump thread doc
    /// comment for why `PtyWrite`/other events never reach here). Consumed
    /// by `server.rs`'s per-connection grid-update forwarder, which turns
    /// each `Wakeup` into a `GridUpdate` sent to the attached client. Cloned
    /// (not moved) when a new client attaches, since more than one client
    /// could plausibly attach to the same session (e.g. briefly during a
    /// tab-restore race) — `async_channel` supports multiple consumers on
    /// the same channel natively.
    pub events_rx: async_channel::Receiver<AlacTermEvent>,
}

impl Session {
    pub fn spawn(
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        bounds: SessionBounds,
    ) -> Result<Self> {
        let shell = tty::Shell::new(program, args);
        let pty_options = tty::Options {
            shell: Some(shell),
            working_directory: cwd.map(std::path::PathBuf::from),
            drain_on_exit: true,
            env: Default::default(),
            #[cfg(not(windows))]
            child_signal_mask: None,
            #[cfg(windows)]
            escape_args: false,
        };

        let pty = tty::new(&pty_options, bounds.into(), 0).context("failed to spawn pty")?;
        let pid = process_pid(&pty);

        let (raw_events_tx, raw_events_rx) = async_channel::unbounded();
        let term = Term::new(Config::default(), &bounds, ServerListener(raw_events_tx.clone()));
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
        let pump_pty_tx = Notifier(pty_tx.0.clone());
        let (wakeup_tx, wakeup_rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            while let Ok(event) = raw_events_rx.recv_blocking() {
                match event {
                    AlacTermEvent::PtyWrite(bytes) => pump_pty_tx.notify(bytes.into_bytes()),
                    AlacTermEvent::Wakeup => {
                        wakeup_tx.try_send(AlacTermEvent::Wakeup).ok();
                    }
                    AlacTermEvent::Exit => {
                        wakeup_tx.try_send(AlacTermEvent::Exit).ok();
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
            last_bounds: bounds,
            pid,
            events_rx: wakeup_rx,
        })
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

    pub fn resize(&mut self, bounds: SessionBounds) {
        if bounds == self.last_bounds {
            return;
        }
        self.last_bounds = bounds;
        self.term.lock().resize(bounds);
        let window_size: WindowSize = bounds.into();
        self.pty_tx.0.send(Msg::Resize(window_size)).ok();
    }

    /// Plain-text snapshot of the current grid — see
    /// See `protocol::ServerMessage::GridUpdate` doc comment for why this is
    /// a full re-snapshot on every change rather than a byte stream.
    /// `alacritty_terminal::term::Cell`'s `fg`/`bg`/`flags` are carried
    /// straight into `protocol::ProtocolCell` (see that type's doc comment
    /// for why it's a protocol-specific type rather than sending `Cell`
    /// itself), and the cursor position/visibility comes from
    /// `RenderableContent::cursor` (hidden cursor - e.g. many full-screen
    /// TUI apps hide it between redraws - becomes `None`, not some sentinel
    /// position).
    pub fn snapshot(&self) -> GridSnapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cursor = (content.cursor.shape != CursorShape::Hidden).then(|| CursorPosition {
            line: content.cursor.point.line.0,
            column: content.cursor.point.column.0,
        });
        let mut rows: Vec<Vec<ProtocolCell>> = Vec::new();
        let mut cell_count = 0;
        for indexed in content.display_iter {
            cell_count += 1;
            let line_ix = indexed.point.line.0.max(0) as usize;
            while rows.len() <= line_ix {
                rows.push(Vec::new());
            }
            let cell = &indexed.cell;
            rows[line_ix].push(ProtocolCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags });
        }
        log::debug!("snapshot: {cell_count} cells visited, {} rows", rows.len());
        GridSnapshot { rows, cursor }
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

    /// Waits for the next `Wakeup` from the session's internal pump thread
    /// (i.e. the grid actually changed) — `events_rx` only ever carries
    /// `Wakeup`/`Exit` (everything else, notably `PtyWrite`, is handled
    /// internally by the pump thread started in `spawn`, never forwarded
    /// here). Returns `false` on `Exit`/channel closed.
    pub async fn next_change(&self) -> bool {
        loop {
            match self.events_rx.recv().await {
                Ok(AlacTermEvent::Wakeup) => return true,
                Ok(AlacTermEvent::Exit) => return false,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Blocking equivalent of `next_change`, for `server.rs`'s
    /// thread-per-connection grid-update forwarder threads (which are plain
    /// OS threads, not running on an async executor).
    pub fn next_change_blocking(&self) -> bool {
        loop {
            match self.events_rx.recv_blocking() {
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

#[cfg(test)]
mod tests {
    //! Verifies `Session::snapshot()` actually carries real color/attribute
    //! data end-to-end (spawn a real PTY, print real ANSI-colored text,
    //! check the resulting `ProtocolCell`s) — not just that the protocol
    //! types compile, which a type-level check alone wouldn't have caught if
    //! e.g. `Cell::fg`/`bg` weren't being read correctly out of alacritty's
    //! `RenderableContent`. Spawns a real `cmd.exe`, no mocking — matches the
    //! rest of this crate's testing style (see `terminal_view`'s
    //! `reconnect_tests`/`spawn_race_tests` for the same philosophy on the
    //! client side).

    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    #[test]
    fn snapshot_carries_real_color_and_text() {
        let bounds = SessionBounds::new(80, 24);
        // `\x1b` is the real ESC byte (0x1b) — a literal `[92m` without it
        // is just plain text to the terminal, not a color escape sequence.
        let command = "echo off & echo \x1b[92mgreentext\x1b[0m & timeout /T 3600";
        let session = Session::spawn(
            "C:\\Windows\\System32\\cmd.exe".into(),
            vec!["/C".into(), command.into()],
            None,
            bounds,
        )
        .expect("failed to spawn cmd.exe for test");

        // Wait for the colored text to actually show up in the grid rather
        // than relying on a fixed sleep — poll snapshot() a bounded number
        // of times, waiting for the next Wakeup between attempts.
        let mut found_row = None;
        for _ in 0..50 {
            let snapshot = session.snapshot();
            if snapshot.rows.iter().any(|row| row.iter().any(|c| c.c == 'g' && c.fg != Color::Named(NamedColor::Foreground))) {
                found_row = Some(snapshot);
                break;
            }
            if !session.next_change_blocking() {
                break;
            }
        }

        let snapshot = found_row.expect("'greentext' with a non-default fg color never appeared in the grid");
        let text: String = snapshot.rows.iter().flat_map(|row| row.iter().map(|c| c.c)).collect();
        assert!(text.contains("greentext"), "grid text should contain the echoed string, got: {text:?}");

        let colored_cell = snapshot
            .rows
            .iter()
            .flat_map(|row| row.iter())
            .find(|c| c.c == 'g' && c.fg != Color::Named(NamedColor::Foreground))
            .expect("at least one 'g' cell should have a non-default foreground color");
        assert_eq!(
            colored_cell.fg,
            Color::Named(NamedColor::BrightGreen),
            "cmd.exe's `[92m` (bright green) should map to NamedColor::BrightGreen"
        );

        session.kill();
    }
}
