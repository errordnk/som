use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;

/// Terminal size in character cells only — the server never renders
/// anything, so unlike `terminal::TerminalBounds` (which carries pixel
/// metrics for the GPUI renderer) this only needs cols/rows to drive
/// `alacritty_terminal::Term`/`tty::new` sizing and PTY resize (SIGWINCH).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBounds {
    pub cols: u16,
    pub rows: u16,
}

impl SessionBounds {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl Default for SessionBounds {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Dimensions for SessionBounds {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

impl From<SessionBounds> for WindowSize {
    fn from(val: SessionBounds) -> Self {
        WindowSize {
            num_lines: val.rows,
            num_cols: val.cols,
            cell_width: 1,
            cell_height: 1,
        }
    }
}
