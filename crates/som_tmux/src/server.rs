//! HOLDER side of the v3 architecture (see `SOM_MUX_PLAN.md`'s "som-tmux
//! v3" section): this is the long-lived process that actually owns the
//! real shell's PTY and keeps a headless `alacritty_terminal::Term` (owned
//! by `Session`) up to date, independent of whether any RELAY is
//! currently connected. A RELAY (the process Som itself spawned into its
//! own PTY — see `crate::relay`) connects here, gets a full state snapshot
//! (`Session::snapshot()`) of the current screen, then a live stream of
//! raw PTY bytes for as long as it stays connected.
//!
//! One HOLDER = exactly one pane (see `protocol::pipe_name`'s doc comment
//! for why this changed from the old per-profile multiplexed design) — so
//! there's no session registry here at all, just one `Session` for this
//! process's entire lifetime.

use crate::bounds::SessionBounds;
use crate::session::Session;
use som_tmux::pipe::{self, PipeConnection};
use som_tmux::protocol::{HandshakeInfo, HolderOutput, RelayInput, pipe_name};
use std::sync::{Arc, Mutex};

/// Runs as the HOLDER for `profile_name`/`pane_id`: spawns `program`/`args`
/// as the real shell (only meaningful the FIRST time this pipe name is
/// used — see `crate::relay` for how a caller decides whether to become a
/// HOLDER or connect as a RELAY to one that already exists), then accepts
/// RELAY connections for as long as the shell process is alive.
///
/// Exits when the shell process exits (nothing left to hold) — mirrors the
/// old design's "0 sessions -> exit", just simplified to "1 session, its
/// exit IS the exit" now that there's no multi-session registry to check
/// emptiness of.
pub fn run(
    profile_name: &str,
    pane_id: &str,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    cursor_shape: Option<String>,
    scrollback: Option<usize>,
) -> anyhow::Result<()> {
    // 80x24 here is just a starting point, not a lasting default — the
    // first RELAY to connect always sends its actual pane size as the
    // second message on the connection (right after the handshake), applied
    // in `handle_relay` before that RELAY ever sees a single redrawn byte.
    // See `crate::relay::run`'s doc comment and `project_som_tmux` memory
    // ("Обновление 20"'s confirmed `RelayInput::Resize` gap) for why this
    // used to just stay 80x24 forever.
    let bounds = SessionBounds::new(80, 24);
    let session = Arc::new(Session::spawn(program, args, cwd, bounds, cursor_shape, scrollback).map_err(|err| {
        log::error!("failed to spawn shell: {err:#}");
        err
    })?);
    log::info!("holder started for profile {profile_name:?} pane {pane_id:?}, session id {}", session.id);

    let pipe_name = pipe_name(profile_name, pane_id);
    let listener = pipe::bind(&pipe_name)?;

    // Exits the whole process once the shell itself exits — spawned once,
    // up front, rather than checked after each connection ends: the shell
    // can exit while zero OR one RELAY is connected, and either way there's
    // nothing left for this process to hold open.
    {
        let session = session.clone();
        // Its OWN subscription — see `Session::subscribe`'s doc comment for
        // why sharing a receiver with `spawn_forwarder`'s threads (as this
        // used to) silently dropped events between them.
        let events_rx = session.subscribe();
        std::thread::spawn(move || {
            loop {
                if !session.next_change_blocking(&events_rx) {
                    log::info!("shell process exited, holder shutting down");
                    std::process::exit(0);
                }
            }
        });
    }

    loop {
        let connection = match pipe::accept_on(&listener) {
            Ok(connection) => connection,
            Err(err) => {
                log::error!("failed to accept relay connection: {err:#}");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };
        log::info!("relay connected");

        let session = session.clone();
        std::thread::spawn(move || {
            if let Err(err) = handle_relay(connection, &session) {
                log::warn!("relay connection ended: {err:#}");
            }
        });
    }
}

fn handle_relay(connection: PipeConnection, session: &Arc<Session>) -> anyhow::Result<()> {
    let connection = Arc::new(connection);
    let writer = Arc::new(Mutex::new(()));

    // Handshake FIRST, before anything else on this connection — see
    // `protocol::HandshakeInfo`'s doc comment. The relay always speaks
    // first (see `crate::relay::run`); the holder just waits for it here
    // and replies in kind. This connection has nothing better to do with a
    // relay's version/platform info than log it — `crate::relay` is where
    // the actual "should I copy a newer binary / restart a live holder"
    // policy decisions get made, using the HOLDER's reply to ITS OWN
    // handshake, sent when it first connects (a fresh pipe connection, not
    // this one) before ever getting to the point of becoming this holder's
    // relay in the first place.
    let relay_info = match read_relay_message(&connection)? {
        RelayInput::Handshake(info) => info,
        other => anyhow::bail!("expected Handshake as the first message from a relay, got {other:?}"),
    };
    log::info!(
        "relay handshake: version={:?} os={:?} arch={:?}",
        relay_info.version, relay_info.os, relay_info.arch
    );
    send(&connection, &writer, &HolderOutput::Handshake(HandshakeInfo::current()))?;

    // Every RELAY sends its actual pane size as the very next message,
    // right after the handshake (see `crate::relay::run`'s doc comment) —
    // applied here BEFORE the first redraw so a (re)attaching pane never
    // has to visibly jump from whatever size the session happened to be at
    // (the hardcoded 80x24 fallback on first spawn, or just however big the
    // window was the last time anything was connected) to its actual
    // current size. Every new connection resizes, not just the session's
    // first ever one — the window may well have been resized while nothing
    // was attached at all (Som closed, or between tabs), so there's no
    // "already correct, skip it" case to special-case here.
    match read_relay_message(&connection)? {
        // `force_resize`, not `resize` — see its doc comment: this MUST
        // notify the shell even when the size happens to already match
        // (a genuinely common case, since the pane usually hasn't actually
        // moved between disconnect and reconnect), because a program like
        // `htop` running inside it only reads the terminal size once at
        // its own startup and needs a fresh SIGWINCH-equivalent to lay
        // itself out correctly for THIS attach, not whatever it happened
        // to see when it first started.
        RelayInput::Resize { cols, rows } => session.force_resize(SessionBounds::new(cols, rows)),
        other => anyhow::bail!("expected an initial Resize as the second message from a relay, got {other:?}"),
    }

    // Snapshot immediately after the handshake+initial resize — see
    // `protocol::HolderOutput::Snapshot`'s doc comment for why this
    // replaces v1's "full ANSI redraw on connect" entirely: a
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
            RelayInput::Resize { cols, rows } => {
                session.resize(SessionBounds::new(cols, rows));
            }
            RelayInput::Close => {
                session.kill();
                return Ok(());
            }
            // Only ever expected once, as the very first message (handled
            // in `handle_relay` before this loop starts) — a second one on
            // the same connection would be a protocol violation from the
            // relay, not something to crash over.
            RelayInput::Handshake(_) => {
                log::warn!("received an unexpected second Handshake mid-connection, ignoring it");
            }
        }
    }
}

/// Spawns two independent threads for one connected RELAY: one forwards
/// live raw PTY bytes (`raw_bytes_rx`, from `Session::subscribe_raw_bytes`)
/// as `HolderOutput::Bytes` messages, the other watches for the real shell
/// exiting (`Session::subscribe`'s `AlacTermEvent::Exit`) to send a final
/// `HolderOutput::ShellExited`. Two separate threads/channels rather than
/// one, since they're fundamentally different event streams now (v1 had
/// only `Wakeup`/`Exit` to work with, so one `next_change_blocking` loop
/// covered both "something changed, redraw" and "it exited" — v3 gets a
/// live byte stream directly, so there's no "redraw" step to trigger off
/// `Wakeup` for anymore, only `Exit` still matters here).
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
    // than competing with the others (and with the shell-exit watcher in
    // `run`) over a single shared receiver.
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
