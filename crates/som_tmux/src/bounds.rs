use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;

/// Terminal size in character cells, plus (unlike a plain cols/rows pair)
/// the REAL font cell size in pixels — the server never renders anything
/// itself, so unlike `terminal::TerminalBounds` (the GPUI renderer's own
/// pixel-accurate bounds) this doesn't need them to drive `alacritty_
/// terminal::Term`/`tty::new` sizing or PTY resize (SIGWINCH) at all, but
/// DOES need them to answer a client program's `\x1b[16t` ("report cell
/// pixel size in pixels") query correctly — see `cell_width`/`cell_height`'s
/// own doc comment for why a wrong (hardcoded placeholder) answer here was
/// a real, confirmed bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBounds {
    pub cols: u16,
    pub rows: u16,
    /// The real font cell width/height in pixels, as reported by Som via
    /// `crate::relay::PIXEL_SIZE_MARKER_PREFIX` — `1` (this type's default,
    /// same value `SessionBounds::new` still produces) means "not yet
    /// known" (e.g. before a RELAY's first resize marker arrives, or for
    /// any caller that genuinely doesn't have a real pixel size to report,
    /// like every unit test in `redraw.rs`). A HOLDER answering `\x1b[16t`
    /// with `1x1` used to make a client's own image-scaling logic (e.g.
    /// `yazi`'s Kitty Graphics driver, which sizes an image to fit N
    /// cells × the reported cell pixel size) downscale every image to a
    /// handful of pixels before Som ever saw it, then upscale that tiny
    /// image back up to fill the real on-screen area — confirmed root
    /// cause of a real report: images rendering visibly blurry/pixelated
    /// specifically in a `tmux: true` tab, never in a plain terminal.
    pub cell_width: u16,
    pub cell_height: u16,
}

impl SessionBounds {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
            cell_width: 1,
            cell_height: 1,
        }
    }

    /// Same as `new`, but also setting the real pixel cell size — `0` for
    /// either dimension (Som's own "not yet known" sentinel, see
    /// `protocol::RelayInput::Resize`'s doc comment) is treated as "keep
    /// whatever this session already had" rather than overwriting a real
    /// value with a placeholder, since a RELAY's resize-poll thread can
    /// send a `cols`/`rows`-only change (a genuine terminal resize) well
    /// before its own pixel-size marker has ever arrived from Som.
    pub fn with_pixel_size(mut self, cell_width: u16, cell_height: u16, previous: Option<SessionBounds>) -> Self {
        self.cell_width = if cell_width > 0 { cell_width } else { previous.map_or(1, |p| p.cell_width) };
        self.cell_height = if cell_height > 0 { cell_height } else { previous.map_or(1, |p| p.cell_height) };
        self
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
            cell_width: val.cell_width,
            cell_height: val.cell_height,
        }
    }
}
