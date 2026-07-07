//! HOLDER side of the transparent-PTY-proxy architecture (see
//! `project_som_tmux` memory, "Обновление 16"-19 for the full design
//! history): this is the long-lived process that actually owns the real
//! shell's PTY and keeps parsing its output into a grid, independent of
//! whether any RELAY is currently connected. A RELAY (the process Som
//! itself spawned into its own PTY — see `crate::relay`) connects here,
//! gets a full ANSI redraw of the current screen, then a live stream of
//! incremental updates for as long as it stays connected.
//!
//! One HOLDER = exactly one pane (see `protocol::pipe_name`'s doc comment
//! for why this changed from the old per-profile multiplexed design) — so
//! there's no session registry here at all, just one `Session` for this
//! process's entire lifetime.

use crate::bounds::SessionBounds;
use crate::redraw::Redrawer;
use crate::session::Session;
use som_tmux_server::pipe::{self, PipeConnection};
use som_tmux_server::protocol::{HandshakeInfo, HolderOutput, RelayInput, pipe_name};
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
pub fn run(profile_name: &str, pane_id: &str, program: String, args: Vec<String>, cwd: Option<String>) -> anyhow::Result<()> {
    let bounds = SessionBounds::new(80, 24);
    let session = Arc::new(Session::spawn(program, args, cwd, bounds).map_err(|err| {
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
        std::thread::spawn(move || {
            loop {
                if !session.next_change_blocking() {
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

    // Full redraw immediately after the handshake — a fresh Redrawer has no
    // prior state, so the very first `Session::redraw` call against it
    // emits the entire current screen (see `Redrawer::new`'s doc comment).
    // This is what gives a (re)attaching RELAY the "restore the screen as
    // it was" behavior without ever replaying raw historical bytes.
    let redrawer = Arc::new(Mutex::new(Redrawer::new()));
    send_redraw(&connection, &writer, session, &redrawer)?;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_forwarder(connection.clone(), writer.clone(), session.clone(), redrawer, stop.clone());

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

fn spawn_forwarder(
    connection: Arc<PipeConnection>,
    writer: Arc<Mutex<()>>,
    session: Arc<Session>,
    redrawer: Arc<Mutex<Redrawer>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if !session.next_change_blocking() {
                send(&connection, &writer, &HolderOutput::ShellExited).ok();
                return;
            }
            if send_redraw(&connection, &writer, &session, &redrawer).is_err() {
                return; // relay disconnected
            }
        }
    });
}

fn send_redraw(
    connection: &PipeConnection,
    writer: &Mutex<()>,
    session: &Session,
    redrawer: &Mutex<Redrawer>,
) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    session.redraw(&mut redrawer.lock().unwrap(), &mut bytes)?;
    if bytes.is_empty() {
        return Ok(());
    }
    send(connection, writer, &HolderOutput::Bytes(bytes))
}

fn send(connection: &PipeConnection, writer: &Mutex<()>, message: &HolderOutput) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    let _guard = writer.lock().unwrap();
    connection.write_message(&payload)?;
    Ok(())
}
