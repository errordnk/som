use crate::bounds::SessionBounds;
use crate::session::Session;
use som_tmux_server::pipe::PipeConnection;
use som_tmux_server::protocol::{ClientMessage, ServerMessage, pipe_name};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Sessions live only as long as at least one is registered — this map IS
/// the process's reason to exist. When it becomes empty (an explicit
/// `CloseSession` removed the last one), the process exits. It survives
/// clients disconnecting without `CloseSession` (Som crashing, Alt+F4,
/// simply closing the whole app) — that's the entire point: those are
/// exactly the cases where a session should keep running for a later
/// `Attach`.
type Sessions = Arc<Mutex<HashMap<Uuid, Session>>>;

pub fn run(profile_name: &str) -> anyhow::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let pipe_name = pipe_name(profile_name);

    log::info!("som-tmux-server listening on {pipe_name} for profile {profile_name:?}");

    loop {
        let connection = match PipeConnection::accept(&pipe_name) {
            Ok(connection) => connection,
            Err(err) => {
                log::error!("failed to accept connection: {err:#}");
                // Defensive: avoid busy-looping if CreateNamedPipeW/
                // ConnectNamedPipe keeps failing for some persistent reason
                // (permissions, resource exhaustion, etc). Hit this exact
                // failure mode once already from a flag misuse — see the
                // doc comment on `PipeConnection::accept`.
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };

        let sessions = sessions.clone();
        let profile_name = profile_name.to_owned();
        std::thread::spawn(move || {
            if let Err(err) = handle_connection(connection, &sessions) {
                log::warn!("connection ended: {err:#}");
            }
            // A client disconnecting (this thread ending) is NOT a reason to
            // kill any session on its own — see the `Sessions` doc comment
            // above. Only check here whether an explicit CloseSession
            // (handled inside `handle_connection`) already emptied the map;
            // if so, there's nothing left for this process to do.
            if sessions.lock().unwrap().is_empty() {
                log::info!("no sessions left for profile {profile_name:?}, exiting");
                std::process::exit(0);
            }
        });
    }
}

fn handle_connection(connection: PipeConnection, sessions: &Sessions) -> anyhow::Result<()> {
    let connection = Arc::new(connection);
    // The connection's single pipe handle is shared between this reader
    // loop and any grid-update-forwarder threads spawned below for attached
    // sessions — writes are serialized by a mutex around the handle's
    // `write_message` call, since two threads writing to the same pipe
    // instance concurrently would interleave bytes.
    let writer = Arc::new(Mutex::new(()));
    // session_id -> stop flag for that session's grid-update forwarder
    // thread, so a CloseSession/disconnect can tell it to stop rather than
    // leaking a thread that blocks forever on `next_change_blocking`.
    let mut forwarders: HashMap<Uuid, Arc<std::sync::atomic::AtomicBool>> = HashMap::new();

    loop {
        let message = connection.read_message()?;
        let message: ClientMessage = serde_json::from_slice(&message)?;

        match message {
            ClientMessage::NewSession { program, args, cwd, cols, rows } => {
                let bounds = SessionBounds::new(cols, rows);
                match Session::spawn(program, args, cwd, bounds) {
                    Ok(session) => {
                        let session_id = session.id;
                        sessions.lock().unwrap().insert(session_id, session);
                        send(&connection, &writer, &ServerMessage::SessionCreated { session_id })?;
                    }
                    Err(err) => {
                        send(&connection, &writer, &ServerMessage::Error { message: err.to_string() })?;
                    }
                }
            }
            ClientMessage::Attach { session_id, cols, rows } => {
                let snapshot = {
                    let mut guard = sessions.lock().unwrap();
                    guard.get_mut(&session_id).map(|session| {
                        session.resize(SessionBounds::new(cols, rows));
                        session.snapshot_text()
                    })
                };
                match snapshot {
                    Some(grid_text) => {
                        send(&connection, &writer, &ServerMessage::GridUpdate { session_id, grid_text })?;
                        spawn_forwarder(session_id, connection.clone(), writer.clone(), sessions.clone(), &mut forwarders);
                    }
                    None => {
                        send(&connection, &writer, &ServerMessage::AttachFailed {
                            session_id,
                            reason: "no such session".into(),
                        })?;
                    }
                }
            }
            ClientMessage::Write { session_id, bytes } => {
                if let Some(session) = sessions.lock().unwrap().get(&session_id) {
                    session.write(bytes);
                }
            }
            ClientMessage::Resize { session_id, cols, rows } => {
                if let Some(session) = sessions.lock().unwrap().get_mut(&session_id) {
                    session.resize(SessionBounds::new(cols, rows));
                }
            }
            ClientMessage::CloseSession { session_id } => {
                if let Some(stop) = forwarders.remove(&session_id) {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let (removed, now_empty) = {
                    let mut guard = sessions.lock().unwrap();
                    let removed = guard.remove(&session_id);
                    (removed, guard.is_empty())
                };
                if let Some(session) = removed {
                    session.kill();
                    send(&connection, &writer, &ServerMessage::SessionClosed { session_id })?;
                }
                // Check right away rather than waiting for this connection to
                // disconnect — Som keeps the pipe connection open after
                // closing one tab (other tabs on the same profile may still
                // be using it), so waiting for disconnect could leave this
                // process running forever with zero sessions.
                if now_empty {
                    log::info!("no sessions left, exiting");
                    std::process::exit(0);
                }
            }
        }
    }
}

fn spawn_forwarder(
    session_id: Uuid,
    connection: Arc<PipeConnection>,
    writer: Arc<Mutex<()>>,
    sessions: Sessions,
    forwarders: &mut HashMap<Uuid, Arc<std::sync::atomic::AtomicBool>>,
) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    forwarders.insert(session_id, stop.clone());

    std::thread::spawn(move || {
        // Clone the receiver handle ONCE, outside the loop. Re-cloning it
        // every iteration (an earlier version of this code did that) opens
        // a window between dropping the old clone and creating a new one —
        // any `Wakeup` sent by the pump thread in that gap is delivered to
        // neither, since `async_channel` is a competing-consumers channel
        // (each message goes to exactly one receiver clone, not broadcast to
        // all of them) and a dropped-then-recreated receiver isn't the same
        // logical subscriber. Found via manual testing: only the first of
        // ~25 `Wakeup`s sent during a burst of PTY output ever reached the
        // client.
        let events_rx = {
            let guard = sessions.lock().unwrap();
            match guard.get(&session_id) {
                Some(session) => session.events_rx.clone(),
                None => return, // session was closed elsewhere
            }
        };
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let changed = matches!(events_rx.recv_blocking(), Ok(alacritty_terminal::event::Event::Wakeup));
            if !changed {
                continue;
            }
            let grid_text = {
                let guard = sessions.lock().unwrap();
                match guard.get(&session_id) {
                    Some(session) => session.snapshot_text(),
                    None => return,
                }
            };
            if send(&connection, &writer, &ServerMessage::GridUpdate { session_id, grid_text }).is_err() {
                return; // client disconnected
            }
        }
    });
}

fn send(connection: &PipeConnection, writer: &Mutex<()>, message: &ServerMessage) -> anyhow::Result<()> {
    let _guard = writer.lock().unwrap();
    let payload = serde_json::to_vec(message)?;
    connection.write_message(&payload)
}
