use crate::cursor::{CursorLayout, HighlightedRange, HighlightedRangeLine};
use gpui::{
    AbsoluteLength, AnyElement, App, AvailableSpace, Bounds, ContentMask, Context, DispatchPhase,
    Element, ElementId, Entity, FocusHandle, Font, FontFeatures, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, Hitbox, Hsla, InputHandler, InteractiveElement, Interactivity,
    IntoElement, LayoutId, Length, ModifiersChangedEvent, MouseButton, MouseMoveEvent, Pixels,
    Point, StatefulInteractiveElement, StrikethroughStyle, Styled, TextRun, TextStyle,
    UTF16Selection, UnderlineStyle, WeakEntity, WhiteSpace, Window, div, fill, point, px, relative,
    size,
};
use itertools::Itertools;
use terminal::terminal_settings::CursorShape;
use settings::Settings;
use std::time::Instant;
use terminal::{
    IndexedCell, Terminal, TerminalBounds, TerminalContent,
    alacritty_terminal::{
        grid::Dimensions,
        index::Point as AlacPoint,
        selection::SelectionRange,
        term::{TermMode, cell::Flags},
        vte::ansi::{
            Color::{self as AnsiColor, Named},
            CursorShape as AlacCursorShape, NamedColor,
        },
    },
    kitty_graphics_placeholder::{self, PlaceholderCell},
    terminal_settings::TerminalSettings,
};
use theme::{ActiveTheme, Theme};
use theme_settings::ThemeSettings;
use ui::utils::ensure_minimum_contrast;
use ui::{ParentElement, Tooltip};
use util::ResultExt;
use workspace::Workspace;

use std::mem;
use std::{fmt::Debug, ops::RangeInclusive, rc::Rc};

use crate::{BlockContext, BlockProperties, ContentMode, TerminalMode, TerminalView};

/// The information generated during layout that is necessary for painting.
pub struct LayoutState {
    hitbox: Hitbox,
    batched_text_runs: Vec<BatchedTextRun>,
    rects: Vec<LayoutRect>,
    relative_highlighted_ranges: Vec<(RangeInclusive<AlacPoint>, Hsla)>,
    cursor: Option<CursorLayout>,
    ime_cursor_bounds: Option<Bounds<Pixels>>,
    background_color: Hsla,
    dimensions: TerminalBounds,
    mode: TermMode,
    display_offset: usize,
    hyperlink_tooltip: Option<AnyElement>,
    block_below_cursor_element: Option<AnyElement>,
    base_text_style: TextStyle,
    content_mode: ContentMode,
}

/// Helper struct for converting data between Alacritty's cursor points, and displayed cursor points.
#[derive(Copy, Clone)]
struct DisplayCursor {
    line: i32,
    col: usize,
}

impl DisplayCursor {
    fn from(cursor_point: AlacPoint, display_offset: usize) -> Self {
        Self {
            line: cursor_point.line.0 + display_offset as i32,
            col: cursor_point.column.0,
        }
    }

    pub fn line(&self) -> i32 {
        self.line
    }

    pub fn col(&self) -> usize {
        self.col
    }
}

/// A batched text run that combines multiple adjacent cells with the same style
#[derive(Debug)]
pub struct BatchedTextRun {
    pub start_point: AlacPoint<i32, i32>,
    pub text: String,
    pub cell_count: usize,
    pub style: TextRun,
    pub font_size: AbsoluteLength,
}

impl BatchedTextRun {
    fn new_from_char(
        start_point: AlacPoint<i32, i32>,
        c: char,
        style: TextRun,
        font_size: AbsoluteLength,
    ) -> Self {
        let mut text = String::with_capacity(100); // Pre-allocate for typical line length
        text.push(c);
        BatchedTextRun {
            start_point,
            text,
            cell_count: 1,
            style,
            font_size,
        }
    }

    fn can_append(&self, other_style: &TextRun) -> bool {
        self.style.font == other_style.font
            && self.style.color == other_style.color
            && self.style.background_color == other_style.background_color
            && self.style.underline == other_style.underline
            && self.style.strikethrough == other_style.strikethrough
    }

    fn append_char(&mut self, c: char) {
        self.append_char_internal(c, true);
    }

    fn append_zero_width_chars(&mut self, chars: &[char]) {
        for &c in chars {
            self.append_char_internal(c, false);
        }
    }

    fn append_char_internal(&mut self, c: char, counts_cell: bool) {
        self.text.push(c);
        if counts_cell {
            self.cell_count += 1;
        }
        self.style.len += c.len_utf8();
    }

    pub fn paint(
        &self,
        origin: Point<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let pos = Point::new(
            origin.x + self.start_point.column as f32 * dimensions.cell_width,
            origin.y + self.start_point.line as f32 * dimensions.line_height,
        );

        let _ = window
            .text_system()
            .shape_line(
                self.text.clone().into(),
                self.font_size.to_pixels(window.rem_size()),
                std::slice::from_ref(&self.style),
                Some(dimensions.cell_width),
            )
            .paint(
                pos,
                dimensions.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
    }
}

#[derive(Clone, Debug, Default)]
pub struct LayoutRect {
    point: AlacPoint<i32, i32>,
    num_of_cells: usize,
    color: Hsla,
}

impl LayoutRect {
    fn new(point: AlacPoint<i32, i32>, num_of_cells: usize, color: Hsla) -> LayoutRect {
        LayoutRect {
            point,
            num_of_cells,
            color,
        }
    }

    pub fn paint(&self, origin: Point<Pixels>, dimensions: &TerminalBounds, window: &mut Window) {
        let position = {
            let alac_point = self.point;
            point(
                (origin.x + alac_point.column as f32 * dimensions.cell_width).floor(),
                origin.y + alac_point.line as f32 * dimensions.line_height,
            )
        };
        let size = point(
            (dimensions.cell_width * self.num_of_cells as f32).ceil(),
            dimensions.line_height,
        )
        .into();

        window.paint_quad(fill(Bounds::new(position, size), self.color));
    }
}

/// Represents a rectangular region with a specific background color
#[derive(Debug, Clone)]
struct BackgroundRegion {
    start_line: i32,
    start_col: i32,
    end_line: i32,
    end_col: i32,
    color: Hsla,
}

impl BackgroundRegion {
    fn new(line: i32, col: i32, color: Hsla) -> Self {
        BackgroundRegion {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
            color,
        }
    }

    /// Check if this region can be merged with another region
    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }

        // Check if regions are adjacent horizontally
        if self.start_line == other.start_line && self.end_line == other.end_line {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }

        // Check if regions are adjacent vertically with same column span
        if self.start_col == other.start_col && self.end_col == other.end_col {
            return self.end_line + 1 == other.start_line || other.end_line + 1 == self.start_line;
        }

        false
    }

    /// Merge this region with another region
    fn merge_with(&mut self, other: &BackgroundRegion) {
        self.start_line = self.start_line.min(other.start_line);
        self.start_col = self.start_col.min(other.start_col);
        self.end_line = self.end_line.max(other.end_line);
        self.end_col = self.end_col.max(other.end_col);
    }
}

/// Merge background regions to minimize the number of rectangles
fn merge_background_regions(regions: Vec<BackgroundRegion>) -> Vec<BackgroundRegion> {
    if regions.is_empty() {
        return regions;
    }

    let mut merged = regions;
    let mut changed = true;

    // Keep merging until no more merges are possible
    while changed {
        changed = false;
        let mut i = 0;

        while i < merged.len() {
            let mut j = i + 1;
            while j < merged.len() {
                if merged[i].can_merge_with(&merged[j]) {
                    let other = merged.remove(j);
                    merged[i].merge_with(&other);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    merged
}

/// The GPUI element that paints the terminal.
/// We need to keep a reference to the model for mouse events, do we need it for any other terminal stuff, or can we move that to connection?
pub struct TerminalElement {
    terminal: Entity<Terminal>,
    terminal_view: Entity<TerminalView>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    interactivity: Interactivity,
    mode: TerminalMode,
    block_below_cursor: Option<Rc<BlockProperties>>,
}

impl InteractiveElement for TerminalElement {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

impl StatefulInteractiveElement for TerminalElement {}

impl TerminalElement {
    pub fn new(
        terminal: Entity<Terminal>,
        terminal_view: Entity<TerminalView>,
        workspace: WeakEntity<Workspace>,
        focus: FocusHandle,
        focused: bool,
        cursor_visible: bool,
        block_below_cursor: Option<Rc<BlockProperties>>,
        mode: TerminalMode,
    ) -> TerminalElement {
        let mut el = TerminalElement {
            terminal,
            terminal_view,
            workspace,
            focused,
            focus: focus.clone(),
            cursor_visible,
            block_below_cursor,
            mode,
            interactivity: Default::default(),
        }
        .track_focus(&focus);
        el.interactivity.base_style.mouse_cursor = Some(gpui::CursorStyle::Arrow);
        el
    }

    //Vec<Range<AlacPoint>> -> Clip out the parts of the ranges

    pub fn layout_grid(
        grid: impl Iterator<Item = IndexedCell>,
        start_line_offset: i32,
        text_style: &TextStyle,
        hyperlink: Option<(HighlightStyle, &RangeInclusive<AlacPoint>)>,
        selection: Option<SelectionRange>,
        minimum_contrast: f32,
        cx: &App,
    ) -> (Vec<LayoutRect>, Vec<BatchedTextRun>) {
        let start_time = Instant::now();
        let theme = cx.theme();
        let sel_bg_color = theme.colors().text_accent;
        let sel_fg_color = if sel_bg_color.l > 0.5 { gpui::black() } else { gpui::white() };

        // Pre-allocate with estimated capacity to reduce reallocations
        let estimated_cells = grid.size_hint().0;
        let estimated_runs = estimated_cells / 10; // Estimate ~10 cells per run
        let estimated_regions = estimated_cells / 20; // Estimate ~20 cells per background region

        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut cell_count = 0;

        // Collect background regions for efficient merging
        let mut background_regions: Vec<BackgroundRegion> = Vec::with_capacity(estimated_regions);
        let mut current_batch: Option<BatchedTextRun> = None;

        // First pass: collect all cells and their backgrounds
        let linegroups = grid.into_iter().chunk_by(|i| i.point.line);
        for (line_index, (_, line)) in linegroups.into_iter().enumerate() {
            let alac_line = start_line_offset + line_index as i32;

            // Flush any existing batch at line boundaries
            if let Some(batch) = current_batch.take() {
                batched_runs.push(batch);
            }

            let mut previous_cell_had_extras = false;

            for cell in line {
                let mut fg = cell.fg;
                let mut bg = cell.bg;
                if cell.flags.contains(Flags::INVERSE) {
                    mem::swap(&mut fg, &mut bg);
                }

                let is_selected = selection.is_some_and(|sel| {
                    sel.contains(AlacPoint::new(cell.point.line, cell.point.column))
                });

                // Collect background regions (skip default background)
                let effective_bg_color = if is_selected {
                    Some(sel_bg_color)
                } else if !matches!(bg, Named(NamedColor::Background)) {
                    Some(convert_color(&bg, theme))
                } else {
                    None
                };
                if let Some(color) = effective_bg_color {
                    let col = cell.point.column.0 as i32;

                    // Try to extend the last region if it's on the same line with the same color
                    if let Some(last_region) = background_regions.last_mut()
                        && last_region.color == color
                        && last_region.start_line == alac_line
                        && last_region.end_line == alac_line
                        && last_region.end_col + 1 == col
                    {
                        last_region.end_col = col;
                    } else {
                        background_regions.push(BackgroundRegion::new(alac_line, col, color));
                    }
                }
                // Skip wide character spacers - they're just placeholders for the second cell of wide characters
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                // Skip Unicode-placeholder-mode cells — this glyph (a
                // private-use codepoint) has no real appearance of its own
                // (most fonts render it as a missing-glyph box) and is
                // painted as an image instead by
                // paint_rich_content_placements, which scans the same grid
                // independently of this text layout pass.
                if cell.c == kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                    continue;
                }

                // Skip spaces that follow cells with extras (emoji variation sequences)
                if cell.c == ' ' && previous_cell_had_extras {
                    previous_cell_had_extras = false;
                    continue;
                }
                // Update tracking for next iteration
                previous_cell_had_extras =
                    matches!(cell.zerowidth(), Some(chars) if !chars.is_empty());

                //Layout current cell text
                {
                    if !is_blank(&cell) {
                        cell_count += 1;
                        let mut cell_style = TerminalElement::cell_style(
                            &cell,
                            fg,
                            bg,
                            theme,
                            text_style,
                            hyperlink,
                            minimum_contrast,
                        );
                        if is_selected {
                            cell_style.color = sel_fg_color;
                        }

                        let cell_point = AlacPoint::new(alac_line, cell.point.column.0 as i32);
                        let zero_width_chars = cell.zerowidth();

                        // Try to batch with existing run
                        if let Some(ref mut batch) = current_batch {
                            if batch.can_append(&cell_style)
                                && batch.start_point.line == cell_point.line
                                && batch.start_point.column + batch.cell_count as i32
                                    == cell_point.column
                            {
                                batch.append_char(cell.c);
                                if let Some(chars) = zero_width_chars {
                                    batch.append_zero_width_chars(chars);
                                }
                            } else {
                                // Flush current batch and start new one
                                let old_batch = current_batch.take().unwrap();
                                batched_runs.push(old_batch);
                                let mut new_batch = BatchedTextRun::new_from_char(
                                    cell_point,
                                    cell.c,
                                    cell_style,
                                    text_style.font_size,
                                );
                                if let Some(chars) = zero_width_chars {
                                    new_batch.append_zero_width_chars(chars);
                                }
                                current_batch = Some(new_batch);
                            }
                        } else {
                            // Start new batch
                            let mut new_batch = BatchedTextRun::new_from_char(
                                cell_point,
                                cell.c,
                                cell_style,
                                text_style.font_size,
                            );
                            if let Some(chars) = zero_width_chars {
                                new_batch.append_zero_width_chars(chars);
                            }
                            current_batch = Some(new_batch);
                        }
                    };
                }
            }
        }

        // Flush any remaining batch
        if let Some(batch) = current_batch {
            batched_runs.push(batch);
        }

        // Second pass: merge background regions and convert to layout rects
        let region_count = background_regions.len();
        let merged_regions = merge_background_regions(background_regions);
        let mut rects = Vec::with_capacity(merged_regions.len() * 2); // Estimate 2 rects per merged region

        // Convert merged regions to layout rects
        // Since LayoutRect only supports single-line rectangles, we need to split multi-line regions
        for region in merged_regions {
            for line in region.start_line..=region.end_line {
                rects.push(LayoutRect::new(
                    AlacPoint::new(line, region.start_col),
                    (region.end_col - region.start_col + 1) as usize,
                    region.color,
                ));
            }
        }

        let layout_time = start_time.elapsed();

        log::debug!(
            "Terminal layout_grid: {} cells processed, \
            {} batched runs created, {} rects (from {} merged regions), \
            layout took {:?}",
            cell_count,
            batched_runs.len(),
            rects.len(),
            region_count,
            layout_time
        );

        (rects, batched_runs)
    }

    /// Computes the cursor position based on the cursor point and terminal dimensions.
    fn cursor_position(cursor_point: DisplayCursor, size: TerminalBounds) -> Option<Point<Pixels>> {
        if cursor_point.line() < size.total_lines() as i32 {
            // When on pixel boundaries round the origin down
            Some(point(
                (cursor_point.col() as f32 * size.cell_width()).floor(),
                (cursor_point.line() as f32 * size.line_height()).floor(),
            ))
        } else {
            None
        }
    }

    /// Checks if a character is a decorative block/box-like character that should
    /// preserve its exact colors without contrast adjustment.
    ///
    /// This specifically targets characters used as visual connectors, separators,
    /// and borders where color matching with adjacent backgrounds is critical.
    /// Regular icons (git, folders, etc.) are excluded as they need to remain readable.
    ///
    /// Fixes https://github.com/zed-industries/zed/issues/34234
    fn is_decorative_character(ch: char) -> bool {
        matches!(
            ch as u32,
            // Unicode Box Drawing and Block Elements
            0x2500..=0x257F // Box Drawing (└ ┐ ─ │ etc.)
            | 0x2580..=0x259F // Block Elements (▀ ▄ █ ░ ▒ ▓ etc.)
            | 0x25A0..=0x25FF // Geometric Shapes (■ ▶ ● etc. - includes triangular/circular separators)

            // Private Use Area - Powerline separator symbols only
            | 0xE0B0..=0xE0B7 // Powerline separators: triangles (E0B0-E0B3) and half circles (E0B4-E0B7)
            | 0xE0B8..=0xE0BF // Powerline separators: corner triangles
            | 0xE0C0..=0xE0CA // Powerline separators: flames (E0C0-E0C3), pixelated (E0C4-E0C7), and ice (E0C8 & E0CA)
            | 0xE0CC..=0xE0D1 // Powerline separators: honeycombs (E0CC-E0CD) and lego (E0CE-E0D1)
            | 0xE0D2..=0xE0D7 // Powerline separators: trapezoid (E0D2 & E0D4) and inverted triangles (E0D6-E0D7)
        )
    }

    /// Whether the application explicitly picked this foreground color and does not
    /// want it adjusted for contrast: 24-bit true color (`\e[38;2;R;G;Bm`) or a
    /// specific entry in the 256-color palette (`\e[38;5;Nm`) where N >= 16 (the
    /// 6x6x6 cube at 16..=231 and the 24-step grayscale ramp at 232..=255).
    /// Indices 0..=15 still go through contrast adjustment since those map to
    /// theme-defined ANSI colors that can clash with the theme background.
    fn is_app_chosen_exact_color(fg: &terminal::alacritty_terminal::vte::ansi::Color) -> bool {
        matches!(
            fg,
            terminal::alacritty_terminal::vte::ansi::Color::Spec(_)
                | terminal::alacritty_terminal::vte::ansi::Color::Indexed(16..=255)
        )
    }

    /// Converts the Alacritty cell styles to GPUI text styles and background color.
    fn cell_style(
        indexed: &IndexedCell,
        fg: terminal::alacritty_terminal::vte::ansi::Color,
        bg: terminal::alacritty_terminal::vte::ansi::Color,
        colors: &Theme,
        text_style: &TextStyle,
        hyperlink: Option<(HighlightStyle, &RangeInclusive<AlacPoint>)>,
        minimum_contrast: f32,
    ) -> TextRun {
        let flags = indexed.cell.flags;
        let skip_contrast = Self::is_app_chosen_exact_color(&fg);
        let mut fg = convert_color(&fg, colors);
        let bg = convert_color(&bg, colors);

        if !skip_contrast && !Self::is_decorative_character(indexed.c) {
            fg = ensure_minimum_contrast(fg, bg, minimum_contrast);
        }

        // Ghostty uses (175/255) as the multiplier (~0.69), Alacritty uses 0.66, Kitty
        // uses 0.75. We're using 0.7 because it's pretty well in the middle of that.
        if flags.intersects(Flags::DIM) {
            fg.a *= 0.7;
        }

        let underline = (flags.intersects(Flags::ALL_UNDERLINES)
            || indexed.cell.hyperlink().is_some())
        .then(|| UnderlineStyle {
            color: Some(fg),
            thickness: Pixels::from(1.0),
            wavy: flags.contains(Flags::UNDERCURL),
        });

        let strikethrough = flags
            .intersects(Flags::STRIKEOUT)
            .then(|| StrikethroughStyle {
                color: Some(fg),
                thickness: Pixels::from(1.0),
            });

        let weight = if flags.intersects(Flags::BOLD) {
            FontWeight::BOLD
        } else {
            text_style.font_weight
        };

        let style = if flags.intersects(Flags::ITALIC) {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };

        let mut result = TextRun {
            len: indexed.c.len_utf8(),
            color: fg,
            background_color: None,
            font: Font {
                weight,
                style,
                ..text_style.font()
            },
            underline,
            strikethrough,
        };

        if let Some((style, range)) = hyperlink
            && range.contains(&indexed.point)
        {
            if let Some(underline) = style.underline {
                result.underline = Some(underline);
            }

            if let Some(color) = style.color {
                result.color = color;
            }
        }

        result
    }

    fn generic_button_handler<E>(
        connection: Entity<Terminal>,
        focus_handle: FocusHandle,
        steal_focus: bool,
        f: impl Fn(&mut Terminal, &E, &mut Context<Terminal>),
    ) -> impl Fn(&E, &mut Window, &mut App) {
        move |event, window, cx| {
            if steal_focus {
                window.focus(&focus_handle, cx);
            } else if !focus_handle.is_focused(window) {
                return;
            }
            connection.update(cx, |terminal, cx| {
                f(terminal, event, cx);

                cx.notify();
            })
        }
    }

    fn register_mouse_listeners(
        &mut self,
        mode: TermMode,
        hitbox: &Hitbox,
        content_mode: &ContentMode,
        window: &mut Window,
    ) {
        let focus = self.focus.clone();
        let terminal = self.terminal.clone();
        let terminal_view = self.terminal_view.clone();

        self.interactivity.on_mouse_down(MouseButton::Left, {
            let terminal = terminal.clone();
            let focus = focus.clone();
            let terminal_view = terminal_view.clone();

            move |e, window, cx| {
                window.focus(&focus, cx);

                let scroll_top = terminal_view.read(cx).scroll_top;
                terminal.update(cx, |terminal, cx| {
                    let mut adjusted_event = e.clone();
                    if scroll_top > Pixels::ZERO {
                        adjusted_event.position.y += scroll_top;
                    }
                    terminal.mouse_down(&adjusted_event, cx);
                    cx.notify();
                })
            }
        });

        window.on_mouse_event({
            let terminal = self.terminal.clone();
            let hitbox = hitbox.clone();
            let focus = focus.clone();
            let terminal_view = terminal_view;
            move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                if e.pressed_button.is_some() && !cx.has_active_drag() && focus.is_focused(window) {
                    let hovered = hitbox.is_hovered(window);

                    let scroll_top = terminal_view.read(cx).scroll_top;
                    terminal.update(cx, |terminal, cx| {
                        if terminal.selection_started() || hovered {
                            let mut adjusted_event = e.clone();
                            if scroll_top > Pixels::ZERO {
                                adjusted_event.position.y += scroll_top;
                            }
                            terminal.mouse_drag(&adjusted_event, hitbox.bounds, cx);
                            cx.notify();
                        }
                    })
                }

                if hitbox.is_hovered(window) {
                    terminal.update(cx, |terminal, cx| {
                        terminal.mouse_move(e, cx);
                    })
                }
            }
        });

        self.interactivity.on_mouse_up(
            MouseButton::Left,
            TerminalElement::generic_button_handler(
                terminal.clone(),
                focus.clone(),
                false,
                move |terminal, e, cx| {
                    terminal.mouse_up(e, cx);
                },
            ),
        );
        self.interactivity.on_mouse_down(
            MouseButton::Middle,
            TerminalElement::generic_button_handler(
                terminal.clone(),
                focus.clone(),
                true,
                move |terminal, e, cx| {
                    terminal.mouse_down(e, cx);
                },
            ),
        );

        if content_mode.is_scrollable() {
            self.interactivity.on_scroll_wheel({
                let terminal_view = self.terminal_view.downgrade();
                move |e, window, cx| {
                    // Ctrl+wheel (Cmd+wheel on macOS) zooms the terminal font,
                    // matching the standard convention used by browsers and
                    // most terminal emulators — same session-only font-size
                    // adjustment as Ctrl+=/Ctrl+-.
                    if e.modifiers.secondary() {
                        let line_height = px(1.0);
                        let y_delta = e.delta.pixel_delta(line_height).y;
                        if y_delta > Pixels::ZERO {
                            theme_settings::increase_buffer_font_size(cx);
                        } else if y_delta < Pixels::ZERO {
                            theme_settings::decrease_buffer_font_size(cx);
                        }
                        return;
                    }
                    terminal_view
                        .update(cx, |terminal_view, cx| {
                            if matches!(terminal_view.mode, TerminalMode::Standalone)
                                || terminal_view.focus_handle.is_focused(window)
                            {
                                terminal_view.scroll_wheel(e, cx);
                                cx.notify();
                            }
                        })
                        .ok();
                }
            });
        }

        // Mouse mode handlers:
        // All mouse modes need the extra click handlers
        if mode.intersects(TermMode::MOUSE_MODE) {
            self.interactivity.on_mouse_down(
                MouseButton::Right,
                TerminalElement::generic_button_handler(
                    terminal.clone(),
                    focus.clone(),
                    true,
                    move |terminal, e, cx| {
                        terminal.mouse_down(e, cx);
                    },
                ),
            );
            self.interactivity.on_mouse_up(
                MouseButton::Right,
                TerminalElement::generic_button_handler(
                    terminal.clone(),
                    focus.clone(),
                    false,
                    move |terminal, e, cx| {
                        terminal.mouse_up(e, cx);
                    },
                ),
            );
            self.interactivity.on_mouse_up(
                MouseButton::Middle,
                TerminalElement::generic_button_handler(
                    terminal,
                    focus,
                    false,
                    move |terminal, e, cx| {
                        terminal.mouse_up(e, cx);
                    },
                ),
            );
        }
    }

    fn rem_size(&self, cx: &mut App) -> Option<Pixels> {
        let settings = ThemeSettings::get_global(cx).clone();
        let buffer_font_size = settings.buffer_font_size(cx);
        let rem_size_scale = {
            // Our default UI font size is 14px on a 16px base scale.
            // This means the default UI font size is 0.875rems.
            let default_font_size_scale = 14. / ui::BASE_REM_SIZE_IN_PX;

            // We then determine the delta between a single rem and the default font
            // size scale.
            let default_font_size_delta = 1. - default_font_size_scale;

            // Finally, we add this delta to 1rem to get the scale factor that
            // should be used to scale up the UI.
            1. + default_font_size_delta
        };

        Some(buffer_font_size * rem_size_scale)
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let height: Length = match self.terminal_view.read(cx).content_mode(window, cx) {
            ContentMode::Inline {
                displayed_lines,
                total_lines: _,
            } => {
                let rem_size = window.rem_size();
                let line_height = f32::from(window.text_style().font_size.to_pixels(rem_size))
                    * TerminalSettings::get_global(cx).line_height.value();
                px(displayed_lines as f32 * line_height).into()
            }
            ContentMode::Scrollable => {
                if let TerminalMode::Embedded { .. } = &self.mode {
                    let term = self.terminal.read(cx);
                    if !term.scrolled_to_top() && !term.scrolled_to_bottom() && self.focused {
                        self.interactivity.occlude_mouse();
                    }
                }

                relative(1.).into()
            }
        };

        let layout_id = self.interactivity.request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |mut style, window, cx| {
                style.size.width = relative(1.).into();
                style.size.height = height;

                window.request_layout(style, None, cx)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let rem_size = self.rem_size(cx);
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_, _, hitbox, window, cx| {
                let hitbox = hitbox.unwrap();
                let settings = ThemeSettings::get_global(cx).clone();

                let buffer_font_size = settings.buffer_font_size(cx);

                let terminal_settings = TerminalSettings::get_global(cx);
                let minimum_contrast = terminal_settings.minimum_contrast;

                let font_family = terminal_settings.font_family.as_ref().map_or_else(
                    || settings.buffer_font.family.clone(),
                    |font_family| font_family.0.clone().into(),
                );

                let font_fallbacks = terminal_settings
                    .font_fallbacks
                    .as_ref()
                    .or(settings.buffer_font.fallbacks.as_ref())
                    .cloned();

                let font_features = terminal_settings
                    .font_features
                    .as_ref()
                    .unwrap_or(&FontFeatures::disable_ligatures())
                    .clone();

                let font_weight = terminal_settings.font_weight.unwrap_or_default();

                let line_height = terminal_settings.line_height.value();

                let font_size = match &self.mode {
                    TerminalMode::Embedded { .. } => {
                        window.text_style().font_size.to_pixels(window.rem_size())
                    }
                    TerminalMode::Standalone => terminal_settings
                        .font_size
                        .map_or(buffer_font_size, |size| {
                            theme_settings::adjusted_font_size(size, cx)
                        }),
                };

                let theme = cx.theme().clone();

                let link_style = HighlightStyle {
                    color: Some(theme.colors().link_text_hover),
                    font_weight: Some(font_weight),
                    font_style: None,
                    background_color: None,
                    underline: Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(theme.colors().link_text_hover),
                        wavy: false,
                    }),
                    strikethrough: None,
                    fade_out: None,
                };

                let text_style = TextStyle {
                    font_family,
                    font_features,
                    font_weight,
                    font_fallbacks,
                    font_size: font_size.into(),
                    font_style: FontStyle::Normal,
                    line_height: px(line_height).into(),
                    background_color: Some(theme.colors().terminal_ansi_background),
                    white_space: WhiteSpace::Normal,
                    // These are going to be overridden per-cell
                    color: theme.colors().terminal_foreground,
                    ..Default::default()
                };

                let text_system = cx.text_system();
                let match_color = theme.colors().search_match_background;
                let gutter;
                let (dimensions, line_height_px) = {
                    let rem_size = window.rem_size();
                    let font_pixels = text_style.font_size.to_pixels(rem_size);
                    let line_height = f32::from(font_pixels) * line_height;
                    let font_id = cx.text_system().resolve_font(&text_style.font());

                    let cell_width = text_system
                        .advance(font_id, font_pixels, 'm')
                        .unwrap()
                        .width;
                    gutter = cell_width;

                    let mut size = bounds.size;
                    size.width -= gutter;
                    let available_height = size.height;

                    // https://github.com/zed-industries/zed/issues/2750
                    // if the terminal is one column wide, rendering 🦀
                    // causes alacritty to misbehave.
                    if size.width < cell_width * 2.0 {
                        size.width = cell_width * 2.0;
                    }

                    let mut origin = bounds.origin;
                    origin.x += gutter;

                    if matches!(self.terminal_view.read(cx).mode, TerminalMode::Standalone) {
                        let scale_factor = window.scale_factor();
                        let line_height_pixels = px(line_height);
                        let line_height_device_px = (f32::from(line_height_pixels) * scale_factor)
                            .round()
                            .max(1.0) as i32;
                        let available_height_device_px =
                            (f32::from(available_height) * scale_factor)
                                .floor()
                                .max(0.0) as i32;

                        let rows =
                            ((available_height_device_px / line_height_device_px) as usize).max(1);
                        let snapped_height_device_px = (rows as i32) * line_height_device_px;
                        let padding_device_px =
                            (available_height_device_px - snapped_height_device_px).max(0);

                        let snapped_height =
                            px(snapped_height_device_px as f32 / scale_factor.max(1.0));
                        let padding = px(padding_device_px as f32 / scale_factor.max(1.0));

                        size.height = snapped_height;
                        if self.terminal.read(cx).scrolled_to_bottom() {
                            origin.y += padding;
                        }
                    }

                    // Snap to device pixels to avoid subpixel jitter while resizing.
                    // Terminal rendering is grid-based; allowing fractional origins can cause the
                    // glyph rasterization to shift between frames, which looks like flicker.
                    let scale_factor = window.scale_factor();
                    let snap_px = |value: Pixels| {
                        Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
                    };
                    origin.x = snap_px(origin.x);
                    origin.y = snap_px(origin.y);

                    (
                        TerminalBounds::new(px(line_height), cell_width, Bounds { origin, size }),
                        line_height,
                    )
                };

                let search_matches = self.terminal.read(cx).matches.clone();

                let background_color = theme.colors().terminal_background;

                let (last_hovered_word, hover_tooltip) =
                    self.terminal.update(cx, |terminal, cx| {
                        terminal.set_size(dimensions);
                        terminal.sync(window, cx);

                        if window.modifiers().secondary()
                            && bounds.contains(&window.mouse_position())
                            && self.terminal_view.read(cx).hover.is_some()
                        {
                            let registered_hover = self.terminal_view.read(cx).hover.as_ref();
                            if terminal.last_content.last_hovered_word.as_ref()
                                == registered_hover.map(|hover| &hover.hovered_word)
                            {
                                (
                                    terminal.last_content.last_hovered_word.clone(),
                                    registered_hover.map(|hover| hover.tooltip.clone()),
                                )
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        }
                    });

                let scroll_top = self.terminal_view.read(cx).scroll_top;
                let hyperlink_tooltip = hover_tooltip.map(|hover_tooltip| {
                    let offset = dimensions.bounds.origin - point(px(0.), scroll_top);
                    let mut element = div()
                        .size_full()
                        .id("terminal-element")
                        .tooltip(Tooltip::text(hover_tooltip))
                        .into_any_element();
                    element.prepaint_as_root(offset, bounds.size.into(), window, cx);
                    element
                });

                let TerminalContent {
                    cells,
                    mode,
                    display_offset,
                    cursor_char,
                    selection,
                    cursor,
                    ..
                } = &self.terminal.read(cx).last_content;
                let mode = *mode;
                let display_offset = *display_offset;

                // searches, highlights to a single range representations
                let mut relative_highlighted_ranges = Vec::new();
                for search_match in search_matches {
                    relative_highlighted_ranges.push((search_match, match_color))
                }
                // Selection is rendered via fg/bg inversion in layout_grid, no overlay needed.

                // then have that representation be converted to the appropriate highlight data structure

                let content_mode = self.terminal_view.read(cx).content_mode(window, cx);

                // Calculate the intersection of the terminal's bounds with the current
                // content mask (the visible viewport after all parent clipping).
                // This allows us to only render cells that are actually visible, which is
                // critical for performance when terminals are inside scrollable containers
                // like the Agent Panel thread view.
                //
                // This optimization is analogous to the editor optimization in PR #45077
                // which fixed performance issues with large AutoHeight editors inside Lists.
                let content_bounds = dimensions.bounds;
                let visible_bounds = window.content_mask().bounds;
                let intersection = visible_bounds.intersect(&content_bounds);

                // If the terminal is entirely outside the viewport, skip all cell processing.
                // This handles the case where the terminal has been scrolled past (above or
                // below the viewport), similar to the editor fix in PR #45077 where start_row
                // could exceed max_row when the editor was positioned above the viewport.
                let (rects, batched_text_runs) = if intersection.size.height <= px(0.)
                    || intersection.size.width <= px(0.)
                {
                    (Vec::new(), Vec::new())
                } else if intersection == content_bounds {
                    // Fast path: terminal fully visible, no clipping needed.
                    // Avoid grouping/allocation overhead by streaming cells directly.
                    TerminalElement::layout_grid(
                        cells.iter().cloned(),
                        0,
                        &text_style,
                        last_hovered_word
                            .as_ref()
                            .map(|last_hovered_word| (link_style, &last_hovered_word.word_match)),
                        *selection,
                        minimum_contrast,
                        cx,
                    )
                } else {
                    // Calculate which screen rows are visible based on pixel positions.
                    // This works for both Scrollable and Inline modes because we filter
                    // by screen position (enumerated line group index), not by the cell's
                    // internal line number (which can be negative in Scrollable mode for
                    // scrollback history).
                    let rows_above_viewport = f32::from(
                        (intersection.top() - content_bounds.top()).max(px(0.)) / line_height_px,
                    ) as usize;
                    let visible_row_count =
                        f32::from((intersection.size.height / line_height_px).ceil()) as usize + 1;

                    TerminalElement::layout_grid(
                        // Group cells by line and filter to only the visible screen rows.
                        // skip() and take() work on enumerated line groups (screen position),
                        // making this work regardless of the actual cell.point.line values.
                        cells
                            .iter()
                            .chunk_by(|c| c.point.line)
                            .into_iter()
                            .skip(rows_above_viewport)
                            .take(visible_row_count)
                            .flat_map(|(_, line_cells)| line_cells)
                            .cloned(),
                        rows_above_viewport as i32,
                        &text_style,
                        last_hovered_word
                            .as_ref()
                            .map(|last_hovered_word| (link_style, &last_hovered_word.word_match)),
                        *selection,
                        minimum_contrast,
                        cx,
                    )
                };

                // Layout cursor. Rectangle is used for IME, so we should lay it out even
                // if we don't end up showing it.
                let cursor_point = DisplayCursor::from(cursor.point, display_offset);
                let cursor_text = {
                    let str_trxt = cursor_char.to_string();
                    let len = str_trxt.len();
                    window.text_system().shape_line(
                        str_trxt.into(),
                        text_style.font_size.to_pixels(window.rem_size()),
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color: theme.colors().terminal_ansi_background,
                            ..Default::default()
                        }],
                        None,
                    )
                };

                // For whitespace, use cell width to avoid cursor stretching.
                // For other characters, use the larger of shaped width and cell width
                // to properly cover wide characters like emojis.
                let cursor_width = if cursor_char.is_whitespace() {
                    dimensions.cell_width()
                } else {
                    cursor_text.width.max(dimensions.cell_width())
                };

                let ime_cursor_bounds = TerminalElement::cursor_position(cursor_point, dimensions)
                    .map(|cursor_position| Bounds {
                        origin: cursor_position,
                        size: size(cursor_width.ceil(), dimensions.line_height),
                    });

                let cursor = if let AlacCursorShape::Hidden = cursor.shape {
                    None
                } else {
                    let focused = self.focused;
                    ime_cursor_bounds.map(move |bounds| {
                        let (shape, text) = match cursor.shape {
                            AlacCursorShape::Block if !focused => (CursorShape::Hollow, None),
                            AlacCursorShape::Block => (CursorShape::Block, Some(cursor_text)),
                            AlacCursorShape::Underline if !focused => (CursorShape::Hollow, None),
                            AlacCursorShape::Underline => (CursorShape::Underline, None),
                            AlacCursorShape::Beam if !focused => (CursorShape::Hollow, None),
                            AlacCursorShape::Beam => (CursorShape::Bar, None),
                            AlacCursorShape::HollowBlock => (CursorShape::Hollow, None),
                            AlacCursorShape::Hidden => unreachable!(),
                        };

                        CursorLayout::new(
                            bounds.origin,
                            bounds.size.width,
                            bounds.size.height,
                            theme.players().local().cursor,
                            shape,
                            text,
                        )
                    })
                };

                let block_below_cursor_element = if let Some(block) = &self.block_below_cursor {
                    let terminal = self.terminal.read(cx);
                    if terminal.last_content.display_offset == 0 {
                        let target_line = terminal.last_content.cursor.point.line.0 + 1;
                        let render = &block.render;
                        let mut block_cx = BlockContext {
                            window,
                            context: cx,
                            dimensions,
                        };
                        let element = render(&mut block_cx);
                        let mut element = div().occlude().child(element).into_any_element();
                        let available_space = size(
                            AvailableSpace::Definite(dimensions.width() + gutter),
                            AvailableSpace::Definite(
                                block.height as f32 * dimensions.line_height(),
                            ),
                        );
                        let origin = Point::new(bounds.origin.x, dimensions.bounds.origin.y)
                            + point(px(0.), target_line as f32 * dimensions.line_height())
                            - point(px(0.), scroll_top);
                        window.with_rem_size(rem_size, |window| {
                            element.prepaint_as_root(origin, available_space, window, cx);
                        });
                        Some(element)
                    } else {
                        None
                    }
                } else {
                    None
                };

                LayoutState {
                    hitbox,
                    batched_text_runs,
                    cursor,
                    ime_cursor_bounds,
                    background_color,
                    dimensions,
                    rects,
                    relative_highlighted_ranges,
                    mode,
                    display_offset,
                    hyperlink_tooltip,
                    block_below_cursor_element,
                    base_text_style: text_style,
                    content_mode,
                }
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let paint_start = Instant::now();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let scroll_top = self.terminal_view.read(cx).scroll_top;

            window.paint_quad(fill(bounds, layout.background_color));
            let origin = layout.dimensions.bounds.origin - Point::new(px(0.), scroll_top);
            let scale_factor = window.scale_factor();
            let snap_px = |value: Pixels| {
                Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
            };
            let origin = point(snap_px(origin.x), snap_px(origin.y));

            let marked_text_cloned: Option<String> = {
                let ime_state = &self.terminal_view.read(cx).ime_state;
                ime_state.as_ref().map(|state| state.marked_text.clone())
            };

            let terminal_input_handler = TerminalInputHandler {
                terminal: self.terminal.clone(),
                terminal_view: self.terminal_view.clone(),
                cursor_bounds: layout.ime_cursor_bounds.map(|bounds| bounds + origin),
                workspace: self.workspace.clone(),
            };

            self.register_mouse_listeners(
                layout.mode,
                &layout.hitbox,
                &layout.content_mode,
                window,
            );
            if window.modifiers().secondary()
                && bounds.contains(&window.mouse_position())
                && self.terminal_view.read(cx).hover.is_some()
            {
                window.set_cursor_style(gpui::CursorStyle::PointingHand, &layout.hitbox);
            } else {
                window.set_cursor_style(gpui::CursorStyle::IBeam, &layout.hitbox);
            }

            let original_cursor = layout.cursor.take();
            let hyperlink_tooltip = layout.hyperlink_tooltip.take();
            let block_below_cursor_element = layout.block_below_cursor_element.take();
            self.interactivity.paint(
                global_id,
                inspector_id,
                bounds,
                Some(&layout.hitbox),
                window,
                cx,
                |_, window, cx| {
                    window.handle_input(&self.focus, terminal_input_handler, cx);

                    window.on_key_event({
                        let this = self.terminal.clone();
                        move |event: &ModifiersChangedEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }

                            this.update(cx, |term, cx| {
                                term.try_modifiers_change(&event.modifiers, window, cx)
                            });
                        }
                    });

                    for rect in &layout.rects {
                        rect.paint(origin, &layout.dimensions, window);
                    }

                    for (relative_highlighted_range, color) in &layout.relative_highlighted_ranges {
                        if let Some((start_y, highlighted_range_lines)) =
                            to_highlighted_range_lines(relative_highlighted_range, layout, origin)
                        {
                            let corner_radius = Pixels::ZERO;
                            let hr = HighlightedRange {
                                start_y,
                                line_height: layout.dimensions.line_height,
                                lines: highlighted_range_lines,
                                color: *color,
                                corner_radius: corner_radius,
                            };
                            hr.paint(true, bounds, window);
                        }
                    }

                    // Paint batched text runs instead of individual cells
                    let text_paint_start = Instant::now();
                    for batch in &layout.batched_text_runs {
                        batch.paint(origin, &layout.dimensions, window, cx);
                    }
                    let text_paint_time = text_paint_start.elapsed();

                    // Som's own rich-content protocol (SRP) placements —
                    // anchored to grid cells written as ordinary (if
                    // specially-encoded) text, so they paint alongside the
                    // text batches. See `paint_rich_content_placements`'s
                    // own doc comment for the encoding.
                    let any_rich_content_animating =
                        paint_rich_content_placements(&self.terminal, origin, layout, window, cx);

                    // At least one rich-content animation is currently
                    // playing on screen — nothing else would otherwise
                    // prompt GPUI to repaint this element on the next tick
                    // (unlike, say, a keystroke or PTY output), so ask for
                    // one explicitly. Mirrors `gpui::elements::img`'s own
                    // GIF/WebP animation driving (`Img::request_layout`'s
                    // `window.request_animation_frame()` call) — see that
                    // module for the equivalent non-terminal codepath this
                    // was modeled on.
                    //
                    // `request_animation_frame()` alone is NOT enough on
                    // Windows: it only marks this element dirty for
                    // whenever the platform's own mechanism next gets
                    // around to actually redrawing (a background VSync
                    // thread calling `RedrawWindow`) — not synchronous, and
                    // confirmed (live testing a real animated GIF through
                    // `somcat`) to never actually fire on its own once the
                    // window is idle: the same "doesn't repaint until you
                    // press a key" gap already documented for multi-chunk
                    // rich-content transfers in `Terminal::process_event`'s
                    // `ApcString` handler, here for animation frame
                    // advancement instead of chunk arrival. An explicit
                    // forced native repaint (unconditional `window.refresh()`
                    // + `draw()` + `present()`, see
                    // `PlatformWindow::force_redraw`'s doc comment) is
                    // needed to actually get the next frame on screen
                    // rather than just have it sit correctly decoded forever.
                    //
                    // MUST be throttled the same way `process_event`'s own
                    // chunk-arrival redraw is
                    // (`Terminal::rich_content_force_redraw_due`, sharing
                    // that same timer/budget) — the earlier assumption
                    // written here ("paint() itself only runs as often as a
                    // real repaint already happens, so this can't runaway")
                    // was WRONG: `force_redraw_windows()` synchronously does
                    // a full `draw()+present()` and `request_animation_frame()`
                    // schedules another `paint()` right after, so once an
                    // animation is active this becomes a tight self-
                    // sustaining paint -> force_redraw -> paint loop with no
                    // pacing at all, confirmed live to starve the PTY-
                    // reader/event-loop task of CPU time badly enough that a
                    // single somcat invocation (one 500x500 GIF, 47 frames)
                    // took over a minute end-to-end instead of the ~5-10s
                    // headless benchmarks (`bench_somcat_*` in
                    // `crates/terminal/src/terminal.rs`) consistently show
                    // with no competing native repaint loop. `read(cx)`
                    // (immutable), NOT `Entity::update` — going through
                    // `update` from inside an in-progress paint call was
                    // tried and reverted earlier in this same investigation:
                    // it broke even the FIRST frame from ever appearing.
                    if any_rich_content_animating {
                        window.request_animation_frame();
                        if self.terminal.read(cx).rich_content_force_redraw_due() {
                            cx.force_redraw_windows();
                        }
                    }

                    if let Some(text_to_mark) = &marked_text_cloned
                        && !text_to_mark.is_empty()
                        && let Some(ime_bounds) = layout.ime_cursor_bounds
                    {
                        let ime_position = (ime_bounds + origin).origin;
                        let mut ime_style = layout.base_text_style.clone();
                        ime_style.underline = Some(UnderlineStyle {
                            color: Some(ime_style.color),
                            thickness: px(1.0),
                            wavy: false,
                        });

                        let shaped_line = window.text_system().shape_line(
                            text_to_mark.clone().into(),
                            ime_style.font_size.to_pixels(window.rem_size()),
                            &[TextRun {
                                len: text_to_mark.len(),
                                font: ime_style.font(),
                                color: ime_style.color,
                                underline: ime_style.underline,
                                ..Default::default()
                            }],
                            None,
                        );

                        // Paint background to cover terminal text behind marked text
                        let ime_background_bounds = Bounds::new(
                            ime_position,
                            size(shaped_line.width, layout.dimensions.line_height),
                        );
                        window.paint_quad(fill(ime_background_bounds, layout.background_color));

                        shaped_line
                            .paint(
                                ime_position,
                                layout.dimensions.line_height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            )
                            .log_err();
                    }

                    if self.cursor_visible
                        && marked_text_cloned.is_none()
                        && let Some(mut cursor) = original_cursor
                    {
                        cursor.paint(origin, window, cx);
                    }

                    if let Some(mut element) = block_below_cursor_element {
                        element.paint(window, cx);
                    }

                    if let Some(mut element) = hyperlink_tooltip {
                        element.paint(window, cx);
                    }

                    log::debug!(
                        "Terminal paint: {} text runs, {} rects, \
                        text paint took {:?}, total paint took {total_paint_time:?}",
                        layout.batched_text_runs.len(),
                        layout.rects.len(),
                        text_paint_time,
                        total_paint_time = paint_start.elapsed()
                    );
                },
            );
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct TerminalInputHandler {
    terminal: Entity<Terminal>,
    terminal_view: Entity<TerminalView>,
    workspace: WeakEntity<Workspace>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(TermMode::ALT_SCREEN)
        {
            None
        } else {
            Some(UTF16Selection {
                range: 0..0,
                reversed: false,
            })
        }
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        self.terminal_view.read(cx).marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.clear_marked_text(view_cx);
            view.commit_text(text, view_cx);
        });

        self.workspace
            .update(cx, |_this, _cx| {
                window.invalidate_character_coordinates();
            })
            .ok();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_marked_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.set_marked_text(new_text.to_string(), view_cx);
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.clear_marked_text(view_cx);
        });
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let term_bounds = self.terminal_view.read(cx).terminal_bounds(cx);

        let mut bounds = self.cursor_bounds?;
        let offset_x = term_bounds.cell_width * range_utf16.start as f32;
        bounds.origin.x += offset_x;

        Some(bounds)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }
}

pub fn is_blank(cell: &IndexedCell) -> bool {
    if cell.c != ' ' {
        return false;
    }

    if cell.bg != AnsiColor::Named(NamedColor::Background) {
        return false;
    }

    if cell.hyperlink().is_some() {
        return false;
    }

    if cell
        .flags
        .intersects(Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::STRIKEOUT)
    {
        return false;
    }

    true
}

/// Paints Som's own rich-content protocol images/animations
/// ([`terminal::rich_content_transport`]). SRP placements are real Unicode-
/// placeholder grid cells (see `Terminal::print_rich_content_placeholder_grid`,
/// same encoding described in [`kitty_graphics_placeholder`]'s module doc
/// comment) — this function scans the visible grid for them, decoding
/// `(session_id, file_id)` from each placeholder cell's foreground/
/// underline color, groups adjacent cells sharing the same id into a
/// bounding box (one image per group, not one paint per cell — `paint_image`
/// has no sub-region/crop support), and resolves pixel data via
/// `RichContentCache`/`rich_content_placements()`. Because these are real
/// grid cells, the terminal's own scroll/history/clear handling already
/// keeps them correctly positioned — no separate paint-bounds computation
/// needed beyond finding which cells decode as placeholders and where they
/// are right now.
fn paint_rich_content_placements(
    terminal: &Entity<Terminal>,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let cell_width = layout.dimensions.cell_width;
    let line_height = layout.dimensions.line_height;
    let mut any_animating = false;

    // Everything needed from `terminal` is collected as OWNED data in
    // this narrowly scoped `update` call — the borrow it takes on
    // `terminal` must end before any call below that needs `cx` mutably
    // (`ShapedLine::paint`, used by the audio widget branch). `update`
    // (not `read`) is required here because scanning the placeholder
    // grid also calls `ensure_rich_content_srv_subscription`, which
    // lazily spawns this placement's `som-srv` progress subscription and
    // applies any progress already observed to `RichContentCache` — both
    // need `&mut Terminal`.
    let (origins, placements, audio_placements, video_placements, max_columns_seen, max_rows_seen, pending_image_drops) =
        terminal.update(cx, |terminal, _cx| {
        // For each (session_id, file_id) group, remember the grid position
        // of whichever visible cell decodes as (row=0, column=0) if one is
        // currently visible — that's the placement's true top-left origin.
        // When row=0/column=0 itself has scrolled out of view (the image's
        // top has scrolled above the viewport), derive the same origin
        // from ANY other visible cell by subtracting its own decoded (row,
        // column) offset: `origin_line = cell.line - decoded.row`. This is
        // why each cell's `PlaceholderCell::row`/`column` (not just its
        // grid position) matters here — the group's on-screen bounding box
        // (min/max of whatever cells are CURRENTLY visible) is NOT the
        // placement's real extent when it's partially scrolled off-screen;
        // only the decoded in-image offset lets every visible cell agree
        // on the same absolute origin regardless of how much of the image
        // is currently clipped.
        // Every placement seen in the placeholder grid needs a `som-srv`
        // subscription, regardless of whether `rich_content_cache` has
        // any bytes for it yet — see `Terminal::
        // ensure_rich_content_srv_subscription`'s own doc comment for
        // why this can't wait until AFTER the cache already has an
        // entry. Done as its own pass (not fused into the loop below)
        // since `poll_rich_content_srv_subscriptions` needs `&mut
        // Terminal` and the loop below borrows `terminal.last_content()`
        // for its whole duration.
        terminal.poll_rich_content_srv_subscriptions();

        let mut origins: std::collections::HashMap<(u32, u32), (i32, i32)> = std::collections::HashMap::new();
        let mut max_columns_seen: std::collections::HashMap<(u32, u32), u32> = std::collections::HashMap::new();
        let mut max_rows_seen: std::collections::HashMap<(u32, u32), u32> = std::collections::HashMap::new();

        for indexed_cell in &terminal.last_content().cells {
            let fg_rgb = match indexed_cell.cell.fg {
                AnsiColor::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                _ => continue,
            };
            let underline_rgb = match indexed_cell.cell.underline_color() {
                Some(AnsiColor::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                Some(_) => continue,
                None => None,
            };
            let diacritics = indexed_cell.cell.zerowidth().unwrap_or(&[]);
            let Some(PlaceholderCell {
                image_id: session_id,
                placement_id: file_id,
                row,
                column: cell_col_in_image,
            }) = kitty_graphics_placeholder::decode_placeholder_cell(
                indexed_cell.cell.c,
                fg_rgb,
                underline_rgb,
                diacritics,
            ) else {
                continue;
            };

            let key = (session_id, file_id);
            let grid_line = indexed_cell.point.line.0;
            let grid_column = indexed_cell.point.column.0 as i32;
            let origin_line = grid_line - row as i32;
            let origin_column = grid_column - cell_col_in_image as i32;
            // Every visible cell of the same placement must derive the
            // exact same origin (they're all offsets from the same
            // top-left corner) — any one of them is equally authoritative,
            // so the first cell seen for a given id wins.
            origins.entry(key).or_insert((origin_line, origin_column));
            // The placement's real column count can't be recomputed from
            // the image's own pixel size at paint time (see
            // `Terminal::record_rich_content_max_column_seen`'s doc
            // comment): remember the widest column any paint pass has
            // actually decoded, persisted on the cache entry so a later
            // paint where the image is only partially visible (scrolled,
            // or a narrower split) doesn't shrink it back down.
            terminal.record_rich_content_max_column_seen(session_id, file_id, cell_col_in_image);
            let entry = max_columns_seen.entry(key).or_insert(0);
            *entry = (*entry).max(terminal.rich_content_max_column_seen(session_id, file_id).unwrap_or(0));
            // Vertical counterpart — see `Terminal::
            // record_rich_content_max_row_seen`'s doc comment for why the
            // painted height needs this (the sending client's grid row
            // count rounds UP from the image's real pixel height, so
            // deriving height purely from the image's own aspect ratio
            // leaves a visible gap in the last reserved row).
            terminal.record_rich_content_max_row_seen(session_id, file_id, row);
            let row_entry = max_rows_seen.entry(key).or_insert(0);
            *row_entry = (*row_entry).max(terminal.rich_content_max_row_seen(session_id, file_id).unwrap_or(0));
        }

        let placements = terminal.rich_content_placements();
        let audio_placements = terminal.rich_content_audio_placements();
        #[cfg(target_os = "windows")]
        let video_placements = terminal.rich_content_video_placements();
        #[cfg(not(target_os = "windows"))]
        let video_placements: Vec<(
            u32,
            u32,
            std::sync::Arc<gpui::RenderImage>,
            bool,
            f32,
            std::time::Duration,
            std::time::Duration,
        )> = Vec::new();
        #[cfg(target_os = "windows")]
        let pending_image_drops = terminal.take_pending_video_image_drops();
        #[cfg(not(target_os = "windows"))]
        let pending_image_drops: Vec<std::sync::Arc<gpui::RenderImage>> = Vec::new();
        (origins, placements, audio_placements, video_placements, max_columns_seen, max_rows_seen, pending_image_drops)
    });

    // Released regardless of whether `origins` turns out empty below —
    // a video player keeps decoding (and therefore keeps replacing
    // `last_rendered`) even on a paint pass where its placeholder cells
    // happen not to be visible, so its drop queue still needs draining
    // every time this runs, not just when there's a placement to paint.
    for image in pending_image_drops {
        cx.drop_image(image, Some(window));
    }

    if origins.is_empty() {
        return false;
    }

    for (key, (origin_line, origin_column)) in origins {
        let (session_id, file_id) = key;

        if let Some((_, _, position_fraction, is_playing, elapsed, duration)) =
            audio_placements.iter().find(|(sid, fid, ..)| *sid == session_id && *fid == file_id)
        {
            let max_column_seen = max_columns_seen.get(&key).copied();
            if let Some((bounds, bar_bounds, stop_bounds)) = paint_rich_content_media_widget(
                max_column_seen,
                *position_fraction,
                *is_playing,
                *elapsed,
                *duration,
                origin,
                origin_line,
                origin_column,
                layout,
                window,
                cx,
            ) {
                let terminal = terminal.read(cx);
                terminal.record_rich_content_placement_bounds(session_id, file_id, bounds);
                if let Some(bar_bounds) = bar_bounds {
                    terminal.record_rich_content_seek_bar_bounds(session_id, file_id, bar_bounds);
                }
                if let Some(stop_bounds) = stop_bounds {
                    terminal.record_rich_content_stop_icon_bounds(session_id, file_id, stop_bounds);
                }
            }
            any_animating |= *is_playing;
            continue;
        }

        // Video: the picture paints through the EXACT SAME `paint_image`
        // call the image/GIF branch below already uses — a single
        // always-current frame, `current_frame` fixed at 0 since
        // `RichContentVideoPlayer::current_frame` already hands back a
        // freshly built one-frame `RenderImage` rather than an index
        // into a multi-frame one — but occupies only `max_row_seen`
        // rows, NOT `max_row_seen + 1`: the placeholder grid's LAST row
        // is reserved for this placement's control widget (see
        // `print_video_placeholder_grid`'s own doc comment for why
        // `somcat` prints one extra row beyond the picture itself), and
        // gets painted separately below via the SAME
        // `paint_rich_content_media_widget` audio already uses — one
        // shared control-row implementation for both content types.
        let video_match = video_placements.iter().find(|(sid, fid, ..)| *sid == session_id && *fid == file_id);
        if let Some((_, _, render_image, is_playing, position_fraction, elapsed, duration)) = video_match {
            let max_column_seen = max_columns_seen.get(&key).copied();
            let max_row_seen = max_rows_seen.get(&key).copied();
            let picture_rows = max_row_seen.map(|r| r.max(1) - 1).unwrap_or(0);

            let display_line = origin_line + layout.display_offset as i32;
            let num_lines = layout.dimensions.num_lines() as i32;

            let image_size = render_image.size(0);
            let (full_width, full_height) = if image_size.width.0 > 0 && image_size.height.0 > 0 {
                let columns = match max_column_seen {
                    Some(max_column) => (max_column + 1) as f32,
                    None => (image_size.width.0 as f32 / f32::from(cell_width)).ceil().max(1.0),
                };
                let width = cell_width * columns;
                // `picture_rows` — the last row is the widget's, not the
                // picture's (see this arm's own doc comment above).
                let height = line_height * picture_rows.max(1) as f32;
                (width, height)
            } else {
                (cell_width, line_height)
            };
            let picture_size = gpui::size(full_width, full_height);

            let row_span = (f32::from(full_height) / f32::from(line_height)).ceil() as i32;
            if display_line + row_span >= 0 && display_line < num_lines {
                let picture_position =
                    point(origin.x + origin_column as f32 * cell_width, origin.y + display_line as f32 * line_height);
                any_animating |= *is_playing;
                window
                    .paint_image(
                        Bounds::new(picture_position, picture_size),
                        gpui::Corners::all(Pixels::ZERO),
                        render_image.clone(),
                        0,
                        false,
                    )
                    .log_err();
            }

            // Widget row sits immediately below the picture, at
            // `origin_line + picture_rows` — `paint_rich_content_media_
            // widget` computes its own `display_line`/on-screen position
            // from `origin_line`/`origin_column` the same way the
            // picture branch above does, just offset down by however
            // many rows the picture actually occupies.
            if let Some((bounds, bar_bounds, stop_bounds)) = paint_rich_content_media_widget(
                max_column_seen,
                *position_fraction,
                *is_playing,
                *elapsed,
                *duration,
                origin,
                origin_line + picture_rows as i32,
                origin_column,
                layout,
                window,
                cx,
            ) {
                let terminal = terminal.read(cx);
                terminal.record_rich_content_placement_bounds(session_id, file_id, bounds);
                if let Some(bar_bounds) = bar_bounds {
                    terminal.record_rich_content_seek_bar_bounds(session_id, file_id, bar_bounds);
                }
                if let Some(stop_bounds) = stop_bounds {
                    terminal.record_rich_content_stop_icon_bounds(session_id, file_id, stop_bounds);
                }
            }
            continue;
        }

        let (render_image, current_frame, is_animating) = {
            let Some((_, _, render_image, current_frame, is_animating)) =
                placements.iter().find(|(sid, fid, ..)| *sid == session_id && *fid == file_id)
            else {
                // Placeholder cells reference a file id whose decoded
                // frame isn't available yet (still decoding) or no
                // longer is (dropped) — nothing to paint, not an error,
                // same reasoning as `paint_kitty_unicode_placeholders`'s
                // identical case.
                continue;
            };
            (render_image, *current_frame, *is_animating)
        };

        let display_line = origin_line + layout.display_offset as i32;
        let num_lines = layout.dimensions.num_lines() as i32;

        // The placement's on-screen width is however many columns the
        // SENDING client's grid actually had (recorded via
        // `record_rich_content_max_column_seen` as cells are scanned
        // above), never recomputed from the image's raw pixel size divided
        // by `cell_width` — those two are different units (the image's
        // physical file pixels vs GPUI's logical/DIP pixels), and dividing
        // one by the other silently produces the wrong column count
        // whenever the display's DPI scale isn't 1.0. Falls back to the
        // pixel-based estimate only on the very first paint, before any
        // placeholder cell has been scanned yet.
        //
        // Height is derived the SAME way (from `max_rows_seen`, the
        // sending client's actual grid row count) rather than from the
        // image's own aspect ratio — `rows = height_px.div_ceil(cell_
        // height)` on the sending side rounds UP to a whole number of
        // cells, so the image's real pixel height essentially never
        // exactly fills that many rows. Scaling height purely by aspect
        // (as this used to) left a visible gap of blank terminal
        // background in the grid's last reserved row, confirmed live as
        // an extra blank line between an image and the next prompt.
        // Filling the whole reserved footprint instead — same principle
        // already applied to width — makes that gap disappear.
        let image_size = render_image.size(0);
        let (full_width, full_height) = if image_size.width.0 > 0 && image_size.height.0 > 0 {
            let columns = match max_columns_seen.get(&key).copied() {
                Some(max_column) => (max_column + 1) as f32,
                None => (image_size.width.0 as f32 / f32::from(cell_width)).ceil().max(1.0),
            };
            let width = cell_width * columns;
            let height = match max_rows_seen.get(&key).copied() {
                Some(max_row) => line_height * (max_row + 1) as f32,
                None => {
                    let scale = f32::from(width) / image_size.width.0 as f32;
                    px(image_size.height.0 as f32 * scale)
                },
            };
            (width, height)
        } else {
            (cell_width, line_height)
        };
        let size = gpui::size(full_width, full_height);

        // The image's real vertical extent in rows, derived from its own
        // painted height rather than re-deriving it from visible cells —
        // used only to early-reject a placement that's ENTIRELY off-screen
        // (both edges outside the viewport), letting `content_mask`
        // (applied by the caller around this whole paint pass) do the
        // actual per-pixel clipping for a partially-visible placement.
        let row_span = (f32::from(full_height) / f32::from(line_height)).ceil() as i32;
        if display_line + row_span < 0 || display_line >= num_lines {
            continue;
        }

        let position =
            point(origin.x + origin_column as f32 * cell_width, origin.y + display_line as f32 * line_height);

        any_animating |= is_animating;
        terminal.read(cx).record_rich_content_placement_bounds(session_id, file_id, Bounds::new(position, size));
        window
            .paint_image(
                Bounds::new(position, size),
                gpui::Corners::all(Pixels::ZERO),
                render_image.clone(),
                current_frame,
                false,
            )
            .log_err();
    }

    any_animating
}

/// Paints one media placement's (audio OR video) inline control row —
/// play/pause glyph, current elapsed time, a seek-bar fill, total
/// duration time, and a stop glyph — filling exactly the cell footprint
/// the sending client's placeholder grid describes (`somcat`'s
/// `AUDIO_WIDGET_COLUMNS`x`AUDIO_WIDGET_ROWS` for audio, or the video
/// picture's own column count plus one extra reserved row for video —
/// see `print_video_placeholder_grid`'s own doc comment; this function
/// trusts whatever footprint the placeholder cells actually describe,
/// via `record_rich_content_max_column_seen`, same as the image branch
/// above, rather than hardcoding either sender's constant here). Shared
/// by both content types rather than duplicated — the row's layout and
/// interaction model (play/pause, seek, stop) doesn't depend on whether
/// the underlying media is audio or video.
#[allow(clippy::too_many_arguments)]
fn paint_rich_content_media_widget(
    max_column_seen: Option<u32>,
    position_fraction: f32,
    is_playing: bool,
    elapsed: std::time::Duration,
    duration: std::time::Duration,
    origin: Point<Pixels>,
    origin_line: i32,
    origin_column: i32,
    layout: &LayoutState,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Bounds<Pixels>, Option<Bounds<Pixels>>, Option<Bounds<Pixels>>)> {
    let cell_width = layout.dimensions.cell_width;
    let line_height = layout.dimensions.line_height;
    let display_line = origin_line + layout.display_offset as i32;
    let num_lines = layout.dimensions.num_lines() as i32;
    if display_line < 0 || display_line >= num_lines {
        return None;
    }

    let columns = max_column_seen.map(|c| c + 1).unwrap_or(1) as f32;
    let width = cell_width * columns;
    let position =
        point(origin.x + origin_column as f32 * cell_width, origin.y + display_line as f32 * line_height);
    let bounds = Bounds::new(position, gpui::size(width, line_height));

    let widget_bg = gpui::rgba(0x1e1e2eff);
    let bar_track = gpui::rgba(0x45475aff);
    let bar_fill = gpui::rgba(0x89dcebff);
    let text_color = gpui::white();

    window.paint_quad(fill(bounds, widget_bg));

    // Layout, left to right: play/pause glyph (1 cell) — current time —
    // padding — seek bar (fills whatever's left) — padding — total
    // time — padding — stop glyph (1 cell). Matches a normal media-
    // player control row's element order, not an arbitrary choice.
    //
    // Uses the terminal's own text style (`layout.base_text_style`), not
    // `TextStyle::default()` — so every glyph/time readout renders in
    // whatever font the user's terminal is actually configured with
    // (Som embeds FiraCode Nerd Font as the default, but
    // `terminal_settings.font_family`/`buffer_font.family` can override
    // that in settings.json), same size/color as everything else
    // painted here.
    let mut text_style = layout.base_text_style.clone();
    text_style.color = text_color;
    text_style.font_size = line_height.into();

    // Nerd Font (Font Awesome subset) codepoints, not plain Unicode
    // ▶/⏸/⏹ — plain Unicode has no glyph at all for these even in Som's
    // own embedded FiraCode Nerd Font (confirmed via `ttf-parser` for
    // pause), so it would already fall back to a missing-glyph box with
    // the DEFAULT font, before a user even touches settings. But
    // `text_style.font()` here isn't necessarily that embedded font at
    // all — a user can point `terminal.font_family` (or the global
    // `buffer_font`) at any installed font, Nerd or not. Query whether
    // the font actually resolved for THIS paint has real glyphs for the
    // Nerd Font codepoints before committing to them; if not, fall back
    // to plain ASCII that renders correctly in any monospace font at
    // all, rather than painting a guaranteed missing-glyph box for the
    // whole widget's lifetime.
    let resolved_font_id = window.text_system().resolve_font(&text_style.font());
    let has_nerd_font_glyphs = window.text_system().has_glyph_for_char(resolved_font_id, '\u{f04c}');
    let (nf_play, nf_pause, nf_stop) = if has_nerd_font_glyphs {
        ("\u{f04b}", "\u{f04c}", "\u{f04d}")
    } else {
        (">", "||", "[]")
    };

    let shape_and_paint = |text: &str, at: Point<Pixels>, window: &mut Window, cx: &mut App| -> Pixels {
        let run = TextRun { len: text.len(), font: text_style.font(), color: text_color, ..Default::default() };
        let line = window.text_system().shape_line(
            text.to_string().into(),
            text_style.font_size.to_pixels(window.rem_size()),
            &[run],
            None,
        );
        line.paint(at, line_height, gpui::TextAlign::Left, None, window, cx).log_err();
        line.width
    };

    // Time readout renders a few points smaller than the play/pause/
    // stop glyphs (which stay at the row's full `line_height`) — purely
    // a visual choice (a full-cell-height clock readout looked oversized
    // next to a slim seek bar), vertically centered within the row by
    // `paint_time`'s own `y_offset` below. Clamped so it can never go
    // non-positive on an already-tiny font.
    let time_font_size: gpui::Pixels = (line_height - gpui::px(4.0)).max(gpui::px(1.0));

    // Time readout ALWAYS renders the same 8-character "HH:MM:SS"
    // template (leading zeros dimmed rather than omitted — see below),
    // so its shaped width is the same real number of pixels every call
    // for a given font — this used to instead assume that width equals
    // `8 * cell_width` exactly, which isn't guaranteed: font metrics at
    // a given font_size don't have to match the terminal grid's own
    // cell width, confirmed live as the time text visibly overlapping
    // the seek bar by roughly one character's width. Measuring the REAL
    // shaped width once (any duration works, since the character count
    // is always identical) and reusing that measurement for every
    // position calculation below removes the assumption entirely —
    // every element's layout follows from what was ACTUALLY painted,
    // not from a guess about font metrics.
    let time_text_width = {
        let probe_text = format_duration(std::time::Duration::ZERO);
        let probe_run =
            TextRun { len: probe_text.len(), font: text_style.font(), color: text_color, ..Default::default() };
        window.text_system().shape_line(probe_text.into(), time_font_size, &[probe_run], None).width
    };
    // Leading digits that are still at their "00" zero-state are dimmed
    // (grey) rather than fully hidden — the fixed "HH:MM:SS" template
    // stays visually present at all times (so the layout never looks
    // like it's missing characters), while genuinely significant digits
    // paint in the normal bright color, same idea a car odometer's dim
    // leading zeros use.
    let dim_color: gpui::Hsla = gpui::rgba(0x585b70ff).into();
    let paint_time = |duration: std::time::Duration, at: Point<Pixels>, window: &mut Window, cx: &mut App| {
        let text = format_duration(duration);
        // First index whose digit isn't part of a leading "00" run —
        // colons are never the first non-zero character themselves, so
        // scanning digit-by-digit and treating a colon as "still dim if
        // everything before it was dim" gives the right split point
        // without needing special-case logic for where the colons fall.
        let mut first_significant = text.len();
        for (i, c) in text.char_indices() {
            if c.is_ascii_digit() && c != '0' {
                first_significant = i;
                break;
            }
        }
        let runs = if first_significant == 0 {
            vec![TextRun { len: text.len(), font: text_style.font(), color: text_color, ..Default::default() }]
        } else if first_significant >= text.len() {
            vec![TextRun { len: text.len(), font: text_style.font(), color: dim_color, ..Default::default() }]
        } else {
            vec![
                TextRun { len: first_significant, font: text_style.font(), color: dim_color, ..Default::default() },
                TextRun {
                    len: text.len() - first_significant,
                    font: text_style.font(),
                    color: text_color,
                    ..Default::default()
                },
            ]
        };
        let line = window.text_system().shape_line(text.into(), time_font_size, &runs, None);
        // Vertically centered within the row — `time_font_size` is
        // smaller than `line_height`, so painting at the row's own top
        // (`at.y` unmodified) would leave it sitting high, not centered.
        let y_offset = (line_height - time_font_size) / 2.0;
        line.paint(point(at.x, at.y + y_offset), time_font_size, gpui::TextAlign::Left, None, window, cx).log_err();
    };

    let play_glyph = if is_playing { nf_pause } else { nf_play };
    shape_and_paint(play_glyph, position, window, cx);

    // Two cells of padding between the play/pause glyph and the
    // current-time readout — without it the two visually ran together
    // (glyph and time text abutting with no gap), confirmed live.
    let current_time_position = point(position.x + cell_width * 2.0, position.y);
    paint_time(elapsed, current_time_position, window, cx);

    // Stop glyph anchored to the trailing cell, total time immediately
    // to its left with two cells of padding — mirrors the leading
    // play/pause + current-time layout on the opposite end (glyph +
    // 2-cell pad + 8-cell fixed-width time = 11 cells on each side).
    let stop_position = point(position.x + width - cell_width, position.y);
    shape_and_paint(nf_stop, stop_position, window, cx);
    let stop_bounds = Bounds::new(stop_position, gpui::size(cell_width, line_height));

    let total_time_position = point(stop_position.x - cell_width * 2.0 - time_text_width, position.y);
    paint_time(duration, total_time_position, window, cx);

    // The seek bar occupies whatever's left between the current-time
    // readout and the total-time readout, with one cell of padding on
    // each side. This rectangle is deliberately narrower than the whole
    // widget, so it's returned separately (not just implied by
    // `bounds`) — hit-testing (`Terminal::seek_fraction_for_position`)
    // needs the bar's OWN extent to compute a seek fraction; using the
    // full widget width as the denominator there previously made a
    // click at the bar's visual midpoint compute a fraction far short
    // of 0.5, confirmed live. Both edges anchor to `time_text_width`
    // (the REAL measured width of the time template — see that
    // variable's own doc comment), not a cell-count guess, so the bar
    // never overlaps either time readout regardless of font metrics.
    let bar_start_x = current_time_position.x + time_text_width + cell_width;
    let bar_end_x = total_time_position.x - cell_width;
    let mut bar_bounds_out = None;
    if bar_end_x > bar_start_x {
        let bar_bounds = Bounds::new(
            point(bar_start_x, position.y + line_height * 0.4),
            gpui::size(bar_end_x - bar_start_x, line_height * 0.2),
        );
        window.paint_quad(fill(bar_bounds, bar_track));
        let fill_width = (bar_end_x - bar_start_x) * position_fraction.clamp(0.0, 1.0);
        if fill_width > Pixels::ZERO {
            let fill_bounds = Bounds::new(bar_bounds.origin, gpui::size(fill_width, bar_bounds.size.height));
            window.paint_quad(fill(fill_bounds, bar_fill));
        }
        bar_bounds_out = Some(bar_bounds);
    }

    Some((bounds, bar_bounds_out, Some(stop_bounds)))
}

fn format_duration(d: std::time::Duration) -> String {
    let total_seconds = d.as_secs();
    format!("{:02}:{:02}:{:02}", total_seconds / 3600, (total_seconds / 60) % 60, total_seconds % 60)
}

fn to_highlighted_range_lines(
    range: &RangeInclusive<AlacPoint>,
    layout: &LayoutState,
    origin: Point<Pixels>,
) -> Option<(Pixels, Vec<HighlightedRangeLine>)> {
    // Step 1. Normalize the points to be viewport relative.
    // When display_offset = 1, here's how the grid is arranged:
    //-2,0 -2,1...
    //--- Viewport top
    //-1,0 -1,1...
    //--------- Terminal Top
    // 0,0  0,1...
    // 1,0  1,1...
    //--- Viewport Bottom
    // 2,0  2,1...
    //--------- Terminal Bottom

    // Normalize to viewport relative, from terminal relative.
    // lines are i32s, which are negative above the top left corner of the terminal
    // If the user has scrolled, we use the display_offset to tell us which offset
    // of the grid data we should be looking at. But for the rendering step, we don't
    // want negatives. We want things relative to the 'viewport' (the area of the grid
    // which is currently shown according to the display offset)
    let unclamped_start = AlacPoint::new(
        range.start().line + layout.display_offset,
        range.start().column,
    );
    let unclamped_end =
        AlacPoint::new(range.end().line + layout.display_offset, range.end().column);

    // Step 2. Clamp range to viewport, and return None if it doesn't overlap
    if unclamped_end.line.0 < 0 || unclamped_start.line.0 > layout.dimensions.num_lines() as i32 {
        return None;
    }

    let clamped_start_line = unclamped_start.line.0.max(0) as usize;

    let clamped_end_line = unclamped_end
        .line
        .0
        .min(layout.dimensions.num_lines() as i32) as usize;

    // Convert the start of the range to pixels
    let start_y = origin.y + clamped_start_line as f32 * layout.dimensions.line_height;

    // Step 3. Expand ranges that cross lines into a collection of single-line ranges.
    //  (also convert to pixels)
    let mut highlighted_range_lines = Vec::new();
    for line in clamped_start_line..=clamped_end_line {
        let mut line_start = 0;
        let mut line_end = layout.dimensions.columns();

        if line == clamped_start_line && unclamped_start.line.0 >= 0 {
            line_start = unclamped_start.column.0;
        }
        if line == clamped_end_line && unclamped_end.line.0 <= layout.dimensions.num_lines() as i32
        {
            line_end = unclamped_end.column.0 + 1; // +1 for inclusive
        }

        highlighted_range_lines.push(HighlightedRangeLine {
            start_x: origin.x + line_start as f32 * layout.dimensions.cell_width,
            end_x: origin.x + line_end as f32 * layout.dimensions.cell_width,
        });
    }

    Some((start_y, highlighted_range_lines))
}

/// Converts a 2, 8, or 24 bit color ANSI color to the GPUI equivalent.
pub fn convert_color(fg: &terminal::alacritty_terminal::vte::ansi::Color, theme: &Theme) -> Hsla {
    let colors = theme.colors();
    match fg {
        // Named and theme defined colors
        terminal::alacritty_terminal::vte::ansi::Color::Named(n) => match n {
            NamedColor::Black => colors.terminal_ansi_black,
            NamedColor::Red => colors.terminal_ansi_red,
            NamedColor::Green => colors.terminal_ansi_green,
            NamedColor::Yellow => colors.terminal_ansi_yellow,
            NamedColor::Blue => colors.terminal_ansi_blue,
            NamedColor::Magenta => colors.terminal_ansi_magenta,
            NamedColor::Cyan => colors.terminal_ansi_cyan,
            NamedColor::White => colors.terminal_ansi_white,
            NamedColor::BrightBlack => colors.terminal_ansi_bright_black,
            NamedColor::BrightRed => colors.terminal_ansi_bright_red,
            NamedColor::BrightGreen => colors.terminal_ansi_bright_green,
            NamedColor::BrightYellow => colors.terminal_ansi_bright_yellow,
            NamedColor::BrightBlue => colors.terminal_ansi_bright_blue,
            NamedColor::BrightMagenta => colors.terminal_ansi_bright_magenta,
            NamedColor::BrightCyan => colors.terminal_ansi_bright_cyan,
            NamedColor::BrightWhite => colors.terminal_ansi_bright_white,
            NamedColor::Foreground => colors.terminal_foreground,
            NamedColor::Background => colors.terminal_ansi_background,
            NamedColor::Cursor => theme.players().local().cursor,
            NamedColor::DimBlack => colors.terminal_ansi_dim_black,
            NamedColor::DimRed => colors.terminal_ansi_dim_red,
            NamedColor::DimGreen => colors.terminal_ansi_dim_green,
            NamedColor::DimYellow => colors.terminal_ansi_dim_yellow,
            NamedColor::DimBlue => colors.terminal_ansi_dim_blue,
            NamedColor::DimMagenta => colors.terminal_ansi_dim_magenta,
            NamedColor::DimCyan => colors.terminal_ansi_dim_cyan,
            NamedColor::DimWhite => colors.terminal_ansi_dim_white,
            NamedColor::BrightForeground => colors.terminal_bright_foreground,
            NamedColor::DimForeground => colors.terminal_dim_foreground,
        },
        // 'True' colors
        terminal::alacritty_terminal::vte::ansi::Color::Spec(rgb) => {
            terminal::rgba_color(rgb.r, rgb.g, rgb.b)
        }
        // 8 bit, indexed colors
        terminal::alacritty_terminal::vte::ansi::Color::Indexed(i) => {
            terminal::get_color_at_index(*i as usize, theme)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AbsoluteLength, Hsla, font};
    use ui::utils::apca_contrast;

    #[test]
    fn test_is_decorative_character() {
        // Box Drawing characters (U+2500 to U+257F)
        assert!(TerminalElement::is_decorative_character('─')); // U+2500
        assert!(TerminalElement::is_decorative_character('│')); // U+2502
        assert!(TerminalElement::is_decorative_character('┌')); // U+250C
        assert!(TerminalElement::is_decorative_character('┐')); // U+2510
        assert!(TerminalElement::is_decorative_character('└')); // U+2514
        assert!(TerminalElement::is_decorative_character('┘')); // U+2518
        assert!(TerminalElement::is_decorative_character('┼')); // U+253C

        // Block Elements (U+2580 to U+259F)
        assert!(TerminalElement::is_decorative_character('▀')); // U+2580
        assert!(TerminalElement::is_decorative_character('▄')); // U+2584
        assert!(TerminalElement::is_decorative_character('█')); // U+2588
        assert!(TerminalElement::is_decorative_character('░')); // U+2591
        assert!(TerminalElement::is_decorative_character('▒')); // U+2592
        assert!(TerminalElement::is_decorative_character('▓')); // U+2593

        // Geometric Shapes - block/box-like subset (U+25A0 to U+25D7)
        assert!(TerminalElement::is_decorative_character('■')); // U+25A0
        assert!(TerminalElement::is_decorative_character('□')); // U+25A1
        assert!(TerminalElement::is_decorative_character('▲')); // U+25B2
        assert!(TerminalElement::is_decorative_character('▼')); // U+25BC
        assert!(TerminalElement::is_decorative_character('◆')); // U+25C6
        assert!(TerminalElement::is_decorative_character('●')); // U+25CF

        // The specific character from the issue
        assert!(TerminalElement::is_decorative_character('◗')); // U+25D7
        assert!(TerminalElement::is_decorative_character('◘')); // U+25D8 (now included in Geometric Shapes)
        assert!(TerminalElement::is_decorative_character('◙')); // U+25D9 (now included in Geometric Shapes)

        // Powerline symbols (Private Use Area)
        assert!(TerminalElement::is_decorative_character('\u{E0B0}')); // Powerline right triangle
        assert!(TerminalElement::is_decorative_character('\u{E0B2}')); // Powerline left triangle
        assert!(TerminalElement::is_decorative_character('\u{E0B4}')); // Powerline right half circle (the actual issue!)
        assert!(TerminalElement::is_decorative_character('\u{E0B6}')); // Powerline left half circle
        assert!(TerminalElement::is_decorative_character('\u{E0CA}')); // Powerline mirrored ice waveform
        assert!(TerminalElement::is_decorative_character('\u{E0D7}')); // Powerline left triangle inverted

        // Characters that should NOT be considered decorative
        assert!(!TerminalElement::is_decorative_character('A')); // Regular letter
        assert!(!TerminalElement::is_decorative_character('$')); // Symbol
        assert!(!TerminalElement::is_decorative_character(' ')); // Space
        assert!(!TerminalElement::is_decorative_character('←')); // U+2190 (Arrow, not in our ranges)
        assert!(!TerminalElement::is_decorative_character('→')); // U+2192 (Arrow, not in our ranges)
        assert!(!TerminalElement::is_decorative_character('\u{F00C}')); // Font Awesome check (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{E711}')); // Devicons (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{EA71}')); // Codicons folder (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{F401}')); // Octicons (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{1F600}')); // Emoji (not in our ranges)
    }

    #[test]
    fn test_decorative_character_boundary_cases() {
        // Test exact boundaries of our ranges
        // Box Drawing range boundaries
        assert!(TerminalElement::is_decorative_character('\u{2500}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{257F}')); // Last char
        assert!(!TerminalElement::is_decorative_character('\u{24FF}')); // Just before

        // Block Elements range boundaries
        assert!(TerminalElement::is_decorative_character('\u{2580}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{259F}')); // Last char

        // Geometric Shapes subset boundaries
        assert!(TerminalElement::is_decorative_character('\u{25A0}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{25FF}')); // Last char
        assert!(!TerminalElement::is_decorative_character('\u{2600}')); // Just after
    }

    #[test]
    fn test_decorative_characters_bypass_contrast_adjustment() {
        // Decorative characters should not be affected by contrast adjustment

        // The specific character from issue #34234
        let problematic_char = '◗'; // U+25D7
        assert!(
            TerminalElement::is_decorative_character(problematic_char),
            "Character ◗ (U+25D7) should be recognized as decorative"
        );

        // Verify some other commonly used decorative characters
        assert!(TerminalElement::is_decorative_character('│')); // Vertical line
        assert!(TerminalElement::is_decorative_character('─')); // Horizontal line
        assert!(TerminalElement::is_decorative_character('█')); // Full block
        assert!(TerminalElement::is_decorative_character('▓')); // Dark shade
        assert!(TerminalElement::is_decorative_character('■')); // Black square
        assert!(TerminalElement::is_decorative_character('●')); // Black circle

        // Verify normal text characters are NOT decorative
        assert!(!TerminalElement::is_decorative_character('A'));
        assert!(!TerminalElement::is_decorative_character('1'));
        assert!(!TerminalElement::is_decorative_character('$'));
        assert!(!TerminalElement::is_decorative_character(' '));
    }

    #[test]
    fn test_is_app_chosen_exact_color() {
        use terminal::alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

        // Indices 0..=15 are theme-overridable ANSI colors; contrast adjustment must still apply.
        assert!(!TerminalElement::is_app_chosen_exact_color(
            &Color::Indexed(0)
        ));
        assert!(!TerminalElement::is_app_chosen_exact_color(
            &Color::Indexed(15)
        ));

        // Boundary: index 16 is the first entry of the 6x6x6 cube — application-chosen.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            16
        )));
        // Interior of the cube.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            17
        )));
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            231
        )));
        // Grayscale ramp boundaries.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            232
        )));
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            255
        )));

        // 24-bit true color is always application-chosen.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Spec(
            Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        )));

        // Named colors are theme-defined and must go through contrast adjustment.
        assert!(!TerminalElement::is_app_chosen_exact_color(&Color::Named(
            NamedColor::Red
        )));
        assert!(!TerminalElement::is_app_chosen_exact_color(&Color::Named(
            NamedColor::Foreground
        )));
    }

    #[test]
    fn test_contrast_adjustment_logic() {
        // Test the core contrast adjustment logic without needing full app context

        // Test case 1: Light colors (poor contrast)
        let white_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 1.0,
            a: 1.0,
        };
        let light_gray_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.95,
            a: 1.0,
        };

        // Should have poor contrast
        let actual_contrast = apca_contrast(white_fg, light_gray_bg).abs();
        assert!(
            actual_contrast < 30.0,
            "White on light gray should have poor APCA contrast: {}",
            actual_contrast
        );

        // After adjustment with minimum APCA contrast of 45, should be darker
        let adjusted = ensure_minimum_contrast(white_fg, light_gray_bg, 45.0);
        assert!(
            adjusted.l < white_fg.l,
            "Adjusted color should be darker than original"
        );
        let adjusted_contrast = apca_contrast(adjusted, light_gray_bg).abs();
        assert!(adjusted_contrast >= 45.0, "Should meet minimum contrast");

        // Test case 2: Dark colors (poor contrast)
        let black_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 1.0,
        };
        let dark_gray_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.05,
            a: 1.0,
        };

        // Should have poor contrast
        let actual_contrast = apca_contrast(black_fg, dark_gray_bg).abs();
        assert!(
            actual_contrast < 30.0,
            "Black on dark gray should have poor APCA contrast: {}",
            actual_contrast
        );

        // After adjustment with minimum APCA contrast of 45, should be lighter
        let adjusted = ensure_minimum_contrast(black_fg, dark_gray_bg, 45.0);
        assert!(
            adjusted.l > black_fg.l,
            "Adjusted color should be lighter than original"
        );
        let adjusted_contrast = apca_contrast(adjusted, dark_gray_bg).abs();
        assert!(adjusted_contrast >= 45.0, "Should meet minimum contrast");

        // Test case 3: Already good contrast
        let good_contrast = ensure_minimum_contrast(black_fg, white_fg, 45.0);
        assert_eq!(
            good_contrast, black_fg,
            "Good contrast should not be adjusted"
        );
    }

    #[test]
    fn test_true_color_red_blue_not_washed_out_on_dark_bg() {
        // Red and blue have inherently low perceptual luminance in APCA.
        // Pure #ff0000 only achieves Lc ~35 against #1e1e1e — below the
        // default Lc 45 threshold. ensure_minimum_contrast would lighten
        // them, washing out the color. This is why cell_style skips the
        // adjustment for Color::Spec (24-bit true color).
        let dark_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.05,
            a: 1.0,
        };

        for (name, r, g, b) in [
            ("red", 225, 80, 80),
            ("blue", 80, 80, 225),
            ("pure red", 255, 0, 0),
        ] {
            let color = terminal::rgba_color(r, g, b);
            let contrast = apca_contrast(color, dark_bg).abs();
            assert!(
                contrast < 45.0,
                "{name} should have APCA < 45 on dark bg, got {contrast}",
            );

            let adjusted = ensure_minimum_contrast(color, dark_bg, 45.0);
            assert!(
                adjusted.l > color.l,
                "{name} would be lightened by contrast adjustment (l: {} -> {})",
                color.l,
                adjusted.l,
            );
        }
    }

    #[test]
    fn test_white_on_white_contrast_issue() {
        // This test reproduces the exact issue from the bug report
        // where white ANSI text on white background should be adjusted

        // Simulate One Light theme colors
        let white_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.98, // #fafafaff is approximately 98% lightness
            a: 1.0,
        };
        let white_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.98, // Same as foreground - this is the problem!
            a: 1.0,
        };

        // With minimum contrast of 0.0, no adjustment should happen
        let no_adjust = ensure_minimum_contrast(white_fg, white_bg, 0.0);
        assert_eq!(no_adjust, white_fg, "No adjustment with min_contrast 0.0");

        // With minimum APCA contrast of 15, it should adjust to a darker color
        let adjusted = ensure_minimum_contrast(white_fg, white_bg, 15.0);
        assert!(
            adjusted.l < white_fg.l,
            "White on white should become darker, got l={}",
            adjusted.l
        );

        // Verify the contrast is now acceptable
        let new_contrast = apca_contrast(adjusted, white_bg).abs();
        assert!(
            new_contrast >= 15.0,
            "Adjusted APCA contrast {} should be >= 15.0",
            new_contrast
        );
    }

    #[test]
    fn test_batched_text_run_can_append() {
        let style1 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let style2 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let style3 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::blue(), // Different color
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let batch = BatchedTextRun::new_from_char(AlacPoint::new(0, 0), 'a', style1, font_size);

        // Should be able to append same style
        assert!(batch.can_append(&style2));

        // Should not be able to append different style
        assert!(!batch.can_append(&style3));
    }

    #[test]
    fn test_batched_text_run_append() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch = BatchedTextRun::new_from_char(AlacPoint::new(0, 0), 'a', style, font_size);

        assert_eq!(batch.text, "a");
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1);

        batch.append_char('b');

        assert_eq!(batch.text, "ab");
        assert_eq!(batch.cell_count, 2);
        assert_eq!(batch.style.len, 2);

        batch.append_char('c');

        assert_eq!(batch.text, "abc");
        assert_eq!(batch.cell_count, 3);
        assert_eq!(batch.style.len, 3);
    }

    #[test]
    fn test_batched_text_run_append_char() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch = BatchedTextRun::new_from_char(AlacPoint::new(0, 0), 'x', style, font_size);

        assert_eq!(batch.text, "x");
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1);

        batch.append_char('y');

        assert_eq!(batch.text, "xy");
        assert_eq!(batch.cell_count, 2);
        assert_eq!(batch.style.len, 2);

        // Test with multi-byte character
        batch.append_char('😀');

        assert_eq!(batch.text, "xy😀");
        assert_eq!(batch.cell_count, 3);
        assert_eq!(batch.style.len, 6); // 1 + 1 + 4 bytes for emoji
    }

    #[test]
    fn test_batched_text_run_append_zero_width_char() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch = BatchedTextRun::new_from_char(AlacPoint::new(0, 0), 'x', style, font_size);

        let combining = '\u{0301}';
        batch.append_zero_width_chars(&[combining]);

        assert_eq!(batch.text, format!("x{}", combining));
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1 + combining.len_utf8());
    }

    #[test]
    fn test_background_region_can_merge() {
        let color1 = Hsla::red();
        let color2 = Hsla::blue();

        // Test horizontal merging
        let mut region1 = BackgroundRegion::new(0, 0, color1);
        region1.end_col = 5;
        let region2 = BackgroundRegion::new(0, 6, color1);
        assert!(region1.can_merge_with(&region2));

        // Test vertical merging with same column span
        let mut region3 = BackgroundRegion::new(0, 0, color1);
        region3.end_col = 5;
        let mut region4 = BackgroundRegion::new(1, 0, color1);
        region4.end_col = 5;
        assert!(region3.can_merge_with(&region4));

        // Test cannot merge different colors
        let region5 = BackgroundRegion::new(0, 0, color1);
        let region6 = BackgroundRegion::new(0, 1, color2);
        assert!(!region5.can_merge_with(&region6));

        // Test cannot merge non-adjacent regions
        let region7 = BackgroundRegion::new(0, 0, color1);
        let region8 = BackgroundRegion::new(0, 2, color1);
        assert!(!region7.can_merge_with(&region8));

        // Test cannot merge vertical regions with different column spans
        let mut region9 = BackgroundRegion::new(0, 0, color1);
        region9.end_col = 5;
        let mut region10 = BackgroundRegion::new(1, 0, color1);
        region10.end_col = 6;
        assert!(!region9.can_merge_with(&region10));
    }

    #[test]
    fn test_background_region_merge() {
        let color = Hsla::red();

        // Test horizontal merge
        let mut region1 = BackgroundRegion::new(0, 0, color);
        region1.end_col = 5;
        let mut region2 = BackgroundRegion::new(0, 6, color);
        region2.end_col = 10;
        region1.merge_with(&region2);
        assert_eq!(region1.start_col, 0);
        assert_eq!(region1.end_col, 10);
        assert_eq!(region1.start_line, 0);
        assert_eq!(region1.end_line, 0);

        // Test vertical merge
        let mut region3 = BackgroundRegion::new(0, 0, color);
        region3.end_col = 5;
        let mut region4 = BackgroundRegion::new(1, 0, color);
        region4.end_col = 5;
        region3.merge_with(&region4);
        assert_eq!(region3.start_col, 0);
        assert_eq!(region3.end_col, 5);
        assert_eq!(region3.start_line, 0);
        assert_eq!(region3.end_line, 1);
    }

    #[test]
    fn test_merge_background_regions() {
        let color = Hsla::red();

        // Test merging multiple adjacent regions
        let regions = vec![
            BackgroundRegion::new(0, 0, color),
            BackgroundRegion::new(0, 1, color),
            BackgroundRegion::new(0, 2, color),
            BackgroundRegion::new(1, 0, color),
            BackgroundRegion::new(1, 1, color),
            BackgroundRegion::new(1, 2, color),
        ];

        let merged = merge_background_regions(regions);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_line, 0);
        assert_eq!(merged[0].end_line, 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 2);

        // Test with non-mergeable regions
        let color2 = Hsla::blue();
        let regions2 = vec![
            BackgroundRegion::new(0, 0, color),
            BackgroundRegion::new(0, 2, color),  // Gap at column 1
            BackgroundRegion::new(1, 0, color2), // Different color
        ];

        let merged2 = merge_background_regions(regions2);
        assert_eq!(merged2.len(), 3);
    }

    #[test]
    fn test_screen_position_filtering_with_positive_lines() {
        // Test the unified screen-position-based filtering approach.
        // This works for both Scrollable and Inline modes because we filter
        // by enumerated line group index, not by cell.point.line values.
        use itertools::Itertools;
        use terminal::IndexedCell;
        use terminal::alacritty_terminal::index::{Column, Line, Point as AlacPoint};
        use terminal::alacritty_terminal::term::cell::Cell;

        // Create mock cells for lines 0-23 (typical terminal with 24 visible lines)
        let mut cells = Vec::new();
        for line in 0..24i32 {
            for col in 0..3i32 {
                cells.push(IndexedCell {
                    point: AlacPoint::new(Line(line), Column(col as usize)),
                    cell: Cell::default(),
                });
            }
        }

        // Scenario: Terminal partially scrolled above viewport
        // First 5 lines (0-4) are clipped, lines 5-15 should be visible
        let rows_above_viewport = 5usize;
        let visible_row_count = 11usize;

        // Apply the same filtering logic as in the render code
        let filtered: Vec<_> = cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        // Should have lines 5-15 (11 lines * 3 cells each = 33 cells)
        assert_eq!(filtered.len(), 11 * 3, "Should have 33 cells for 11 lines");

        // First filtered cell should be line 5
        assert_eq!(
            filtered.first().unwrap().point.line,
            Line(5),
            "First cell should be on line 5"
        );

        // Last filtered cell should be line 15
        assert_eq!(
            filtered.last().unwrap().point.line,
            Line(15),
            "Last cell should be on line 15"
        );
    }

    #[test]
    fn test_screen_position_filtering_with_negative_lines() {
        // This is the key test! In Scrollable mode, cells have NEGATIVE line numbers
        // for scrollback history. The screen-position filtering approach works because
        // we filter by enumerated line group index, not by cell.point.line values.
        use itertools::Itertools;
        use terminal::IndexedCell;
        use terminal::alacritty_terminal::index::{Column, Line, Point as AlacPoint};
        use terminal::alacritty_terminal::term::cell::Cell;

        // Simulate cells from a scrolled terminal with scrollback
        // These have negative line numbers representing scrollback history
        let mut scrollback_cells = Vec::new();
        for line in -588i32..=-578i32 {
            for col in 0..80i32 {
                scrollback_cells.push(IndexedCell {
                    point: AlacPoint::new(Line(line), Column(col as usize)),
                    cell: Cell::default(),
                });
            }
        }

        // Scenario: First 3 screen rows clipped, show next 5 rows
        let rows_above_viewport = 3usize;
        let visible_row_count = 5usize;

        // Apply the same filtering logic as in the render code
        let filtered: Vec<_> = scrollback_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        // Should have 5 lines * 80 cells = 400 cells
        assert_eq!(filtered.len(), 5 * 80, "Should have 400 cells for 5 lines");

        // First filtered cell should be line -585 (skipped 3 lines from -588)
        assert_eq!(
            filtered.first().unwrap().point.line,
            Line(-585),
            "First cell should be on line -585"
        );

        // Last filtered cell should be line -581 (5 lines: -585, -584, -583, -582, -581)
        assert_eq!(
            filtered.last().unwrap().point.line,
            Line(-581),
            "Last cell should be on line -581"
        );
    }

    #[test]
    fn test_screen_position_filtering_skip_all() {
        // Test what happens when we skip more rows than exist
        use itertools::Itertools;
        use terminal::IndexedCell;
        use terminal::alacritty_terminal::index::{Column, Line, Point as AlacPoint};
        use terminal::alacritty_terminal::term::cell::Cell;

        let mut cells = Vec::new();
        for line in 0..10i32 {
            cells.push(IndexedCell {
                point: AlacPoint::new(Line(line), Column(0)),
                cell: Cell::default(),
            });
        }

        // Skip more rows than exist
        let rows_above_viewport = 100usize;
        let visible_row_count = 5usize;

        let filtered: Vec<_> = cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        assert_eq!(
            filtered.len(),
            0,
            "Should have no cells when all are skipped"
        );
    }

    #[test]
    fn test_layout_grid_positioning_math() {
        // Test the math that layout_grid uses for positioning.
        // When we skip N rows, we pass N as start_line_offset to layout_grid,
        // which positions the first visible line at screen row N.

        // Scenario: Terminal at y=-100px, line_height=20px
        // First 5 screen rows are above viewport (clipped)
        // So we skip 5 rows and pass offset=5 to layout_grid

        let terminal_origin_y = -100.0f32;
        let line_height = 20.0f32;
        let rows_skipped = 5;

        // The first visible line (at offset 5) renders at:
        // y = terminal_origin + offset * line_height = -100 + 5*20 = 0
        let first_visible_y = terminal_origin_y + rows_skipped as f32 * line_height;
        assert_eq!(
            first_visible_y, 0.0,
            "First visible line should be at viewport top (y=0)"
        );

        // The 6th visible line (at offset 10) renders at:
        let sixth_visible_y = terminal_origin_y + (rows_skipped + 5) as f32 * line_height;
        assert_eq!(
            sixth_visible_y, 100.0,
            "6th visible line should be at y=100"
        );
    }

    #[test]
    fn test_unified_filtering_works_for_both_modes() {
        // This test proves that the unified screen-position filtering approach
        // works for BOTH positive line numbers (Inline mode) and negative line
        // numbers (Scrollable mode with scrollback).
        //
        // The key insight: we filter by enumerated line group index (screen position),
        // not by cell.point.line values. This makes the filtering agnostic to the
        // actual line numbers in the cells.
        use itertools::Itertools;
        use terminal::IndexedCell;
        use terminal::alacritty_terminal::index::{Column, Line, Point as AlacPoint};
        use terminal::alacritty_terminal::term::cell::Cell;

        // Test with positive line numbers (Inline mode style)
        let positive_cells: Vec<_> = (0..10i32)
            .flat_map(|line| {
                (0..3i32).map(move |col| IndexedCell {
                    point: AlacPoint::new(Line(line), Column(col as usize)),
                    cell: Cell::default(),
                })
            })
            .collect();

        // Test with negative line numbers (Scrollable mode with scrollback)
        let negative_cells: Vec<_> = (-10i32..0i32)
            .flat_map(|line| {
                (0..3i32).map(move |col| IndexedCell {
                    point: AlacPoint::new(Line(line), Column(col as usize)),
                    cell: Cell::default(),
                })
            })
            .collect();

        let rows_to_skip = 3usize;
        let rows_to_take = 4usize;

        // Filter positive cells
        let positive_filtered: Vec<_> = positive_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_to_skip)
            .take(rows_to_take)
            .flat_map(|(_, cells)| cells)
            .collect();

        // Filter negative cells
        let negative_filtered: Vec<_> = negative_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_to_skip)
            .take(rows_to_take)
            .flat_map(|(_, cells)| cells)
            .collect();

        // Both should have same count: 4 lines * 3 cells = 12
        assert_eq!(positive_filtered.len(), 12);
        assert_eq!(negative_filtered.len(), 12);

        // Positive: lines 3, 4, 5, 6
        assert_eq!(positive_filtered.first().unwrap().point.line, Line(3));
        assert_eq!(positive_filtered.last().unwrap().point.line, Line(6));

        // Negative: lines -7, -6, -5, -4
        assert_eq!(negative_filtered.first().unwrap().point.line, Line(-7));
        assert_eq!(negative_filtered.last().unwrap().point.line, Line(-4));
    }
}
