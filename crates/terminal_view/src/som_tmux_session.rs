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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// What a fresh session should be spawned with if this pane's connection
/// dies and reattaching to the same `session_id` doesn't work (server
/// genuinely gone, not just a network blip) — see `reconnect_blocking`.
/// Cloned into every `SomTmuxSession` so the health-check reconnect path
/// (run from a plain OS thread, see `spawn_read_loop`) has everything it
/// needs without reaching back into GPUI state.
#[derive(Clone)]
struct SpawnSpec {
    profile_name: String,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
}

/// A live connection to a session's `som-tmux-server`, shared between the
/// background read loop and anything that wants to send a `ClientMessage`
/// (keystrokes, resize, explicit close) without opening a redundant second
/// connection — `PipeConnection` is `Send + Sync` (each read/write uses its
/// own `OVERLAPPED`, see `som_tmux_server::pipe`), so this is safe to hand
/// out from multiple call sites.
///
/// `connection` and `session_id` are behind a `Mutex` (not just handed out
/// once) because the health-check reconnect path (see `spawn_read_loop`'s
/// disconnect handling) can *replace* both: a dropped connection first tries
/// `Attach` on the same `session_id` (the server may just be a moment slow
/// to respond, or Som's own pipe hiccuped — the session itself is still
/// alive), and if that fails, falls back to spawning a brand new session
/// (server process genuinely gone) with a new `session_id`. Every clone of
/// this `SomTmuxSession` (the view holds one, `spawn_read_loop`'s reader
/// thread holds another) needs to see the swap, which is why the mutable
/// state is behind `Arc<Mutex<_>>` rather than being plain fields on a
/// `#[derive(Clone)]` struct (that would give each clone its own copy,
/// silently diverging the moment one side reconnects).
#[derive(Clone)]
pub struct SomTmuxSession {
    connection: Arc<Mutex<Arc<PipeConnection>>>,
    session_id: Arc<Mutex<Uuid>>,
    spawn_spec: SpawnSpec,
    cols: Arc<Mutex<(u16, u16)>>,
}

impl SomTmuxSession {
    pub fn session_id(&self) -> Uuid {
        *self.session_id.lock().unwrap()
    }

    fn connection(&self) -> Arc<PipeConnection> {
        self.connection.lock().unwrap().clone()
    }

    pub fn send(&self, message: ClientMessage) -> Result<()> {
        send(&self.connection(), &message)
    }

    pub fn write(&self, bytes: Vec<u8>) -> Result<()> {
        self.send(ClientMessage::Write { session_id: self.session_id(), bytes })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        *self.cols.lock().unwrap() = (cols, rows);
        self.send(ClientMessage::Resize { session_id: self.session_id(), cols, rows })
    }

    /// Explicit "kill this pane's process for real" — as opposed to just
    /// dropping the connection, which leaves the session alive for a later
    /// `Attach` (see the detach-vs-kill semantics in `project_som_tmux`
    /// memory / `SOM_MUX_PLAN.md`).
    pub fn close(&self) -> Result<()> {
        self.send(ClientMessage::CloseSession { session_id: self.session_id() })
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
        let spawn_spec = SpawnSpec { profile_name: profile_name.clone(), program: program.clone(), args: args.clone(), cwd: cwd.clone() };
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

        let session = SomTmuxSession {
            connection: Arc::new(Mutex::new(Arc::new(connection))),
            session_id: Arc::new(Mutex::new(session_id)),
            spawn_spec,
            cols: Arc::new(Mutex::new((cols, rows))),
        };
        Ok((session, grid_text))
    })
}

/// Attaches to an already-known session id (restore path) instead of
/// creating a new one. Same `Send`-boundary shape as `create_session` — see
/// its doc comment; call `start_read_loop` separately once this resolves.
/// `program`/`args`/`cwd` are only needed for the health-check reconnect
/// path's eventual fallback (see `reconnect_blocking`) if the server this
/// attaches to later dies for real — they're never sent as part of this
/// initial `Attach`.
pub fn attach_session(
    profile_name: String,
    session_id: Uuid,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    cx: &AsyncApp,
) -> Task<Result<(SomTmuxSession, String)>> {
    cx.background_spawn(async move {
        let spawn_spec = SpawnSpec { profile_name: profile_name.clone(), program, args, cwd };
        let connection = som_tmux_client::connect_or_spawn(&profile_name)
            .context("failed to connect to som-tmux-server")?;
        send(&connection, &ClientMessage::Attach { session_id, cols, rows })?;
        let reply = recv(&connection)?;
        let grid_text = match reply {
            ServerMessage::GridUpdate { grid_text, .. } => grid_text,
            ServerMessage::AttachFailed { reason, .. } => bail!("attach failed: {reason}"),
            other => bail!("unexpected reply to Attach: {other:?}"),
        };
        let session = SomTmuxSession {
            connection: Arc::new(Mutex::new(Arc::new(connection))),
            session_id: Arc::new(Mutex::new(session_id)),
            spawn_spec,
            cols: Arc::new(Mutex::new((cols, rows))),
        };
        Ok((session, grid_text))
    })
}

/// Health-check reconnect, run synchronously from the blocking reader thread
/// (`spawn_read_loop`) once its connection dies — see that function's doc
/// comment for when this fires. Blocking, not `Send`-constrained by GPUI (no
/// `AsyncApp`/`Entity` involved at all here, this is plain networking code).
///
/// Model (see `project_som_tmux` memory's health-check section for the
/// full rationale): try `Attach` on the same `session_id`; if that succeeds,
/// the session survived (the common, cheap case — the pipe hiccuped but the
/// server and its session are still alive). If it doesn't, fall back to
/// spawning a brand new session with the original `program`/`args`/`cwd`
/// (same fallback already used when a saved session from db.json can't be
/// restored). Returns the new grid text alongside whichever session id ended
/// up live, so the caller can tell which case happened (same id =
/// reattached, different id = fresh session) and update the view/registry
/// accordingly.
///
/// Retries with backoff only apply to *connecting* (`connect_or_spawn`
/// itself failing, or the connection dying mid-handshake) — a genuine
/// network/pipe blip that's worth waiting out. An `AttachFailed` reply on a
/// connection that DID succeed is a definitive, protocol-level "no such
/// session" — retrying the exact same `Attach` on a fresh connection can't
/// change that answer, so it goes straight to the fallback instead of
/// burning the retry budget. (This distinction matters in practice: earlier
/// version of this function retried the whole connect+Attach cycle on ANY
/// failure including `AttachFailed`, and each retry's dropped connection
/// made a server with zero real sessions exit immediately — see
/// `som_tmux_server::server`'s "0 sessions → exit" — turning every retry
/// into a respawn race instead of actually waiting for anything.)
fn reconnect_blocking(spawn_spec: &SpawnSpec, session_id: Uuid, cols: u16, rows: u16) -> Result<(PipeConnection, Uuid, String)> {
    const CONNECT_RETRY_ATTEMPTS: u32 = 5;
    const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

    let mut last_err = None;
    for attempt in 0..CONNECT_RETRY_ATTEMPTS {
        match som_tmux_client::connect_or_spawn(&spawn_spec.profile_name) {
            Ok(connection) => {
                match try_attach(&connection, session_id, cols, rows) {
                    Ok(grid_text) => return Ok((connection, session_id, grid_text)),
                    // Definitive "no such session" on a connection that DID
                    // work — don't retry, go straight to the fallback below.
                    Err(AttachOutcome::AttachFailed) => break,
                    Err(AttachOutcome::TransportError(err)) => last_err = Some(err),
                }
            }
            Err(err) => last_err = Some(err),
        }
        if attempt + 1 < CONNECT_RETRY_ATTEMPTS {
            std::thread::sleep(CONNECT_RETRY_DELAY);
        }
    }
    if let Some(err) = &last_err {
        log::info!("som-tmux reconnect: giving up reattaching to {session_id} ({err:#}), falling back to a new session");
    }

    // Server genuinely gone (or never came back within the retry budget) —
    // fall back to a brand new session, same as the db.json restore path.
    //
    // Retries the whole connect+NewSession+Attach handshake a few times
    // rather than doing it once: `connect_or_spawn`'s first `PipeConnection::
    // connect` can transiently succeed against a named pipe instance whose
    // owning process is mid-exit (e.g. the very server this function just
    // gave up reattaching to, if its "0 sessions -> exit" hasn't fully torn
    // the pipe down yet on Windows's side) — that "successful" connect is
    // actually to a dead end, and the subsequent send/recv fails. Retrying
    // the full handshake lets `connect_or_spawn` fall through to actually
    // spawning a fresh server once the stale pipe instance is gone. Found by
    // testing: attaching to a session id no server had ever created failed
    // reliably on the very first fallback attempt for exactly this reason.
    const FALLBACK_RETRY_ATTEMPTS: u32 = 5;
    const FALLBACK_RETRY_DELAY: Duration = Duration::from_millis(300);

    let mut last_err = None;
    for attempt in 0..FALLBACK_RETRY_ATTEMPTS {
        let attempt_result = (|| -> Result<(PipeConnection, Uuid, String)> {
            let connection = som_tmux_client::connect_or_spawn(&spawn_spec.profile_name)?;
            send(
                &connection,
                &ClientMessage::NewSession {
                    program: spawn_spec.program.clone(),
                    args: spawn_spec.args.clone(),
                    cwd: spawn_spec.cwd.clone(),
                    cols,
                    rows,
                },
            )?;
            let new_session_id = match recv(&connection)? {
                ServerMessage::SessionCreated { session_id } => session_id,
                ServerMessage::Error { message } => bail!("som-tmux-server: {message}"),
                other => bail!("unexpected reply to NewSession: {other:?}"),
            };
            send(&connection, &ClientMessage::Attach { session_id: new_session_id, cols, rows })?;
            let grid_text = match recv(&connection)? {
                ServerMessage::GridUpdate { grid_text, .. } => grid_text,
                ServerMessage::AttachFailed { reason, .. } => bail!("attach failed right after creating fallback session: {reason}"),
                other => bail!("unexpected reply to Attach: {other:?}"),
            };
            Ok((connection, new_session_id, grid_text))
        })();
        match attempt_result {
            Ok(result) => return Ok(result),
            Err(err) if attempt + 1 < FALLBACK_RETRY_ATTEMPTS => {
                last_err = Some(err);
                std::thread::sleep(FALLBACK_RETRY_DELAY);
            }
            Err(err) => return Err(err.context("fallback session creation failed after retries")),
        }
    }
    // Unreachable given the loop above always returns, but keeps the
    // compiler happy without an early bail on `last_err` being `None`.
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("fallback session creation failed")))
}

/// Distinguishes "the session doesn't exist" (definitive, don't retry) from
/// "couldn't even complete the exchange" (transient, worth retrying) — see
/// `reconnect_blocking`'s doc comment for why the distinction matters.
enum AttachOutcome {
    AttachFailed,
    TransportError(anyhow::Error),
}

fn try_attach(connection: &PipeConnection, session_id: Uuid, cols: u16, rows: u16) -> std::result::Result<String, AttachOutcome> {
    send(connection, &ClientMessage::Attach { session_id, cols, rows }).map_err(AttachOutcome::TransportError)?;
    match recv(connection).map_err(AttachOutcome::TransportError)? {
        ServerMessage::GridUpdate { grid_text, .. } => Ok(grid_text),
        ServerMessage::AttachFailed { .. } => Err(AttachOutcome::AttachFailed),
        other => Err(AttachOutcome::TransportError(anyhow::anyhow!("unexpected reply to Attach: {other:?}"))),
    }
}

/// What the blocking reader thread (`spawn_read_loop`) reports back to the
/// GPUI-side task, which applies it to the view — see that function's doc
/// comment for the full flow.
enum ReadLoopEvent {
    GridUpdate(String),
    /// The connection died and a health-check reconnect attempt is
    /// underway — lets the view show a "Reconnecting…" indicator instead of
    /// silently freezing on stale content.
    Reconnecting,
    /// Reconnect finished. `session_id`/`grid_text` reflect whichever
    /// happened: reattached to the same session (id unchanged) or fell back
    /// to a brand new one (id changed) — see `reconnect_blocking`.
    Reconnected { session_id: Uuid, grid_text: String },
}

/// Starts forwarding `GridUpdate`s for `session`'s live session id into
/// `view`. Must be called from a context where `AsyncApp` is directly
/// available (not from inside a `background_spawn`'d future) — see
/// `create_session`'s doc comment for why the two are split.
pub fn start_read_loop(session: &SomTmuxSession, view: WeakEntity<SomTmuxView>, cx: &mut AsyncApp) {
    spawn_read_loop(session.clone(), view, cx.clone());
}

/// Runs for as long as `session` exists, on its own OS thread (the pipe API
/// is blocking — see `som_tmux_server::pipe`). Forwards each `GridUpdate`
/// for the current session id into `view` — but NOT by calling
/// `view.update()` directly from this thread: `AsyncApp`/`WeakEntity` hold
/// `Rc`-based state that isn't `Send`, so they can't cross into a plain OS
/// thread at all (this was a real compile error, not a style choice). The
/// blocking reader instead pushes events through a thread-safe channel; a
/// separate task spawned on GPUI's own executor (via `cx.spawn`, which —
/// unlike `std::thread::spawn` — runs somewhere `AsyncApp` is valid) drains
/// that channel and does the actual `view.update()` calls.
///
/// Health-check: when `recv` errors (pipe broken — server gone, network
/// blip, or Som's own connection object torn down), this does NOT exit
/// immediately the way it used to. It reports `Reconnecting`, then calls
/// `reconnect_blocking` (attach-retry, falling back to a fresh session — see
/// its doc comment). Once that resolves — successfully or not — the loop
/// re-reads from whatever connection is now current on `session` (swapped in
/// via `session.connection`/`session.session_id`'s mutexes) and continues.
/// The only ways this thread actually exits for good: an explicit
/// `SessionClosed` for the current session id (tab closed via `on_removed`),
/// or `reconnect_blocking` itself failing outright (couldn't even spawn a
/// fallback session — e.g. the profile's `som-tmux-server.exe` binary is
/// missing) — that last case is rare enough it's logged rather than retried
/// forever.
fn spawn_read_loop(session: SomTmuxSession, view: WeakEntity<SomTmuxView>, cx: AsyncApp) {
    let (tx, rx) = async_channel::unbounded::<ReadLoopEvent>();

    std::thread::spawn(move || {
        loop {
            let connection = session.connection();
            let session_id = session.session_id();
            let message = recv(&connection);
            match message {
                Ok(ServerMessage::GridUpdate { session_id: id, grid_text }) if id == session_id => {
                    if tx.send_blocking(ReadLoopEvent::GridUpdate(grid_text)).is_err() {
                        return; // GPUI-side task has stopped listening
                    }
                }
                Ok(ServerMessage::SessionClosed { session_id: id }) if id == session_id => return,
                Ok(_) => continue, // stale message for a since-replaced session id, or unrelated
                Err(_) => {
                    if tx.send_blocking(ReadLoopEvent::Reconnecting).is_err() {
                        return;
                    }
                    let (cols, rows) = *session.cols.lock().unwrap();
                    match reconnect_blocking(&session.spawn_spec, session_id, cols, rows) {
                        Ok((new_connection, new_session_id, grid_text)) => {
                            *session.connection.lock().unwrap() = Arc::new(new_connection);
                            *session.session_id.lock().unwrap() = new_session_id;
                            if tx
                                .send_blocking(ReadLoopEvent::Reconnected { session_id: new_session_id, grid_text })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(err) => {
                            log::error!("som-tmux reconnect failed permanently: {err:#}");
                            return;
                        }
                    }
                }
            }
        }
    });

    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            let updated = view.update(cx, |view, cx| match event {
                ReadLoopEvent::GridUpdate(grid_text) => view.apply_grid_update(grid_text, cx),
                ReadLoopEvent::Reconnecting => view.set_reconnecting(true, cx),
                ReadLoopEvent::Reconnected { session_id, grid_text } => {
                    view.apply_reconnect(session_id, grid_text, cx)
                }
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

#[cfg(test)]
mod reconnect_tests {
    //! Exercises `reconnect_blocking` against a *real* `som-tmux-server`
    //! process (spawned via `connect_or_spawn`, same as production) rather
    //! than mocking the protocol — the whole point of the health-check is
    //! how it behaves against actual process/pipe death, which a mock
    //! wouldn't exercise honestly. Deliberately does NOT touch Som's UI or
    //! send any keystrokes (see `feedback_autoit_focus_check` memory: that
    //! class of test has repeatedly landed in the wrong window and reset an
    //! unrelated editor session) — everything here is protocol-level only.
    //!
    //! Requires `som-tmux-server.exe` to exist next to the test binary,
    //! which is only true when run as `cargo test -p terminal_view`'s
    //! *integration*-style setup from `target/debug/` — running from
    //! `target/debug/deps/` (the default for `cargo test`'s unit tests)
    //! won't find it via `current_exe()`. Marked `#[ignore]` for that
    //! reason; run explicitly with the server binary copied alongside, e.g.:
    //! `cargo build -p som_tmux_server && cp target/debug/som-tmux-server.exe target/debug/deps/ && cargo test -p terminal_view reconnect_tests -- --ignored --test-threads=1`

    use super::*;

    fn unique_profile(label: &str) -> String {
        format!("test-reconnect-{label}-{}", Uuid::new_v4())
    }

    #[test]
    #[ignore = "requires som-tmux-server.exe next to the test binary; see module doc comment"]
    fn reattaches_to_same_session_when_server_still_alive() {
        let profile = unique_profile("reattach");
        let connection = som_tmux_client::connect_or_spawn(&profile).unwrap();
        send(&connection, &ClientMessage::NewSession {
            program: "C:\\Windows\\System32\\cmd.exe".into(),
            args: vec![],
            cwd: None,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let session_id = match recv(&connection).unwrap() {
            ServerMessage::SessionCreated { session_id } => session_id,
            other => panic!("unexpected reply: {other:?}"),
        };
        // First connection's job is done — a fresh connection below stands
        // in for the health-check's reconnect attempt, same as production
        // (a new `connect_or_spawn` call, not reusing the old handle).
        drop(connection);

        let spawn_spec = SpawnSpec {
            profile_name: profile,
            program: "C:\\Windows\\System32\\cmd.exe".into(),
            args: vec![],
            cwd: None,
        };
        let (_connection, reconnected_id, _grid_text) =
            reconnect_blocking(&spawn_spec, session_id, 80, 24).unwrap();
        assert_eq!(reconnected_id, session_id, "should reattach to the original session, not fall back to a new one");
    }

    #[test]
    #[ignore = "requires som-tmux-server.exe next to the test binary; see module doc comment"]
    fn falls_back_to_new_session_when_session_id_unknown() {
        // No server for this profile has ever created this session id —
        // simulates "server process genuinely gone" without needing to
        // actually kill a process mid-test.
        let profile = unique_profile("fallback");
        let bogus_session_id = Uuid::new_v4();
        let spawn_spec = SpawnSpec {
            profile_name: profile,
            program: "C:\\Windows\\System32\\cmd.exe".into(),
            args: vec![],
            cwd: None,
        };
        let (_connection, new_session_id, _grid_text) =
            reconnect_blocking(&spawn_spec, bogus_session_id, 80, 24).unwrap();
        assert_ne!(new_session_id, bogus_session_id, "should have fallen back to a brand new session");
    }
}
