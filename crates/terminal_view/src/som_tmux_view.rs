//! View component for `tmux:true` panes — deliberately separate from
//! `TerminalView` rather than patching it, since `TerminalView` is tightly
//! coupled to a live local `alacritty_terminal::Term` in nearly every
//! method, while this renders a plain-text grid snapshot arriving over IPC
//! from `som-tmux-server` (no colors/attributes yet — a known limitation of
//! the protocol's first iteration, see `som_tmux_server::protocol`).
//!
//! This is designed as a real, permanent component (not a throwaway shim):
//! the long-term direction for som-tmux is for it to become the default
//! path for every profile, so this view should hold up as a lasting
//! citizen of the `Item`/`Pane` UI, not just enough code to prove the
//! concept.

use crate::som_tmux_session::SomTmuxSession;
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Keystroke,
    Render, SharedString, WeakEntity, Window, div, prelude::*,
};
use settings::Settings;
use terminal::mappings::keys::to_esc_str;
use terminal::alacritty_terminal::term::TermMode;
use terminal::terminal_settings::TerminalSettings;
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use ui::{Icon, IconName, Label, h_flex, prelude::*};
use util::ResultExt;
use uuid::Uuid;
use workspace::{
    Workspace, WorkspaceId,
    item::{Item, ItemEvent, TabContentParams},
};

pub enum Event {
    Closed,
}

pub struct SomTmuxView {
    session: SomTmuxSession,
    grid_text: String,
    focus_handle: FocusHandle,
    tab_name: Option<String>,
    /// Profile's configured tab icon (a Nerd Font glyph, same convention as
    /// `TerminalView`'s `custom_icon` — see that type's `tab_content` for
    /// the pattern this mirrors). Was accepted but silently discarded by
    /// `add_center_tmux_terminal_named` until now — a known gap (tmux tabs
    /// never showed their profile's icon), not a design choice.
    tab_icon: Option<String>,
    workspace: WeakEntity<Workspace>,
    workspace_id: Option<WorkspaceId>,
    /// Set while the health-check reconnect (see `som_tmux_session`'s
    /// `spawn_read_loop`/`reconnect_blocking`) is in flight after this
    /// pane's connection died — surfaced in `render` as a small banner so
    /// the pane doesn't just look frozen with no explanation.
    reconnecting: bool,
}

impl SomTmuxView {
    pub fn new(
        session: SomTmuxSession,
        initial_grid_text: String,
        tab_name: Option<String>,
        tab_icon: Option<String>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session,
            grid_text: initial_grid_text,
            focus_handle: cx.focus_handle(),
            tab_name,
            tab_icon,
            workspace,
            workspace_id,
            reconnecting: false,
        }
    }

    /// Called by the read loop (`som_tmux_session::spawn_read_loop`) when a
    /// new `GridUpdate` arrives for this pane's session.
    pub fn apply_grid_update(&mut self, grid_text: String, cx: &mut Context<Self>) {
        self.grid_text = grid_text;
        cx.notify();
    }

    /// Called by the read loop right when it detects the connection died and
    /// is about to attempt a health-check reconnect (see
    /// `som_tmux_session::reconnect_blocking`).
    pub fn set_reconnecting(&mut self, reconnecting: bool, cx: &mut Context<Self>) {
        self.reconnecting = reconnecting;
        cx.notify();
    }

    /// Called by the read loop once a health-check reconnect resolves —
    /// `session_id` may differ from before (fell back to a brand new
    /// session because the old one couldn't be reattached; see
    /// `reconnect_blocking`'s doc comment). When it does, the workspace's
    /// `som_tab_tmux_sessions` registry (used to write db.json's
    /// `tmux_sessions` field) needs to be refreshed too, or a later restore
    /// would try to attach to a session id that's no longer live.
    pub fn apply_reconnect(&mut self, session_id: Uuid, grid_text: String, cx: &mut Context<Self>) {
        self.reconnecting = false;
        self.grid_text = grid_text;
        let item_id = cx.entity_id();
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, _cx| {
                workspace.set_tmux_sessions_for_item(item_id, vec![session_id]);
            });
        }
        cx.notify();
    }

    pub fn session_id(&self) -> Uuid {
        self.session.session_id()
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bytes) = keystroke_to_bytes(&event.keystroke) {
            self.session.write(bytes).log_err();
            cx.stop_propagation();
        }
    }
}

/// Converts a keystroke to the bytes a shell expects to receive, the same
/// way `Terminal::try_keystroke` does — but without needing a live `Term`,
/// since this pane doesn't have one. Uses `TermMode::default()` (no
/// alt-screen/app-cursor/etc awareness): a known limitation until the
/// protocol carries terminal mode state from the server (it currently only
/// sends plain grid text).
///
/// `to_esc_str` alone only covers special keys (arrows, function keys,
/// Ctrl-combos, tab/enter/escape) — it deliberately returns `None` for
/// plain printable characters, since in `TerminalView` those go through a
/// completely different path (`EntityInputHandler::replace_text_in_range`,
/// which is IME-aware and handles composition). `SomTmuxView` doesn't
/// implement that trait (yet — a known gap, not a design choice: proper IME
/// support would need marked-text/composition state this view doesn't have
/// anywhere to put), so as a first-pass fallback, `key_char` (the actual
/// typed character, keyboard-layout-aware — see `Keystroke`'s doc comment)
/// is used directly when `to_esc_str` has nothing to say. This covers plain
/// typing correctly; it does NOT yet correctly cover IME composition for
/// non-Latin input methods.
fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    if let Some(esc) = to_esc_str(keystroke, &TermMode::default(), false) {
        return Some(esc.into_owned().into_bytes());
    }
    keystroke.key_char.as_ref().map(|s| s.as_bytes().to_vec())
}

impl Focusable for SomTmuxView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<Event> for SomTmuxView {}
impl EventEmitter<ItemEvent> for SomTmuxView {}

impl Render for SomTmuxView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // GPUI's text layout doesn't have a CSS-`pre`-like whitespace mode
        // that preserves spaces/newlines verbatim, so each grid row is its
        // own element rather than relying on that — this is a plain-text
        // monospace render, not yet accounting for cell colors/attributes
        // (the protocol doesn't carry those yet, see module doc comment).
        //
        // Same font-resolution order `TerminalElement` uses (see
        // `terminal_element.rs`): the user's configured terminal font
        // family if set, falling back to the theme's buffer font — NOT a
        // hardcoded "monospace" (that was a bug: it ignored the user's
        // actual terminal font setting, e.g. a Nerd Font needed for icons).
        let terminal_settings = TerminalSettings::get_global(cx);
        let theme_settings = ThemeSettings::get_global(cx);
        let font_family = terminal_settings
            .font_family
            .as_ref()
            .map_or_else(|| theme_settings.buffer_font.family.clone(), |f| f.0.clone().into());

        div()
            .id("som-tmux-view")
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::key_down))
            .font_family(font_family)
            .whitespace_nowrap()
            .overflow_hidden()
            .flex()
            .flex_col()
            .when(self.reconnecting, |el| {
                // Health-check reconnect in progress (see
                // `som_tmux_session::spawn_read_loop`'s doc comment) — the
                // pane's content below is stale until this clears, so make
                // that visible rather than leaving it looking silently
                // frozen.
                el.child(
                    div()
                        .bg(cx.theme().status().warning_background)
                        .text_color(cx.theme().status().warning)
                        .px_2()
                        .child("Reconnecting…"),
                )
            })
            .children(
                self.grid_text
                    .lines()
                    .map(|line| div().child(line.to_string()))
                    .collect::<Vec<_>>(),
            )
    }
}

impl Item for SomTmuxView {
    type Event = Event;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        self.tab_name.clone().unwrap_or_else(|| "tmux".to_string()).into()
    }

    /// Same icon convention as `TerminalView::tab_content` (a Nerd Font
    /// glyph string from the profile's `icon` setting) — overriding this
    /// rather than the simpler `tab_icon` hook because that one only
    /// supports a `ui::Icon` from `IconName`'s fixed enum, not an arbitrary
    /// glyph character.
    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let title = self.tab_content_text(0, cx);

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
            .child(icon_element)
            .child(Label::new(SharedString::from(title)).color(params.text_color()))
            .into_any()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            Event::Closed => f(ItemEvent::CloseItem),
        }
    }

    /// Splitting a tmux pane means asking the server for a brand-new
    /// session (a fresh `NewSession`, not a clone of this one) — there's no
    /// local `Term` to clone, and cloning the *existing* session's PTY
    /// isn't meaningful (unlike `TerminalView`'s split, which spawns a new
    /// independent shell process too, just via `alacritty_terminal`
    /// directly). Not implemented yet: needs the client connection module
    /// to actually request a new session and construct a new `SomTmuxView`
    /// around it. Until then, tmux panes can't be split — `can_split`
    /// stays `false` (the trait default), which callers already handle by
    /// simply not offering the split action.
    fn can_split(&self) -> bool {
        false
    }

    /// Called when this tab is actually closed through the UI (not when Som
    /// itself just exits — that path never reaches here at all, the process
    /// just ends). This is the "explicit kill" half of the detach-vs-kill
    /// semantics (see `project_som_tmux` memory / `SOM_MUX_PLAN.md`): send
    /// `CloseSession` so the server actually terminates this pane's PTY and,
    /// if it was the server's last session, exits itself — otherwise the
    /// server would sit there listening forever with no live sessions at
    /// all, leaking a process for every tmux tab the user has ever closed.
    fn on_removed(&self, _cx: &mut Context<Self>) {
        self.session.close().log_err();
    }
}
