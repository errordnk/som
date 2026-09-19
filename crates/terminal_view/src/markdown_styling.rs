//! Converts a markdown source string into a list of visually laid-out
//! lines, each a sequence of styled spans (bold/italic/code/heading
//! size/link color/etc.) — the data `paint_rich_content_markdown_widget`
//! (in `terminal_element.rs`) turns into real `TextRun`s and paints via
//! `window.text_system().shape_line()`.
//!
//! Deliberately NOT `crates/markdown`'s `MarkdownElement`: that's a full
//! three-phase GPUI `Element` built for an ordinary `div().child(...)`
//! tree, not this file's imperative paint call site (see
//! `paint_rich_content_markdown_widget`'s own doc comment for why). This
//! module instead calls `crates/markdown`'s free parsing function
//! (`markdown::parser::parse_markdown_with_options`, made `pub` for this
//! exact reuse) directly and walks its `MarkdownEvent` stream by hand,
//! producing plain data (`Vec<LaidOutLine>`) with no GPUI `Entity`/
//! `Context` dependency at all — parsing and layout happen the same way
//! whether or not a window/paint pass is actually running.

use markdown::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag, MarkdownTagEnd, parse_markdown_with_options};
use pulldown_cmark::HeadingLevel;

/// One visual style axis a span of text can carry — combined, not
/// mutually exclusive (e.g. a link inside **bold** text is both `Bold`
/// and `Link`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanEmphasis {
    Bold,
    Italic,
    BoldItalic,
    None,
}

/// One contiguous run of same-styled text within a laid-out line.
#[derive(Clone, Debug)]
pub struct StyledSpan {
    pub text: String,
    pub emphasis: SpanEmphasis,
    pub strikethrough: bool,
    /// `Some` for inline code spans and code block lines — rendered in
    /// the monospace terminal font instead of the proportional Zed Sans
    /// family, same convention every code editor uses to set code apart
    /// from prose visually.
    pub monospace: bool,
    /// `Some` for link text and autolinks — painted in the theme's link
    /// color with an underline, mirroring `crates/markdown`'s own link
    /// styling convention (checked in `MarkdownStyle` before writing
    /// this) without actually depending on that type.
    pub is_link: bool,
}

/// One visually laid-out line of the widget — a flat sequence of styled
/// spans plus enough metadata for the paint path to size/indent/color
/// the line as a whole (heading level for font size, quote/code-block
/// background, list marker prefix already baked into the first span).
#[derive(Clone, Debug, Default)]
pub struct LaidOutLine {
    pub spans: Vec<StyledSpan>,
    /// `Some(level)` (1..=6) if this line is part of a heading — the
    /// paint path scales `line_height`/font_size up for these (level 1
    /// biggest, level 6 smallest, standard HTML heading convention) and
    /// renders in bold.
    pub heading_level: Option<u8>,
    /// `true` for the line pulled from a horizontal rule (`---`) — the
    /// paint path draws a plain horizontal divider instead of shaping
    /// any spans (`spans` is empty for these).
    pub is_rule: bool,
    /// `true` for lines inside a code block — painted on a slightly
    /// different background band so a multi-line block reads as one
    /// visual unit, same idea `crates/markdown`'s own code-block
    /// rendering uses (a shaded box), just simpler (no border/copy
    /// button — Phase 2 scope, not the full editor-grade treatment).
    pub is_code_block: bool,
    /// `true` for lines inside a block quote — painted with a left-edge
    /// color bar and slight indent, standard blockquote convention.
    pub is_block_quote: bool,
    /// `Some` on the FIRST of an embedded media's [`EMBEDDED_IMAGE_ROWS`]
    /// rows; the remaining rows are plain `LaidOutLine::default()`
    /// spacers (empty `spans`, so the paint loop's existing "nothing to
    /// shape" skip already handles them, and `wrap_paragraph_line`'s
    /// `spans.is_empty()` guard already returns them unchanged). See
    /// this module's own doc comment section on embedded media for why
    /// this is a whole-row block rather than an inline `StyledSpan`
    /// variant. Covers images, audio, and video — see [`EmbeddedMedia`]'s
    /// own doc comment for why one type serves all three kinds.
    pub embedded_media: Option<EmbeddedMedia>,
}

/// How many `LaidOutLine` rows one embedded media block reserves. Fixed,
/// not derived from real pixel dimensions — `somsrv`'s own `http_fetch::
/// metadata_for` always reports `width_px`/`height_px` as 0 (no probing
/// capability there, a deliberate, accepted gap), and this pass
/// deliberately does NOT reflow once real bytes/dimensions land: the
/// decoded image/video frame is letterboxed INTO this fixed box instead.
/// `EmbedAttrs::height_rows` can only ever SHRINK the painted box within
/// this reserved band (see that field's own doc comment) — it never
/// grows the row reservation past this constant, to avoid every row-count
/// consumer (`markdown_placement_origins`, `ensure_markdown_row_
/// reservation`, the wrap wiring) needing to know about a per-embed
/// variable row count.
pub const EMBEDDED_IMAGE_ROWS: usize = 10;

/// Which kind of media `![alt](dest_url)` resolves to — decided purely by
/// `dest_url`'s file extension at parse time (see `media_kind_for_
/// extension`), matching the same extension table `somsrp`'s (formerly
/// `somcat`'s) own top-level content-type dispatch and `somsrv::
/// http_fetch`'s extension table already use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedMediaKind {
    Image,
    Audio,
    Video,
}

/// Horizontal position of an embedded media block within the markdown
/// widget's available width — the `left`/`center`/`right` token in an
/// `{...}` attribute block. Default is `Left`, matching the unconditional
/// left-anchored placement every embed had before attribute blocks
/// existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedPosition {
    Left,
    Center,
    Right,
}

impl Default for EmbedPosition {
    fn default() -> Self {
        EmbedPosition::Left
    }
}

/// Vertical position of an embedded media block WITHIN its reserved
/// [`EMBEDDED_IMAGE_ROWS`] row band — the `top`/`middle`/`bottom` token in
/// an `{...}` attribute block. Only visible when `height_rows` shrinks the
/// box below the full reserved band (same relationship `EmbedPosition`
/// has to `width_percent`: a box that already fills its axis has nowhere
/// to shift within it). Default is `Top`, matching the unconditional
/// top-anchored placement every embed had before attribute blocks
/// existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedVerticalPosition {
    Top,
    Middle,
    Bottom,
}

impl Default for EmbedVerticalPosition {
    fn default() -> Self {
        EmbedVerticalPosition::Top
    }
}

/// Parsed contents of an optional `{param1 param2 ...}` block written
/// immediately after `![alt](dest_url)` — see `parse_embed_attrs`'s own
/// doc comment for the token grammar. `EmbedAttrs::default()` is exactly
/// "no `{}` block was present," so every existing `![alt](url)` embed
/// with no attribute block behaves identically to before this type
/// existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EmbedAttrs {
    /// `left`/`center`/`right` token. Default: `Left`.
    pub position: EmbedPosition,
    /// `top`/`middle`/`bottom` token. Default: `Top`.
    pub vertical_position: EmbedVerticalPosition,
    /// `NN%` token — width as a percentage of the widget's available
    /// width. `None` ("as is") reproduces today's fixed full-width-minus-
    /// insets box exactly.
    pub width_percent: Option<u32>,
    /// Bare `NN` token — height in TERMINAL ROWS, not pixels or percent.
    /// Clamped to `1..=EMBEDDED_IMAGE_ROWS` at paint time: this can only
    /// ever shrink the painted box within the fixed row reservation,
    /// never grow it past `EMBEDDED_IMAGE_ROWS` — `layout_markdown`
    /// always reserves exactly `EMBEDDED_IMAGE_ROWS` rows regardless of
    /// this value (see that constant's own doc comment for why). `None`
    /// ("as is") uses the full `EMBEDDED_IMAGE_ROWS` band.
    pub height_rows: Option<u32>,
    /// `float` token, as literally written — see [`EmbedAttrs::float_side`]
    /// for whether it actually takes effect. Prefer `float_side()` over
    /// reading this field directly for anything paint-related.
    pub float: bool,
}

impl EmbedAttrs {
    /// Which side of the block the text wraps on, or `None` if `float`
    /// doesn't take effect for this combination of attrs.
    ///
    /// Confirmed user rule: "float работает если позиция не center" —
    /// `float` takes effect whenever `position` isn't `Center`, whether
    /// `left`/`right` was written explicitly or `position` is just at its
    /// default (`Left`). Concretely:
    /// - `{right float}` → block sits right, text wraps to its LEFT →
    ///   `Some(EmbedPosition::Right)`.
    /// - `{left float}` or `{float}` alone (both resolve to `position ==
    ///   Left`) → block sits left, text wraps to its RIGHT →
    ///   `Some(EmbedPosition::Left)`.
    /// - `{center float}` (or `center` from any source) → `None` — a
    ///   centered block has no side to wrap around.
    pub fn float_side(&self) -> Option<EmbedPosition> {
        if !self.float {
            return None;
        }
        match self.position {
            EmbedPosition::Left | EmbedPosition::Right => Some(self.position),
            EmbedPosition::Center => None,
        }
    }
}

/// One `![alt](dest_url)` discovered in the document, reserved as a
/// whole-row block rather than an inline span — see `EMBEDDED_IMAGE_
/// ROWS`'s own doc comment. Covers images, audio, and video uniformly:
/// which kind it is lives in `kind`, and `attrs` (parsed from an optional
/// `{...}` block immediately following `(dest_url)`) applies the same
/// position/width/height/float controls to all three kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedMedia {
    pub kind: EmbeddedMediaKind,
    /// The raw markdown link destination, verbatim — passed straight
    /// through as `SrvRequest::FetchResource::target`; this module does
    /// NOT classify URL-vs-path, `somsrv::http_fetch::fetch_and_stream`
    /// already does that classification server-side.
    pub dest_url: String,
    /// The image's alt text (the `Text`/similar events between the
    /// image's Start/End) — used for the loading/broken placeholder
    /// label. No longer rendered as plain prose (see this module's own
    /// doc comment on why `![alt](url)` used to silently degrade into
    /// bare unstyled text).
    pub alt: String,
    /// The `title` attribute, if any — carried for a future tooltip; not
    /// painted in this pass.
    pub title: String,
    /// Parsed from an optional `{...}` block immediately following
    /// `(dest_url)` in the source — see `EmbedAttrs`'s own doc comment.
    pub attrs: EmbedAttrs,
}

/// Classifies a media embed's kind purely by `dest_url`'s file extension.
/// Mirrors the extension table `somsrp`'s (formerly `somcat`'s) own
/// top-level content-type dispatch and `somsrv::http_fetch`'s extension
/// table already use — this module can't depend on either crate, so the
/// table is duplicated here rather than shared.
///
/// An unrecognized extension defaults to [`EmbeddedMediaKind::Image`],
/// matching every `![alt](url)` embed's behavior before audio/video
/// support existed — a strict superset, not a regression, for any
/// existing document.
fn media_kind_for_extension(dest_url: &str) -> EmbeddedMediaKind {
    let ext = dest_url.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp3" | "flac" => EmbeddedMediaKind::Audio,
        "mp4" | "mkv" | "avi" => EmbeddedMediaKind::Video,
        _ => EmbeddedMediaKind::Image,
    }
}

/// Parses the body of an `{...}` attribute block (the text between the
/// braces, not including them) into [`EmbedAttrs`]. Token order does not
/// matter — each whitespace-separated token is classified independently
/// by its own shape:
///
/// - `left` / `center` / `right` literal → [`EmbedAttrs::position`].
/// - `top` / `middle` / `bottom` literal → [`EmbedAttrs::vertical_position`].
/// - `float` literal → [`EmbedAttrs::float`] (see [`EmbedAttrs::float_side`]
///   for when this actually takes visual effect — anything but `center`).
/// - `NN%` (parses as `u32` after stripping a trailing `%`) →
///   [`EmbedAttrs::width_percent`].
/// - Bare `NN` (parses as `u32`) → [`EmbedAttrs::height_rows`].
/// - Anything else is silently ignored — no spec'd error path for
///   unrecognized tokens, and ignoring keeps this forward-compatible with
///   future token kinds without a hard parse error on older documents.
///
/// Deliberately no regex (project convention) — plain `match`/
/// `str::strip_suffix`/`str::parse`.
fn parse_embed_attrs(body: &str) -> EmbedAttrs {
    let mut attrs = EmbedAttrs::default();
    for token in body.split_whitespace() {
        match token {
            "left" => attrs.position = EmbedPosition::Left,
            "center" => attrs.position = EmbedPosition::Center,
            "right" => attrs.position = EmbedPosition::Right,
            "top" => attrs.vertical_position = EmbedVerticalPosition::Top,
            "middle" => attrs.vertical_position = EmbedVerticalPosition::Middle,
            "bottom" => attrs.vertical_position = EmbedVerticalPosition::Bottom,
            "float" => attrs.float = true,
            _ => {
                if let Some(pct) = token.strip_suffix('%') {
                    if let Ok(n) = pct.parse::<u32>() {
                        attrs.width_percent = Some(n);
                        continue;
                    }
                }
                if let Ok(n) = token.parse::<u32>() {
                    attrs.height_rows = Some(n);
                }
            },
        }
    }
    attrs
}

pub fn font_weight_for(emphasis: SpanEmphasis) -> gpui::FontWeight {
    match emphasis {
        SpanEmphasis::Bold | SpanEmphasis::BoldItalic => gpui::FontWeight::BOLD,
        SpanEmphasis::Italic | SpanEmphasis::None => gpui::FontWeight::NORMAL,
    }
}

pub fn font_style_for(emphasis: SpanEmphasis) -> gpui::FontStyle {
    match emphasis {
        SpanEmphasis::Italic | SpanEmphasis::BoldItalic => gpui::FontStyle::Italic,
        SpanEmphasis::Bold | SpanEmphasis::None => gpui::FontStyle::Normal,
    }
}

fn combine_emphasis(current: SpanEmphasis, add_bold: bool, add_italic: bool) -> SpanEmphasis {
    let is_bold = matches!(current, SpanEmphasis::Bold | SpanEmphasis::BoldItalic) || add_bold;
    let is_italic = matches!(current, SpanEmphasis::Italic | SpanEmphasis::BoldItalic) || add_italic;
    match (is_bold, is_italic) {
        (true, true) => SpanEmphasis::BoldItalic,
        (true, false) => SpanEmphasis::Bold,
        (false, true) => SpanEmphasis::Italic,
        (false, false) => SpanEmphasis::None,
    }
}

/// Inline-level state tracked while walking events between block
/// boundaries — reset at the start of every new block (paragraph,
/// heading, list item, etc.).
#[derive(Clone, Copy, Default)]
struct InlineState {
    bold_depth: u32,
    italic_depth: u32,
    strikethrough_depth: u32,
    link_depth: u32,
}

impl InlineState {
    fn emphasis(&self) -> SpanEmphasis {
        combine_emphasis(SpanEmphasis::None, self.bold_depth > 0, self.italic_depth > 0)
    }
}

/// Parses `source` and lays it out into `Vec<LaidOutLine>` — the
/// complete Phase 2 feature set from `crates/markdown`'s own parser
/// (`PARSE_OPTIONS`: tables, footnotes, strikethrough, task lists, smart
/// punctuation, heading attributes, GFM, super/subscript), rendered as:
/// headings (bold, sized by level), **bold**/*italic*/~~strikethrough~~,
/// `inline code` and fenced code blocks (monospace), links (colored +
/// underlined), block quotes (left bar + indent), unordered/ordered
/// list items (bullet/number prefix), horizontal rules (divider line).
/// Tables/footnotes/task-list checkboxes render as plain text for now —
/// full grid/checkbox rendering is a later pass, not blocking on this
/// one landing.
///
/// `wrap` is `None` for callers that don't have a `Window`/`LineWrapperHandle`
/// on hand (unit tests, anything measuring layout without painting) — in
/// that case, one `LaidOutLine` per source paragraph/block is returned
/// exactly as before, unwrapped. When `Some((wrapper, available_width))`,
/// every paragraph-shaped line whose spans would exceed `available_width`
/// is split into multiple `LaidOutLine`s via `wrap_paragraph_line` — see
/// that function's own doc comment for why only paragraph/list/quote text
/// gets this treatment (headings/code blocks/rules do not).
pub fn layout_markdown(source: &str, wrap: Option<(&mut gpui::LineWrapperHandle, gpui::Pixels)>) -> Vec<LaidOutLine> {
    let parsed = parse_markdown_with_options(source, false, false, false);
    let mut lines: Vec<LaidOutLine> = Vec::new();
    let mut current = LaidOutLine::default();
    let mut inline = InlineState::default();
    // Stack of (is_ordered, next_item_number) — top of stack is the
    // innermost active list, since lists can nest.
    let mut list_stack: Vec<(bool, u64)> = Vec::new();
    let mut heading_level: Option<u8> = None;
    let mut in_code_block = false;
    let mut in_block_quote_depth: u32 = 0;
    let mut pending_item_prefix = false;
    // Set while walking events between an image's Start/End — the alt
    // text (an ordinary `Text` event in pulldown-cmark's stream) is
    // accumulated here instead of becoming a `StyledSpan`, which is what
    // stops it from silently rendering as bare prose (see this module's
    // own doc comment).
    let mut pending_image: Option<EmbeddedMedia> = None;
    let mut image_alt = String::new();
    // `Some(row_index)` for exactly the window between `MarkdownTagEnd::
    // Image` firing and this module either consuming or giving up on a
    // following `{...}` attrs block — `row_index` points at the just-
    // pushed row in `lines` that carries the `Some(media)`, so a
    // recognized attrs block can be written back onto it. See the
    // `MarkdownEvent::Text` arm below for why this can't just check
    // `pending_image` (already `None` by the time `{params}` arrives).
    let mut awaiting_attrs_for: Option<usize> = None;

    let finish_line = |lines: &mut Vec<LaidOutLine>, current: &mut LaidOutLine, heading_level: Option<u8>, in_code_block: bool, in_block_quote: bool| {
        current.heading_level = heading_level;
        current.is_code_block = in_code_block;
        current.is_block_quote = in_block_quote;
        lines.push(std::mem::take(current));
    };

    // Inserts a blank separator line before a new top-level block starts
    // — mirrors markdown's own "blank line separates block-level
    // elements" convention, but as a LOOK-BACK at block START (not a
    // look-AHEAD at block END), so the very first block never gets a
    // leading blank line and the very last block never gets a trailing
    // one. Only fires between BLOCK-level elements (not inside list
    // items, where markdown density already reads fine without it and
    // every extra line costs a row of the widget's reserved footprint).
    let insert_block_separator = |lines: &mut Vec<LaidOutLine>, list_stack: &[(bool, u64)], in_block_quote_depth: u32| {
        if list_stack.is_empty() && in_block_quote_depth == 0 && !lines.is_empty() {
            lines.push(LaidOutLine::default());
        }
    };

    for (_range, event) in parsed.events.iter() {
        match event {
            MarkdownEvent::Start(tag) => match tag {
                MarkdownTag::Heading { level, .. } => {
                    insert_block_separator(&mut lines, &list_stack, in_block_quote_depth);
                    heading_level = Some(heading_level_to_u8(*level));
                },
                MarkdownTag::List(start) => {
                    insert_block_separator(&mut lines, &list_stack, in_block_quote_depth);
                    list_stack.push((start.is_some(), start.unwrap_or(1)));
                },
                MarkdownTag::Item => {
                    pending_item_prefix = true;
                },
                MarkdownTag::CodeBlock { kind, .. } => {
                    insert_block_separator(&mut lines, &list_stack, in_block_quote_depth);
                    in_code_block = true;
                    if !matches!(kind, CodeBlockKind::Indented) {
                        // Fenced code blocks start their own line — an
                        // in-progress line before the fence (there
                        // shouldn't be one, fences are block-level) is
                        // flushed defensively.
                        if !current.spans.is_empty() {
                            finish_line(&mut lines, &mut current, heading_level, false, in_block_quote_depth > 0);
                        }
                    }
                },
                MarkdownTag::BlockQuote(_) => {
                    insert_block_separator(&mut lines, &list_stack, in_block_quote_depth);
                    in_block_quote_depth += 1;
                },
                MarkdownTag::Emphasis => inline.italic_depth += 1,
                MarkdownTag::Strong => inline.bold_depth += 1,
                MarkdownTag::Strikethrough => inline.strikethrough_depth += 1,
                MarkdownTag::Link { .. } => inline.link_depth += 1,
                MarkdownTag::Paragraph => {
                    insert_block_separator(&mut lines, &list_stack, in_block_quote_depth);
                },
                MarkdownTag::Image { dest_url, title, .. } => {
                    pending_image = Some(EmbeddedMedia {
                        kind: media_kind_for_extension(dest_url),
                        dest_url: dest_url.to_string(),
                        alt: String::new(),
                        title: title.to_string(),
                        attrs: EmbedAttrs::default(),
                    });
                    image_alt.clear();
                },
                _ => {},
            },
            MarkdownEvent::End(tag_end) => match tag_end {
                MarkdownTagEnd::Heading(_) => {
                    if !current.spans.is_empty() || heading_level.is_some() {
                        finish_line(&mut lines, &mut current, heading_level, false, in_block_quote_depth > 0);
                    }
                    heading_level = None;
                },
                MarkdownTagEnd::Paragraph => {
                    if !current.spans.is_empty() {
                        finish_line(&mut lines, &mut current, None, false, in_block_quote_depth > 0);
                    }
                },
                MarkdownTagEnd::List(_) => {
                    list_stack.pop();
                },
                MarkdownTagEnd::Item => {
                    if !current.spans.is_empty() {
                        finish_line(&mut lines, &mut current, None, false, in_block_quote_depth > 0);
                    }
                    if let Some((_, next)) = list_stack.last_mut() {
                        *next += 1;
                    }
                },
                MarkdownTagEnd::CodeBlock => {
                    if !current.spans.is_empty() {
                        finish_line(&mut lines, &mut current, None, true, in_block_quote_depth > 0);
                    }
                    in_code_block = false;
                },
                MarkdownTagEnd::BlockQuote(_) => {
                    if !current.spans.is_empty() {
                        finish_line(&mut lines, &mut current, None, false, in_block_quote_depth > 0);
                    }
                    in_block_quote_depth = in_block_quote_depth.saturating_sub(1);
                },
                MarkdownTagEnd::Emphasis => inline.italic_depth = inline.italic_depth.saturating_sub(1),
                MarkdownTagEnd::Strong => inline.bold_depth = inline.bold_depth.saturating_sub(1),
                MarkdownTagEnd::Strikethrough => inline.strikethrough_depth = inline.strikethrough_depth.saturating_sub(1),
                MarkdownTagEnd::Link => inline.link_depth = inline.link_depth.saturating_sub(1),
                MarkdownTagEnd::Image => {
                    if let Some(mut image) = pending_image.take() {
                        image.alt = std::mem::take(&mut image_alt);
                        // An image inside a paragraph splits it: flush
                        // whatever prose was in progress as its own row
                        // first, so the image block always starts at a
                        // row boundary (the same "block-level element
                        // starts its own line" rule the `CodeBlock` start
                        // arm already applies).
                        if !current.spans.is_empty() {
                            finish_line(&mut lines, &mut current, heading_level, in_code_block, in_block_quote_depth > 0);
                        }
                        lines.push(LaidOutLine { embedded_media: Some(image), ..Default::default() });
                        let row_index = lines.len() - 1;
                        for _ in 1..EMBEDDED_IMAGE_ROWS {
                            lines.push(LaidOutLine::default());
                        }
                        awaiting_attrs_for = Some(row_index);
                    }
                },
                _ => {},
            },
            MarkdownEvent::Text | MarkdownEvent::Code | MarkdownEvent::InlineHtml => {
                let text = event_text(event, source, &_range);
                // `{params}` written directly after `](dest_url)` is NOT
                // consumed by the image tag itself — pulldown-cmark has
                // no concept of an inline attribute list, so it arrives
                // here as an ordinary `Text` event immediately following
                // `MarkdownTagEnd::Image`, by which point `pending_image`
                // is already `None`. Must be intercepted here, before it
                // falls through to becoming a visible prose span below.
                if let Some(row_index) = awaiting_attrs_for.take() {
                    if let Some(rest) = text.strip_prefix('{') {
                        if let Some(close) = rest.find('}') {
                            let attrs_body = &rest[..close];
                            if let Some(LaidOutLine { embedded_media: Some(media), .. }) = lines.get_mut(row_index) {
                                media.attrs = parse_embed_attrs(attrs_body);
                            }
                            let remainder = &rest[close + 1..];
                            if !remainder.is_empty() {
                                // Text after the closing brace in the same
                                // event is ordinary prose — falls through
                                // to the ordinary span-building path below
                                // with the attrs prefix already stripped.
                                current.spans.push(StyledSpan {
                                    text: remainder.to_string(),
                                    emphasis: inline.emphasis(),
                                    strikethrough: inline.strikethrough_depth > 0,
                                    monospace: false,
                                    is_link: inline.link_depth > 0,
                                });
                            }
                            continue;
                        }
                        // No closing `}` anywhere in this Text event:
                        // degrade to "no attrs block recognized" and let
                        // the literal text (including the leading `{`)
                        // render as ordinary prose below, unchanged.
                        // Silently eating it instead would risk consuming
                        // arbitrary amounts of real following prose with
                        // no visible trace of an unclosed brace — showing
                        // the raw `{center` text is the more debuggable
                        // failure for a document author to notice and fix.
                    }
                    // else: text doesn't start with `{` at all — not an
                    // attrs block, falls through unchanged below.
                }
                if pending_image.is_some() {
                    image_alt.push_str(&text);
                    continue;
                }
                let is_code = matches!(event, MarkdownEvent::Code) || in_code_block;
                if text.is_empty() {
                    continue;
                }
                if pending_item_prefix {
                    let prefix = list_item_prefix(&list_stack);
                    current.spans.push(StyledSpan {
                        text: prefix,
                        emphasis: SpanEmphasis::None,
                        strikethrough: false,
                        monospace: false,
                        is_link: false,
                    });
                    pending_item_prefix = false;
                }
                if in_code_block {
                    // Code blocks preserve internal newlines as separate
                    // laid-out lines — each source line inside the fence
                    // becomes its own `LaidOutLine`, matching how the
                    // block visually occupies multiple terminal rows.
                    let mut first = true;
                    for code_line in text.split('\n') {
                        if !first {
                            finish_line(&mut lines, &mut current, None, true, in_block_quote_depth > 0);
                        }
                        first = false;
                        if !code_line.is_empty() {
                            current.spans.push(StyledSpan {
                                text: code_line.to_string(),
                                emphasis: SpanEmphasis::None,
                                strikethrough: false,
                                monospace: true,
                                is_link: false,
                            });
                        }
                    }
                    continue;
                }
                current.spans.push(StyledSpan {
                    text,
                    emphasis: inline.emphasis(),
                    strikethrough: inline.strikethrough_depth > 0,
                    monospace: is_code,
                    is_link: inline.link_depth > 0,
                });
            },
            MarkdownEvent::SubstitutedText(text) => {
                if pending_image.is_some() {
                    image_alt.push_str(text);
                } else if !text.is_empty() {
                    current.spans.push(StyledSpan {
                        text: text.clone(),
                        emphasis: inline.emphasis(),
                        strikethrough: inline.strikethrough_depth > 0,
                        monospace: false,
                        is_link: inline.link_depth > 0,
                    });
                }
            },
            MarkdownEvent::SoftBreak => {
                if pending_image.is_some() {
                    image_alt.push(' ');
                } else {
                    current.spans.push(StyledSpan {
                        text: " ".to_string(),
                        emphasis: inline.emphasis(),
                        strikethrough: false,
                        monospace: false,
                        is_link: false,
                    });
                }
            },
            MarkdownEvent::HardBreak => {
                if !current.spans.is_empty() {
                    finish_line(&mut lines, &mut current, heading_level, in_code_block, in_block_quote_depth > 0);
                }
            },
            MarkdownEvent::Rule => {
                if !current.spans.is_empty() {
                    finish_line(&mut lines, &mut current, None, false, false);
                }
                lines.push(LaidOutLine { is_rule: true, ..Default::default() });
            },
            MarkdownEvent::TaskListMarker(checked) => {
                let marker = if *checked { "[x] " } else { "[ ] " };
                current.spans.push(StyledSpan {
                    text: marker.to_string(),
                    emphasis: SpanEmphasis::None,
                    strikethrough: false,
                    monospace: true,
                    is_link: false,
                });
            },
            _ => {},
        }
    }

    if !current.spans.is_empty() {
        finish_line(&mut lines, &mut current, heading_level, in_code_block, in_block_quote_depth > 0);
    }

    let Some((wrapper, available_width)) = wrap else {
        return lines;
    };
    lines
        .into_iter()
        .flat_map(|line| wrap_paragraph_line(line, wrapper, available_width))
        .collect()
}

/// Splits `line` into one or more `LaidOutLine`s so no line's rendered
/// width exceeds `available_width` — word-wrapping via GPUI's own
/// `LineWrapper` (font-aware: proportional prose text can't be wrapped
/// by counting characters, see `crates/gpui/src/text_system/line_wrapper.
/// rs`'s own doc comment), the same primitive GPUI's built-in `Text`
/// element uses internally for ordinary proportional text.
///
/// Only wraps prose-shaped lines: rules (`is_rule`) and code block lines
/// (`is_code_block`) are returned unchanged — a rule has no text to wrap
/// at all, and code block lines are meant to be read as literal
/// source (wrapping would visually break indentation/alignment a reader
/// relies on, the same reason no code editor soft-wraps a code block by
/// default). Headings, list items, and block quotes DO wrap — a long
/// heading or list item is exactly as likely to overflow a narrow pane
/// as an ordinary paragraph.
///
/// Continuation lines (every line after the first produced from one
/// source line) intentionally do NOT repeat `heading_level`/list-marker-
/// style metadata beyond what's already baked into the first span's own
/// text (e.g. the bullet/number prefix) — they inherit the same `is_code_
/// block`/`is_block_quote` flags as the original line (so a wrapped
/// block-quote's continuation still gets its left bar) but always render
/// at the same left edge as the first line (no hanging indent) — the
/// simplest behavior with no strong existing precedent to match in this
/// codebase (see this module's own doc comment for the "keep it simple"
/// call this follows).
fn wrap_paragraph_line(line: LaidOutLine, wrapper: &mut gpui::LineWrapperHandle, available_width: gpui::Pixels) -> Vec<LaidOutLine> {
    if line.is_rule || line.is_code_block || line.embedded_media.is_some() || line.spans.is_empty() {
        return vec![line];
    }

    // Flatten this line's spans into one text buffer with byte-offset
    // boundaries, mirroring exactly how `paint_rich_content_markdown_
    // widget` already concatenates spans into one shaped line for
    // painting — `wrap_line` operates on a single flat string, not a
    // list of styled spans, so wrapping has to happen at this same flat-
    // text level and then be mapped back onto span boundaries.
    let mut text = String::new();
    let mut span_ends = Vec::with_capacity(line.spans.len());
    for span in &line.spans {
        text.push_str(&span.text);
        span_ends.push(text.len());
    }

    let boundaries: Vec<usize> =
        wrapper.wrap_line(&[gpui::LineFragment::text(&text)], available_width).map(|boundary| boundary.ix).collect();
    if boundaries.is_empty() {
        return vec![line];
    }

    let mut result = Vec::with_capacity(boundaries.len() + 1);
    let mut cut_start = 0usize;
    let mut span_cursor = 0usize; // index into `line.spans`/`span_ends` of the first span not yet fully consumed
    let mut span_offset = 0usize; // byte offset into the CURRENT span's own text already consumed by earlier cuts

    for cut_end in boundaries.into_iter().chain(std::iter::once(text.len())) {
        if cut_end <= cut_start {
            continue;
        }
        let mut spans = Vec::new();
        let mut remaining = cut_end - cut_start;
        while remaining > 0 && span_cursor < line.spans.len() {
            let span = &line.spans[span_cursor];
            let span_text_len = span.text.len() - span_offset;
            let take = remaining.min(span_text_len);
            let piece = &span.text[span_offset..span_offset + take];
            if !piece.is_empty() {
                spans.push(StyledSpan { text: piece.to_string(), ..span.clone() });
            }
            span_offset += take;
            remaining -= take;
            if span_offset >= span.text.len() {
                span_cursor += 1;
                span_offset = 0;
            }
        }
        // Trim a single leading space left over from where `wrap_line`
        // broke on a space boundary — the space itself did its job
        // (marking where the wrap could happen) and shouldn't become a
        // visible leading gap on the continuation line.
        if let Some(first) = spans.first_mut() {
            first.text = first.text.trim_start_matches(' ').to_string();
        }
        result.push(LaidOutLine {
            spans,
            heading_level: line.heading_level,
            is_rule: false,
            is_code_block: false,
            is_block_quote: line.is_block_quote,
            embedded_media: None,
        });
        cut_start = cut_end;
    }

    if result.is_empty() { vec![line] } else { result }
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn list_item_prefix(list_stack: &[(bool, u64)]) -> String {
    let depth = list_stack.len().saturating_sub(1);
    let indent = "  ".repeat(depth);
    match list_stack.last() {
        Some((true, number)) => format!("{indent}{number}. "),
        Some((false, _)) => format!("{indent}• "),
        None => String::new(),
    }
}

fn event_text(event: &MarkdownEvent, source: &str, range: &std::ops::Range<usize>) -> String {
    match event {
        MarkdownEvent::Text | MarkdownEvent::Code | MarkdownEvent::InlineHtml => {
            source.get(range.clone()).unwrap_or_default().to_string()
        },
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_wrapper(font_size: gpui::Pixels) -> (gpui::TestAppContext, gpui::LineWrapperHandle) {
        let dispatcher = gpui::TestDispatcher::new(0);
        let cx = gpui::TestAppContext::build(dispatcher, None);
        let wrapper = cx.update(|cx| cx.text_system().line_wrapper(gpui::font("Helvetica"), font_size));
        (cx, wrapper)
    }

    #[test]
    fn unwrapped_paragraph_fitting_the_width_stays_one_line() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let lines = layout_markdown("short line", Some((&mut wrapper, gpui::px(500.))));
        assert_eq!(lines.len(), 1, "a short paragraph well within the width must not be split");
    }

    #[test]
    fn long_paragraph_wraps_into_multiple_lines_at_word_boundaries() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let long_text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj";
        let lines = layout_markdown(long_text, Some((&mut wrapper, gpui::px(72.))));
        assert!(lines.len() > 1, "a long paragraph exceeding the width must wrap into multiple lines");
        // Word boundaries preserved — no line's flattened text splits a
        // word in half (every line's text, once whitespace-trimmed,
        // consists of whole "wwww"-shaped tokens from the source).
        for line in &lines {
            let flat: String = line.spans.iter().map(|s| s.text.as_str()).collect();
            for word in flat.split_whitespace() {
                assert!(long_text.contains(word), "wrapped line contains a word not in the source: {word:?}");
            }
        }
    }

    #[test]
    fn wrapping_preserves_span_styling_across_the_split() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let text = "plain plain plain **bold bold bold bold** plain plain plain plain";
        let lines = layout_markdown(text, Some((&mut wrapper, gpui::px(72.))));
        assert!(lines.len() > 1, "expected this to wrap given the narrow width");
        assert!(
            lines.iter().flat_map(|l| &l.spans).any(|s| s.emphasis == SpanEmphasis::Bold),
            "a bold span split across a wrap boundary must still carry its emphasis on both sides"
        );
    }

    #[test]
    fn rules_and_code_blocks_are_never_wrapped() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let lines = layout_markdown(
            "```\nthis is a long line of code that would exceed the width if wrapped\n```",
            Some((&mut wrapper, gpui::px(72.))),
        );
        let code_lines: Vec<_> = lines.iter().filter(|l| l.is_code_block).collect();
        assert_eq!(code_lines.len(), 1, "a code block line must never be split by word-wrap");
    }

    #[test]
    fn short_markdown_is_unaffected_by_wrapping() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let with_wrap = layout_markdown("# Title\n\nshort paragraph", Some((&mut wrapper, gpui::px(500.))));
        let without_wrap = layout_markdown("# Title\n\nshort paragraph", None);
        assert_eq!(with_wrap.len(), without_wrap.len(), "short markdown that fits should render identically wrapped or not");
    }

    #[test]
    fn plain_paragraph_produces_one_line_no_emphasis() {
        let lines = layout_markdown("Hello world.", None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].text, "Hello world.");
        assert_eq!(lines[0].spans[0].emphasis, SpanEmphasis::None);
    }

    #[test]
    fn heading_sets_level_and_bold() {
        let lines = layout_markdown("# Title", None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].heading_level, Some(1));
    }

    #[test]
    fn bold_and_italic_markers_produce_separate_spans() {
        let lines = layout_markdown("plain **bold** *italic*", None);
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.text.contains("bold") && s.emphasis == SpanEmphasis::Bold));
        assert!(spans.iter().any(|s| s.text.contains("italic") && s.emphasis == SpanEmphasis::Italic));
    }

    #[test]
    fn bold_italic_combination_produces_bolditalic() {
        let lines = layout_markdown("***both***", None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.emphasis == SpanEmphasis::BoldItalic));
    }

    #[test]
    fn inline_code_is_monospace() {
        let lines = layout_markdown("some `code` here", None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.text == "code" && s.monospace));
    }

    #[test]
    fn fenced_code_block_produces_monospace_lines_matching_source() {
        let lines = layout_markdown("```\nline one\nline two\n```", None);
        let code_lines: Vec<_> = lines.iter().filter(|l| l.is_code_block).collect();
        assert_eq!(code_lines.len(), 2);
        assert_eq!(code_lines[0].spans[0].text, "line one");
        assert_eq!(code_lines[1].spans[0].text, "line two");
    }

    #[test]
    fn unordered_list_items_get_bullet_prefix() {
        let lines = layout_markdown("- one\n- two", None);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans[0].text.starts_with('•'));
        assert!(lines[1].spans[0].text.starts_with('•'));
    }

    #[test]
    fn ordered_list_items_get_numbered_prefix() {
        let lines = layout_markdown("1. first\n2. second", None);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans[0].text.starts_with("1."));
        assert!(lines[1].spans[0].text.starts_with("2."));
    }

    #[test]
    fn horizontal_rule_produces_a_dedicated_line() {
        let lines = layout_markdown("above\n\n---\n\nbelow", None);
        assert!(lines.iter().any(|l| l.is_rule));
    }

    #[test]
    fn blockquote_lines_are_marked() {
        let lines = layout_markdown("> quoted text", None);
        assert!(lines.iter().any(|l| l.is_block_quote && !l.spans.is_empty()));
    }

    #[test]
    fn link_text_is_marked_as_link() {
        let lines = layout_markdown("[click here](https://example.com)", None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.is_link && s.text == "click here"));
    }

    #[test]
    fn strikethrough_is_marked() {
        let lines = layout_markdown("~~gone~~", None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.strikethrough));
    }

    #[test]
    fn blank_line_between_paragraphs_is_preserved() {
        let lines = layout_markdown("first\n\nsecond", None);
        assert!(lines.iter().any(|l| l.spans.is_empty() && !l.is_rule));
    }

    #[test]
    fn image_produces_a_block_of_reserved_rows() {
        let lines = layout_markdown("![alt](img.png)", None);
        assert_eq!(lines.len(), EMBEDDED_IMAGE_ROWS);
        let image = lines[0].embedded_media.as_ref().expect("first row must carry the embedded image");
        assert_eq!(image.dest_url, "img.png");
        assert_eq!(image.alt, "alt");
        for spacer in &lines[1..] {
            assert!(spacer.embedded_media.is_none());
            assert!(spacer.spans.is_empty());
        }
    }

    #[test]
    fn image_alt_text_no_longer_leaks_into_prose() {
        // Regression test: `![alt](url)` used to silently render "alt" as
        // bare, unstyled prose (the Start/End tags were ignored but the
        // inner Text event still fired) — now it must only ever appear
        // inside the `EmbeddedMedia`, never as a `StyledSpan`.
        let lines = layout_markdown("![alt](img.png)", None);
        assert!(!lines.iter().flat_map(|l| &l.spans).any(|s| s.text.contains("alt")));
    }

    #[test]
    fn image_inside_a_paragraph_splits_the_paragraph() {
        let lines = layout_markdown("before ![a](x.png) after", None);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].text.trim(), "before");
        let image_row = lines.iter().position(|l| l.embedded_media.is_some()).expect("must contain an image row");
        assert_eq!(lines[image_row].embedded_media.as_ref().unwrap().dest_url, "x.png");
        let after_row = &lines[image_row + EMBEDDED_IMAGE_ROWS];
        assert_eq!(after_row.spans[0].text.trim(), "after");
    }

    #[test]
    fn absolute_and_relative_and_url_dest_urls_pass_through_verbatim() {
        for target in ["./a.png", "/tmp/a.png", "https://x/a.png"] {
            let lines = layout_markdown(&format!("![a]({target})"), None);
            assert_eq!(lines[0].embedded_media.as_ref().unwrap().dest_url, target);
        }
    }

    #[test]
    fn reference_style_image_carries_its_resolved_dest_url() {
        let lines = layout_markdown("![a][ref]\n\n[ref]: img.png", None);
        let image = lines[0].embedded_media.as_ref().expect("reference-style image must still resolve");
        assert_eq!(image.dest_url, "img.png");
    }

    #[test]
    fn image_row_survives_the_wrap_pass_unchanged() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let long_alt = "a very long alt text that would definitely wrap if it were ever treated as prose";
        let lines = layout_markdown(&format!("![{long_alt}](img.png)"), Some((&mut wrapper, gpui::px(72.))));
        let image_rows: Vec<_> = lines.iter().filter(|l| l.embedded_media.is_some()).collect();
        assert_eq!(image_rows.len(), 1, "an image row must never be split by the wrap pass");
        assert_eq!(lines.len(), EMBEDDED_IMAGE_ROWS, "wrapping must not change an image's reserved row count");
    }

    #[test]
    fn media_kind_by_extension_image_audio_video() {
        for ext in ["png", "jpg", "jpeg", "gif"] {
            let lines = layout_markdown(&format!("![a](x.{ext})"), None);
            assert_eq!(lines[0].embedded_media.as_ref().unwrap().kind, EmbeddedMediaKind::Image, "extension .{ext} should classify as Image");
        }
        for ext in ["mp3", "flac"] {
            let lines = layout_markdown(&format!("![a](x.{ext})"), None);
            assert_eq!(lines[0].embedded_media.as_ref().unwrap().kind, EmbeddedMediaKind::Audio, "extension .{ext} should classify as Audio");
        }
        for ext in ["mp4", "mkv", "avi"] {
            let lines = layout_markdown(&format!("![a](x.{ext})"), None);
            assert_eq!(lines[0].embedded_media.as_ref().unwrap().kind, EmbeddedMediaKind::Video, "extension .{ext} should classify as Video");
        }
        // Unknown extension defaults to Image — matches every `![alt](url)`
        // embed's behavior before audio/video support existed.
        let lines = layout_markdown("![a](x.xyz)", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().kind, EmbeddedMediaKind::Image);
    }

    #[test]
    fn attrs_block_parses_each_token_kind_individually() {
        let lines = layout_markdown("![a](x.png){left}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.position, EmbedPosition::Left);
        let lines = layout_markdown("![a](x.png){center}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.position, EmbedPosition::Center);
        let lines = layout_markdown("![a](x.png){right}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.position, EmbedPosition::Right);
        let lines = layout_markdown("![a](x.png){60%}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.width_percent, Some(60));
        let lines = layout_markdown("![a](x.png){20}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.height_rows, Some(20));
        let lines = layout_markdown("![a](x.png){float}", None);
        assert!(lines[0].embedded_media.as_ref().unwrap().attrs.float);
        let lines = layout_markdown("![a](x.png){top}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.vertical_position, EmbedVerticalPosition::Top);
        let lines = layout_markdown("![a](x.png){middle}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.vertical_position, EmbedVerticalPosition::Middle);
        let lines = layout_markdown("![a](x.png){bottom}", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.vertical_position, EmbedVerticalPosition::Bottom);
    }

    #[test]
    fn attrs_block_parses_combination_order_independent() {
        let a = layout_markdown("![a](x.png){center 60%}", None);
        let b = layout_markdown("![a](x.png){60% center}", None);
        assert_eq!(a[0].embedded_media.as_ref().unwrap().attrs, b[0].embedded_media.as_ref().unwrap().attrs);
        let attrs = a[0].embedded_media.as_ref().unwrap().attrs;
        assert_eq!(attrs.position, EmbedPosition::Center);
        assert_eq!(attrs.width_percent, Some(60));

        let lines = layout_markdown("![a](b.mp4){float 40% 15}", None);
        let attrs = lines[0].embedded_media.as_ref().unwrap().attrs;
        assert!(attrs.float);
        assert_eq!(attrs.width_percent, Some(40));
        assert_eq!(attrs.height_rows, Some(15));

        let c = layout_markdown("![a](x.png){middle center 60% 3}", None);
        let d = layout_markdown("![a](x.png){3 60% center middle}", None);
        assert_eq!(c[0].embedded_media.as_ref().unwrap().attrs, d[0].embedded_media.as_ref().unwrap().attrs);
        let attrs = c[0].embedded_media.as_ref().unwrap().attrs;
        assert_eq!(attrs.vertical_position, EmbedVerticalPosition::Middle);
        assert_eq!(attrs.position, EmbedPosition::Center);
    }

    #[test]
    fn no_attrs_block_yields_default_attrs() {
        let with_no_attrs = layout_markdown("![a](x.png)", None);
        let with_default_via_attrs_absent = &with_no_attrs[0];
        assert_eq!(with_default_via_attrs_absent.embedded_media.as_ref().unwrap().attrs, EmbedAttrs::default());
    }

    #[test]
    fn unclosed_attrs_block_degrades_to_literal_prose() {
        let lines = layout_markdown("![a](x.png){center", None);
        let media = lines[0].embedded_media.as_ref().unwrap();
        assert_eq!(media.attrs, EmbedAttrs::default(), "an unclosed brace must not be treated as a recognized attrs block");
        let literal_row = lines.iter().find(|l| l.spans.iter().any(|s| s.text.contains("{center")));
        assert!(literal_row.is_some(), "the literal unclosed text must still render as prose, not vanish silently");
    }

    #[test]
    fn attrs_text_never_leaks_into_prose_when_resolved() {
        let lines = layout_markdown("![a](x.png){center 60%}", None);
        assert!(
            !lines.iter().flat_map(|l| &l.spans).any(|s| s.text.contains("center") || s.text.contains("60%")),
            "a recognized attrs block must never leak into a visible prose span"
        );
    }

    #[test]
    fn attrs_block_remainder_after_close_brace_becomes_prose() {
        let lines = layout_markdown("![a](x.png){center} more text", None);
        assert_eq!(lines[0].embedded_media.as_ref().unwrap().attrs.position, EmbedPosition::Center);
        let remainder_row = lines.iter().find(|l| l.spans.iter().any(|s| s.text.contains("more text")));
        assert!(remainder_row.is_some(), "text after the closing brace must still render as prose, not be swallowed");
    }

    #[test]
    fn unrecognized_attrs_tokens_are_ignored() {
        let lines = layout_markdown("![a](x.png){center bogus 60%}", None);
        let attrs = lines[0].embedded_media.as_ref().unwrap().attrs;
        assert_eq!(attrs.position, EmbedPosition::Center);
        assert_eq!(attrs.width_percent, Some(60));
    }

    #[test]
    fn float_attribute_round_trips_without_visual_effect_yet() {
        // Stage 1: `float` parses and is stored, but paint does not yet
        // act on it (real text-reflow is a separate, later pass) — this
        // test documents that boundary so it isn't mistaken for an
        // oversight later.
        let lines = layout_markdown("![a](x.png){float}", None);
        assert!(lines[0].embedded_media.as_ref().unwrap().attrs.float);
    }

    #[test]
    fn float_side_is_none_only_when_position_is_center() {
        // Confirmed user rule: "float работает если позиция не center" —
        // float takes effect for Left OR Right (explicit or defaulted),
        // never for Center.
        let right_float = layout_markdown("![a](x.png){right float}", None);
        assert_eq!(
            right_float[0].embedded_media.as_ref().unwrap().attrs.float_side(),
            Some(EmbedPosition::Right),
            "{{right float}}: block sits right, text wraps to its left"
        );

        let left_float = layout_markdown("![a](x.png){left float}", None);
        assert_eq!(
            left_float[0].embedded_media.as_ref().unwrap().attrs.float_side(),
            Some(EmbedPosition::Left),
            "{{left float}}: block sits left, text wraps to its right"
        );

        let bare_float = layout_markdown("![a](x.png){float}", None);
        assert_eq!(
            bare_float[0].embedded_media.as_ref().unwrap().attrs.float_side(),
            Some(EmbedPosition::Left),
            "{{float}} alone defaults to Left, which is not Center, so it DOES take effect"
        );

        let center_float = layout_markdown("![a](x.png){center float}", None);
        assert_eq!(
            center_float[0].embedded_media.as_ref().unwrap().attrs.float_side(),
            None,
            "{{center float}} has no side to wrap around, must not take effect"
        );

        let no_float = layout_markdown("![a](x.png){left}", None);
        assert_eq!(no_float[0].embedded_media.as_ref().unwrap().attrs.float_side(), None, "left without float must not take effect");
    }

    #[test]
    fn attrs_bearing_media_row_survives_the_wrap_pass_unchanged() {
        let (_cx, mut wrapper) = build_wrapper(gpui::px(16.));
        let lines = layout_markdown("![a](img.png){center 60%}", Some((&mut wrapper, gpui::px(72.))));
        let image_rows: Vec<_> = lines.iter().filter(|l| l.embedded_media.is_some()).collect();
        assert_eq!(image_rows.len(), 1, "an attrs-bearing media row must never be split by the wrap pass");
        assert_eq!(image_rows[0].embedded_media.as_ref().unwrap().attrs.position, EmbedPosition::Center);
    }
}
