use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Color;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One rendered grid cell's content and appearance — deliberately a
/// protocol-specific type rather than sending `alacritty_terminal::term::
/// cell::Cell` directly, even though that type already derives `Serialize`/
/// `Deserialize` (its `serde` feature is enabled by default in this
/// workspace's `Cargo.toml`). Keeping the wire format decoupled from
/// alacritty's internal `Cell` (which also carries an `Option<Arc<
/// CellExtra>>` for hyperlinks/zero-width chars/underline color — not
/// needed yet) means a future alacritty version bump can't silently change
/// what's on the wire, which matters more here than usual: `som-tmux`'s
/// eventual WSL/SSH transports (see `project_som_tmux` memory) may end up
/// with a server binary built from a different commit than the client Som
/// is running, and the two only need to agree on THIS type, not on
/// alacritty's internals. `Color`/`Flags` themselves ARE reused as-is
/// (rather than redefining yet another color enum) — they're small, stable
/// enough, and already exactly what `terminal_view::terminal_element::
/// convert_color` expects, so the client can hand a cell's `fg`/`bg`
/// straight to that existing function instead of converting through an
/// intermediate representation first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolCell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

/// Cursor position within the grid, using the same `line`/`column`
/// convention as `alacritty_terminal::index::Point` (line can be negative
/// when scrolled into history — not currently possible here since the
/// server never scrolls back, but kept as `i32` to match `Point` exactly
/// rather than assuming `usize` and having to special-case it later).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CursorPosition {
    pub line: i32,
    pub column: usize,
}

/// A full grid snapshot: every visible cell, row-major, plus where the
/// cursor currently is (`None` if the terminal has hidden it, e.g. many
/// full-screen TUI apps hide the cursor between redraws — see
/// `alacritty_terminal::term::RenderableCursor`'s `CursorShape::Hidden`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub rows: Vec<Vec<ProtocolCell>>,
    pub cursor: Option<CursorPosition>,
}

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
    GridUpdate { session_id: Uuid, snapshot: GridSnapshot },
    AttachFailed { session_id: Uuid, reason: String },
    SessionClosed { session_id: Uuid },
    Error { message: String },
}
