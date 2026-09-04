//! Daemon side of som-srv's HOLDER/RELAY architecture: a single shared,
//! host-scoped process (see `project_som_tmux` memory for the ORIGINAL
//! per-pane-HOLDER design this superseded) that owns every real shell PTY
//! for every `tmux: true` session on this machine, each behind a headless
//! `alacritty_terminal::Term` (owned by `Session`), independent of whether
//! any RELAY is currently connected to any of them. A RELAY (the process
//! Som itself spawned into its own PTY — see `crate::relay`) connects
//! here, identifies which session it belongs to via `RelayInput::Register`,
//! gets a full state snapshot (`Session::snapshot()`) of the current
//! screen, then a live stream of raw PTY bytes for as long as it stays
//! connected.
//!
//! One daemon process, many sessions — `SESSIONS` below is the registry
//! `Register` looks up/inserts into, keyed by `(client_id, pane_id)` (see
//! `Register`'s own doc comment for why `client_id` is part of the key:
//! multi-tenant isolation on a shared remote host).

use crate::bounds::SessionBounds;
use crate::session::Session;
use crate::srv_cache::SrvCache;
use som_srv::pipe::{self, PipeConnection};
use som_srv::protocol::{ConnectionKind, HandshakeInfo, HolderOutput, RelayInput, SessionInfo, SrvRequest, SrvResponse, daemon_socket_path};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// How many connections (RELAY or `Srv`) are CURRENTLY being handled —
/// incremented for the whole lifetime of each connection's handler thread
/// (`handle_relay`/`handle_srv_request`), decremented via [`ConnectionGuard`]'s
/// `Drop` impl regardless of how that thread exits (clean return or an
/// early `?`-propagated error). This is the signal [`spawn_idle_shutdown_
/// watcher`] polls to decide whether the daemon has gone idle — see that
/// function's own doc comment for why a raw connection count (not the
/// `SessionRegistry`/`SrvCache` state) is the right thing to watch: a
/// short-lived `Srv` request (`ListSessions`, `RequestByteRange`, etc.)
/// briefly bumping this to 1 and back down within milliseconds is fine —
/// the watcher only acts after seeing 0 for the FULL idle window, so a
/// connection this brief never has a chance to look like sustained
/// idleness.
static LIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// RAII guard bumping [`LIVE_CONNECTIONS`] up on construction, back down
/// on `Drop` — used instead of manual increment/decrement pairs so every
/// early-return path (there are many, via `?`, in both `handle_relay` and
/// `handle_srv_request`) still decrements correctly without needing its
/// own explicit cleanup.
struct ConnectionGuard;

impl ConnectionGuard {
    fn new() -> Self {
        LIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        LIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How long the daemon waits, after [`LIVE_CONNECTIONS`] first reads 0,
/// before actually exiting — see [`spawn_idle_shutdown_watcher`]'s own doc
/// comment for why this needs to be a real window rather than an
/// immediate exit.
const IDLE_SHUTDOWN_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Spawns a background thread that exits the WHOLE daemon process
/// (`std::process::exit(0)`) once [`LIVE_CONNECTIONS`] has read 0 for a
/// full [`IDLE_SHUTDOWN_DELAY`] window, polling every second. This closes
/// a real, previously-open gap: unlike the old per-pane HOLDER (which
/// exited naturally when its one shell process died), this shared daemon
/// used to keep running forever once started, even after every Som
/// window and every `somcat` process on the machine had long since
/// closed — confirmed live (2026-09-04) as a stray `som-srv.exe` still
/// resident well after the Som window that started it had been closed,
/// requiring a manual `taskkill` before a fresh daemon (or a test run
/// binding the same named pipe) could start cleanly.
///
/// A plain "exit the instant the count hits 0" check is deliberately NOT
/// what this does: the count legitimately dips to 0 for a moment between
/// two unrelated connections all the time (e.g. a `somcat` process
/// finishing its own `RegisterRangeResponder` connection's teardown right
/// as a brand new one is dialing in — see `ConnectionGuard`'s own doc
/// comment) — exiting on that instant would kill the daemon out from
/// under a legitimate reconnect. Re-checking after `IDLE_SHUTDOWN_DELAY`
/// has fully elapsed with the count STILL at 0 the whole time turns a
/// momentary dip into a much stronger, much less risky signal: nothing at
/// all has connected in a real window of time, not just "nothing is
/// connected in this exact instant."
fn spawn_idle_shutdown_watcher() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if LIVE_CONNECTIONS.load(Ordering::SeqCst) != 0 {
                continue;
            }
            let idle_since = std::time::Instant::now();
            let mut still_idle = true;
            while idle_since.elapsed() < IDLE_SHUTDOWN_DELAY {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if LIVE_CONNECTIONS.load(Ordering::SeqCst) != 0 {
                    still_idle = false;
                    break;
                }
            }
            if still_idle {
                log::info!(
                    "som-srv daemon idle for {:?} with no live connections, shutting down",
                    IDLE_SHUTDOWN_DELAY
                );
                std::process::exit(0);
            }
        }
    });
}

/// Registry key: `client_id` is `None` for a local/WSL RELAY, `Some(...)`
/// for a real SSH RELAY (see `RelayInput::Register::client_id`'s doc
/// comment) — kept as an owned tuple (not borrowed) since it's the
/// `HashMap` key itself, populated once per session and never mutated.
type SessionKey = (Option<String>, String);

/// One entry in the registry: the live `Session` plus the bookkeeping
/// fields `handle_relay`'s cleanup path and `SrvRequest::ListSessions`
/// both need — `profile_name` purely for `SessionInfo` (nothing here
/// uses it for lookup, `SessionKey` already covers that), `tmux` for
/// deciding whether an ungraceful disconnect tears the session down
/// (`tmux: false`) or leaves it running for a later reconnect (`tmux:
/// true`, matching today's existing HOLDER survival behavior).
struct RegisteredSession {
    session: Arc<Session>,
    profile_name: String,
    tmux: bool,
}

type SessionRegistry = Arc<Mutex<HashMap<SessionKey, Arc<RegisteredSession>>>>;

/// Runs as the shared daemon: binds the ONE fixed, well-known address for
/// this machine (`daemon_socket_path`) and accepts connections for as
/// long as the process runs — unlike the old per-pane HOLDER, this never
/// exits just because one particular session's shell exited; it is
/// host-scoped infrastructure serving every session on this machine.
/// Every fresh connection starts with a `ConnectionKind` byte (see that
/// type's doc comment) that decides whether it's dispatched to
/// `handle_relay` (a PTY session) or `handle_srv_request` (the binary
/// side-channel / admin session-management queries) — both share the
/// SAME `registry`.
pub fn run() -> anyhow::Result<()> {
    let registry: SessionRegistry = Arc::new(Mutex::new(HashMap::new()));
    let cache = SrvCache::new();

    let socket_path = daemon_socket_path();
    let listener = pipe::bind(&socket_path)?;
    log::info!("som-srv daemon listening on {socket_path:?}");

    spawn_idle_shutdown_watcher();

    loop {
        let connection = match pipe::accept_on(&listener) {
            Ok(connection) => connection,
            Err(err) => {
                log::error!("failed to accept connection: {err:#}");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };

        let registry = registry.clone();
        let cache = cache.clone();
        std::thread::spawn(move || {
            let _connection_guard = ConnectionGuard::new();
            let kind = match ConnectionKind::read_from(&connection) {
                Ok(kind) => kind,
                Err(err) => {
                    log::warn!("failed to read ConnectionKind, dropping connection: {err:#}");
                    return;
                }
            };
            let result = match kind {
                ConnectionKind::Relay => {
                    log::info!("relay connected");
                    handle_relay(connection, &registry)
                }
                ConnectionKind::Srv => handle_srv_request(connection, &registry, &cache),
            };
            if let Err(err) = result {
                log::warn!("{kind:?} connection ended: {err:#}");
            }
        });
    }
}

/// Just the fields `find_or_create_session` needs out of a `RelayInput::
/// Register` — split out from the wire message itself so a reconnect path
/// (which discards everything except the lookup key) doesn't need to
/// construct a throwaway value for fields it will never use.
struct RegisterFields {
    profile_name: String,
    tmux: bool,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    cursor_shape: Option<String>,
    scrollback: Option<usize>,
}

/// Finds or creates the `Session` this RELAY's `Register` message refers
/// to. A cache miss spawns a brand new shell (mirrors exactly what the old
/// per-pane HOLDER's `run()` used to do at process startup, just moved
/// here and gated on "first time we've seen this key" instead of "this
/// process's entire reason for existing"). A cache hit ignores every field
/// on `Register` except the lookup key itself — see `RelayInput::Register`'s
/// own doc comment for why a stale/different value on reconnect must never
/// respawn or reconfigure a live session.
fn find_or_create_session(registry: &SessionRegistry, key: SessionKey, register: RegisterFields) -> anyhow::Result<Arc<RegisteredSession>> {
    let mut sessions = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = sessions.get(&key) {
        return Ok(existing.clone());
    }

    // 80x24 here is just a starting point, not a lasting default — the
    // first RELAY to connect always sends its actual pane size as the
    // third message on the connection (right after the handshake and
    // Register), applied in `handle_relay` before that RELAY ever sees a
    // single redrawn byte. See `crate::relay::run`'s doc comment and
    // `project_som_tmux` memory ("Обновление 20"'s confirmed
    // `RelayInput::Resize` gap) for why this used to just stay 80x24
    // forever.
    let bounds = SessionBounds::new(80, 24);
    let session = Session::spawn(register.program, register.args, register.cwd, bounds, register.cursor_shape, register.scrollback)
        .map_err(|err| {
            log::error!("failed to spawn shell: {err:#}");
            err
        })?;
    log::info!("session created for profile {:?} pane {:?}, session id {}", register.profile_name, key.1, session.id);

    let registered =
        Arc::new(RegisteredSession { session: Arc::new(session), profile_name: register.profile_name, tmux: register.tmux });
    sessions.insert(key, registered.clone());
    Ok(registered)
}

/// Removes `key` from the registry — called on `RelayInput::Close`
/// (always, regardless of `tmux`) or on an ungraceful disconnect of a
/// `tmux: false` session (see `handle_relay`'s cleanup path). A `tmux:
/// true` session's ungraceful disconnect does NOT call this — it stays in
/// the registry exactly like today's existing HOLDER survival behavior.
fn remove_session(registry: &SessionRegistry, key: &SessionKey) {
    let mut sessions = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(registered) = sessions.remove(key) {
        registered.session.kill();
        // The one unambiguous proof this session died for good (as
        // opposed to merely this one connection dropping, which also
        // happens on every ordinary disconnect-without-Close) — same
        // role the old per-pane HOLDER process's own "shell process
        // exited, holder shutting down" log line played before the
        // shared-daemon migration, just logged to this daemon's own log
        // instead of a since-removed per-pane HOLDER log file.
        log::info!("session for pane {:?} closed for good, session removed from registry", key.1);
    }
}

fn handle_relay(connection: PipeConnection, registry: &SessionRegistry) -> anyhow::Result<()> {
    let connection = Arc::new(connection);
    let writer = Arc::new(Mutex::new(()));

    // Handshake FIRST, before anything else on this connection — see
    // `protocol::HandshakeInfo`'s doc comment. The relay always speaks
    // first (see `crate::relay::run`); the daemon just waits for it here
    // and replies in kind.
    let relay_info = match read_relay_message(&connection)? {
        RelayInput::Handshake(info) => info,
        other => anyhow::bail!("expected Handshake as the first message from a relay, got {other:?}"),
    };
    log::info!(
        "relay handshake: version={:?} os={:?} arch={:?}",
        relay_info.version, relay_info.os, relay_info.arch
    );
    send(&connection, &writer, &HolderOutput::Handshake(HandshakeInfo::current()))?;

    // Register SECOND — identifies which session this connection belongs
    // to now that a single daemon serves every pane on this machine (see
    // `RelayInput::Register`'s own doc comment).
    let key;
    let registered = match read_relay_message(&connection)? {
        RelayInput::Register { profile_name, pane_id, client_id, tmux, program, args, cwd, cursor_shape, scrollback } => {
            key = (client_id, pane_id);
            find_or_create_session(
                registry,
                key.clone(),
                RegisterFields { profile_name, tmux, program, args, cwd, cursor_shape, scrollback },
            )?
        }
        other => anyhow::bail!("expected Register as the second message from a relay, got {other:?}"),
    };
    let session = &registered.session;

    // Every RELAY sends its actual pane size as the very next message,
    // right after handshake+register (see `crate::relay::run`'s doc
    // comment) — applied here BEFORE the first redraw so a (re)attaching
    // pane never has to visibly jump from whatever size the session
    // happened to be at (the hardcoded 80x24 fallback on first spawn, or
    // just however big the window was the last time anything was
    // connected) to its actual current size. Every new connection
    // resizes, not just the session's first ever one — the window may
    // well have been resized while nothing was attached at all (Som
    // closed, or between tabs), so there's no "already correct, skip it"
    // case to special-case here.
    match read_relay_message(&connection)? {
        // `force_resize`, not `resize` — see its doc comment: this MUST
        // notify the shell even when the size happens to already match
        // (a genuinely common case, since the pane usually hasn't actually
        // moved between disconnect and reconnect), because a program like
        // `htop` running inside it only reads the terminal size once at
        // its own startup and needs a fresh SIGWINCH-equivalent to lay
        // itself out correctly for THIS attach, not whatever it happened
        // to see when it first started.
        RelayInput::Resize { cols, rows, cell_width, cell_height } => {
            session.force_resize(SessionBounds::new(cols, rows).with_pixel_size(cell_width, cell_height, None))
        }
        other => anyhow::bail!("expected an initial Resize as the third message from a relay, got {other:?}"),
    }

    // Snapshot immediately after the handshake+register+initial resize —
    // see `protocol::HolderOutput::Snapshot`'s doc comment: this way a
    // (re)attaching RELAY gets the ENTIRE terminal state (grid, cursor,
    // and the full `TermMode` bitflags) directly, correct from the first
    // frame, with nothing to replay. Subscribing to raw bytes BEFORE
    // sending the snapshot (not after) is deliberate — otherwise a byte
    // written by the real shell in the gap between "snapshot captured"
    // and "subscription registered" would be silently lost, visible to
    // the user as dropped output right after a reattach.
    let raw_bytes_rx = session.subscribe_raw_bytes();
    send(&connection, &writer, &HolderOutput::Snapshot(session.snapshot()))?;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_forwarder(connection.clone(), writer.clone(), session.clone(), raw_bytes_rx, stop.clone());

    let result = read_loop(&connection, session);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    // `tmux: false` never outlives its connection — an ungraceful
    // disconnect tears it down exactly like an explicit `Close` would,
    // since there's nothing worth persisting for a non-tmux profile (this
    // matches today's plain, non-tmux PTY behavior: closing the tab ends
    // the shell). `tmux: true` only ever gets torn down by an explicit
    // `Close` (handled inside `read_loop` itself, which already calls
    // `session.kill()` before returning in that case) — a plain dropped
    // connection here leaves it in the registry for a later reconnect,
    // indefinitely, exactly like today's existing HOLDER survival
    // behavior.
    if !registered.tmux {
        remove_session(registry, &key);
    }

    result
}

fn read_relay_message(connection: &PipeConnection) -> anyhow::Result<RelayInput> {
    let message = connection.read_message()?;
    Ok(serde_json::from_slice(&message)?)
}

fn read_loop(connection: &PipeConnection, session: &Session) -> anyhow::Result<()> {
    loop {
        let message = read_relay_message(connection)?;
        match message {
            RelayInput::Bytes(bytes) => session.write(bytes),
            RelayInput::Resize { cols, rows, cell_width, cell_height } => {
                session.resize(SessionBounds::new(cols, rows).with_pixel_size(cell_width, cell_height, None));
            }
            RelayInput::Close => {
                session.kill();
                return Ok(());
            }
            // Only ever expected once each, as the first/second messages
            // (handled in `handle_relay` before this loop starts) — a
            // repeat on the same connection would be a protocol violation
            // from the relay, not something to crash over.
            RelayInput::Handshake(_) => {
                log::warn!("received an unexpected second Handshake mid-connection, ignoring it");
            }
            RelayInput::Register { .. } => {
                log::warn!("received an unexpected second Register mid-connection, ignoring it");
            }
        }
    }
}

/// Handles one `Srv`-kind connection — `somcat` streaming `PutChunk`s,
/// Som subscribing to progress, or an admin tool (the `kill_orphaned_
/// holders`/`kill_all_holders_for_redeploy` replacements) listing/killing
/// sessions. All requests after the handshake are read in a plain loop —
/// unlike `handle_relay`, there's no fixed message ORDER to enforce here
/// (a `somcat` connection sends a stream of `PutChunk`s and nothing else;
/// a Som progress connection sends exactly one `SubscribeProgress` then
/// waits; an admin connection sends one `ListSessions`/`KillSession` and
/// disconnects) — each request is handled independently as it arrives.
fn handle_srv_request(connection: PipeConnection, registry: &SessionRegistry, cache: &SrvCache) -> anyhow::Result<()> {
    let connection = Arc::new(connection);
    let writer = Arc::new(Mutex::new(()));

    match read_srv_message(&connection)? {
        SrvRequest::Handshake(info) => {
            log::info!("srv client handshake: version={:?} os={:?} arch={:?}", info.version, info.os, info.arch);
        }
        other => anyhow::bail!("expected Handshake as the first message from an srv client, got {other:?}"),
    }
    send_srv(&connection, &writer, &SrvResponse::Handshake(HandshakeInfo::current()))?;

    // Tracks whether THIS connection has already registered itself as the
    // sender route for a given key — see the `PutChunk` arm below. A
    // `HashSet` rather than re-registering on every chunk: harmless
    // either way (re-`insert`ing the same closure into `SrvCache`'s
    // routing map is a no-op in effect), but this keeps the common case
    // (thousands of chunks for the same one or two files) from taking
    // `SrvCache`'s internal lock on every single `PutChunk`.
    let mut registered_routes: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

    // Every key THIS connection has registered itself as the range
    // responder for (`RegisterRangeResponder`) — `somcat`'s pull-model
    // responder connection, one per video/audio playback. Used below, on
    // this function's own exit (any return path: graceful or via `?`),
    // to detect the Ctrl+C case: the user killed `somcat` directly rather
    // than Som ever sending `SrvRequest::EndPlayback` for it, so nothing
    // else would otherwise tell Som this placement's source is gone. The
    // reader loop below is wrapped in its own closure specifically so
    // this cleanup can run on EVERY exit path (`?`-propagated error or a
    // clean return) rather than duplicating it at each one.
    let mut range_responder_keys: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

    let result = (|| -> anyhow::Result<()> {
        loop {
            let message = read_srv_message(&connection)?;
            match message {
            SrvRequest::PutChunk { session_id, file_id, offset, data, total_size, content_type, metadata } => {
                if registered_routes.insert((session_id, file_id)) {
                    let connection = connection.clone();
                    let writer = writer.clone();
                    cache.register_sender_route(
                        session_id,
                        file_id,
                        Arc::new(move |request| forward_srv_request(&connection, &writer, &request)),
                    );
                }
                cache.put_chunk(&SrvCache::default_cache_dir(), session_id, file_id, offset, &data, total_size, content_type, metadata)?;
            }
            SrvRequest::SubscribeProgress { session_id, file_id } => {
                let connection = connection.clone();
                let writer = writer.clone();
                cache.subscribe(
                    session_id,
                    file_id,
                    Arc::new(move |response| send_srv(&connection, &writer, &response)),
                );
            }
            SrvRequest::RequestByteRange { session_id, file_id, offset, len } => {
                cache.route_byte_range_request(session_id, file_id, SrvRequest::RequestByteRange { session_id, file_id, offset, len });
            }
            SrvRequest::StopPlayback { session_id, file_id } => {
                cache.notify_stop_playback(session_id, file_id);
            }
            SrvRequest::EndPlayback { session_id, file_id } => {
                cache.route_end_playback(session_id, file_id);
            }
            SrvRequest::RegisterRangeResponder { session_id, file_id } => {
                range_responder_keys.insert((session_id, file_id));
                let connection = connection.clone();
                let writer = writer.clone();
                cache.register_range_responder_route(
                    session_id,
                    file_id,
                    Arc::new(move |request| forward_srv_request(&connection, &writer, &request)),
                );
            }
            SrvRequest::ListSessions { client_id } => {
                let sessions = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let matching: Vec<SessionInfo> = sessions
                    .iter()
                    .filter(|((session_client_id, _), _)| *session_client_id == client_id)
                    .map(|((session_client_id, pane_id), registered)| SessionInfo {
                        profile_name: registered.profile_name.clone(),
                        pane_id: pane_id.clone(),
                        client_id: session_client_id.clone(),
                    })
                    .collect();
                drop(sessions);
                send_srv(&connection, &writer, &SrvResponse::Sessions(matching))?;
            }
            SrvRequest::KillSession { client_id, pane_id } => {
                remove_session(registry, &(client_id, pane_id));
                send_srv(&connection, &writer, &SrvResponse::Killed)?;
            }
            SrvRequest::Handshake(_) => {
                log::warn!("received an unexpected second Handshake mid-connection, ignoring it");
            }
            SrvRequest::RunLuaScript { session_id, file_id, script_source } => {
                if let Err(err) = crate::lua::run_and_stream(cache, session_id, file_id, &script_source) {
                    log::error!("Lua script for ({session_id:08x}, {file_id:08x}) failed: {err:#}");
                }
            }
            }
        }
    })();

    // This connection is gone (cleanly closed or errored out of the loop
    // above via `?`) — for every key it was registered as the range
    // responder for, tell Som playback has ended, exactly as if an
    // explicit `SrvRequest::EndPlayback` had arrived first. Covers the
    // Ctrl+C case specifically: the user killed `somcat` directly, so no
    // `StopPlayback`/`EndPlayback` message was ever sent by anyone —
    // without this, the placement would sit there forever still showing
    // as "playing" with a dead source behind it, since nothing else
    // would ever tell Som otherwise. The natural-EOF and explicit-stop
    // cases (Som sends `EndPlayback` itself, `somcat` reacts and exits on
    // its own) also end up here once `somcat`'s connection subsequently
    // closes, but `notify_stop_playback` is a no-op if Som already tore
    // the placement down — see that method's own doc comment.
    for (session_id, file_id) in range_responder_keys {
        cache.notify_stop_playback(session_id, file_id);
    }

    result
}

fn read_srv_message(connection: &PipeConnection) -> anyhow::Result<SrvRequest> {
    let message = connection.read_message()?;
    Ok(serde_json::from_slice(&message)?)
}

fn send_srv(connection: &PipeConnection, writer: &Mutex<()>, message: &SrvResponse) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    let _guard = writer.lock().unwrap();
    connection.write_message(&payload)?;
    Ok(())
}

/// Writes a daemon-INITIATED `SrvRequest` down a connection that's
/// normally the SENDER of `SrvRequest`s (`somcat`'s `PutChunk` connection)
/// — used ONLY to forward `RequestByteRange` to whichever client
/// registered itself as the sender route for a given `(session_id,
/// file_id)` (see `SrvCache::register_sender_route`). The wire framing is
/// symmetric (both `SrvRequest` and `SrvResponse` are just JSON over the
/// same length-prefixed `PipeConnection` messages), so this is otherwise
/// identical to `send_srv` — kept as a separate function purely so the
/// name at each call site says which DIRECTION of message is actually
/// going out, since a `somcat`-side reader loop needs to expect
/// `SrvRequest`s arriving unsolicited on what it otherwise treats as its
/// own outbound-only connection.
fn forward_srv_request(connection: &PipeConnection, writer: &Mutex<()>, message: &SrvRequest) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    let _guard = writer.lock().unwrap();
    connection.write_message(&payload)?;
    Ok(())
}

/// Spawns two independent threads for one connected RELAY: one forwards
/// live raw PTY bytes (`raw_bytes_rx`, from `Session::subscribe_raw_bytes`)
/// as `HolderOutput::Bytes` messages, the other watches for the real shell
/// exiting (`Session::subscribe`'s `AlacTermEvent::Exit`) to send a final
/// `HolderOutput::ShellExited`. Two separate threads/channels rather than
/// one, since they're fundamentally different event streams: a live byte
/// stream is forwarded directly (no "redraw" step to trigger), while only
/// `Exit` needs its own watcher here.
fn spawn_forwarder(
    connection: Arc<PipeConnection>,
    writer: Arc<Mutex<()>>,
    session: Arc<Session>,
    raw_bytes_rx: async_channel::Receiver<Vec<u8>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    {
        let connection = connection.clone();
        let writer = writer.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            loop {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Ok(bytes) = raw_bytes_rx.recv_blocking() else {
                    return; // session gone (all senders dropped)
                };
                if send(&connection, &writer, &HolderOutput::Bytes(bytes)).is_err() {
                    return; // relay disconnected
                }
            }
        });
    }

    // Its OWN subscription — see `Session::subscribe`'s doc comment. Every
    // connected RELAY gets one of these (a fresh `spawn_forwarder` call per
    // `handle_relay`), and each one now genuinely sees every `Exit` rather
    // than competing with the others over a single shared receiver.
    let events_rx = session.subscribe();
    std::thread::spawn(move || {
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if !session.next_change_blocking(&events_rx) {
                send(&connection, &writer, &HolderOutput::ShellExited).ok();
                return;
            }
            // `next_change_blocking` returning `true` means `Wakeup` —
            // irrelevant here now (the raw-bytes thread above already
            // forwards the actual content independently); loop back
            // around waiting specifically for `Exit`.
        }
    });
}

fn send(connection: &PipeConnection, writer: &Mutex<()>, message: &HolderOutput) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    let _guard = writer.lock().unwrap();
    connection.write_message(&payload)?;
    Ok(())
}
