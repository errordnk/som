//! A minimal `Item` shown in a tab's position BEFORE its real `Terminal`
//! has finished connecting — see `terminal_panel::TerminalPanel::
//! restore_som_tabs`'s doc comment for why this exists: every tab (local
//! and slow real SSH `tmux: true` connections alike) used to only appear
//! in the tab bar once its terminal was fully created, so restoring N
//! tabs drew them one at a time as each connection finished instead of
//! all at once, in `db.json`'s order, up front.
//!
//! Deliberately NOT a general "loading item" abstraction — this exists
//! purely so `restore_som_tabs` can insert a tab at its final position
//! immediately (name/icon known from `db.json`/settings.json alone, no
//! real terminal needed for that) and then hand off to `Pane::
//! replace_item_at` once the real `TerminalView` is ready. No spinner or
//! other loading indicator in the tab's own content — per explicit
//! feedback, the titlebar's drag-zone spinner (`workspace::
//! SomRestoreActivity`) is the ONLY loading indicator for this work.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div,
};
use ui::{Icon, IconName, Label, h_flex, prelude::*};
use workspace::item::{Item, ItemEvent, TabContentParams};

pub struct PendingTerminalTab {
    tab_name: Option<String>,
    tab_icon: Option<String>,
    focus_handle: FocusHandle,
}

impl PendingTerminalTab {
    pub fn new(tab_name: Option<String>, tab_icon: Option<String>, cx: &mut Context<Self>) -> Self {
        Self { tab_name, tab_icon, focus_handle: cx.focus_handle() }
    }
}

impl EventEmitter<ItemEvent> for PendingTerminalTab {}

impl Focusable for PendingTerminalTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PendingTerminalTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Plain empty pane content — matches the color a real TerminalView
        // would show before its first paint, so there's no visible flash
        // when `replace_item_at` swaps this for the real thing.
        div().size_full().bg(cx.theme().colors().terminal_background)
    }
}

impl Item for PendingTerminalTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.tab_name.clone().unwrap_or_else(|| "Terminal".to_string()).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let title = self.tab_name.clone().unwrap_or_else(|| "Terminal".to_string());

        let icon_element: AnyElement = if let Some(icon) = &self.tab_icon {
            div()
                .font_family("FiraCode Nerd Font")
                .text_color(params.text_color().color(cx))
                .child(icon.clone())
                .into_any_element()
        } else {
            Icon::new(IconName::Terminal).color(params.text_color()).into_any_element()
        };

        h_flex()
            .gap_2()
            .when(!params.selected, |this| this.track_focus(&self.focus_handle))
            .child(icon_element)
            .child(Label::new(title).color(params.text_color()))
            .into_any()
    }
}
