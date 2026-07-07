//! Serializes an `alacritty_terminal` grid back into ANSI escape sequences,
//! the way tmux's `tty.c` redraws a client's screen from its own internal
//! `grid.c` state rather than replaying raw bytes from the child process.
//!
//! Why this exists at all (see `project_som_tmux` memory, "Обновление 16"
//! for the full history): `som-tmux-server` needs to be a transparent PTY
//! proxy — Som's own `TerminalElement`/`TerminalView` must never change, so
//! whatever `som-tmux-server` writes to its own stdout has to be plain ANSI
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

use alacritty_terminal::index::Point;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use std::io::Write;

/// One cell's content/appearance, as tracked in `Redrawer`'s per-position
/// history — deliberately a plain tuple-like struct rather than reusing
/// `alacritty_terminal::term::cell::Cell` (which carries an `Option<Arc<
/// CellExtra>>` this diff doesn't need, same reasoning as `protocol::
/// ProtocolCell` — see that type's doc comment).
#[derive(Clone, Copy, PartialEq)]
struct PaintedCell {
    c: char,
    fg: Color,
    bg: Color,
    flags: Flags,
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
        }
    }

    /// Forces the next `redraw` call to behave like a fresh `Redrawer` —
    /// used when a new client attaches to an already-running session and
    /// needs the full current screen, not just whatever's changed since the
    /// last client's last redraw (each client connection gets its own
    /// `Redrawer`, so this is mostly for clarity/explicit intent rather than
    /// a shared-state concern).
    pub fn reset(&mut self) {
        *self = Self::new();
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
        let content = term.renderable_content();
        let mut new_grid: Vec<Vec<PaintedCell>> = Vec::new();

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = &indexed.cell;
            let line_ix = point.line.0.max(0) as usize;
            while new_grid.len() <= line_ix {
                new_grid.push(Vec::new());
            }
            let painted = PaintedCell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags };
            new_grid[line_ix].push(painted);

            // Wide-char spacers are placeholders for the second column of a
            // double-width character — the actual character was already
            // written for the first column; writing the spacer's (blank)
            // `c` here would blank out half of every wide character. Still
            // recorded into `new_grid` above (a real cell occupies that
            // position and must be diffed against next time), just never
            // painted on its own.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

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
                .and_then(|row| row.get(new_grid[line_ix].len() - 1))
                .copied();
            if previously_painted == Some(painted) {
                continue;
            }

            self.move_to(point, out)?;
            self.set_attrs(cell.fg, cell.bg, cell.flags, out)?;
            write!(out, "{}", cell.c)?;
            // Advance the virtual cursor by exactly the columns this
            // character occupies, so the next cell's `move_to` can tell
            // whether a cursor-position escape is actually needed or
            // whether writing continued naturally left-to-right.
            self.cursor_col += if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
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

    fn set_attrs<W: Write>(&mut self, fg: Color, bg: Color, flags: Flags, out: &mut W) -> std::io::Result<()> {
        if fg == self.fg && bg == self.bg && flags == self.flags {
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

        self.fg = fg;
        self.bg = bg;
        self.flags = flags;
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

    #[test]
    fn redraw_round_trips_colored_text_through_a_real_parser() {
        let bounds = SessionBounds::new(80, 24);
        let command = "echo off & echo \x1b[92mgreentext\x1b[0m & timeout /T 3600";
        let session = Session::spawn(
            "C:\\Windows\\System32\\cmd.exe".into(),
            vec!["/C".into(), command.into()],
            None,
            bounds,
        )
        .expect("failed to spawn cmd.exe for test");

        // Wait for the real output to land, same polling approach as
        // Session's own test.
        let mut ready = false;
        for _ in 0..50 {
            if session
                .snapshot()
                .rows
                .iter()
                .any(|row| row.iter().any(|c| c.c == 'g'))
            {
                ready = true;
                break;
            }
            if !session.next_change_blocking() {
                break;
            }
        }
        assert!(ready, "'greentext' never appeared in the source session's grid");

        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed writing to a Vec<u8>");

        // Feed the redrawn bytes into a brand-new, independent Term via a
        // real VTE parser — this is the actual "does a real terminal
        // emulator agree with what we generated" check, not just "did we
        // produce SOME bytes".
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &bytes);

        let target_text: String = target_term
            .renderable_content()
            .display_iter
            .map(|indexed| indexed.cell.c)
            .collect();
        assert!(
            target_text.contains("greentext"),
            "redrawn bytes, reparsed by a fresh Term, should contain the echoed text; got grid text: {target_text:?}"
        );

        let green_cell = target_term
            .renderable_content()
            .display_iter
            .find(|indexed| indexed.cell.c == 'g' && indexed.cell.fg != Color::Named(NamedColor::Foreground))
            .expect("reparsed grid should have a non-default-colored 'g' cell");
        assert_eq!(
            green_cell.cell.fg,
            Color::Named(NamedColor::BrightGreen),
            "color should survive the redraw -> reparse round trip"
        );

        session.kill();
    }

    #[test]
    fn redraw_after_no_change_emits_nothing_new() {
        let bounds = SessionBounds::new(80, 24);
        let session = Session::spawn(
            "C:\\Windows\\System32\\cmd.exe".into(),
            vec!["/C".into(), "echo off & echo idle & timeout /T 3600".into()],
            None,
            bounds,
        )
        .expect("failed to spawn cmd.exe for test");

        for _ in 0..50 {
            if session.snapshot().rows.iter().any(|row| row.iter().any(|c| c.c == 'i')) {
                break;
            }
            if !session.next_change_blocking() {
                break;
            }
        }

        let mut redrawer = Redrawer::new();
        let mut first = Vec::new();
        session.redraw(&mut redrawer, &mut first).expect("redraw should succeed writing to a Vec<u8>");
        assert!(!first.is_empty(), "first redraw against a fresh Redrawer should emit the whole screen");

        // Redrawing again with the SAME Redrawer (same state, nothing in
        // the grid changed) should emit nothing but cursor re-placement —
        // this is the whole point of diffing: an idle pane shouldn't cost
        // anything to "redraw" repeatedly.
        let mut second = Vec::new();
        session.redraw(&mut redrawer, &mut second).expect("redraw should succeed writing to a Vec<u8>");
        assert!(
            second.len() < first.len() / 2,
            "redrawing unchanged content should emit far less than the first full redraw; first={} second={}",
            first.len(),
            second.len()
        );

        session.kill();
    }
}
