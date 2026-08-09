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

                // Skip Kitty Unicode-placeholder-mode cells — this glyph
                // (a private-use codepoint, KITTY_GRAPHICS_PLAN.md Stage 4)
                // has no real appearance of its own (most fonts render it
                // as a missing-glyph box) and is painted as an image
                // instead by paint_kitty_unicode_placeholders, which scans
                // the same grid independently of this text layout pass.
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

                    // Kitty graphics placements with a negative z-index draw
                    // UNDER text — must happen before the text batches below.
                    // Non-negative (including the default, 0) draw OVER text
                    // — see the second paint_kitty_placements call below,
                    // after the text batches.
                    let mut any_kitty_animating =
                        paint_kitty_placements(&self.terminal, origin, layout, window, cx, |z| z < 0);

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

                    // Non-negative z-index placements draw OVER text — see
                    // the under-text call above, before the highlight/text
                    // painting, for the negative-z-index half of this split.
                    any_kitty_animating |=
                        paint_kitty_placements(&self.terminal, origin, layout, window, cx, |z| z >= 0);

                    // Unicode-placeholder-mode images (KITTY_GRAPHICS_PLAN.md
                    // Stage 4) — anchored to grid cells written as ordinary
                    // (if specially-encoded) text, so they paint alongside
                    // the text batches rather than through the cursor-
                    // relative PlacementInfo path above.
                    any_kitty_animating |=
                        paint_kitty_unicode_placeholders(&self.terminal, origin, layout, window, cx);

                    // Som's own rich-content protocol (SRP) — entirely
                    // independent from Kitty above (own store, own
                    // marker byte), painted over text like Kitty's
                    // non-negative-z-index placements since SRP has no
                    // z-index concept of its own yet. Shares the same
                    // `any_kitty_animating`-driven force-redraw/animation-
                    // frame logic below rather than a separate throttle —
                    // see `Terminal::kitty_force_redraw_due`'s doc comment
                    // for why one shared budget matters (two independent
                    // throttles could each individually stay "due" and
                    // double the effective forced-redraw rate).
                    any_kitty_animating |=
                        paint_rich_content_placements(&self.terminal, origin, layout, window, cx);

                    // At least one Kitty graphics animation is currently
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
                    // Kitty transmits in `Terminal::process_event`'s
                    // `ApcString` handler, here for animation frame
                    // advancement instead of chunk arrival. An explicit
                    // forced native repaint (unconditional `window.refresh()`
                    // + `draw()` + `present()`, see
                    // `PlatformWindow::force_redraw`'s doc comment) is
                    // needed to actually get the next frame on screen
                    // rather than just have it sit correctly decoded in
                    // `ImageStore` forever.
                    //
                    // MUST be throttled the same way `process_event`'s own
                    // chunk-arrival redraw is (`Terminal::kitty_force_redraw_due`,
                    // sharing that same timer/budget) — the earlier
                    // assumption written here ("paint() itself only runs as
                    // often as a real repaint already happens, so this
                    // can't runaway") was WRONG: `force_redraw_windows()`
                    // synchronously does a full `draw()+present()` and
                    // `request_animation_frame()` schedules another `paint()`
                    // right after, so once an animation is active this
                    // becomes a tight self-sustaining paint -> force_redraw
                    // -> paint loop with no pacing at all, confirmed live to
                    // starve the PTY-reader/event-loop task of CPU time
                    // badly enough that a single somcat invocation (one
                    // 500x500 GIF, 47 frames) took over a minute end-to-end
                    // instead of the ~5-10s headless benchmarks
                    // (`bench_somcat_*` in `crates/terminal/src/terminal.rs`)
                    // consistently show with no competing native repaint
                    // loop. `read(cx)` (immutable), NOT `Entity::update` —
                    // going through `update` from inside an in-progress
                    // paint call was tried and reverted earlier in this
                    // same investigation: it broke even the FIRST frame
                    // from ever appearing.
                    if any_kitty_animating {
                        window.request_animation_frame();
                        if self.terminal.read(cx).kitty_force_redraw_due() {
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

/// Paint every currently-live Kitty graphics protocol placement whose
/// z-index satisfies `z_filter` — called twice from `paint` (see call
/// sites), once for under-text (`z < 0`) and once for over-text (`z >= 0`)
/// placements, so images correctly layer with the surrounding text instead
/// of all landing in one z-order relative to it.
///
/// Only cursor-relative placements (`KITTY_GRAPHICS_PLAN.md` Stage 3) are
/// handled here — unicode-placeholder-mode placements (Stage 4) render
/// through the normal text-cell path instead, since they're anchored to
/// specific grid cells written as regular (if specially-encoded) glyphs.
/// Returns `true` if at least one painted placement is a currently-playing
/// animation — the caller uses this to decide whether to call
/// `window.request_animation_frame()` so GPUI keeps repainting this element
/// on its own (see this function's callers in `TerminalElement::paint`).
fn paint_kitty_placements(
    terminal: &Entity<Terminal>,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
    cx: &mut App,
    z_filter: impl Fn(i32) -> bool,
) -> bool {
    let terminal = terminal.read(cx);

    // Cursor-relative placements (unlike Unicode placeholders — see this
    // function's doc comment) are anchored to absolute main-screen grid
    // coordinates tracked entirely outside alacritty_terminal's own grid,
    // so switching to the alt screen (vim, less, any full-screen TUI) does
    // NOT automatically stop them from being iterated here the way it does
    // for placeholder cells. Kitty's own terminal parks (hides, without
    // discarding) main-screen placements while the alt screen is active —
    // matched here by simply skipping the paint pass entirely; the
    // placements themselves stay in ImageStore untouched and reappear the
    // moment the alt screen exits, same as the text underneath them
    // already does for free.
    if terminal.last_content().mode.contains(TermMode::ALT_SCREEN) {
        return false;
    }

    let num_lines = layout.dimensions.num_lines() as i32;
    let num_columns = layout.dimensions.num_columns();
    let mut any_animating = false;

    for (_image_id, _placement_id, info, image) in terminal.kitty_placements() {
        if !z_filter(info.z_index) {
            continue;
        }

        let Some((position, size)) = kitty_placement_bounds(
            info.anchor,
            info.columns,
            info.rows,
            layout.display_offset,
            num_lines,
            num_columns,
            origin,
            layout.dimensions.cell_width,
            layout.dimensions.line_height,
        ) else {
            // Scrolled out of view — nothing to paint this frame. Not an
            // error: this is the normal state for a placement anchored to
            // scrollback content the user isn't currently looking at.
            continue;
        };

        any_animating |= image.is_animating();
        window
            .paint_image(
                Bounds::new(position, size),
                gpui::Corners::all(Pixels::ZERO),
                image.render_image.clone(),
                image.current_frame(),
                false,
            )
            .log_err();
    }

    any_animating
}

/// Paints Som's own rich-content protocol images/animations
/// ([`terminal::rich_content_transport`]) — independent from
/// [`paint_kitty_placements`] above (separate protocol, separate store,
/// see that module's doc comment for why), but reusing the exact same
/// grid-to-pixel math ([`kitty_placement_bounds`]) since both protocols
/// anchor a placement to a cursor-relative grid cell the same way.
///
/// SRP has no `columns`/`rows` sizing command the way Kitty's `a=p` does
/// (see `rich_content_cache::CacheEntry::anchor`'s doc comment on why
/// placement is implicit, decided by first-chunk arrival rather than an
/// explicit command) — so the footprint is always derived from the
/// decoded frame's own natural pixel size via [`natural_cell_span`]
/// instead of an explicit override, unlike Kitty's `columns`/`rows`
/// (which — see `kitty_placement_bounds`'s own doc comment — currently
/// default to a plain 1x1 cell when unset, since Kitty senders routinely
/// DO specify `c=`/`r=` and this codebase hasn't needed a natural-size
/// fallback there yet).
fn paint_rich_content_placements(
    terminal: &Entity<Terminal>,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let terminal = terminal.read(cx);
    if terminal.last_content().mode.contains(TermMode::ALT_SCREEN) {
        return false;
    }

    let num_lines = layout.dimensions.num_lines() as i32;
    let num_columns = layout.dimensions.num_columns();
    let mut any_animating = false;

    for (anchor, render_image, current_frame, is_animating) in terminal.rich_content_placements() {
        let (columns, rows) =
            natural_cell_span(render_image.size(0), layout.dimensions.cell_width, layout.dimensions.line_height);
        let Some((position, size)) = kitty_placement_bounds(
            anchor,
            Some(columns),
            Some(rows),
            layout.display_offset,
            num_lines,
            num_columns,
            origin,
            layout.dimensions.cell_width,
            layout.dimensions.line_height,
        ) else {
            continue;
        };

        any_animating |= is_animating;
        window
            .paint_image(Bounds::new(position, size), gpui::Corners::all(Pixels::ZERO), render_image, current_frame, false)
            .log_err();
    }

    any_animating
}

/// How many grid cells wide/tall a decoded frame's own pixel dimensions
/// naturally span, given the current font's cell metrics — `ceil`'d up
/// (a frame narrower than one full cell still needs at least 1 column/row
/// to be visible at all, and rounding DOWN would clip the last partial
/// cell's worth of image data instead of just leaving a little unused
/// margin in it). `image_size` and `cell_width`/`line_height` are both
/// treated as the same unit here (as the rest of this module's grid-to-
/// pixel math already does for `cell_width`/`line_height` themselves —
/// see `kitty_placement_bounds`) rather than converting through a device-
/// pixel-vs-logical-pixel scale factor, since the visible result is a
/// GIF stretched to fill whole cells either way, not pixel-perfect 1:1
/// image-to-screen mapping.
fn natural_cell_span(image_size: gpui::Size<gpui::DevicePixels>, cell_width: Pixels, line_height: Pixels) -> (u32, u32) {
    let width_px: f32 = i32::from(image_size.width) as f32;
    let height_px: f32 = i32::from(image_size.height) as f32;
    let columns = (width_px / f32::from(cell_width)).ceil().max(1.0) as u32;
    let rows = (height_px / f32::from(line_height)).ceil().max(1.0) as u32;
    (columns, rows)
}

/// Compute a Kitty graphics placement's paint bounds, or `None` if it's
/// currently scrolled out of the visible viewport. Pulled out of
/// `paint_kitty_placements` as a pure function (no `Window`/`App`
/// dependency) specifically so the grid-to-pixel math is unit-testable
/// without a full GPUI paint context.
#[allow(clippy::too_many_arguments)]
fn kitty_placement_bounds(
    anchor: (i32, usize),
    columns: Option<u32>,
    rows: Option<u32>,
    display_offset: usize,
    num_lines: i32,
    num_columns: usize,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
) -> Option<(Point<Pixels>, gpui::Size<Pixels>)> {
    // anchor is stored in absolute grid coordinates (see
    // PlacementInfo::anchor's doc comment) — display_offset converts that to
    // the currently-scrolled viewport's coordinate space, exactly mirroring
    // DisplayCursor::from's formula for the terminal cursor itself.
    let display_line = anchor.0 + display_offset as i32;
    if display_line < 0 || display_line >= num_lines {
        return None;
    }
    if anchor.1 >= num_columns {
        return None;
    }

    // columns/rows (c=/r= in the protocol) override the image's natural
    // cell footprint when the sender wants to scale it; default to a single
    // cell when unset. A more accurate natural-size fallback (deriving cell
    // span from the image's pixel dimensions and current cell metrics) is a
    // reasonable follow-up, not required for this stage to be useful.
    let cell_columns = columns.unwrap_or(1).max(1) as f32;
    let cell_rows = rows.unwrap_or(1).max(1) as f32;

    let position = point(
        origin.x + anchor.1 as f32 * cell_width,
        origin.y + display_line as f32 * line_height,
    );
    let size = gpui::size(cell_width * cell_columns, line_height * cell_rows);

    Some((position, size))
}

/// Scan the currently-visible grid for Kitty Unicode-placeholder-mode
/// cells (`KITTY_GRAPHICS_PLAN.md` Stage 4) and paint the image(s) they
/// reference.
///
/// Unlike cursor-relative placements ([`paint_kitty_placements`]), these
/// aren't tracked in `ImageStore`'s `placements` map at all — the grid
/// cells themselves (each one a [`kitty_graphics_placeholder::PLACEHOLDER_CHAR`]
/// glyph plus row/column-encoding combining diacritics, written by
/// whatever program is using this mode, e.g. `yazi`) ARE the placement.
/// This function only needs `ImageStore::image(id)` to resolve pixel data —
/// positioning comes entirely from where those cells landed in the grid.
fn paint_kitty_unicode_placeholders(
    terminal: &Entity<Terminal>,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let terminal = terminal.read(cx);
    let cell_width = layout.dimensions.cell_width;
    let line_height = layout.dimensions.line_height;
    let mut any_animating = false;

    // Group every placeholder cell seen this frame by (image_id,
    // placement_id): a multi-cell image is written as many adjacent
    // placeholder glyphs, one per covered cell, and the group's on-screen
    // bounding box (in display-relative line/column units) is exactly the
    // rectangle the whole image should be painted into — painting the full
    // image once per group, not once per cell, since paint_image has no
    // sub-region/crop support to draw just one cell's slice of it.
    let mut groups: std::collections::HashMap<(u32, u32), CellGroupBounds> =
        std::collections::HashMap::new();

    for indexed_cell in &terminal.last_content().cells {
        let fg_rgb = match indexed_cell.cell.fg {
            AnsiColor::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
            // Named/Indexed colors resolve through the active theme
            // palette (see convert_color) and can't round-trip an
            // arbitrary 24-bit id — Kitty's own spec requires senders to
            // emit true-color escapes for placeholder mode specifically
            // so the id survives exactly. A cell using a non-true-color
            // fg is therefore never a valid placeholder cell.
            _ => continue,
        };
        let underline_rgb = match indexed_cell.cell.underline_color() {
            Some(AnsiColor::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
            Some(_) => continue,
            None => None,
        };
        let diacritics = indexed_cell.cell.zerowidth().unwrap_or(&[]);
        let Some(PlaceholderCell { image_id, placement_id, .. }) =
            kitty_graphics_placeholder::decode_placeholder_cell(
                indexed_cell.cell.c,
                fg_rgb,
                underline_rgb,
                diacritics,
            )
        else {
            continue;
        };

        let line = indexed_cell.point.line.0;
        let column = indexed_cell.point.column.0 as i32;
        groups
            .entry((image_id, placement_id))
            .and_modify(|bounds| bounds.extend(line, column))
            .or_insert_with(|| CellGroupBounds::new(line, column));
    }

    if !groups.is_empty() {
        log::trace!(
            "kitty placeholders: {} group(s): {:?}",
            groups.len(),
            groups.keys().collect::<Vec<_>>()
        );
    }

    for ((image_id, _placement_id), bounds) in groups {
        let Some(image) = terminal.kitty_image(image_id) else {
            log::trace!("kitty placeholders: no stored image for id {image_id}");
            // Placeholder cells reference an image id the sender never
            // transmitted (or that was since deleted) — nothing to paint,
            // not an error (the program driving this may simply not have
            // sent the Transmit yet on the very first frame after writing
            // placeholder text, or deleted the image while placeholder
            // text lingered in scrollback).
            continue;
        };

        let display_line = bounds.min_line + layout.display_offset as i32;
        let display_line_end = bounds.max_line + layout.display_offset as i32;
        let num_lines = layout.dimensions.num_lines() as i32;
        if display_line_end < 0 || display_line >= num_lines {
            continue;
        }

        let position = point(
            origin.x + bounds.min_col as f32 * cell_width,
            origin.y + display_line.max(0) as f32 * line_height,
        );
        let visible_line_span = (display_line_end.min(num_lines - 1) - display_line.max(0) + 1)
            .max(1);
        let cell_area = gpui::size(
            cell_width * (bounds.max_col - bounds.min_col + 1) as f32,
            line_height * visible_line_span as f32,
        );

        // Fit the image inside the placeholder cells' rectangle preserving
        // its aspect ratio, rather than stretching it to fill. Senders pick
        // that cell rectangle by rounding the image's pixel size UP to whole
        // cells (yazi: `ceil(pixels / cell_size)`), so it's almost never
        // exactly the image's proportions — stretching to fill visibly
        // distorts every image whose size isn't an exact multiple of the
        // cell size. Anchored at the rectangle's top-left, which is where
        // the sender positioned the first placeholder cell.
        let image_size = image.render_image.size(0);
        let size = if image_size.width.0 > 0 && image_size.height.0 > 0 {
            let scale = (f32::from(cell_area.width) / image_size.width.0 as f32)
                .min(f32::from(cell_area.height) / image_size.height.0 as f32);
            gpui::size(
                px(image_size.width.0 as f32 * scale),
                px(image_size.height.0 as f32 * scale),
            )
        } else {
            cell_area
        };

        log::trace!(
            "kitty placeholders: painting image {image_id} at {position:?} size {size:?} \
             (cell area {cell_area:?}, image {}x{}, grid lines {}..{}, cols {}..{})",
            image_size.width.0,
            image_size.height.0,
            bounds.min_line,
            bounds.max_line,
            bounds.min_col,
            bounds.max_col,
        );

        any_animating |= image.is_animating();
        window
            .paint_image(
                Bounds::new(position, size),
                gpui::Corners::all(Pixels::ZERO),
                image.render_image.clone(),
                image.current_frame(),
                false,
            )
            .log_err();
    }

    any_animating
}

/// The on-screen (grid, not pixel) bounding box of one group of Unicode
/// placeholder cells sharing an (image_id, placement_id), accumulated as
/// [`paint_kitty_unicode_placeholders`] scans the visible grid.
struct CellGroupBounds {
    min_line: i32,
    max_line: i32,
    min_col: i32,
    max_col: i32,
}

impl CellGroupBounds {
    fn new(line: i32, col: i32) -> Self {
        Self { min_line: line, max_line: line, min_col: col, max_col: col }
    }

    fn extend(&mut self, line: i32, col: i32) {
        self.min_line = self.min_line.min(line);
        self.max_line = self.max_line.max(line);
        self.min_col = self.min_col.min(col);
        self.max_col = self.max_col.max(col);
    }
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
    fn kitty_placement_visible_computes_pixel_bounds() {
        let (position, size) = kitty_placement_bounds(
            (2, 5),   // anchor: line 2, column 5
            Some(10), // c=10
            Some(3),  // r=3
            0,        // display_offset: not scrolled
            24,       // num_lines
            80,       // num_columns
            point(px(0.), px(0.)), // origin
            px(8.),   // cell_width
            px(16.),  // line_height
        )
        .expect("placement within viewport should be visible");

        assert_eq!(position, point(px(40.), px(32.))); // 5*8, 2*16
        assert_eq!(size, gpui::size(px(80.), px(48.))); // 10*8, 3*16
    }

    #[test]
    fn kitty_placement_defaults_to_one_cell_when_columns_rows_unset() {
        let (_, size) = kitty_placement_bounds(
            (0, 0),
            None,
            None,
            0,
            24,
            80,
            point(px(0.), px(0.)),
            px(8.),
            px(16.),
        )
        .unwrap();

        assert_eq!(size, gpui::size(px(8.), px(16.)));
    }

    #[test]
    fn kitty_placement_scrolled_above_viewport_is_hidden() {
        // anchor line -5, but display_offset only brings it to line -2 —
        // still above the top of the viewport (line 0).
        let result = kitty_placement_bounds(
            (-5, 0),
            None,
            None,
            3,
            24,
            80,
            point(px(0.), px(0.)),
            px(8.),
            px(16.),
        );
        assert!(result.is_none());
    }

    #[test]
    fn natural_cell_span_rounds_up_to_whole_cells() {
        // A 100x50px image with 8x16 cells: 100/8 = 12.5 -> 13 columns,
        // 50/16 = 3.125 -> 4 rows. Rounding UP (not down/nearest) matters
        // here — a frame that's e.g. 8.1 cells wide still needs 9 columns
        // to show its last sliver of pixel data, not 8 (which would clip
        // it).
        let (columns, rows) = natural_cell_span(gpui::size(gpui::DevicePixels(100), gpui::DevicePixels(50)), px(8.), px(16.));
        assert_eq!((columns, rows), (13, 4));
    }

    #[test]
    fn natural_cell_span_exact_multiple_does_not_round_up_an_extra_cell() {
        // Exactly 10 cells wide/tall (80px / 8px, 160px / 16px) must stay
        // at 10, not 11 — floating point ceil() of an exact integer ratio
        // must not spuriously round up due to representation error.
        let (columns, rows) = natural_cell_span(gpui::size(gpui::DevicePixels(80), gpui::DevicePixels(160)), px(8.), px(16.));
        assert_eq!((columns, rows), (10, 10));
    }

    #[test]
    fn natural_cell_span_never_returns_zero_cells_for_a_tiny_image() {
        // A 1x1px image is still at least 1 column/row — a placement that
        // computed to 0 cells would be invisible even though it has real
        // pixel data to show.
        let (columns, rows) = natural_cell_span(gpui::size(gpui::DevicePixels(1), gpui::DevicePixels(1)), px(8.), px(16.));
        assert_eq!((columns, rows), (1, 1));
    }

    #[test]
    fn natural_cell_span_scales_with_a_realistic_gif_size() {
        // giphy.gif (the checked-in fixture used across the rich-content
        // test suite) is 480x480px — sanity-checks the whole function
        // against a real, not synthetic, size at a plausible terminal
        // font's cell metrics.
        let (columns, rows) = natural_cell_span(gpui::size(gpui::DevicePixels(480), gpui::DevicePixels(480)), px(8.), px(17.));
        assert_eq!(columns, 60); // 480 / 8, exact
        assert_eq!(rows, 29); // ceil(480 / 17) = ceil(28.235...)
    }

    #[test]
    fn kitty_placement_scrolled_into_viewport_is_visible() {
        // Same anchor as above, but display_offset now brings it exactly to
        // line 0 — the top of the viewport.
        let result = kitty_placement_bounds(
            (-5, 0),
            None,
            None,
            5,
            24,
            80,
            point(px(0.), px(0.)),
            px(8.),
            px(16.),
        );
        assert!(result.is_some());
    }

    #[test]
    fn kitty_placement_below_viewport_is_hidden() {
        let result = kitty_placement_bounds(
            (24, 0), // exactly at num_lines — one past the last visible line
            None,
            None,
            0,
            24,
            80,
            point(px(0.), px(0.)),
            px(8.),
            px(16.),
        );
        assert!(result.is_none());
    }

    #[test]
    fn kitty_placement_beyond_last_column_is_hidden() {
        let result = kitty_placement_bounds(
            (0, 80), // exactly at num_columns — one past the last column
            None,
            None,
            0,
            24,
            80,
            point(px(0.), px(0.)),
            px(8.),
            px(16.),
        );
        assert!(result.is_none());
    }

    #[test]
    fn cell_group_bounds_single_cell() {
        let bounds = CellGroupBounds::new(3, 5);
        assert_eq!((bounds.min_line, bounds.max_line), (3, 3));
        assert_eq!((bounds.min_col, bounds.max_col), (5, 5));
    }

    #[test]
    fn cell_group_bounds_extends_to_cover_every_cell() {
        // Simulates scanning a 2x3 block of placeholder cells in whatever
        // order the grid iterator happens to visit them.
        let mut bounds = CellGroupBounds::new(10, 20);
        bounds.extend(10, 21);
        bounds.extend(10, 22);
        bounds.extend(11, 20);
        bounds.extend(11, 21);
        bounds.extend(11, 22);

        assert_eq!((bounds.min_line, bounds.max_line), (10, 11));
        assert_eq!((bounds.min_col, bounds.max_col), (20, 22));
    }

    #[test]
    fn cell_group_bounds_extend_out_of_order_still_finds_correct_min_max() {
        let mut bounds = CellGroupBounds::new(5, 5);
        bounds.extend(2, 8);
        bounds.extend(9, 1);
        assert_eq!((bounds.min_line, bounds.max_line), (2, 9));
        assert_eq!((bounds.min_col, bounds.max_col), (1, 8));
    }

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
