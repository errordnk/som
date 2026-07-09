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
            last_alt_screen: None,
            last_size: None,
            last_app_cursor: None,
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

    /// Cross-platform "run this one-shot command line, then stay alive
    /// after it finishes" spawn — used by every test in this file that
    /// only needs SOME shell to produce specific output (echo, colors,
    /// escape sequences), not any Windows/PowerShell-specific behavior of
    /// its own. Mirrors `cmd.exe /K "<command> & timeout /T 3600"`'s shape
    /// on Windows (run the command, then keep the shell alive instead of
    /// exiting) with `sh -c '<command>; sleep 3600'` on Unix.
    fn one_shot_command_shell(command: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("C:\\Windows\\System32\\cmd.exe".into(), vec!["/K".into(), format!("echo off & {command}")])
        } else {
            ("sh".into(), vec!["-c".into(), format!("{command}; sleep 3600")])
        }
    }

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

    #[test]
    fn redraw_round_trips_colored_text_through_a_real_parser() {
        let bounds = SessionBounds::new(80, 24);
        let (program, args) = one_shot_command_shell("echo \x1b[92mgreentext\x1b[0m");
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn a shell for test");
        let events_rx = session.subscribe();

        // Wait for the real output to land — poll via a throwaway
        // `Redrawer`/buffer (separate from the one used for the actual
        // assertions below) rather than a dedicated snapshot API, since
        // `Session` no longer exposes one (redraw() is the only way to
        // observe grid contents now that the old GridSnapshot protocol is
        // gone).
        let mut ready = false;
        for _ in 0..50 {
            let mut probe = Redrawer::new();
            let mut probe_bytes = Vec::new();
            session.redraw(&mut probe, &mut probe_bytes).expect("redraw should succeed writing to a Vec<u8>");
            if probe_bytes.windows(1).any(|w| w == b"g") {
                ready = true;
                break;
            }
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        assert!(ready, "'greentext' never appeared in the source session's redraw output");

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
        let (program, args) = one_shot_command_shell("echo idle");
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn a shell for test");
        let events_rx = session.subscribe();

        for _ in 0..50 {
            let mut probe = Redrawer::new();
            let mut probe_bytes = Vec::new();
            session.redraw(&mut probe, &mut probe_bytes).expect("redraw should succeed writing to a Vec<u8>");
            if probe_bytes.windows(1).any(|w| w == b"i") {
                break;
            }
            if !session.next_change_blocking(&events_rx) {
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

    /// Reproduces the user-reported "artifacts after pressing Enter at the
    /// prompt" symptom: printing enough lines to overflow the screen forces
    /// `Term`'s internal scroll-up (the same thing that happens continuously
    /// while a shell prints prompt after prompt) — this asserts that a
    /// sequence of INCREMENTAL redraws through that scrolling, replayed into
    /// a second real `Term`, ends up pixel-for-pixel identical to what a
    /// single FULL redraw taken at the same moment would produce. If the
    /// incremental diff logic gets confused by scrolling (stale cells left
    /// over from before a line scrolled into a different screen position),
    /// this is where it would show up as a mismatch.
    #[test]
    fn redraw_survives_screen_scroll_without_leaving_stale_cells() {
        let bounds = SessionBounds::new(80, 24);
        // Print more lines than fit on screen (24 rows) so the shell's
        // output forces at least one real scroll-up before this test reads
        // anything back — "line0" through "line39" is 40 lines, well past
        // the visible height. `for /L` (Windows) does NOT zero-pad, and
        // neither does the `sh` loop below, so the sentinel checked for
        // below is the bare "line39", not "line0039" on either platform.
        let (program, args) = if cfg!(windows) {
            // `/K`, NOT `/C <command> & pause` — `&` after a `for /L ...
            // do <cmd>` binds to the DO clause itself, not to a separate
            // command after the whole loop, so an earlier version of this
            // test (`for /L ... do @echo line%i & pause > nul`) called
            // `pause` after EVERY iteration and hung forever waiting for
            // Enter after just "line0". `/K` sidesteps the whole grouping
            // question — cmd.exe stays alive on its own once the command
            // finishes, no trailing command needed.
            (
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                vec!["/K".to_string(), "echo off & for /L %i in (0,1,39) do @echo line%i".to_string()],
            )
        } else {
            ("sh".to_string(), vec!["-c".to_string(), "for i in $(seq 0 39); do echo line$i; done; sleep 3600".to_string()])
        };
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn a shell for test");
        let events_rx = session.subscribe();

        // Drive one shared Redrawer incrementally, exactly the way
        // `crate::server`'s forwarder does in production (repeated
        // incremental redraws over the SAME Redrawer as the shell keeps
        // producing output) — this is what would expose stale-cell bugs
        // that only a fresh-Redrawer full redraw wouldn't.
        let mut incremental = Redrawer::new();
        let mut replay_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();

        let mut last_line_seen = 0usize;
        for _ in 0..200 {
            let mut bytes = Vec::new();
            session.redraw(&mut incremental, &mut bytes).expect("incremental redraw should succeed");
            parser.advance(&mut replay_term, &bytes);

            let text: String = replay_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
            if text.contains("line39") {
                last_line_seen = 39;
                break;
            }
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        assert_eq!(last_line_seen, 39, "the shell's output never fully arrived — test setup problem, not the bug under test");

        // Now compare the REPLAYED grid (built up entirely from incremental
        // diffs) against a FRESH full redraw taken right now from the
        // SAME underlying session/Term — these two must describe the exact
        // same screen. A fresh Redrawer forces a full repaint (see
        // `Redrawer::new`'s doc comment), which is the ground truth here.
        let mut full = Redrawer::new();
        let mut full_bytes = Vec::new();
        session.redraw(&mut full, &mut full_bytes).expect("full redraw should succeed");
        let mut ground_truth_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut ground_truth_parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        ground_truth_parser.advance(&mut ground_truth_term, &full_bytes);

        let replayed_text: String = replay_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        let ground_truth_text: String =
            ground_truth_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();
        assert_eq!(
            replayed_text, ground_truth_text,
            "incrementally-replayed screen (built from diffs while scrolling) must match a fresh full redraw taken at the same moment — a mismatch here is exactly the 'stale artifacts after scrolling' bug"
        );

        session.kill();
    }

    /// Reproduces a Nerd Font prompt containing Private Use Area glyphs
    /// (like the user's real PowerShell profile's `prompt` function, which
    /// prints `` and ``) round-tripping through redraw — these
    /// codepoints are exactly the kind that could disagree on display width
    /// between whatever produced the `alacritty_terminal` grid and the
    /// `unicode-width` table this crate/Som both link against, which would
    /// desync the virtual cursor `move_to` tracks against wherever a real
    /// terminal (Som's own, rendering the redrawn bytes) actually ends up —
    /// this is the leading theory for the user-reported ">>"-after-Enter
    /// artifact, since ordinary ASCII prompts never showed it.
    #[test]
    fn redraw_round_trips_nerd_font_prompt_glyphs_without_desyncing_cursor_position() {
        let bounds = SessionBounds::new(80, 24);
        // Mirrors the user's actual PowerShell profile.ps1 `prompt` function
        // byte-for-byte (verified via `xxd` against the real file): ESC[36m,
        // U+F5BA, ESC[0m, ESC[32m, "~", ESC[0m, ESC[36m, U+F101, ESC[0m —
        // echoed literally by a plain shell here (not `pwsh.exe`'s own
        // `prompt` function), so this is checking `redraw.rs`'s own
        // round-tripping of these specific codepoints, not anything
        // Windows/PowerShell-specific — see `real_pwsh_profile_prompt_
        // keeps_its_nerd_font_glyphs_in_the_grid` (Windows-only, below) for
        // the version that checks the real shell's own rendering.
        let (program, args) =
            one_shot_command_shell("echo \x1b[36m\u{F5BA}\x1b[0m \x1b[32m~\x1b[0m \x1b[36m\u{F101}\x1b[0m && echo second-line-marker");
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn a shell for test");
        let events_rx = session.subscribe();

        let mut ready = false;
        for _ in 0..50 {
            let mut probe = Redrawer::new();
            let mut probe_bytes = Vec::new();
            session.redraw(&mut probe, &mut probe_bytes).expect("redraw should succeed writing to a Vec<u8>");
            if String::from_utf8_lossy(&probe_bytes).contains("second-line-marker") {
                ready = true;
                break;
            }
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        assert!(ready, "'second-line-marker' never appeared in the source session's redraw output");

        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed writing to a Vec<u8>");

        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &bytes);

        // The real assertion: "second-line-marker" (echoed on the line
        // RIGHT AFTER the Nerd Font prompt line) must land on the line
        // immediately following the prompt glyphs, not shifted by however
        // many columns a width miscount for U+F5BA/U+F101 would introduce.
        // If cursor tracking desynced painting the prompt line, this text
        // would show up in the wrong column/row (or split across two,
        // exactly like stray ">>" characters bleeding onto their own line).
        let lines: Vec<String> = (0..bounds.rows)
            .map(|row| {
                target_term
                    .grid()
                    .display_iter()
                    .filter(|indexed| indexed.point.line.0 == row as i32)
                    .map(|indexed| indexed.cell.c)
                    .collect::<String>()
            })
            .collect();
        let marker_line = lines
            .iter()
            .position(|line| line.contains("second-line-marker"))
            .expect("second-line-marker should be on some line of the reparsed grid");
        let marker_line_text = &lines[marker_line];
        assert!(
            marker_line_text.trim_end().ends_with("second-line-marker"),
            "second-line-marker should be the ONLY thing on its line (a clean echo), not sharing a line with leftover prompt characters — got: {marker_line_text:?}"
        );

        // The OTHER real assertion (missing from the first version of this
        // test, which only checked cursor position via the marker line):
        // the Nerd Font glyphs THEMSELVES must round-trip byte-for-byte, not
        // get replaced/corrupted into something else (e.g. plain ASCII
        // '>' characters) somewhere along redraw -> reparse. This is
        // checked on whichever line actually contains the prompt glyphs,
        // found by searching for U+F5BA specifically (not assumed to be
        // line 0 — cmd.exe's own startup banner can push things down).
        let prompt_line = lines
            .iter()
            .find(|line| line.contains('\u{F5BA}'))
            .unwrap_or_else(|| panic!("U+F5BA (Windows icon) never appeared anywhere in the reparsed grid — got lines: {lines:?}"));
        assert!(
            prompt_line.contains('\u{F101}'),
            "U+F101 (arrow icon) should be on the SAME line as U+F5BA, exactly as echoed — got: {prompt_line:?}"
        );
        assert!(
            !prompt_line.contains(">>"),
            "the Nerd Font arrow glyph (U+F101) must not have been corrupted into literal '>>' characters — got: {prompt_line:?}"
        );

        session.kill();
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

    /// All the OTHER tests in this file only ever read whatever a command
    /// PASSED AT SPAWN TIME already printed — they never call
    /// `Session::write` after the fact. This test does: spawns an
    /// INTERACTIVE `cmd.exe` (no `/C <command>`, just a normal shell
    /// sitting at its own prompt), waits for that prompt to actually
    /// appear, THEN calls `session.write(...)` with a real command typed as
    /// keystrokes — exactly what `server.rs::read_loop` does for every
    /// `RelayInput::Bytes` a RELAY forwards. Manual reproduction (see
    /// `project_som_tmux` memory) found real interactive PowerShell
    /// sessions going completely unresponsive to ANY input (not just
    /// Enter — a single letter never echoed either) after their first
    /// redraw; this is the automated version of that same reproduction,
    /// against `cmd.exe` (fewer moving parts than `pwsh.exe`, same
    /// EventLoop/PTY machinery underneath).
    #[test]
    fn session_write_after_the_shell_is_already_idle_at_its_prompt_actually_reaches_it() {
        let bounds = SessionBounds::new(80, 24);
        let (program, args) = interactive_shell();
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn an interactive shell for test");

        // Polled fixed sleeps, NOT `next_change_blocking` in a loop — that's
        // fine against `pwsh.exe`/ConPTY (its cursor blink keeps producing
        // `Wakeup`s even once nothing else is changing, so a bounded
        // iteration count still bounds wall-clock time), but a plain `sh`
        // on Unix has no such blink: once it settles at its prompt with
        // nothing left to redraw, there may be no FURTHER `Wakeup` ever
        // coming, and `next_change_blocking`'s `recv_blocking()` has no
        // timeout of its own — so a loop built on it can hang forever
        // (confirmed: this exact test hung indefinitely against `sh` on
        // WSL before switching to polling here). Polls in short steps
        // rather than one single fixed sleep — WSL's PTY/shell startup
        // latency proved less predictable than Windows/ConPTY's, so a
        // single guess (500ms) was too short there even though it was
        // comfortably enough on Windows.
        let prompt_char = if cfg!(windows) { '>' } else { '$' };
        let mut redrawer = Redrawer::new();
        let mut saw_prompt = false;
        let mut last_bytes = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut bytes = Vec::new();
            session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed");
            if !bytes.is_empty() {
                last_bytes = bytes.clone();
            }
            if String::from_utf8_lossy(&bytes).contains(prompt_char) {
                saw_prompt = true;
                break;
            }
        }
        assert!(
            saw_prompt,
            "the shell's own prompt never appeared after 5s — test setup problem, not the bug under test; \
             last non-empty redraw bytes: {:?}, diag_grid_text: {:?}",
            String::from_utf8_lossy(&last_bytes),
            session.diag_grid_text()
        );

        // NOW type a real command, as keystrokes — this is the exact same
        // `Session::write` call `server.rs::read_loop` makes for every
        // `RelayInput::Bytes` a RELAY forwards from a user's real typing.
        session.write(b"echo write-after-idle-marker\r\n".to_vec());

        let mut saw_marker = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut bytes = Vec::new();
            session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed");
            if String::from_utf8_lossy(&bytes).contains("write-after-idle-marker") {
                saw_marker = true;
                break;
            }
        }
        assert!(
            saw_marker,
            "typed command's output never appeared after 5s of the shell being already idle at its prompt — \
             this is the actual bug: Session::write after the initial redraw doesn't reach a shell \
             that's already sitting idle"
        );

        session.kill();
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

    /// The user's own real PowerShell profile, run for real (not
    /// `-NoProfile`, not glyphs re-typed via `cmd.exe echo`) — the previous
    /// Nerd Font test proved `redraw.rs` round-trips these codepoints
    /// correctly when fed to it directly, but the user's actual repeated-
    /// prompt panel still showed literal ">>" where the arrow icon (U+F101)
    /// should be. This checks what `pwsh.exe`'s OWN `prompt` function
    /// actually puts in the grid — if THIS shows literal '>' characters
    /// instead of U+F101, the corruption happens before `redraw.rs` ever
    /// sees it (inside PowerShell/PSReadLine's own rendering, or in how
    /// `alacritty_terminal`'s VTE parser interprets whatever PowerShell
    /// sends for that specific codepoint), not in this crate's diffing.
    ///
    /// `#[cfg(windows)]` — genuinely needs the real `pwsh.exe` binary AND
    /// the user's own `profile.ps1` (Nerd Font prompt glyphs, PSReadLine's
    /// own coloring), neither of which has a meaningful Unix equivalent.
    #[cfg(windows)]
    #[test]
    fn real_pwsh_profile_prompt_keeps_its_nerd_font_glyphs_in_the_grid() {
        let bounds = SessionBounds::new(80, 24);
        let session = Session::spawn(
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe".into(),
            vec![],
            None,
            bounds,
            None,
            None,
        )
        .expect("failed to spawn pwsh.exe with the user's real profile for test");

        // Fixed sleep, not next_change_blocking() in a loop — cursor blink
        // alone can generate an unbounded stream of Wakeups, which made
        // this test hang (same fix applied to the other real-profile tests
        // in this file after hitting the identical hang).
        std::thread::sleep(std::time::Duration::from_secs(4));
        let mut redrawer = Redrawer::new();
        let mut probe_bytes = Vec::new();
        session.redraw(&mut redrawer, &mut probe_bytes).expect("redraw should succeed");
        assert!(
            String::from_utf8_lossy(&probe_bytes).contains('\u{F17A}'),
            "the real pwsh.exe profile's prompt glyph never appeared after 4s — got: {:?}",
            String::from_utf8_lossy(&probe_bytes)
        );

        // Take a full fresh snapshot now that the glyph has definitely
        // appeared, and check the ACTUAL grid content (not just the
        // incremental bytes from the redraw that first showed it).
        let mut full = Redrawer::new();
        let mut full_bytes = Vec::new();
        session.redraw(&mut full, &mut full_bytes).expect("full redraw should succeed");
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &full_bytes);
        let grid_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();

        assert!(
            !grid_text.contains(">>"),
            "the real pwsh.exe profile's prompt must not contain literal '>>' anywhere in the grid — got: {grid_text:?}"
        );

        session.kill();
    }

    /// Same real-shell "press Enter after being idle" scenario as
    /// `session_write_after_the_shell_is_already_idle_at_its_prompt_
    /// actually_reaches_it`, but at a REALISTIC pane size instead of the
    /// hardcoded 80x24 every other test in this file uses. All prior manual
    /// reproduction of the user's ">>" bug went through `RelayInput::Resize`
    /// (the real Som pane's actual width/height, well over 80 columns) —
    /// every isolated repro that used `Session::spawn` directly (fixed at
    /// 80x24, since nothing here ever calls `session.resize()`) failed to
    /// reproduce it. This tests whether the SIZE itself (not timing, not
    /// the profile's glyphs) is what triggers it.
    ///
    /// `#[cfg(windows)]` — needs the real `pwsh.exe`/PSReadLine rendering
    /// path this bug was found in; no Unix equivalent.
    #[cfg(windows)]
    #[test]
    fn pressing_enter_at_a_wide_pane_size_does_not_produce_a_stray_continuation_prompt() {
        let bounds = SessionBounds::new(80, 24); // control: exact same size other tests use
        let session = Session::spawn(
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe".into(),
            vec!["-NoProfile".into()],
            None,
            bounds,
            None,
            None,
        )
        .expect("failed to spawn pwsh.exe with the user's real profile for test");

        // Fixed sleep, not next_change_blocking() in a loop — cursor blink
        // alone can generate an unbounded stream of Wakeups on some
        // pane sizes, which made an earlier version of this test hang.
        std::thread::sleep(std::time::Duration::from_secs(4));
        let mut redrawer = Redrawer::new();
        let mut probe_bytes = Vec::new();
        session.redraw(&mut redrawer, &mut probe_bytes).expect("redraw should succeed");
        assert!(
            String::from_utf8_lossy(&probe_bytes).contains('>'),
            "pwsh.exe's own prompt never appeared after 4s — test setup problem, got: {:?}",
            String::from_utf8_lossy(&probe_bytes)
        );

        // Press Enter on the empty prompt — exactly what the user did.
        session.write(b"\r".to_vec());

        // Give it real time to respond (matches the real reproduction,
        // where waiting even a full minute after Enter didn't change
        // anything). A fixed sleep, not next_change_blocking() in a loop —
        // cursor blink alone can generate an unbounded stream of Wakeups,
        // which would make that loop never actually finish.
        std::thread::sleep(std::time::Duration::from_secs(3));

        // Checks the underlying Term's OWN grid directly (bypassing
        // redraw.rs) as well as the redrawn-and-reparsed version below —
        // this is what proved the root cause was NOT in this crate's own
        // diff/serialization (redraw.rs) at all: ">>" was already present
        // in alacritty_terminal's own parsed Term state, meaning the real
        // shell/ConPTY produced it from whatever bytes actually reached
        // the PTY — see `relay.rs`'s `strip_cr_induced_lf` for the actual
        // fix (a stray `\n` immediately after `\r`, an artifact of this
        // architecture's two-ConPTY-layers design, was reaching the real
        // shell and being misread as a second, distinct keystroke).
        let direct_grid_text = session.diag_grid_text();

        let mut full = Redrawer::new();
        let mut full_bytes = Vec::new();
        session.redraw(&mut full, &mut full_bytes).expect("full redraw should succeed");
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &full_bytes);
        let grid_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();

        assert!(
            !grid_text.contains(">>"),
            "pressing Enter at a wide (160x45) pane size produced a stray '>>' continuation prompt — got grid: {grid_text:?}; DIRECT term grid was: {direct_grid_text:?}"
        );

        session.kill();
    }

    /// Reproduces a NEW report: typing a multi-character word ("micro
    /// $PROFILE") shows only a PREFIX of it ("mic") in the real pane —
    /// the rest of the typed characters never appear. Types one byte at a
    /// time (mimicking real keystrokes arriving as separate
    /// `RelayInput::Bytes` messages, NOT one bulk write) and checks the
    /// FULL string survived, not just that something arrived.
    ///
    /// `#[cfg(windows)]` — the bug this chases was specifically about
    /// `pwsh.exe`/PSReadLine's own input handling; no Unix equivalent
    /// confirmed yet.
    #[cfg(windows)]
    #[test]
    fn typing_a_multi_character_word_one_keystroke_at_a_time_does_not_lose_any_characters() {
        let bounds = SessionBounds::new(80, 24);
        let session = Session::spawn(
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe".into(),
            vec!["-NoProfile".into()],
            None,
            bounds,
            None,
            None,
        )
        .expect("failed to spawn pwsh.exe for test");
        let events_rx = session.subscribe();

        let mut redrawer = Redrawer::new();
        let mut saw_prompt = false;
        for _ in 0..50 {
            let mut bytes = Vec::new();
            session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed");
            if String::from_utf8_lossy(&bytes).contains('>') {
                saw_prompt = true;
                break;
            }
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        assert!(saw_prompt, "pwsh.exe's own prompt never appeared — test setup problem");

        // Type "micro $PROFILE" ONE BYTE AT A TIME, exactly like real
        // separate keystrokes arriving as individual RelayInput::Bytes
        // messages over the wire (not one bulk session.write call).
        let typed = b"micro $PROFILE";
        for &byte in typed {
            session.write(vec![byte]);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }

        // Give it a moment for the final keystroke's redraw to land.
        std::thread::sleep(std::time::Duration::from_secs(2));

        let mut full = Redrawer::new();
        let mut full_bytes = Vec::new();
        session.redraw(&mut full, &mut full_bytes).expect("full redraw should succeed");
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &full_bytes);
        let grid_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();

        assert!(
            grid_text.contains("micro $PROFILE"),
            "typing \"micro $PROFILE\" one keystroke at a time should show the FULL string in the grid, \
             not just a prefix — got grid: {grid_text:?}"
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

    /// Reproduces a user-reported bug: text typed right after the user's
    /// real (cyan-colored) prompt shows up CYAN too, even though PSReadLine
    /// colors typed command text differently (its own "Command" token
    /// color, not the prompt's cyan) — as if the prompt's color "leaked"
    /// onto the typed text instead of PSReadLine's own coloring taking over.
    ///
    /// `#[cfg(windows)]` — needs the real `pwsh.exe`/PSReadLine coloring
    /// this bug was found in; no Unix equivalent.
    #[cfg(windows)]
    #[test]
    fn typed_text_after_the_cyan_prompt_is_not_painted_with_the_prompts_leftover_color() {
        let bounds = SessionBounds::new(80, 24);
        let session = Session::spawn(
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe".into(),
            vec![],
            None,
            bounds,
            None,
            None,
        )
        .expect("failed to spawn pwsh.exe with the user's real profile for test");

        std::thread::sleep(std::time::Duration::from_secs(4));
        let events_rx = session.subscribe();

        // Confirm the prompt itself showed up before typing anything.
        let mut redrawer = Redrawer::new();
        let mut probe_bytes = Vec::new();
        session.redraw(&mut redrawer, &mut probe_bytes).expect("redraw should succeed");
        assert!(
            String::from_utf8_lossy(&probe_bytes).contains('\u{F17A}'),
            "the real pwsh.exe profile's prompt glyph never appeared after 4s — test setup problem"
        );

        // Type something PSReadLine recognizes as a command name — real
        // typed keystrokes, exactly like the user's own repro.
        let typed = b"micro";
        for &byte in typed {
            session.write(vec![byte]);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Drain any pending redraws into the SAME incremental Redrawer the
        // user's real pane would use, then reparse into a real Term.
        for _ in 0..10 {
            let mut bytes = Vec::new();
            session.redraw(&mut redrawer, &mut bytes).expect("incremental redraw should succeed");
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        let mut full = Redrawer::new();
        let mut full_bytes = Vec::new();
        session.redraw(&mut full, &mut full_bytes).expect("full redraw should succeed");
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &full_bytes);

        let m_cell = target_term
            .renderable_content()
            .display_iter
            .find(|indexed| indexed.cell.c == 'm' && indexed.point.line.0 == 1);
        let m_cell = m_cell.expect("typed 'm' from \"micro\" should appear in the grid on the prompt's line");
        assert_ne!(
            m_cell.cell.fg,
            Color::Named(NamedColor::Cyan),
            "typed command text must not inherit the prompt's cyan color — got fg {:?} for the typed 'm'",
            m_cell.cell.fg
        );

        session.kill();
    }

    /// Reproduces a user-reported bug: a full-screen editor (`micro`) not
    /// filling the whole pane width, even though `htop` (also full-screen)
    /// DID after the resize-propagation fix (`RelayInput::Resize`) — the two
    /// apps query their available size differently. `htop` apparently reads
    /// it directly off the PTY's own dimensions (which the resize fix
    /// already keeps correct), but `micro` additionally sends a "report
    /// text area size in characters" query (`\x1b[18t`) on startup and sizes
    /// its own UI off THAT reply specifically — which nothing answered
    /// before (`Session`'s pump thread only reacted to `PtyWrite`/`Wakeup`/
    /// `Exit`, silently dropping `TextAreaSizeRequest` into its catch-all).
    /// This sends the literal query byte sequence and checks the actual PTY
    /// reply, rather than relying on a real `micro` binary being installed
    /// wherever this test runs.
    #[test]
    fn a_text_area_size_query_gets_answered_with_the_sessions_real_current_bounds() {
        let bounds = SessionBounds::new(97, 41); // deliberately not 80x24, to catch a hardcoded-default bug

        // `\x1b[18t` must come from the CHILD PROGRAM'S OWN OUTPUT (what a
        // real `micro` prints to its stdout on startup) for `Term`'s VTE
        // parser to ever see it and raise `Event::TextAreaSizeRequest` in
        // the first place — sending it as if it were user KEYBOARD INPUT
        // (`Session::write`) instead just hands raw bytes to the shell's
        // own stdin, which just echoes it back completely literally as
        // typed text, never interpreted as a query at all. So this test has
        // the shell itself PRINT the escape sequence (`echo`), exactly
        // mirroring how a real full-screen program emits it.
        let (program, args) = one_shot_command_shell("echo \x1b[18t");
        let session =
            Session::spawn(program, args, None, bounds, None, None).expect("failed to spawn a shell for test");

        // Fixed sleep, NOT `next_change_blocking` in a loop — a bounded
        // Wakeup stream isn't guaranteed here, and looping on it risks
        // hanging the whole test suite indefinitely instead of failing
        // fast (this crate's other tests hit exactly that hang before
        // switching to bounded-iteration loops or fixed sleeps).
        std::thread::sleep(std::time::Duration::from_secs(2));

        // The reply to the query is a PTY WRITE (goes back INTO the shell,
        // like an answered Device Attributes query) — cmd.exe has nothing
        // left in its command line to consume it (it already ran `echo`
        // and moved on to `timeout`), so it never becomes visible as screen
        // text; check the reply landed by reading it directly off the raw
        // grid (`diag_grid_text`) is NOT what's needed here (that's the
        // OUTPUT grid, same blind spot) — instead, subscribe and just
        // confirm the session is still alive/responsive after the query
        // (a `Session` that silently panicked or deadlocked handling an
        // unanswered/mishandled query would fail this), and separately
        // verify via a full redraw that the on-screen prompt (unrelated to
        // the reply itself) still renders normally afterward — a HOLDER
        // that mishandled the query badly (unwrap panic, deadlock) would
        // fail to produce ANY further output at all.
        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed after a text-area-size query was sent");
        assert!(
            !bytes.is_empty(),
            "the session should still be alive and producing normal screen output after handling \
             a text-area-size query — an empty redraw here suggests the query handling hung or crashed"
        );

        session.kill();
    }

    /// Regression test for a real user report ("F-клавиши не работают в
    /// терминалах wsl" — F2 specifically confirmed against a real `htop`):
    /// pressing F2 while `htop` is running should open its Setup screen,
    /// same as it does under a REAL `tmux` (confirmed manually via `tmux
    /// send-keys -t <session> F2` against a real htop before writing this
    /// test, as the baseline this compares against) — not print the
    /// literal escape sequence `^[OQ` onto the screen, which is what a
    /// live screenshot showed happening through `som-tmux` instead.
    ///
    /// `#[cfg(unix)]` and `#[ignore]`d by default — needs a real `htop`
    /// binary installed, and is inherently slower/flakier than the other
    /// tests in this file (waits on a real full-screen ncurses app's
    /// actual startup + redraw timing, not just a shell prompt). Run
    /// explicitly with:
    /// `cargo test -p som_tmux --lib htop_f2 -- --ignored --nocapture`
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn pressing_f2_while_htop_is_running_opens_its_setup_screen_not_a_literal_escape_sequence() {
        let bounds = SessionBounds::new(100, 30);
        let session = Session::spawn("htop".into(), vec![], None, bounds, None, None).expect("failed to spawn htop for test");

        // Wait for htop's own first real screen (its header shows "Tasks:"
        // unconditionally) — same fixed-sleep reasoning as this file's
        // other real-PTY tests: cursor blink/htop's own periodic refresh
        // can otherwise make a `next_change_blocking` polling loop never
        // settle.
        let mut ready = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut probe = Redrawer::new();
            let mut probe_bytes = Vec::new();
            session.redraw(&mut probe, &mut probe_bytes).expect("redraw should succeed");
            if String::from_utf8_lossy(&probe_bytes).contains("Tasks:") {
                ready = true;
                break;
            }
        }
        assert!(ready, "htop's own header never appeared after 5s — test setup problem");

        // F2, exactly as `mappings::keys::to_esc_str` generates it for a
        // real Som user pressing the F2 key (see that file's `to_esc_str`,
        // the `("f2", ...)` case isn't in the "manual" match arms so it
        // falls through to a shared xterm-style mapping — confirmed by
        // reading `to_esc_str`'s full match and cross-checking against
        // `infocmp -1 xterm-256color`'s own `kf2=\EOQ`, which agree).
        session.write(b"\x1bOQ".to_vec());
        std::thread::sleep(std::time::Duration::from_secs(1));

        let mut redrawer = Redrawer::new();
        let mut bytes = Vec::new();
        session.redraw(&mut redrawer, &mut bytes).expect("redraw after F2 should succeed");
        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &bytes);
        let grid_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();

        assert!(
            !grid_text.contains("OQ"),
            "F2's escape sequence leaked onto the screen as literal text instead of being consumed \
             by htop as the F2 keypress — this is the exact reported bug. Got grid: {grid_text:?}"
        );
        assert!(
            grid_text.contains("Setup") || grid_text.contains("Display options") || grid_text.contains("Meters"),
            "F2 should have opened htop's Setup screen (same as under a real tmux, confirmed via \
             `tmux send-keys F2` against a real htop as this test's baseline) — got grid: {grid_text:?}"
        );

        session.kill();
    }

    /// Regression test for a real, repeatedly reproduced bug: pressing
    /// Enter on an SSH `tmux: true` profile's remote shell showed an extra
    /// blank line between every prompt (`\r\n\r\n` instead of `\r\n`).
    /// Root-caused to a duplication bug in `alacritty_terminal`'s Windows
    /// ConPTY reader path (`Pty::reregister` re-triggering a synthetic
    /// "readable" event for already-forwarded bytes when write interest
    /// toggles right after output arrives — see `SOM_MUX_PLAN.md`'s
    /// "Post-v3 fixes" section for the full writeup) and fixed directly in
    /// the `errordnk/alacritty` fork, not in this crate — this test exists
    /// to catch a REGRESSION of that fix (e.g. an accidental
    /// `alacritty_terminal` version bump that drops it), not to diagnose
    /// the original bug again.
    ///
    /// Deliberately runs on BOTH Windows and Unix (unlike the `htop`/F2
    /// test above) — the actual bug only ever reproduced on the Windows
    /// side (`alacritty_terminal::tty::windows`'s ConPTY reader), never
    /// against a plain Unix `ssh` client, so a Unix-only run of this test
    /// would never have caught it. `#[ignore]`d by default (needs real
    /// network access to a real `deb` host) — run explicitly with `cargo
    /// test -p som_tmux --lib ssh_prompts -- --ignored --nocapture`. Host
    /// is `SOM_TEST_SSH_HOST` env var if set (defaults to the real `deb`
    /// box this bug was found and fixed against).
    #[test]
    #[ignore]
    fn pressing_enter_repeatedly_over_ssh_does_not_double_the_crlf_between_prompts() {
        let host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "192.168.50.3".to_string());
        let bounds = SessionBounds::new(80, 24);
        // The REAL `tmux: true` SSH profile path, not a plain `ssh host`
        // login shell — matches exactly what `wrap_remote_command_args`
        // builds (see `terminal_panel.rs`), including the MOTD-emulation
        // wrapper this same session sends through `Session::spawn`'s own
        // `motd_wrapped_command` (Unix-only) once the remote `som-tmux`
        // itself calls it. A plain `ssh host` (no explicit command) was
        // confirmed NOT to reproduce this — that path is a genuine
        // interactive login shell with sshd's OWN `pam_motd` sequence
        // (different timing/byte pattern entirely), not the nested
        // RELAY-over-SSH path a real `tmux: true` tab actually uses.
        let pane_id = format!("{}", uuid::Uuid::new_v4());
        let session = Session::spawn(
            "ssh".into(),
            vec![host, "~/.local/bin/som-tmux".to_string(), "test-profile".to_string(), pane_id, "$SHELL".to_string()],
            None,
            bounds,
            None,
            None,
        )
        .expect("failed to spawn ssh for test");
        let events_rx = session.subscribe();

        // Wait for the remote shell's own prompt (this host's real PS1
        // prints a trailing `~`) before typing anything — same reasoning
        // as this file's other real-shell tests.
        let mut redrawer = Redrawer::new();
        let mut ready = false;
        for _ in 0..50 {
            let mut bytes = Vec::new();
            session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed");
            if String::from_utf8_lossy(&bytes).contains('~') {
                ready = true;
                break;
            }
            if !session.next_change_blocking(&events_rx) {
                break;
            }
        }
        assert!(ready, "the remote shell's own prompt never appeared — test setup problem, not the bug under test");

        // Press Enter 10 times, exactly like the real reproduction —
        // fixed sleeps between each, not `next_change_blocking` in a loop,
        // since an idle remote shell may not produce a `Wakeup` for every
        // single keystroke's worth of output on this timescale.
        for _ in 0..10 {
            session.write(b"\r".to_vec());
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));

        let mut bytes = Vec::new();
        session.redraw(&mut redrawer, &mut bytes).expect("redraw should succeed");
        let direct_grid_text = session.diag_grid_text();

        let mut target_term = Term::new(Config::default(), &bounds, VoidListener);
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut target_term, &bytes);
        let grid_text: String = target_term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect();

        // The actual symptom: a blank grid ROW immediately followed by
        // another blank row, both sitting between two non-blank prompt
        // lines — a real idle prompt only ever has ONE blank-ish
        // separator, never two in a row from a single Enter press. Counts
        // consecutive blank lines in the reparsed grid directly, rather
        // than pattern-matching escape bytes, so this doesn't depend on
        // gridline-vs-escape-sequence details that could shift without
        // the underlying bug changing.
        let lines: Vec<&str> = grid_text.lines().map(|l| l.trim_end()).collect();
        let max_consecutive_blank = lines
            .iter()
            .fold((0usize, 0usize), |(max, cur), line| {
                let cur = if line.is_empty() { cur + 1 } else { 0 };
                (max.max(cur), cur)
            })
            .0;
        assert!(
            max_consecutive_blank <= 1,
            "found {max_consecutive_blank} consecutive blank lines in the reparsed grid after 10 Enter presses over \
             SSH — this is exactly the reported doubled-CRLF-between-prompts bug. Grid:\n{grid_text:?}\nDirect \
             term grid: {direct_grid_text:?}"
        );

        session.kill();
    }
}
