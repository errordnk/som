//! Serializes an `alacritty_terminal` grid back into ANSI escape sequences,
//! the way tmux's `tty.c` redraws a client's screen from its own internal
//! `grid.c` state rather than replaying raw bytes from the child process.
//!
//! Why this exists at all (see `project_som_tmux` memory, "Обновление 16"
//! for the full history): `som-tmux` needs to be a transparent PTY
//! proxy — Som's own `TerminalElement`/`TerminalView` must never change, so
//! whatever `som-tmux` writes to its own stdout has to be plain ANSI
//! bytes that any terminal emulator (including alacritty, which is what Som
//! itself uses) can parse normally. Raw byte replay from the child process
//! doesn't work here — a client reattaching after a resize, or after missing
//! a chunk of output while disconnected, has no way to know what state the
//! screen was actually left in. So instead, exactly like tmux, this walks
//! the *parsed* grid (already available from `Session`'s own `Term`) and
//! emits the minimal set of escape codes needed to reproduce it, diffing
//! against what was last written so unchanged runs don't get re-emitted.
//!
//! The diff state (`Redrawer`) is intentionally the same code path for both
//! an incremental update and a full redraw on (re)attach — a full redraw is
//! just this same diffing logic starting from a blank slate (`Redrawer::new`
//! initializes as "nothing painted yet, cursor unknown"), never a special
//! second code path. This mirrors tty.c's own model 1:1.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use std::io::Write;

/// One cell's content/appearance, as tracked in `Redrawer`'s per-position
/// history — deliberately a plain tuple-like struct rather than reusing
/// `alacritty_terminal::term::cell::Cell` (which carries an `Option<Arc<
/// CellExtra>>` this diff doesn't need in full, same reasoning as
/// `protocol::ProtocolCell` — see that type's doc comment), except for
/// `zerowidth`/`underline_color`, which — unlike `hyperlink` — this diff DOES
/// need: Som's rich-content protocol's Unicode-placeholder-mode cells
/// (`crates/terminal/src/kitty_graphics_placeholder.rs`) are encoded
/// entirely as an ordinary base char (`U+10EEEE`) plus combining diacritics
/// in `zerowidth` plus an `underline_color` distinct from `fg` — dropping
/// either here would silently corrupt every placeholder cell's row/column/
/// id on the very first redraw after a resize, alt-screen swap, or
/// reconnect (all of which force a full repaint through this same path).
#[derive(Clone, PartialEq)]
struct PaintedCell {
    c: char,
    fg: Color,
    bg: Color,
    flags: Flags,
    underline_color: Option<Color>,
    zerowidth: Vec<char>,
}

/// Tracks what's actually been written to the client so far, so repainting
/// only emits escape codes for cells that ACTUALLY CHANGED since the last
/// redraw — exactly the role tmux's `tty.c` plays (its own `cell_fg`/
/// `cell_bg`/`cx`/`cy` fields), extended here to per-cell content diffing
/// (`last_grid`) since most of a typical screen is unchanged blank space
/// between updates and re-painting all of it every time would defeat the
/// entire point of diffing.
pub struct Redrawer {
    /// Previous redraw's full grid contents, row-major — `None` for a row
    /// that's never been painted (or after `reset()`), forcing every cell
    /// in that row to be treated as changed. Indexed the same way
    /// `RenderableContent::display_iter` walks the grid (`point.line.0`
    /// clamped to `0` as the row index — negative history-scrollback lines
    /// never occur here since this only ever walks the VISIBLE screen).
    last_grid: Vec<Option<Vec<PaintedCell>>>,
    /// Virtual write cursor — tracks where the NEXT cell write would land
    /// if painting just continues left-to-right, so `move_to` can skip
    /// emitting a position escape when it's not needed. Reset (via
    /// `have_written = false`) after `place_real_cursor` repositions the
    /// real cursor, since that's a different position than wherever the
    /// last cell write happened to leave things.
    cursor_line: i32,
    cursor_col: usize,
    fg: Color,
    bg: Color,
    flags: Flags,
    have_written: bool,
    /// The REAL terminal cursor's last-painted position/visibility — kept
    /// separate from `cursor_line`/`cursor_col` above (which track the
    /// virtual write-cursor mid-repaint) so `place_real_cursor` can skip
    /// re-emitting anything when the actual cursor hasn't moved since the
    /// last redraw — the same "nothing changed, emit nothing" diffing this
    /// whole module exists for, applied to the cursor itself.
    real_cursor: Option<(Point, CursorShape)>,
    /// Last-emitted underline color, distinct from `fg`/`bg` — `None` means
    /// "no explicit underline color set" (falls back to `fg` for rendering,
    /// same as `alacritty_terminal::term::cell::Cell::underline_color()`'s
    /// own `None` meaning). Tracked the same way `fg`/`bg`/`flags` are, so
    /// `set_attrs` only emits SGR 58 (set underline color) / 59 (reset to
    /// default) when it actually changes.
    underline_color: Option<Color>,
    /// Whether the LAST `redraw` call saw `Term` in the alternate screen
    /// buffer (`None` before the first call, same "unknown, don't skip
    /// anything" role as `last_grid` being empty). A real terminal swaps
    /// buffers atomically in hardware — nothing needs diffing across that
    /// swap, the whole screen just changes. `last_grid`, however, has no
    /// idea a swap even happened: it's plain per-position content, and the
    /// primary and alternate buffers are two logically unrelated screens
    /// that happen to share coordinates. A cell that's `'e'` at (5, 10) in
    /// the alt screen (mid-word in some TUI app like `htop`'s process list)
    /// and *also* happens to be `'e'` at (5, 10) in whatever the primary
    /// screen shows right after exiting looks IDENTICAL to the diff, so it
    /// gets silently skipped — leaving that stale glyph on screen. This is
    /// the confirmed root cause of a reported bug: leftover `htop` process-
    /// list fragments (and stale colors from its own SGR state) staying
    /// visible after quitting it back to a plain PowerShell prompt. Forcing
    /// a full repaint (mirroring `Redrawer::new`'s "nothing painted yet")
    /// whenever this flag flips is the fix — see `redraw`'s check of it.
    last_alt_screen: Option<bool>,
    /// The (cols, rows) `Term` reported on the LAST `redraw` call (`None`
    /// before the first call). Same reasoning as `last_alt_screen`: a resize
    /// reflows every row (`Term::resize` re-wraps existing lines to the new
    /// width), so position (row, col) means a completely different piece of
    /// content before vs. after — `last_grid` comparing them cell-by-cell
    /// can "match" by pure coincidence just like the alt-screen case, and
    /// wrongly skip repainting a cell that actually holds unrelated,
    /// reflowed-into-place content now.
    last_size: Option<(usize, usize)>,
    /// Whether DECCKM (Application Cursor Keys, `TermMode::APP_CURSOR`) was
    /// active on the HOLDER's `Term` as of the last `redraw` call — `None`
    /// before the first call. Unlike `last_alt_screen`/`last_size`, this
    /// isn't consulted to decide whether to force a full repaint (a cursor-
    /// key mode change doesn't invalidate on-screen content the way an
    /// alt-screen swap or resize does) — it exists purely so `redraw` can
    /// detect the FLIP and re-emit the real `\x1b[?1h`/`\x1b[?1l` DECCKM
    /// sequence to the client. Without this, the client-side `Term` (the
    /// RELAY's own, on Som's side of the connection) never learns the real
    /// shell/app enabled application cursor keys at all — confirmed root
    /// cause of a real user report ("не работают F-клавиши в терминалах
    /// wsl/ssh"): `redraw`'s diffing walks the HOLDER's PARSED GRID
    /// content (mirroring tmux's own `tty.c` model — see this module's doc
    /// comment), which by design never replays raw mode-switching escape
    /// codes the way a naive byte-forwarding proxy would, since they carry
    /// no visible on-screen content of their own. That's correct for
    /// everything that actually painted a cell, but DECCKM controls
    /// nothing about what's ON screen — only how Som should encode the
    /// user's NEXT keypress (`mappings::keys::to_esc_str` checks
    /// `TermMode::APP_CURSOR` to choose between e.g. `\x1b[A` and `\x1bOA`
    /// for the up arrow) — so it has to be forwarded explicitly, the same
    /// way `ALT_SCREEN` already gets tracked (just for a different
    /// purpose: forcing a repaint, not re-emitting anything).
    last_app_cursor: Option<bool>,
    /// The mouse-tracking-related `TermMode` bits active on the HOLDER's
    /// `Term` as of the last `redraw` call — `None` before the first call.
    /// Same category of problem as `last_app_cursor` (see its doc comment):
    /// mouse mode controls nothing about what's ON screen, only whether
    /// `terminal::Terminal::mouse_mode` starts forwarding clicks/motion to
    /// the PTY at all, so the grid diff below — which only ever reproduces
    /// visible cell content — can never carry it, and it has to be
    /// re-emitted explicitly on every flip instead. Confirmed root cause of
    /// a real bug report: a `tmux: true` tab's mouse stopped working
    /// entirely in a mouse-driven client (`yazi`) — but ONLY after a
    /// (re)attach, since a client that enabled mouse mode on a connection
    /// that never dropped had already sent the real `?1000h` etc. bytes
    /// down the ordinary live-byte-forwarding path (`HolderOutput::Bytes`,
    /// unaffected by this — see `crate::server`'s `spawn_forwarder`), which
    /// only bypasses `Redrawer` (and this gap) once a FRESH `Snapshot` is
    /// what a reconnecting RELAY sees first.
    last_mouse_mode: Option<TermMode>,
}

impl Redrawer {
    /// A fresh `Redrawer` with no prior state — the next `redraw` call
    /// against this will therefore emit a full repaint (every non-default
    /// cell gets its own SGR + position as needed), which is exactly what a
    /// newly (re)attached client needs to see the whole current screen.
    pub fn new() -> Self {
        Self {
            last_grid: Vec::new(),
            cursor_line: -1,
            cursor_col: usize::MAX,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            have_written: false,
            real_cursor: None,
            underline_color: None,
            last_alt_screen: None,
            last_size: None,
            last_app_cursor: None,
            last_mouse_mode: None,
        }
    }

    /// Walks `term`'s current visible grid and writes the minimal ANSI
    /// bytes needed to bring the client's screen up to date, given this
    /// `Redrawer`'s record of what was last written. Always ends by
    /// positioning the real cursor at its actual location (the row-by-row
    /// write loop moves the "virtual" cursor around while painting cells,
    /// so it doesn't end up in the right place on its own) and setting its
    /// shape/visibility.
    pub fn redraw<W: Write, T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &Term<T>,
        out: &mut W,
    ) -> std::io::Result<()> {
        // See `last_alt_screen`'s doc comment: a swap between the primary
        // and alternate screen buffers invalidates every position-based
        // diff this `Redrawer` has recorded, since the two buffers are
        // logically unrelated screens that only coincidentally share
        // coordinates. `None` (the very first call) is deliberately NOT
        // treated as a swap here — `last_grid` already starts empty, so
        // that first call is already a full repaint on its own merit.
        let alt_screen = term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        if self.last_alt_screen.is_some_and(|was_alt| was_alt != alt_screen) {
            self.last_grid.clear();
            // The virtual write-cursor (`cursor_line`/`cursor_col`,
            // consulted by `move_to`'s "did painting continue naturally"
            // optimization) and the real terminal cursor's last-known
            // position/shape (`real_cursor`) both describe positions in
            // whichever buffer was active a moment ago — neither is
            // meaningful once the underlying buffer just changed out from
            // under them. Left stale, the first cell painted after a swap
            // could coincidentally match wherever `cursor_line`/`cursor_col`
            // already point, making `move_to` skip emitting a real CUP even
            // though the ACTUAL terminal cursor (on Som's side, receiving
            // these bytes) is still sitting wherever the old buffer's last
            // redraw left it — text then starts painting from the wrong
            // on-screen position.
            self.have_written = false;
            self.real_cursor = None;
        }
        self.last_alt_screen = Some(alt_screen);

        // See `last_size`'s doc comment: a resize reflows every row, so
        // `last_grid`'s cell-by-cell comparison against pre-resize content
        // is comparing logically unrelated text that only coincidentally
        // shares a (row, col) position now. Same fix as the alt-screen swap
        // above — force a full repaint rather than let the diff trust
        // coordinates that no longer mean what they used to.
        let size = (term.columns(), term.screen_lines());
        if self.last_size.is_some_and(|was_size| was_size != size) {
            self.last_grid.clear();
            self.have_written = false;
            self.real_cursor = None;
        }
        self.last_size = Some(size);

        // See `last_app_cursor`'s doc comment: unlike `alt_screen`/`size`
        // above, this doesn't invalidate `last_grid` — DECCKM has no
        // effect on what's actually painted on screen. It DOES need to be
        // re-sent to the client explicitly on every flip, since (also
        // unlike alt-screen/resize) there's no other mechanism that would
        // ever tell the client-side `Term` this happened — the grid
        // diffing below only reproduces visible cell content, never mode-
        // switching escapes, by design (see this module's doc comment).
        let app_cursor = term.mode().contains(alacritty_terminal::term::TermMode::APP_CURSOR);
        if self.last_app_cursor.is_some_and(|was_set| was_set != app_cursor) || self.last_app_cursor.is_none() {
            out.write_all(if app_cursor { b"\x1b[?1h" } else { b"\x1b[?1l" })?;
        }
        self.last_app_cursor = Some(app_cursor);

        // See `last_mouse_mode`'s doc comment: re-emit the exact DECSET/
        // DECRST pair for each individual mouse-tracking bit that actually
        // flipped since the last redraw — one pair per protocol variant
        // (`?1000`=click, `?1002`=drag, `?1003`=all-motion, `?1006`=SGR
        // coordinate encoding, `?1005`=UTF-8 coordinate encoding), not a
        // single blanket "mouse on/off" toggle, since a client (`yazi`
        // sends `?1000h?1002h?1015h?1006h` together) can combine several
        // of these and a coarser toggle would lose which ones specifically
        // were requested.
        let mouse_mode = term.mode().intersection(
            TermMode::MOUSE_REPORT_CLICK
                | TermMode::MOUSE_DRAG
                | TermMode::MOUSE_MOTION
                | TermMode::SGR_MOUSE
                | TermMode::UTF8_MOUSE,
        );
        if self.last_mouse_mode != Some(mouse_mode) {
            for (bit, csi) in [
                (TermMode::MOUSE_REPORT_CLICK, "?1000"),
                (TermMode::MOUSE_DRAG, "?1002"),
                (TermMode::MOUSE_MOTION, "?1003"),
                (TermMode::UTF8_MOUSE, "?1005"),
                (TermMode::SGR_MOUSE, "?1006"),
            ] {
                let was_set = self.last_mouse_mode.is_some_and(|prev| prev.contains(bit));
                let now_set = mouse_mode.contains(bit);
                if was_set != now_set || self.last_mouse_mode.is_none() {
                    if now_set {
                        write!(out, "\x1b[{csi}h")?;
                    } else if self.last_mouse_mode.is_some() {
                        // Only RESET a bit that a PRIOR redraw actually SET
                        // — on the very first redraw (`last_mouse_mode ==
                        // None`), a bit that's simply off needs no `l`
                        // sequence at all (that's already the client's
                        // default state).
                        write!(out, "\x1b[{csi}l")?;
                    }
                }
            }
        }
        self.last_mouse_mode = Some(mouse_mode);

        let content = term.renderable_content();
        let mut new_grid: Vec<Vec<PaintedCell>> = Vec::new();

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = &indexed.cell;
            let line_ix = point.line.0.max(0) as usize;
            while new_grid.len() <= line_ix {
                new_grid.push(Vec::new());
            }
            let underline_color = cell.underline_color();
            let zerowidth = cell.zerowidth().map(|chars| chars.to_vec()).unwrap_or_default();
            let painted = PaintedCell {
                c: cell.c,
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
                underline_color,
                zerowidth,
            };

            // Wide-char spacers are placeholders for the second column of a
            // double-width character — the actual character was already
            // written for the first column; writing the spacer's (blank)
            // `c` here would blank out half of every wide character. Still
            // recorded into `new_grid` below (a real cell occupies that
            // position and must be diffed against next time), just never
            // painted on its own.
            let is_wide_char_spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);

            // The actual diff: skip this cell ENTIRELY (no position, no
            // SGR, no character byte at all) if it's pixel-for-pixel
            // identical to what was there after the last redraw — this is
            // what makes an idle pane with a mostly-unchanged screen cheap
            // to redraw repeatedly, not just the escape-code-formatting
            // micro-optimizations in `move_to`/`set_attrs` below.
            let previously_painted = self
                .last_grid
                .get(line_ix)
                .and_then(|row| row.as_ref())
                .and_then(|row| row.get(new_grid[line_ix].len()));
            let unchanged = previously_painted == Some(&painted);

            if is_wide_char_spacer || unchanged {
                new_grid[line_ix].push(painted);
                continue;
            }

            self.move_to(point, out)?;
            self.set_attrs(cell.fg, cell.bg, cell.flags, underline_color, out)?;
            write!(out, "{}", cell.c)?;
            // Zero-width combining diacritics (Kitty Unicode-placeholder-
            // mode row/column encoding, or ordinary combining marks/emoji
            // variation selectors) attach to whatever base character a
            // real terminal parser last saw — writing them immediately
            // after the base char above reproduces that attachment exactly
            // the way the original PTY output did.
            for zw in &painted.zerowidth {
                write!(out, "{zw}")?;
            }
            // Advance the virtual cursor by exactly the columns this
            // character occupies, so the next cell's `move_to` can tell
            // whether a cursor-position escape is actually needed or
            // whether writing continued naturally left-to-right.
            self.cursor_col += if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            new_grid[line_ix].push(painted);
        }

        self.last_grid = new_grid.into_iter().map(Some).collect();
        self.place_real_cursor(&content.cursor, out)
    }

    /// Emits a cursor-position escape (`CUP`) only if painting continued
    /// naturally from wherever the virtual cursor already was — the same
    /// optimization tty.c makes (sequential left-to-right writes don't need
    /// their own explicit position first).
    fn move_to<W: Write>(&mut self, point: Point, out: &mut W) -> std::io::Result<()> {
        let line = point.line.0;
        let col = point.column.0;
        if self.have_written && line == self.cursor_line && col == self.cursor_col {
            return Ok(());
        }
        // ANSI cursor positions are 1-indexed; `line` can be negative for
        // scrollback rows that never occur here (this walks
        // `renderable_content`, the visible screen only) but is kept `i32`
        // to match `Point` — clamp defensively rather than underflow.
        write!(out, "\x1b[{};{}H", (line + 1).max(1), col + 1)?;
        self.cursor_line = line;
        self.cursor_col = col;
        self.have_written = true;
        Ok(())
    }

    fn set_attrs<W: Write>(
        &mut self,
        fg: Color,
        bg: Color,
        flags: Flags,
        underline_color: Option<Color>,
        out: &mut W,
    ) -> std::io::Result<()> {
        if fg == self.fg
            && bg == self.bg
            && flags == self.flags
            && underline_color == self.underline_color
        {
            return Ok(());
        }
        // Simplest correct approach: always reset then re-apply every
        // attribute that should be set, rather than trying to compute a
        // minimal transition between two arbitrary attribute sets — tmux's
        // tty.c does something more elaborate (tracking exactly which SGR
        // bits changed to avoid a full reset+reapply), but that's a pure
        // byte-count optimization, not a correctness requirement; this is
        // simpler and still emits far less than a naive "SGR on every
        // single cell regardless of whether anything changed" approach.
        write!(out, "\x1b[0m")?;
        if flags.intersects(Flags::BOLD | Flags::DIM_BOLD) {
            write!(out, "\x1b[1m")?;
        }
        if flags.contains(Flags::DIM) {
            write!(out, "\x1b[2m")?;
        }
        if flags.contains(Flags::ITALIC) {
            write!(out, "\x1b[3m")?;
        }
        if flags.contains(Flags::UNDERLINE) {
            write!(out, "\x1b[4m")?;
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            write!(out, "\x1b[21m")?;
        } else if flags.contains(Flags::UNDERCURL) {
            write!(out, "\x1b[4:3m")?;
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            write!(out, "\x1b[4:4m")?;
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            write!(out, "\x1b[4:5m")?;
        }
        if flags.contains(Flags::INVERSE) {
            write!(out, "\x1b[7m")?;
        }
        if flags.contains(Flags::HIDDEN) {
            write!(out, "\x1b[8m")?;
        }
        if flags.contains(Flags::STRIKEOUT) {
            write!(out, "\x1b[9m")?;
        }
        write_fg(fg, out)?;
        write_bg(bg, out)?;
        // SGR 58/59 (underline color) — reset above (SGR 0) already cleared
        // any previously-set underline color, so `None` needs no explicit
        // action here; only a real `Some` needs emitting. True-color/
        // indexed-underline-color are the two forms the Unicode-placeholder-
        // mode encoding actually uses (file id via underline color, see
        // kitty_graphics_placeholder's doc comment), so both are supported
        // even though Named isn't a meaningful
        // underline color in practice.
        if let Some(color) = underline_color {
            match color {
                Color::Spec(rgb) => write_truecolor(rgb, 58, out)?,
                Color::Indexed(i) => write!(out, "\x1b[58;5;{i}m")?,
                Color::Named(_) => {},
            }
        }

        self.fg = fg;
        self.bg = bg;
        self.flags = flags;
        self.underline_color = underline_color;
        Ok(())
    }

    /// Diffed against `self.real_cursor` — an idle pane whose cursor hasn't
    /// moved since the last redraw shouldn't cost any bytes here, same as
    /// unchanged cells above cost nothing in `move_to`/`set_attrs`.
    fn place_real_cursor<W: Write>(
        &mut self,
        cursor: &alacritty_terminal::term::RenderableCursor,
        out: &mut W,
    ) -> std::io::Result<()> {
        let new_state = (cursor.point, cursor.shape);
        if self.real_cursor == Some(new_state) {
            return Ok(());
        }
        self.real_cursor = Some(new_state);

        if cursor.shape == CursorShape::Hidden {
            write!(out, "\x1b[?25l")?;
            return Ok(());
        }
        write!(out, "\x1b[?25h")?;
        write!(out, "\x1b[{};{}H", cursor.point.line.0 + 1, cursor.point.column.0 + 1)?;
        // DECSCUSR (cursor shape) — best-effort, not universally supported
        // by every terminal but alacritty (which is what Som uses to render
        // its own PTY output, including this server's) does understand it.
        let shape_code = match cursor.shape {
            CursorShape::Block => 2,
            CursorShape::Underline => 4,
            CursorShape::Beam => 6,
            CursorShape::HollowBlock => 2,
            CursorShape::Hidden => unreachable!("handled above"),
        };
        write!(out, "\x1b[{shape_code} q")?;
        // Invalidate the virtual-cursor tracking used by `move_to` — the
        // next cell write (on the next redraw) must not assume painting
        // can continue from wherever this final positioning left it.
        self.have_written = false;
        Ok(())
    }
}

fn write_fg<W: Write>(color: Color, out: &mut W) -> std::io::Result<()> {
    match color {
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::BrightForeground) => {
            write!(out, "\x1b[39m")
        }
        Color::Named(named) => write!(out, "\x1b[{}m", named_to_sgr(named, false)),
        Color::Spec(rgb) => write_truecolor(rgb, 38, out),
        Color::Indexed(i) => write!(out, "\x1b[38;5;{i}m"),
    }
}

fn write_bg<W: Write>(color: Color, out: &mut W) -> std::io::Result<()> {
    match color {
        Color::Named(NamedColor::Background) => write!(out, "\x1b[49m"),
        Color::Named(named) => write!(out, "\x1b[{}m", named_to_sgr(named, true)),
        Color::Spec(rgb) => write_truecolor(rgb, 48, out),
        Color::Indexed(i) => write!(out, "\x1b[48;5;{i}m"),
    }
}

fn write_truecolor<W: Write>(rgb: Rgb, base: u8, out: &mut W) -> std::io::Result<()> {
    write!(out, "\x1b[{base};2;{};{};{}m", rgb.r, rgb.g, rgb.b)
}

/// Maps a `NamedColor` to its base SGR code (30-37/90-97 for foreground,
/// 40-47/100-107 for background — `is_bg` picks which table). Named colors
/// without a direct SGR equivalent (the `Dim*` variants, `Cursor`,
/// `BrightForeground`/`BrightBackground`) fall back to their non-dim/non-
/// bright counterpart — losing the "dim" distinction in this fallback case
/// is a known, accepted simplification (the `Flags::DIM` attribute set
/// separately in `set_attrs` still conveys dimness for the common case
/// where a program sets SGR 2 rather than picking one of alacritty's
/// distinct "dim" `NamedColor` variants directly).
fn named_to_sgr(named: NamedColor, is_bg: bool) -> u8 {
    let base = if is_bg { 40 } else { 30 };
    let bright_base = if is_bg { 100 } else { 90 };
    match named {
        NamedColor::Black | NamedColor::DimBlack => base,
        NamedColor::Red | NamedColor::DimRed => base + 1,
        NamedColor::Green | NamedColor::DimGreen => base + 2,
        NamedColor::Yellow | NamedColor::DimYellow => base + 3,
        NamedColor::Blue | NamedColor::DimBlue => base + 4,
        NamedColor::Magenta | NamedColor::DimMagenta => base + 5,
        NamedColor::Cyan | NamedColor::DimCyan => base + 6,
        NamedColor::White | NamedColor::DimWhite => base + 7,
        NamedColor::BrightBlack => bright_base,
        NamedColor::BrightRed => bright_base + 1,
        NamedColor::BrightGreen => bright_base + 2,
        NamedColor::BrightYellow => bright_base + 3,
        NamedColor::BrightBlue => bright_base + 4,
        NamedColor::BrightMagenta => bright_base + 5,
        NamedColor::BrightCyan => bright_base + 6,
        NamedColor::BrightWhite => bright_base + 7,
        // Foreground/Background handled by the caller before this is
        // reached; Cursor/BrightForeground/BrightBackground/DimForeground
        // have no natural SGR slot — fall back to the plain default.
        _ => if is_bg { 49 } else { 39 },
    }
}

#[cfg(test)]
mod tests {
    //! Verifies the diff logic actually elides unchanged output, and that a
    //! full redraw (fresh `Redrawer`) against a real colored PTY produces
    //! bytes that, when fed back through a SECOND real `alacritty_terminal::
    //! Term`, reconstruct the same cell contents/colors — i.e. round-trips
    //! correctly through a real ANSI parser, not just "looks plausible".
    //! Real PTY, no mocking (same testing philosophy as `Session`'s own
    //! tests and `terminal_view`'s `reconnect_tests`/`spawn_race_tests`).

    use super::*;
    use crate::bounds::SessionBounds;
    use crate::session::Session;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;

    /// Cross-platform "just an interactive shell sitting at its own
    /// prompt" spawn — used by tests that only care about `Session::write`
    /// reaching an idle shell / broadcast semantics, not any
    /// Windows-specific prompt rendering. Unsets `PS1` via `env -u PS1`
    /// (Unix only) before running `sh` — `Session::spawn` inherits this
    /// test process's entire environment, and `dash` (a common `/bin/sh`)
    /// happily uses an inherited `PS1` as-is even though it's not really a
    /// `dash`-specific setting; on a machine whose OWN interactive shell
    /// has a Nerd-Font-glyph `PS1` set (as this repo's own dev machine
    /// does), that leaked straight through and made `sh`'s prompt contain
    /// no plain `$` at all, which is what this test is actually looking
    /// for (confirmed by dumping the actual redraw bytes when this test
    /// failed on WSL for what looked like a timing issue but wasn't).
    fn interactive_shell() -> (String, Vec<String>) {
        if cfg!(windows) {
            ("C:\\Windows\\System32\\cmd.exe".into(), vec![])
        } else {
            ("env".into(), vec!["-u".into(), "PS1".into(), "sh".into()])
        }
    }

    /// No PTY at all — feeds the raw prompt bytes DIRECTLY into a real VTE
    /// `Processor`/`Term`, bypassing `Session`/ConPTY/the real shell
    /// entirely. If U+F101/U+F5BA survive THIS but not the full
    /// `Session::spawn` + real `pwsh.exe` path, the corruption is happening
    /// somewhere between the real shell's actual output and what
    /// `alacritty_terminal`'s `EventLoop`/PTY reader hands to `Term` (e.g.
    /// ConPTY itself, or some encoding step in between) — NOT in `Term`'s
    /// own VTE parsing of these codepoints, and NOT in `redraw.rs`.
    #[test]
    fn raw_vte_processor_keeps_nerd_font_glyphs_with_no_pty_involved() {
        let bounds = SessionBounds::new(80, 24);
        let raw = "\x1b[36m\u{F5BA}\x1b[0m \x1b[32m~\x1b[0m \x1b[36m\u{F101}\x1b[0m".as_bytes();

        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, raw);

        let grid_text: String = term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        assert!(
            grid_text.contains('\u{F5BA}'),
            "U+F5BA should survive raw VTE parsing with no PTY involved at all — got: {grid_text:?}"
        );
        assert!(
            grid_text.contains('\u{F101}'),
            "U+F101 should survive raw VTE parsing with no PTY involved at all — got: {grid_text:?}"
        );
    }

    /// This is the REAL root cause the previous test's manual reproduction
    /// eventually traced back to (see `project_som_tmux` memory) — proven
    /// directly here rather than just indirectly through symptoms. A real
    /// HOLDER process always has AT LEAST two threads competing to read
    /// `Session`'s change-notification channel: the shell-exit watcher in
    /// `server.rs::run` (spawned once, for the session's whole life) and
    /// one redraw-forwarder per connected RELAY (`spawn_forwarder`, called
    /// fresh from every `handle_relay`). Before `Session::subscribe`
    /// existed, they all called `next_change_blocking()` against the SAME
    /// single `events_rx` field — `async_channel` is a competing-consumers
    /// queue (each message goes to exactly one waiting receiver), not a
    /// broadcast, so a `Wakeup` meant for the redraw-forwarder could be
    /// silently stolen by the unrelated exit-watcher thread instead, which
    /// does nothing with a successful result besides looping straight back
    /// around to wait for the next one. This asserts that TWO independent
    /// subscribers (mimicking that real thread topology) EACH see a
    /// `Wakeup` for the same real PTY output — with the old shared-receiver
    /// design, at most one of them ever would.
    #[test]
    fn two_independent_subscribers_each_see_every_wakeup_not_just_one_of_them() {
        let bounds = SessionBounds::new(80, 24);
        let (program, args) = interactive_shell();
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn an interactive shell for test");

        // Two subscribers, mimicking the shell-exit watcher and a redraw
        // forwarder both wanting every change — spawned BEFORE the shell
        // prints anything, so both are genuinely competing for the same
        // real events, not just independently polling stale state.
        let subscriber_a = session.subscribe();
        let subscriber_b = session.subscribe();

        // The shell prints its own startup prompt unprompted — that alone
        // is enough real PTY activity to generate at least one real Wakeup
        // for both subscribers to (correctly) both see.
        assert!(
            session.next_change_blocking(&subscriber_a),
            "subscriber A should see the shell's own startup activity as a Wakeup"
        );
        assert!(
            session.next_change_blocking(&subscriber_b),
            "subscriber B should ALSO see the same startup activity — if this hangs or fails, \
             the broadcast is only reaching one subscriber, which is exactly the bug that caused \
             a real HOLDER's redraw-forwarder to never learn the screen changed"
        );

        session.kill();
    }

    /// Reproduces a user-reported bug: leftover fragments of a full-screen
    /// TUI app (e.g. `htop`) staying visible after quitting back to a plain
    /// shell prompt, screenshotted live — text like "erver.exe" (a chopped-
    /// up remnant of "...som-tmux.exe" from the process list) and
    /// stale cyan coloring stuck on screen well after `htop` exited and the
    /// shell's own plain-colored prompt was showing underneath.
    ///
    /// Root cause: entering/exiting the alternate screen buffer (`\x1b[?
    /// 1049h`/`l`, what any full-screen TUI app uses) is a HARDWARE-level
    /// atomic swap on a real terminal — no diffing needed, the whole screen
    /// just changes. But `Redrawer::last_grid` has no concept of that swap:
    /// it's plain per-position content. A cell that happens to hold the same
    /// character both right before AND right after the swap (overwhelmingly
    /// likely for a mostly-blank-space screen, or for any character that
    /// just happens to coincide at that position across two logically
    /// unrelated screens) gets silently skipped by the diff — leaving it
    /// exactly as it was, with the OLD colors too, even though the
    /// underlying `Term` correctly switched to a completely different
    /// buffer.
    ///
    /// Deliberately drives `Term`/`Redrawer` directly (no real PTY/`Session`)
    /// so the exact bytes are controlled precisely, rather than depending on
    /// a real `htop`/`micro` binary being installed wherever this test runs.
    #[test]
    fn redraw_repaints_the_whole_screen_after_an_alt_screen_swap_even_where_content_coincides() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        let mut redrawer = Redrawer::new();

        // Primary screen starts with just a bare prompt on line 0 — nothing
        // ever gets printed at, say, column 20 on line 5, so that cell stays
        // whatever a freshly-cleared cell is: a plain, uncolored space.
        parser.advance(&mut term, b"PS>");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("initial redraw should succeed");

        // Enter the alternate screen (mode 1049h) and draw cyan text on line
        // 5 — standing in for htop's own colored process-list text — that
        // does NOT reach all the way to column 20, so column 20 on line 5 is
        // ALSO a plain, uncolored space in the alt screen: same character,
        // same (lack of) color as the primary screen's untouched cell at the
        // exact same position, purely by coincidence.
        parser.advance(&mut term, "\x1b[?1049h\x1b[6;1H\x1b[36merver.exe\x1b[0m".as_bytes());
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("alt-screen-entry redraw should succeed");

        // A SECOND alt-screen redraw with DIFFERENT text on line 5 (as if
        // htop just re-rendered its process list with different content) —
        // this is what actually leaves `last_grid` holding alt-screen
        // content at the moment of exit, exercising the realistic "several
        // redraws happened while in the alt screen" case rather than just
        // the single entry redraw.
        parser.advance(&mut term, "\x1b[6;1H\x1b[K\x1b[36maltScreenService.\x1b[0m".as_bytes());
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("second alt-screen redraw should succeed");

        // Exit the alternate screen (mode 1049l) — the real primary screen
        // never had anything on line 5 at all, so with the bug, every cell
        // there that happens to still read as a plain uncolored space (true
        // for most of the line, since "altScreenService." is short) gets
        // skipped by the diff — leaving the alt-screen app's leftover text
        // fragments and stale cyan visible right where the empty primary
        // screen should be.
        parser.advance(&mut term, b"\x1b[?1049l");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("alt-screen-exit redraw should succeed");

        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut target_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        target_parser.advance(&mut target_term, &bytes);

        let visible_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        assert!(
            !visible_text.contains("Service") && !visible_text.contains("erver.exe"),
            "no fragment of the alt-screen app's leftover text should survive on the primary screen after exiting — \
             this is exactly the reported 'stale htop text stuck on screen after quitting' bug: got {visible_text:?}"
        );

        let stale_cyan = target_term
            .renderable_content()
            .display_iter
            .any(|indexed| indexed.cell.fg == Color::Named(NamedColor::Cyan) || indexed.cell.fg == Color::Named(NamedColor::BrightCyan));
        assert!(
            !stale_cyan,
            "no cell should still be showing the alt-screen app's cyan coloring after exiting back to the plain primary screen"
        );
    }

    /// Regression test for a real user report ("не работают F-клавиши в
    /// терминалах wsl/ssh"): `redraw`'s grid-diffing (mirroring tmux's
    /// `tty.c` model — see this module's doc comment) only ever replays
    /// VISIBLE cell content, never mode-switching escape codes, since those
    /// carry no on-screen content of their own. That's correct for
    /// everything that actually painted a cell, but DECCKM (`\x1b[?1h`/
    /// `\x1b[?1l`, Application Cursor Keys mode — what `htop`/`vim`/`less`
    /// and most full-screen TUI apps enable) controls nothing about what's
    /// ON screen, only how the client (Som, via `mappings::keys::
    /// to_esc_str`) should encode the user's NEXT arrow-key/Home/End
    /// press. Left unforwarded, the client-side `Term` never learns the
    /// real app enabled it, so Som keeps sending non-application-mode
    /// sequences (`\x1b[A`) that many such apps don't recognize as arrow
    /// keys at all.
    ///
    /// Deliberately drives `Term`/`Redrawer` directly (no real PTY/
    /// `Session`), same reasoning as the alt-screen tests above.
    #[test]
    fn redraw_forwards_decckm_app_cursor_mode_changes_to_the_client() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        let mut redrawer = Redrawer::new();

        // A bare prompt, app cursor mode not yet enabled — the very first
        // redraw should already reflect that (explicitly, not just by
        // omission), since a freshly (re)attaching client has no other way
        // to know the CURRENT mode either.
        parser.advance(&mut term, b"PS>");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("initial redraw should succeed");

        let mut client_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut client_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            !client_term.mode().contains(alacritty_terminal::term::TermMode::APP_CURSOR),
            "client should start without APP_CURSOR set, matching the real shell's own initial state"
        );

        // The real app (e.g. htop) enables application cursor keys.
        parser.advance(&mut term, b"\x1b[?1h");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("redraw after enabling DECCKM should succeed");
        assert!(
            bytes.windows(5).any(|w| w == b"\x1b[?1h"),
            "redraw should have forwarded the literal DECCKM-enable sequence — got bytes: {bytes:?}"
        );
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            client_term.mode().contains(alacritty_terminal::term::TermMode::APP_CURSOR),
            "client-side Term should have APP_CURSOR set after redraw forwarded the real app's DECCKM-enable — \
             this is the exact bug: without forwarding it, Som keeps encoding arrow keys in the WRONG mode for \
             apps like htop/vim/less that rely on APP_CURSOR being correctly mirrored"
        );

        // The app exits, or otherwise disables it again.
        parser.advance(&mut term, b"\x1b[?1l");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("redraw after disabling DECCKM should succeed");
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            !client_term.mode().contains(alacritty_terminal::term::TermMode::APP_CURSOR),
            "client-side Term should have APP_CURSOR cleared again after redraw forwarded the real app's DECCKM-disable"
        );
    }

    /// Same category of bug as the DECCKM test above, but for mouse
    /// tracking — confirmed root cause of a real user report: a `tmux:
    /// true` tab's mouse worked fine on a connection that never dropped
    /// (those bytes go through the ordinary live-forwarding path, see
    /// `crate::server::spawn_forwarder`), but stopped working ENTIRELY —
    /// clicks in `yazi` did nothing — specifically after a (re)attach,
    /// because `Redrawer::redraw` never re-emitted the mouse-mode DECSET
    /// bytes a (re)connecting RELAY's fresh `Snapshot` needs, even though
    /// the real app (`yazi`, on its own first-ever startup, well before
    /// this particular reconnect) had already enabled mouse mode long ago.
    #[test]
    fn redraw_forwards_mouse_mode_changes_to_the_client() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        let mut redrawer = Redrawer::new();

        parser.advance(&mut term, b"PS>");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("initial redraw should succeed");

        let mut client_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut client_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            !client_term.mode().contains(alacritty_terminal::term::TermMode::MOUSE_MODE),
            "client should start without mouse mode set, matching the real shell's own initial state"
        );

        // yazi's own startup sequence: click + drag + urxvt + SGR encoding,
        // all together in one write (see yazi-tty's `EnableMouseCapture`).
        // Mouse-tracking sub-modes are mutually exclusive in alacritty's own
        // `Handler::set_private_mode` (each one clears `MOUSE_MODE` before
        // setting its own bit — see that function's "Mouse protocols are
        // mutually exclusive" comment) — yazi's four SETs together end up
        // with only the LAST one, `?1002` (`MOUSE_DRAG`), actually active,
        // not all three `MOUSE_MODE` bits at once. `.intersects`, not
        // `.contains`, is the right check here — any ONE of them being on
        // is what makes `Terminal::mouse_mode` (`self.last_content.mode.
        // intersects(TermMode::MOUSE_MODE)`) start forwarding clicks.
        parser.advance(&mut term, b"\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("redraw after enabling mouse mode should succeed");
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            client_term.mode().intersects(alacritty_terminal::term::TermMode::MOUSE_MODE),
            "client-side Term should have mouse mode set after redraw forwarded yazi's mouse-enable — this is \
             the exact bug: without forwarding it, Som's `Terminal::mouse_mode` never returns true, so clicks \
             never get forwarded to the PTY at all, even though the real app IS listening for them"
        );
        assert!(
            client_term.mode().contains(alacritty_terminal::term::TermMode::SGR_MOUSE),
            "client-side Term should also have picked up SGR coordinate encoding specifically"
        );

        // Mouse mode disabled again (e.g. app exits back to a plain shell).
        parser.advance(&mut term, b"\x1b[?1006l\x1b[?1015l\x1b[?1002l\x1b[?1000l");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("redraw after disabling mouse mode should succeed");
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            !client_term.mode().intersects(alacritty_terminal::term::TermMode::MOUSE_MODE),
            "client-side Term should have mouse mode cleared again after redraw forwarded the real app's disable"
        );
    }

    /// The specific scenario from the real bug report: mouse mode was
    /// already ON (the real app enabled it long before this particular
    /// (re)connect — e.g. `yazi` was already running, minutes earlier,
    /// when the RELAY dropped and reconnected) — so `redraw`'s very FIRST
    /// call for this fresh `Redrawer` (mirroring what a genuinely fresh
    /// `Snapshot`-triggered redraw looks like on reconnect, not the
    /// incremental-update case the two tests above cover) has to notice
    /// mouse mode is ALREADY set and forward it, not just react to a
    /// FLIP — there's no earlier `redraw` call for a flip to be relative
    /// to.
    #[test]
    fn redraw_forwards_already_enabled_mouse_mode_on_the_very_first_call() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();

        // Mouse mode enabled BEFORE the first redraw ever runs — exactly
        // what a HOLDER's long-lived headless `Term` looks like when a
        // fresh RELAY (re)connects well after `yazi` already started.
        parser.advance(&mut term, b"\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h");

        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("first redraw should succeed");

        let mut client_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut client_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        client_parser.advance(&mut client_term, &bytes);
        assert!(
            client_term.mode().intersects(alacritty_terminal::term::TermMode::MOUSE_MODE),
            "the very first redraw against a HOLDER that already has mouse mode on must forward it — this is \
             the real (re)connect bug: a client that only reacts to a mode FLIP relative to a prior redraw call \
             never notices a mode that was already on before this Redrawer even existed, got bytes: {bytes:?}"
        );
    }

    /// A second, narrower reproduction of the same alt-screen bug family,
    /// targeting the virtual write-cursor specifically (`cursor_line`/
    /// `cursor_col`, consulted by `move_to`'s "painting continued naturally,
    /// skip the explicit CUP" optimization) rather than `last_grid`. Text
    /// appearing at scrambled, seemingly-random screen positions (exactly
    /// what the user's screenshots showed: fragments like "xe" / "exe" sitting
    /// alone on otherwise-blank lines, unrelated to any real command output)
    /// is the signature of this specific half of the bug: the first cell
    /// painted right after an alt-screen swap happens to sit at the exact
    /// (line, column) the virtual cursor was already "at" from the OTHER
    /// buffer's last redraw, so `move_to` wrongly elides the CUP escape —
    /// but the REAL cursor (on Som's side, watching these bytes) is still
    /// sitting wherever the old buffer's redraw actually left it, so the
    /// character lands in the wrong place on the real screen even though
    /// this crate's own bookkeeping thinks it's exactly where it should be.
    #[test]
    fn redraw_repositions_the_cursor_explicitly_right_after_an_alt_screen_swap() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        let mut redrawer = Redrawer::new();

        // Primary screen: paint 'x' at (line 10, col 5), THEN do a second,
        // separate redraw that emits nothing else — this leaves the virtual
        // write-cursor sitting at exactly (line 10, col 5) with `have_written
        // = true`, but with no bytes generated by THIS redraw call to
        // accidentally satisfy the assertion below on their own.
        parser.advance(&mut term, "\x1b[11;6Hx".as_bytes());
        let mut warmup = Vec::new();
        redrawer.redraw(&term, &mut warmup).expect("warmup redraw should succeed");
        let mut settle = Vec::new();
        redrawer.redraw(&term, &mut settle).expect("settle redraw should succeed");
        assert!(settle.is_empty(), "sanity: nothing changed, the settle redraw should emit nothing");

        // Enter the alt screen and paint a character at that SAME position,
        // (line 10, col 5) — under the bug, `move_to` sees this matches the
        // virtual cursor's last-known position and skips the CUP, even
        // though the buffer swap means the REAL cursor (Som's own, watching
        // these bytes land) is nowhere near there anymore.
        parser.advance(&mut term, "\x1b[?1049h\x1b[11;6Hy".as_bytes());
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("alt-screen redraw should succeed");

        // Reparse into a fresh, independent `Term` — a REAL terminal, not
        // this crate's own (possibly-buggy) bookkeeping — and check where
        // 'y' actually landed. This is what the previous version of this
        // test got wrong: whether `move_to` happened to elide an explicit
        // CUP escape for 'y' specifically is an implementation detail (a
        // full repaint after the swap — this fix's OTHER half, via
        // `last_grid.clear()` — naturally writes every preceding blank cell
        // on the way there, making an explicit CUP for 'y' itself
        // unnecessary AND correct); what actually matters is that a real
        // terminal parsing these bytes ends up with 'y' in the right place.
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut target_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        target_parser.advance(&mut target_term, &bytes);
        let y_cell = target_term
            .renderable_content()
            .display_iter
            .find(|indexed| indexed.cell.c == 'y');
        assert_eq!(
            y_cell.map(|indexed| (indexed.point.line.0, indexed.point.column.0)),
            Some((10, 5)),
            "'y' must land at (line 10, col 5) on a real terminal parsing these bytes — a wrong position here \
             is exactly the reported 'text scattered at scrambled positions after using htop/micro' bug"
        );
    }

    /// Same bug family as the alt-screen swap tests above, one more trigger:
    /// a resize reflows every existing row to the new width (`Term::resize`
    /// re-wraps long lines), so position (row, col) can hold completely
    /// different, unrelated content before vs. after — `last_grid`'s plain
    /// per-position comparison has no way to know that, and can wrongly
    /// treat a post-resize cell as "unchanged" just because it happens to
    /// hold the same character the pre-resize cell at that exact spot did.
    #[test]
    fn redraw_repaints_the_whole_screen_after_a_resize_even_where_content_coincides() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        let mut redrawer = Redrawer::new();

        // A long line that will reflow differently once the width shrinks.
        parser.advance(&mut term, b"the quick brown fox jumps over the lazy dog");
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("initial redraw should succeed");

        // Shrink to a much narrower width — this forces `Term` to re-wrap
        // the line above across multiple rows, shifting most of its
        // characters to different (row, col) positions than before.
        let narrow_bounds = SessionBounds::new(10, 24);
        term.resize(narrow_bounds);
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("post-resize redraw should succeed");

        let mut target_term = Term::new(Config::default(), &narrow_bounds, VoidListener);
        let mut target_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        target_parser.advance(&mut target_term, &bytes);

        let mut ground_truth = Redrawer::new();
        let mut ground_truth_bytes = Vec::new();
        ground_truth.redraw(&term, &mut ground_truth_bytes).expect("ground-truth full redraw should succeed");
        let mut ground_truth_term = Term::new(Config::default(), &narrow_bounds, VoidListener);
        let mut ground_truth_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        ground_truth_parser.advance(&mut ground_truth_term, &ground_truth_bytes);

        let replayed_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        let ground_truth_text: String =
            ground_truth_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        assert_eq!(
            replayed_text, ground_truth_text,
            "the incremental redraw right after a resize must match a fresh full redraw taken at the same moment — \
             a mismatch here means reflowed text is being scrambled by a diff that trusted stale coordinates"
        );
    }

    /// A Unicode-placeholder-mode cell (see
    /// `crates/terminal/src/kitty_graphics_placeholder.rs`'s doc comment —
    /// the encoding Som's own rich-content protocol uses to represent image
    /// placements as real grid text) is a base char (`U+10EEEE`) plus
    /// zero-width combining diacritics (row/column) plus a distinct
    /// underline color (file id) on top of a true-color foreground (session
    /// id). None of those three pieces are ordinary `fg`/`bg`/`flags` —
    /// before `PaintedCell` tracked `zerowidth`/`underline_color`, `redraw`
    /// would have emitted the base char with its true-color fg (matching
    /// the pre-existing `write_fg` path) but silently dropped the
    /// diacritics AND the underline color, corrupting every placeholder
    /// cell's encoded row/column/id on the very first
    /// resize/alt-screen-swap/reconnect redraw.
    #[test]
    fn redraw_preserves_placeholder_diacritics_and_underline_color() {
        let bounds = SessionBounds::new(80, 24);
        let mut term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();

        // fg = true-color image id (0x00123456), underline color = true-color
        // placement id (0x00000001), base char + two combining diacritics
        // (row 0 = U+0305, column 1 = U+030D) — a realistic placeholder cell
        // as some other program (not Som itself) would have written it.
        let raw = "\x1b[38;2;18;52;86m\x1b[58;2;0;0;1m\u{10EEEE}\u{0305}\u{030D}\x1b[0m".as_bytes();
        parser.advance(&mut term, raw);

        // Sanity: the source Term itself actually captured all three pieces
        // — otherwise this test would trivially pass no matter what
        // `redraw` does, having proven nothing.
        let source_cell = term
            .renderable_content()
            .display_iter
            .find(|indexed| indexed.cell.c == '\u{10EEEE}')
            .expect("source Term should contain the placeholder cell")
            .cell
            .clone();
        assert_eq!(source_cell.underline_color(), Some(Color::Spec(Rgb { r: 0, g: 0, b: 1 })));
        assert_eq!(source_cell.zerowidth(), Some(&['\u{0305}', '\u{030D}'][..]));

        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        redrawer.redraw(&term, &mut bytes).expect("redraw should succeed");

        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut target_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        target_parser.advance(&mut target_term, &bytes);

        let replayed_cell = target_term
            .renderable_content()
            .display_iter
            .find(|indexed| indexed.cell.c == '\u{10EEEE}')
            .expect("redrawn output should still contain the placeholder cell")
            .cell
            .clone();
        assert_eq!(
            replayed_cell.fg,
            Color::Spec(Rgb { r: 18, g: 52, b: 86 }),
            "image id (encoded via true-color fg) must survive redraw"
        );
        assert_eq!(
            replayed_cell.underline_color(),
            Some(Color::Spec(Rgb { r: 0, g: 0, b: 1 })),
            "placement id (encoded via underline color) must survive redraw"
        );
        assert_eq!(
            replayed_cell.zerowidth(),
            Some(&['\u{0305}', '\u{030D}'][..]),
            "row/column diacritics must survive redraw"
        );
    }

}
