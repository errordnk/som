//! One-shot admin client helpers — connects to the daemon already running
//! on THIS machine (`protocol::daemon_socket_path()`) and does exactly one
//! `SrvRequest`/`SrvResponse` round trip, then disconnects. Used by
//! `main.rs`'s `--list-sessions`/`--kill-session` CLI subcommands, which
//! exist specifically so a REMOTE cleanup caller (Som's own
//! `kill_orphaned_holders`/`kill_all_holders_for_redeploy` replacements in
//! `terminal_panel.rs`) can reuse the exact SSH-probe-and-read-stdout
//! pattern `ensure_remote_binary_deployed`'s `--version` probe already
//! established, rather than opening a raw pipe connection over SSH from
//! Som's own side for something this simple.

use crate::pipe::PipeConnection;
use crate::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse, daemon_socket_path};

fn connect_and_handshake() -> anyhow::Result<PipeConnection> {
    let connection = PipeConnection::connect(&daemon_socket_path())?;
    ConnectionKind::Srv.write_to(&connection)?;
    send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
    match read(&connection)? {
        SrvResponse::Handshake(_) => Ok(connection),
        other => anyhow::bail!("expected Handshake as the first response from som-srv, got {other:?}"),
    }
}

fn send(connection: &PipeConnection, message: &SrvRequest) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    connection.write_message(&payload)?;
    Ok(())
}

fn read(connection: &PipeConnection) -> anyhow::Result<SrvResponse> {
    let message = connection.read_message()?;
    Ok(serde_json::from_slice(&message)?)
}

/// Lists every session the LOCAL daemon currently holds for `client_id`
/// (`None` for a local/WSL RELAY's own sessions).
pub fn list_sessions(client_id: Option<String>) -> anyhow::Result<Vec<crate::protocol::SessionInfo>> {
    let connection = connect_and_handshake()?;
    send(&connection, &SrvRequest::ListSessions { client_id })?;
    match read(&connection)? {
        SrvResponse::Sessions(sessions) => Ok(sessions),
        other => anyhow::bail!("expected Sessions as the response to ListSessions, got {other:?}"),
    }
}

/// Tears down one specific session — no-op if it isn't in the registry
/// (see `SrvRequest::KillSession`'s own doc comment).
pub fn kill_session(client_id: Option<String>, pane_id: String) -> anyhow::Result<()> {
    let connection = connect_and_handshake()?;
    send(&connection, &SrvRequest::KillSession { client_id, pane_id })?;
    match read(&connection)? {
        SrvResponse::Killed => Ok(()),
        other => anyhow::bail!("expected Killed as the response to KillSession, got {other:?}"),
    }
}
