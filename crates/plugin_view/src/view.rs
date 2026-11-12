//! The panel of one plugin instance: a tab that draws the instance's view
//! (header, blocks, input line) and sends what the user does back to it.
//!
//! Stage 0 (`PLUGINS_DESIGN.md` section 12): blocks are drawn in a plain
//! scrolling column (no virtualization yet), the input is one line typed
//! through key events (no IME composition), and closing the tab stops the
//! instance, since there is no way yet to reattach to it later.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, ScrollHandle, SharedString,
    Window, div, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use somsrv::link::SrvLink;
use srp_client::plugin::{Block, BlockId, SomCall, SomReply, ViewEvent, ViewModel, ViewOp};
use theme_settings::ThemeSettings;
use settings::Settings as _;
use ui::{ActiveTheme as _, Button, ButtonCommon, ButtonStyle, Clickable, Color, Label, LabelCommon, LabelSize, h_flex, v_flex};
use workspace::item::{Item, ItemEvent};

use crate::client::{Incoming, PluginClient};
use crate::loader::Plugin;

pub enum PluginViewEvent {
    TitleChanged,
}

enum Connection {
    Starting,
    Running(Arc<PluginClient>),
    /// The instance ended or could not start; the text says why.
    Ended(SharedString),
}

pub struct PluginView {
    plugin: Plugin,
    focus_handle: FocusHandle,
    connection: Connection,
    view: ViewModel,
    /// One `Markdown` per `markdown` block, with the text it was made from.
    markdowns: HashMap<BlockId, (String, Entity<Markdown>)>,
    input: String,
    /// Text an IME is composing, shown after `input` until committed.
    marked: Option<String>,
    scroll: ScrollHandle,
}

impl PluginView {
    pub fn new(
        plugin: Plugin,
        link: Option<Arc<SrvLink>>,
        cwd: String,
        pane_id: Option<String>,
        config: serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let instance = uuid::Uuid::new_v4().to_string();
        let start_plugin = plugin.clone();
        let started = cx.background_spawn(async move {
            PluginClient::start(link, instance, &start_plugin, cwd, pane_id, config, None)
        });
        cx.spawn(async move |this, cx| {
            let (client, incoming) = match started.await {
                Ok(started) => started,
                Err(err) => {
                    this.update(cx, |this, cx| this.end(format!("could not start: {err:#}"), cx)).ok();
                    return;
                }
            };
            if this.update(cx, |this, cx| {
                this.connection = Connection::Running(client);
                cx.notify();
            })
            .is_err()
            {
                return;
            }
            while let Ok(message) = incoming.recv().await {
                if this.update(cx, |this, cx| this.receive(message, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();

        Self {
            plugin,
            focus_handle,
            connection: Connection::Starting,
            view: ViewModel::default(),
            markdowns: HashMap::new(),
            input: String::new(),
            marked: None,
            scroll: ScrollHandle::new(),
        }
    }

    fn client(&self) -> Option<&Arc<PluginClient>> {
        match &self.connection {
            Connection::Running(client) => Some(client),
            _ => None,
        }
    }

    fn send(&self, event: ViewEvent) {
        if let Some(client) = self.client()
            && let Err(err) = client.event(event)
        {
            log::warn!("plugin {}: sending an event failed: {err:#}", self.plugin.name);
        }
    }

    fn end(&mut self, why: String, cx: &mut Context<Self>) {
        self.connection = Connection::Ended(why.into());
        cx.notify();
    }

    fn receive(&mut self, message: Incoming, cx: &mut Context<Self>) {
        match message {
            Incoming::View(op) => {
                let old_title = self.view.header.title.clone();
                let follow = matches!(&op, ViewOp::Append { parent: None, .. } | ViewOp::Batch(_));
                if let ViewOp::ScrollTo { id } = &op
                    && let Some(index) = self.view.blocks.iter().position(|block| block.id == *id)
                {
                    self.scroll.scroll_to_item(index);
                }
                if let Err(err) = self.view.apply(&op) {
                    log::warn!("plugin {}: a view change did not apply: {err}", self.plugin.name);
                }
                self.sync_markdowns(cx);
                if follow {
                    self.scroll.scroll_to_bottom();
                }
                if self.view.header.title != old_title {
                    cx.emit(PluginViewEvent::TitleChanged);
                }
            }
            Incoming::SomRequest { request, call } => {
                let reply = self.answer(call, cx);
                if let Some(client) = self.client()
                    && let Err(err) = client.reply(request, reply)
                {
                    log::warn!("plugin {}: answering a request failed: {err:#}", self.plugin.name);
                }
            }
            Incoming::State(_state) => {
                // Kept in db.json once instances are restarted from there (stage 4).
            }
            Incoming::Exited(error) => {
                self.end(error.map(|error| format!("the plugin failed: {error}")).unwrap_or_else(|| "ended".into()), cx)
            }
            Incoming::Disconnected(why) => self.end(format!("disconnected: {why}"), cx),
        }
        cx.notify();
    }

    /// Answers a `som.*` call. The permission is checked here again: the
    /// daemon on the other machine is not trusted to have done it.
    fn answer(&mut self, call: SomCall, cx: &mut Context<Self>) -> SomReply {
        if let Some(permission) = call.permission()
            && !self.plugin.grants(permission)
        {
            return SomReply::Err("denied".into());
        }
        match call {
            SomCall::ClipboardWrite { text } => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                SomReply::Ok(serde_json::Value::Null)
            }
            SomCall::OpenUrl { url } => {
                cx.open_url(&url);
                SomReply::Ok(serde_json::Value::Null)
            }
            SomCall::Notify { title, body } => {
                log::info!("plugin {}: {title}{}", self.plugin.name, body.map(|body| format!(": {body}")).unwrap_or_default());
                SomReply::Ok(serde_json::Value::Null)
            }
            SomCall::OpenTerminal { .. } | SomCall::Preview { .. } | SomCall::Confirm { .. } | SomCall::OpenPlugin { .. } => {
                SomReply::Err("unsupported".into())
            }
        }
    }

    /// Keeps one `Markdown` per `markdown` block, in step with its text.
    fn sync_markdowns(&mut self, cx: &mut Context<Self>) {
        let mut texts = Vec::new();
        collect_markdown(&self.view.blocks, &mut texts);
        let mut kept = HashMap::with_capacity(texts.len());
        for (id, text) in texts {
            let entry = match self.markdowns.remove(&id) {
                Some((old, markdown)) if old == text => (old, markdown),
                Some((_, markdown)) => {
                    markdown.update(cx, |markdown, cx| markdown.replace(text.clone(), cx));
                    (text, markdown)
                }
                None => {
                    let markdown = cx.new(|cx| Markdown::new(text.clone().into(), cx));
                    (text, markdown)
                }
            };
            kept.insert(id, entry);
        }
        self.markdowns = kept;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.view.input.enabled {
            if event.keystroke.key == "escape" {
                self.send(ViewEvent::Interrupt);
            }
            return;
        }
        let modifiers = &event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "enter" if !modifiers.shift => {
                let text = std::mem::take(&mut self.input);
                self.send(ViewEvent::Submit { text });
            }
            "enter" => self.input.push('\n'),
            "backspace" => {
                self.input.pop();
            }
            "escape" => self.send(ViewEvent::Interrupt),
            "v" if modifiers.control || modifiers.platform => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.input.push_str(&text);
                }
            }
            // Typed text comes through the input handler (`EntityInputHandler`
            // below): keyboard layouts, characters sent without a key, IME.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn render_block(&self, block: &Block, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let text = |key: &str| block.props.get(key).and_then(|value| value.as_str()).unwrap_or_default().to_owned();
        let mono = ThemeSettings::get_global(cx).buffer_font.family.clone();
        let colors = cx.theme().colors();
        match block.kind.as_str() {
            "markdown" => match self.markdowns.get(&block.id) {
                Some((_, markdown)) => div()
                    .w_full()
                    .child(MarkdownElement::new(markdown.clone(), MarkdownStyle::themed(MarkdownFont::Editor, window, cx)))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            "code" => div()
                .w_full()
                .p_2()
                .rounded_sm()
                .bg(colors.editor_background)
                .font_family(mono)
                .text_sm()
                .child(text("text"))
                .into_any_element(),
            "diff" => {
                let mut lines = v_flex().w_full().p_2().rounded_sm().bg(colors.editor_background).font_family(mono).text_sm();
                if !text("path").is_empty() {
                    lines = lines.child(Label::new(text("path")).color(Color::Muted).size(LabelSize::Small));
                }
                for line in text("old").lines() {
                    lines = lines.child(div().text_color(Color::Deleted.color(cx)).child(format!("- {line}")));
                }
                for line in text("new").lines() {
                    lines = lines.child(div().text_color(Color::Created.color(cx)).child(format!("+ {line}")));
                }
                lines.into_any_element()
            }
            "group" => {
                let id = block.id;
                let collapsed = block.collapsed;
                let marker = if collapsed { "▸" } else { "▾" };
                let mut title = format!("{marker} {}", text("title"));
                if !text("badge").is_empty() {
                    title.push_str(&format!("  ·  {}", text("badge")));
                }
                let header = div()
                    .id(("group", id))
                    .cursor_pointer()
                    .child(Label::new(title).color(Color::Muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(block) = this.view.block_mut(id) {
                            block.collapsed = !collapsed;
                        }
                        this.send(ViewEvent::Toggle { id, collapsed: !collapsed });
                        cx.notify();
                    }));
                let mut group = v_flex().w_full().gap_1().child(header);
                if !collapsed {
                    let children: Vec<AnyElement> =
                        block.children.iter().map(|child| self.render_block(child, window, cx)).collect();
                    group = group.child(v_flex().pl_4().gap_1().children(children));
                }
                group.into_any_element()
            }
            "actions" => {
                let id = block.id;
                let buttons = block.props.get("buttons").and_then(|buttons| buttons.as_array()).cloned().unwrap_or_default();
                h_flex()
                    .gap_2()
                    .children(buttons.into_iter().enumerate().map(|(index, button)| {
                        let button_id = button.get("id").and_then(|value| value.as_str()).unwrap_or_default().to_owned();
                        let label = button.get("label").and_then(|value| value.as_str()).unwrap_or(&button_id).to_owned();
                        Button::new(("action", id as usize * 64 + index), label).style(ButtonStyle::Outlined).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.send(ViewEvent::Action { block_id: id, button_id: button_id.clone() });
                                cx.notify();
                            },
                        ))
                    }))
                    .into_any_element()
            }
            "kv" => {
                let rows = block.props.get("rows").and_then(|rows| rows.as_array()).cloned().unwrap_or_default();
                v_flex()
                    .children(rows.into_iter().map(|row| {
                        let cell = |index: usize| row.get(index).map(value_text).unwrap_or_default();
                        h_flex()
                            .gap_2()
                            .child(Label::new(cell(0)).color(Color::Muted).size(LabelSize::Small))
                            .child(Label::new(cell(1)).size(LabelSize::Small))
                    }))
                    .into_any_element()
            }
            "progress" => Label::new(format!("… {}", text("label"))).color(Color::Muted).into_any_element(),
            "text" => {
                let color = match text("style").as_str() {
                    "muted" => Color::Muted,
                    "error" => Color::Error,
                    _ => Color::Default,
                };
                div().w_full().text_color(color.color(cx)).child(text("text")).into_any_element()
            }
            // A kind this Som does not know: its props as text.
            other => div()
                .w_full()
                .font_family(mono)
                .text_sm()
                .text_color(Color::Muted.color(cx))
                .child(format!("[{other}] {}", block.props))
                .into_any_element(),
        }
    }
}

fn collect_markdown(blocks: &[Block], out: &mut Vec<(BlockId, String)>) {
    for block in blocks {
        if block.kind == "markdown" {
            out.push((block.id, block.props.get("text").and_then(|text| text.as_str()).unwrap_or_default().to_owned()));
        }
        collect_markdown(&block.children, out);
    }
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl Drop for PluginView {
    /// Until a closed panel can be reopened on its running instance
    /// (stage 4), closing it ends the instance, or it would run unseen forever.
    fn drop(&mut self) {
        if let Some(client) = self.client()
            && let Err(err) = client.stop()
        {
            log::debug!("plugin {}: stopping on close failed: {err:#}", self.plugin.name);
        }
    }
}

/// The byte range of `text` that the UTF-16 range `range` covers (clamped).
fn utf16_to_bytes(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let mut start = text.len();
    let mut end = text.len();
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= range.start && start == text.len() {
            start = index;
        }
        if units >= range.end {
            end = index;
            break;
        }
        units += ch.len_utf16();
    }
    start.min(end)..end
}

/// The input line as the platform's text input sees it: one line whose
/// caret is always at the end; IME composition is kept in `marked`.
impl gpui::EntityInputHandler for PluginView {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = utf16_to_bytes(&self.input, range);
        let text = self.input[bytes.clone()].to_owned();
        *adjusted_range = Some(
            self.input[..bytes.start].encode_utf16().count()..self.input[..bytes.end].encode_utf16().count(),
        );
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let end = self.input.encode_utf16().count();
        Some(gpui::UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        let marked = self.marked.as_ref()?;
        let start = self.input.encode_utf16().count();
        Some(start..start + marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.view.input.enabled {
            return;
        }
        self.marked = None;
        // Control characters arrive here too (Enter, Backspace as WM_CHAR);
        // those keys are handled in `on_key_down`.
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        match range {
            Some(range) if range.start < self.input.encode_utf16().count() => {
                let bytes = utf16_to_bytes(&self.input, range);
                self.input.replace_range(bytes, &text);
            }
            _ => self.input.push_str(&text),
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        element_bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.view.input.enabled
    }
}

impl EventEmitter<PluginViewEvent> for PluginView {}

impl Focusable for PluginView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PluginView {
    type Event = PluginViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.view.header.title.clone().unwrap_or_else(|| self.plugin.title()).into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            PluginViewEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }
}

impl Render for PluginView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = &self.view.header;
        let title = header.title.clone().unwrap_or_else(|| self.plugin.title());
        let status = match &self.connection {
            Connection::Starting => Some("starting…".to_owned()),
            Connection::Ended(why) => Some(why.to_string()),
            Connection::Running(_) => header.status.clone(),
        };
        let mut head = h_flex().w_full().gap_3().px_3().py_2().child(Label::new(title));
        if let Some(subtitle) = &header.subtitle {
            head = head.child(Label::new(subtitle.clone()).color(Color::Muted).size(LabelSize::Small));
        }
        if let Some(status) = status {
            head = head.child(Label::new(status).color(Color::Muted).size(LabelSize::Small));
        }
        if let Some(error) = &header.error {
            head = head.child(Label::new(error.clone()).color(Color::Error).size(LabelSize::Small));
        }

        let blocks: Vec<AnyElement> = self.view.blocks.clone().iter().map(|block| self.render_block(block, window, cx)).collect();
        let colors = cx.theme().colors();
        let input = self.view.input.clone();
        let input_line = input.enabled.then(|| {
            let shown = if self.input.is_empty() && self.marked.is_none() {
                div().text_color(Color::Muted.color(cx)).child(input.placeholder.clone().unwrap_or_default())
            } else {
                div().child(format!("{}{}▏", self.input, self.marked.as_deref().unwrap_or_default()))
            };
            // Registers this view as the window's text input while focused.
            let entity = cx.entity();
            let focus = self.focus_handle.clone();
            let handler = gpui::canvas(
                |_, _, _| {},
                move |bounds, (), window, cx| {
                    window.handle_input(&focus, gpui::ElementInputHandler::new(bounds, entity), cx);
                },
            )
            .absolute()
            .size_full();
            div().relative().w_full().px_3().py_2().border_t_1().border_color(colors.border).child(shown).child(handler)
        });

        v_flex()
            .size_full()
            .bg(colors.editor_background)
            .track_focus(&self.focus_handle)
            .key_context("PluginView")
            .on_key_down(cx.listener(Self::on_key_down))
            .child(head)
            .child(
                div()
                    .id("plugin-blocks")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px_3()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(v_flex().w_full().gap_2().pb_2().children(blocks)),
            )
            .children(input_line)
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_to_bytes;

    #[test]
    fn utf16_ranges_map_to_byte_ranges_with_cyrillic_and_emoji() {
        let text = "aб😀c";
        // a = 1 unit / 1 byte, б = 1 / 2, 😀 = 2 / 4, c = 1 / 1
        assert_eq!(utf16_to_bytes(text, 0..1), 0..1);
        assert_eq!(utf16_to_bytes(text, 1..2), 1..3);
        assert_eq!(utf16_to_bytes(text, 2..4), 3..7);
        assert_eq!(utf16_to_bytes(text, 4..5), 7..8);
        assert_eq!(utf16_to_bytes(text, 5..5), 8..8);
        assert_eq!(utf16_to_bytes(text, 3..99), 7..8, "clamped; a half emoji starts after it");
    }
}
