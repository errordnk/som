use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One profile (e.g. "dnk") gets exactly one server process, listening on
/// this pipe name. The server multiplexes many sessions (panes) over the
/// single connection per client, keyed by `session_id` — NOT one pipe per
/// session. See SOM_MUX_PLAN.md / memory `project_som_tmux` for why.
pub fn pipe_name(profile_name: &str) -> String {
    let sanitized: String = profile_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!(r"\\.\pipe\som-tmux-{sanitized}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Create a brand-new session running `program`/`args` in `cwd`, sized
    /// to `cols`x`rows`. The server picks the `session_id`.
    NewSession {
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Attach to an existing session (survived a Som restart because it was
    /// never explicitly closed). Reply is `ServerMessage::Snapshot` with the
    /// session's current grid content, then the connection switches to live
    /// streaming (`ServerMessage::Output`).
    Attach { session_id: Uuid, cols: u16, rows: u16 },
    Write { session_id: Uuid, bytes: Vec<u8> },
    Resize { session_id: Uuid, cols: u16, rows: u16 },
    /// Explicit close (tab/pane closed via UI) — kills the session's PTY for
    /// real, as opposed to just dropping the connection (which leaves the
    /// session alive so it can be re-attached to after a Som restart).
    CloseSession { session_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    SessionCreated { session_id: Uuid },
    /// Sent once right after `Attach` succeeds, AND every time the session's
    /// grid changes afterwards (alacritty's IO thread parses PTY bytes into
    /// its own `Term` and only exposes a "something changed, redraw" signal
    /// — not the raw bytes — so this is a full grid re-snapshot on each
    /// change, not a byte stream). A newly (re)attaching client gets
    /// already-parsed screen state instead of a raw-byte replay (which would
    /// risk splitting an escape sequence mid-stream, or misrendering
    /// alternate-screen state across the gap).
    ///
    /// Known limitation (first iteration): plain text only, no
    /// colors/attributes/cursor position yet.
    GridUpdate { session_id: Uuid, grid_text: String },
    AttachFailed { session_id: Uuid, reason: String },
    SessionClosed { session_id: Uuid },
    Error { message: String },
}
