//! Bridges a blocking `som-tmux-server` pipe connection (see
//! `som_tmux_client`) into GPUI: a background OS thread reads incoming
//! `ServerMessage`s and forwards them into GPUI's async world, where they're
//! applied to the owning `SomTmuxView`; writes (keystrokes, resize) go out
//! over the same shared connection rather than opening a new one per call.
//!
//! One `SomTmuxSession` = one pipe connection = one session (pane). Several
//! panes of the same profile end up as several independent connections to
//! the same server process (the protocol supports multiplexing many
//! sessions over a single connection, but keeping it one-connection-per-pane
//! here is simpler and avoids one pane's slow reader stalling another's
//! updates).

use crate::som_tmux_client;
use crate::som_tmux_view::SomTmuxView;
use anyhow::{Context as _, Result, bail};
use gpui::{AppContext as _, AsyncApp, Task, WeakEntity};
use som_tmux_server::pipe::PipeConnection;
use som_tmux_server::protocol::{ClientMessage, ServerMessage};
use std::sync::Arc;
use uuid::Uuid;

/// A live connection to a session's `som-tmux-server`, shared between the
/// background read loop and anything that wants to send a `ClientMessage`
/// (keystrokes, resize, explicit close) without opening a redundant second
/// connection — `PipeConnection` is `Send + Sync` (each read/write uses its
/// own `OVERLAPPED`, see `som_tmux_server::pipe`), so this is safe to hand
/// out from multiple call sites.
#[derive(Clone)]
pub struct SomTmuxSession {
    connection: Arc<PipeConnection>,
    pub session_id: Uuid,
}

impl SomTmuxSession {
    pub fn send(&self, message: ClientMessage) -> Result<()> {
        send(&self.connection, &message)
    }

    pub fn write(&self, bytes: Vec<u8>) -> Result<()> {
        self.send(ClientMessage::Write { session_id: self.session_id, bytes })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.send(ClientMessage::Resize { session_id: self.session_id, cols, rows })
    }

    /// Explicit "kill this pane's process for real" — as opposed to just
    /// dropping the connection, which leaves the session alive for a later
    /// `Attach` (see the detach-vs-kill semantics in `project_som_tmux`
    /// memory / `SOM_MUX_PLAN.md`).
    pub fn close(&self) -> Result<()> {
        self.send(ClientMessage::CloseSession { session_id: self.session_id })
    }
}

/// Connects (spawning the server if needed), sends `NewSession` for
/// `program`/`args`/`cwd`, then immediately follows up with `Attach` on the
/// resulting session id. The `Attach` isn't optional here even though the
/// session is brand new: the server only starts the background thread that
/// forwards `GridUpdate`s (see `som_tmux_server::server`'s handling of
/// `ClientMessage::Attach`) once a client attaches — `NewSession` alone
/// leaves the connection completely silent afterwards, no matter how much
/// the shell inside it prints. (Found by testing: a freshly created tab
/// stayed blank forever — the PTY was alive and running, the server just
/// had nothing forwarding its output anywhere.) Purely the blocking network
/// part — runs on `cx.background_spawn`, which requires the future to be
/// `Send`; that's why this doesn't take a `view`/`AsyncApp` at all (both
/// hold `Rc`-based state that isn't `Send` and can't cross into a
/// background task). Callers must follow up with `start_read_loop` once
/// this resolves, from a context where `AsyncApp` is available (e.g. inside
/// a `cx.spawn` on the entity creating the pane), to actually start
/// forwarding `GridUpdate`s into a view.
pub fn create_session(
    profile_name: String,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    cx: &AsyncApp,
) -> Task<Result<(SomTmuxSession, String)>> {
    cx.background_spawn(async move {
        let connection = som_tmux_client::connect_or_spawn(&profile_name)
            .context("failed to connect to som-tmux-server")?;
        send(&connection, &ClientMessage::NewSession { program, args, cwd, cols, rows })?;
        let reply = recv(&connection)?;
        let session_id = match reply {
            ServerMessage::SessionCreated { session_id } => session_id,
            ServerMessage::Error { message } => bail!("som-tmux-server: {message}"),
            other => bail!("unexpected reply to NewSession: {other:?}"),
        };

        send(&connection, &ClientMessage::Attach { session_id, cols, rows })?;
        let reply = recv(&connection)?;
        let grid_text = match reply {
            ServerMessage::GridUpdate { grid_text, .. } => grid_text,
            ServerMessage::AttachFailed { reason, .. } => bail!("attach failed right after creating the session: {reason}"),
            other => bail!("unexpected reply to Attach: {other:?}"),
        };

        Ok((SomTmuxSession { connection: Arc::new(connection), session_id }, grid_text))
    })
}

/// Attaches to an already-known session id (restore path) instead of
/// creating a new one. Same `Send`-boundary shape as `create_session` — see
/// its doc comment; call `start_read_loop` separately once this resolves.
pub fn attach_session(
    profile_name: String,
    session_id: Uuid,
    cols: u16,
    rows: u16,
    cx: &AsyncApp,
) -> Task<Result<(SomTmuxSession, String)>> {
    cx.background_spawn(async move {
        let connection = som_tmux_client::connect_or_spawn(&profile_name)
            .context("failed to connect to som-tmux-server")?;
        send(&connection, &ClientMessage::Attach { session_id, cols, rows })?;
        let reply = recv(&connection)?;
        let grid_text = match reply {
            ServerMessage::GridUpdate { grid_text, .. } => grid_text,
            ServerMessage::AttachFailed { reason, .. } => bail!("attach failed: {reason}"),
            other => bail!("unexpected reply to Attach: {other:?}"),
        };
        Ok((SomTmuxSession { connection: Arc::new(connection), session_id }, grid_text))
    })
}

/// Starts forwarding `GridUpdate`s for `session.session_id` into `view`.
/// Must be called from a context where `AsyncApp` is directly available
/// (not from inside a `background_spawn`'d future) — see `create_session`'s
/// doc comment for why the two are split.
pub fn start_read_loop(session: &SomTmuxSession, view: WeakEntity<SomTmuxView>, cx: &mut AsyncApp) {
    spawn_read_loop(session.connection.clone(), session.session_id, view, cx.clone());
}

/// Runs for as long as the connection is alive, on its own OS thread (the
/// pipe API is blocking — see `som_tmux_server::pipe`). Forwards each
/// `GridUpdate` for `session_id` into `view` — but NOT by calling
/// `view.update()` directly from this thread: `AsyncApp`/`WeakEntity` hold
/// `Rc`-based state that isn't `Send`, so they can't cross into a plain OS
/// thread at all (this was a real compile error, not a style choice). The
/// blocking reader instead pushes each grid snapshot through a thread-safe
/// channel; a separate task spawned on GPUI's own executor (via `cx.spawn`,
/// which — unlike `std::thread::spawn` — runs somewhere `AsyncApp` is valid)
/// drains that channel and does the actual `view.update()` calls. Exits
/// quietly when the connection closes (server gone, or an explicit
/// `close()` tore it down) or `view` itself has been dropped (tab closed) —
/// neither is an error worth logging loudly, both are expected end states.
fn spawn_read_loop(
    connection: Arc<PipeConnection>,
    session_id: Uuid,
    view: WeakEntity<SomTmuxView>,
    cx: AsyncApp,
) {
    let (tx, rx) = async_channel::unbounded::<String>();

    std::thread::spawn(move || {
        loop {
            let message = match recv(&connection) {
                Ok(message) => message,
                Err(_) => return, // connection closed
            };
            match message {
                ServerMessage::GridUpdate { session_id: id, grid_text } if id == session_id => {
                    if tx.send_blocking(grid_text).is_err() {
                        return; // GPUI-side task has stopped listening
                    }
                }
                ServerMessage::SessionClosed { session_id: id } if id == session_id => return,
                _ => {}
            }
        }
    });

    cx.spawn(async move |cx| {
        while let Ok(grid_text) = rx.recv().await {
            let updated = view.update(cx, |view, cx| {
                view.apply_grid_update(grid_text, cx);
            });
            if updated.is_err() {
                return; // view entity has been dropped
            }
        }
    })
    .detach();
}

fn send(connection: &PipeConnection, message: &ClientMessage) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    connection.write_message(&payload)
}

fn recv(connection: &PipeConnection) -> Result<ServerMessage> {
    let bytes = connection.read_message()?;
    Ok(serde_json::from_slice(&bytes)?)
}
