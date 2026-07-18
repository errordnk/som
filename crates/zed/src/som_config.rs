use fs::Fs;
use futures::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, UpdateGlobal as _};
use serde::Deserialize;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use indexmap::IndexMap;
use std::{collections::HashMap, sync::Arc};
use paths;

fn som_action_to_gpui(action: &str) -> Option<(&'static str, Option<&'static str>, bool)> {
    // Returns (gpui_action_name, optional_context, needs_object_syntax)
    match action {
        "Copy"         => Some(("terminal::Copy",                   Some("Terminal"), false)),
        "Paste"        => Some(("terminal::Paste",                  Some("Terminal"), false)),
        "CloseTab"     => Some(("workspace::SomCloseTab",            None,            false)),
        "NewPane"      => Some(("workspace::SomSplitPane",          None,            false)),
        "UnSplitTab"   => Some(("workspace::SomUnsplitPane",        None,            false)),
        "ClosePane"    => Some(("workspace::SomClosePane",          None,            false)),
        "NextPane"     => Some(("workspace::SomActivateNextPane",   Some("Terminal"), false)),
        "PrevPane"     => Some(("workspace::SomActivatePrevPane",   Some("Terminal"), false)),
        "NextTab"      => Some(("workspace::SomActivateNextTab",    Some("Terminal"), false)),
        "PrevTab"      => Some(("workspace::SomActivatePrevTab",    Some("Terminal"), false)),
        "Quit"         => Some(("zed::Quit",                        None,            false)),
        "IncreaseFont" => Some(("zed::IncreaseBufferFontSize",      None,            true)),
        "DecreaseFont" => Some(("zed::DecreaseBufferFontSize",      None,            true)),
        "ResetFont"    => Some(("zed::ResetBufferFontSize",         None,            true)),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SomConfig {
    pub window: WindowConfig,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub scroll: ScrollConfig,
    pub log: LogConfig,
    pub tabs: Vec<TabProfile>,
    pub keys: IndexMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowConfig {
    pub theme: Option<String>,
    pub mode: Option<String>,
    pub padding: Option<PaddingConfig>,
    pub selection: Option<String>,
    /// Explicit windowed-mode startup position, physical pixels. Only takes
    /// effect when `mode: "windowed"` AND both this and `size` have all
    /// their fields present and non-zero — otherwise Som falls back to
    /// `db.json`'s remembered geometry, same as when neither is set at all.
    /// See `WindowConfig::explicit_windowed_bounds`.
    pub position: Option<WindowPositionConfig>,
    pub size: Option<WindowSizeConfig>,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowPositionConfig {
    pub top: u32,
    pub left: u32,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowSizeConfig {
    pub width: u32,
    pub height: u32,
}

/// How the window should be placed when it's first created. Parsed from
/// `window.mode` (`"windowed"` / `"maximized"` / `"minimized"` / `"fullscreen"`);
/// unrecognized or absent values fall back to `Windowed`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Minimized,
    Fullscreen,
}

impl WindowConfig {
    pub fn mode(&self) -> WindowMode {
        match self.mode.as_deref() {
            Some("maximized") => WindowMode::Maximized,
            Some("minimized") => WindowMode::Minimized,
            Some("fullscreen") => WindowMode::Fullscreen,
            _ => WindowMode::Windowed,
        }
    }

    /// `(top, left, width, height)` if `position` and `size` are both set
    /// and every one of their four fields is non-zero — the all-or-nothing
    /// rule from `position`/`size`'s own doc comment above. `None` means
    /// "fall back to db.json", exactly as if neither were set.
    pub fn explicit_windowed_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        match (self.position, self.size) {
            (Some(position), Some(size))
                if position.top != 0
                    && position.left != 0
                    && size.width != 0
                    && size.height != 0 =>
            {
                Some((position.top, position.left, size.width, size.height))
            }
            _ => None,
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct PaddingConfig {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct LogConfig {
    pub level: String,
    pub days: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "all".to_string(), days: 7 }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FontConfig {
    pub face: Option<String>,
    pub size: Option<f32>,
    /// Integer 100-900 (same scale as Zed's own `buffer_font_weight`/
    /// `terminal.font_weight` — 400 = normal, 700 = bold). Values outside
    /// that range are clamped by Zed's own settings pipeline, not here.
    pub weight: Option<f32>,
    pub line_height: Option<f32>,
    /// OpenType feature tag -> value. Accepts `true`/`false` (e.g. `{"calt":
    /// false}` to disable ligatures) as well as integers (e.g. `{"cv01": 7}`
    /// to pick a stylistic-set variant) — same shape as Zed's own
    /// `terminal.font_features`/`buffer_font_features`.
    pub features: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CursorConfig {
    pub shape: Option<String>,
    pub color: Option<String>,
    pub blinking: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ScrollConfig {
    pub scroll_multiplier: Option<f32>,
    pub max_scroll_history: Option<u32>,
    pub alternate_scroll: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct TabProfile {
    pub name: String,
    pub icon: Option<String>,
    pub shell: Option<String>,
    pub home: Option<String>,
    /// Opt-in to som-tmux: the tab's terminal(s) run inside a detached
    /// `som-tmux` process that survives Som closing, instead of a
    /// plain PTY owned directly by Som. Defaults to `false` (explicit
    /// opt-in) for now; the long-term direction is to default this to
    /// `true` everywhere it's supported, keeping the field only as an
    /// explicit opt-out.
    pub tmux: bool,
    /// Marks this profile as the one opened by `Ctrl+N`/the `+` button/
    /// the very first tab on a fresh install, instead of `tabs[0]`. At
    /// most one profile may set this — `parse_with_defaults` rejects
    /// settings.json as invalid if more than one does. If none do,
    /// `tabs[0]` remains the default (see `TabProfiles::default_index`).
    pub default: bool,
}

impl SomConfig {
    pub fn load_embedded() -> Self {
        #[cfg(target_os = "windows")]
        let asset_name = "windows.json";
        #[cfg(target_os = "macos")]
        let asset_name = "macos.json";
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let asset_name = "linux.json";

        let user_config_path = paths::config_dir().join("settings.json");

        // Write default config if missing
        if !user_config_path.exists() {
            if let Some(asset) = assets::Assets::get(asset_name) {
                if let Some(parent) = user_config_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let shell = util::shell::get_windows_system_shell();
                let escaped = shell.replace('\\', "\\\\").replace('"', "\\\"");
                let contents = std::str::from_utf8(&asset.data)
                    .unwrap_or_default()
                    .replace("\"$SHELL\"", &format!("\"{}\"", escaped));
                let _ = std::fs::write(&user_config_path, contents.as_bytes());
            }
        }

        // Start with embedded defaults, then overlay user file on top.
        // This ensures new default keys are always present even when the user
        // created their settings.json before those keys were added.
        let mut config: SomConfig = assets::Assets::get(asset_name)
            .and_then(|f| serde_json::from_slice(&f.data).ok())
            .unwrap_or_default();

        if let Ok(data) = std::fs::read(&user_config_path) {
            if let Ok(user) = serde_json::from_slice::<SomConfig>(&data) {
                // Merge: user fields override defaults; user keys override default keys.
                // For the keys map specifically: start from defaults, then insert user keys.
                let mut merged_keys = config.keys.clone();
                for (k, v) in &user.keys {
                    merged_keys.insert(k.clone(), v.clone());
                }
                config = user;
                config.keys = merged_keys;
            }
        }

        config
    }

    pub fn apply_keys(&self, cx: &mut App) {
        let mut entries: Vec<String> = Vec::new();

        // Essential menu navigation bindings (no default keymap file in som)
        let menu_bindings = [
            ("up",     "menu", "menu::SelectPrevious"),
            ("down",   "menu", "menu::SelectNext"),
            ("tab",    "menu", "menu::SelectNext"),
            ("enter",  "menu", "menu::Confirm"),
            ("escape", "menu", "menu::Cancel"),
        ];
        for (keystroke, ctx, action) in &menu_bindings {
            entries.push(format!(
                "{{ \"context\": \"{ctx}\", \"bindings\": {{ \"{keystroke}\": \"{action}\" }} }}"
            ));
        }

        // Built-in font size bindings (persist:false = session only)
        let font_bindings = [
            ("ctrl-=", "zed::IncreaseBufferFontSize"),
            ("ctrl--", "zed::DecreaseBufferFontSize"),
        ];
        for (keystroke, action) in &font_bindings {
            entries.push(format!(
                "{{ \"bindings\": {{ \"{keystroke}\": [\"{action}\", {{ \"persist\": false }}] }} }}"
            ));
        }

        // User-defined bindings from windows.json "keys"
        for (keystroke_raw, action_name) in &self.keys {
            let keystroke = keystroke_raw.replace('\\', "\\\\");
            // NewTab / NewTab1..NewTab10 → open tabs[0]..tabs[9] by name.
            // Bare "NewTab" (no number) opens whichever profile has
            // "default": true (validated to be at most one — see
            // parse_with_defaults), or tabs[0] if none do.
            if action_name == "NewTab" || action_name.strip_prefix("NewTab").map_or(false, |s| s.parse::<usize>().is_ok()) {
                let is_bare = action_name == "NewTab";
                let idx = if is_bare {
                    self.tabs.iter().position(|t| t.default).unwrap_or(0) + 1
                } else {
                    action_name["NewTab".len()..].parse::<usize>().unwrap_or(1)
                };
                if idx >= 1 && idx <= 10 {
                    if let Some(tab) = self.tabs.get(idx - 1) {
                        let name = tab.name.replace('"', "\\\"");
                        if let Some(shell) = &tab.shell {
                            let shell_escaped = shell.replace('\\', "\\\\").replace('"', "\\\"");
                            entries.push(format!(
                                "{{ \"bindings\": {{ \"{keystroke}\": [\"workspace::NewTerminal\", {{ \"tab_name\": \"{name}\", \"shell\": \"{shell_escaped}\" }}] }} }}"
                            ));
                        } else {
                            entries.push(format!(
                                "{{ \"bindings\": {{ \"{keystroke}\": [\"workspace::NewTerminal\", {{ \"tab_name\": \"{name}\" }}] }} }}"
                            ));
                        }
                        continue;
                    }
                    // Explicitly numbered (NewTab1..NewTab10) but settings.json's
                    // tabs[] doesn't have that many profiles — surface an error
                    // instead of silently opening tabs[0]/the default profile
                    // (bare "NewTab" can't hit this: `idx` there always comes
                    // from an existing tabs[] entry or the 0 fallback).
                    if !is_bare {
                        entries.push(format!(
                            "{{ \"bindings\": {{ \"{keystroke}\": [\"workspace::NewTerminal\", {{ \"missing_profile_index\": {idx} }}] }} }}"
                        ));
                        continue;
                    }
                }
                // fallback — no tab defined, use plain NewTerminal
                entries.push(format!(
                    "{{ \"bindings\": {{ \"{keystroke}\": \"workspace::NewTerminal\" }} }}"
                ));
                continue;
            }
            if let Some((gpui_action, ctx, as_obj)) = som_action_to_gpui(action_name) {
                let binding = if as_obj {
                    format!("[\"{gpui_action}\", {{ \"persist\": false }}]")
                } else {
                    format!("\"{gpui_action}\"")
                };
                if let Some(ctx) = ctx {
                    entries.push(format!(
                        "{{ \"context\": \"{ctx}\", \"bindings\": {{ \"{keystroke}\": {binding} }} }}"
                    ));
                } else {
                    entries.push(format!(
                        "{{ \"bindings\": {{ \"{keystroke}\": {binding} }} }}"
                    ));
                }
            }
        }

        if entries.is_empty() {
            return;
        }

        let json = format!("[{}]", entries.join(", "));
        match KeymapFile::load(&json, cx) {
            KeymapFileLoadResult::Success { key_bindings } => cx.bind_keys(key_bindings),
            KeymapFileLoadResult::SomeFailedToLoad { key_bindings, error_message } => {
                log::warn!("Some keybindings failed to load: {}", error_message.0);
                cx.bind_keys(key_bindings);
            }
            KeymapFileLoadResult::JsonParseFailure { error } => {
                log::warn!("Failed to parse som keybindings: {error}");
            }
        }
    }

    pub fn load_nord_theme(&self, cx: &mut App) {
        if let Some(data) = assets::Assets::get("nord.json") {
            let themes_dir = paths::themes_dir();
            let nord_path = themes_dir.join("nord.json");
            if !nord_path.exists() {
                if let Err(e) = std::fs::create_dir_all(themes_dir) {
                    log::warn!("Failed to create themes dir: {e}");
                } else if let Err(e) = std::fs::write(&nord_path, &*data.data) {
                    log::warn!("Failed to write nord.json to themes dir: {e}");
                }
            }
            let registry = theme::ThemeRegistry::global(cx);
            if let Err(e) = theme_settings::load_user_theme(&registry, &data.data) {
                log::warn!("Failed to load Nord theme: {e}");
            }
        }
    }

    pub fn apply_settings(&self, cx: &mut App) {
        let mut parts: Vec<String> = Vec::new();
        let mut terminal_parts: Vec<String> = Vec::new();

        if let Some(theme) = &self.window.theme {
            parts.push(format!("\"theme\": \"{}\"", theme));
        }

        // Build experimental.theme_overrides.players[0] from cursor.color / window.selection
        let mut player_parts: Vec<String> = Vec::new();
        if let Some(cursor_color) = self.cursor.color.as_deref().filter(|s| !s.is_empty()) {
            player_parts.push(format!("\"cursor\": \"{}\"", cursor_color));
        }
        let mut theme_override_parts: Vec<String> = Vec::new();
        if !player_parts.is_empty() {
            theme_override_parts.push(format!(
                "\"players\": [{{ {} }}]",
                player_parts.join(", ")
            ));
        }
        // `window.selection` drives the terminal's selection-highlight color,
        // which is hardcoded in the renderer to `theme.colors().text_accent`
        // (see `terminal_element.rs::layout_grid`'s `sel_bg_color`) — not the
        // `players[].selection` field despite the similar name, which only
        // affects unrelated UI elements. Overriding `text.accent` also
        // recolors a few other accent-colored UI bits (e.g. search-match
        // highlighting) as a side effect, since it's a single shared theme
        // color, not a terminal-specific one.
        if let Some(selection_color) = self.window.selection.as_deref().filter(|s| !s.is_empty()) {
            theme_override_parts.push(format!("\"text.accent\": \"{}\"", selection_color));
        }
        if !theme_override_parts.is_empty() {
            parts.push(format!(
                "\"experimental.theme_overrides\": {{ {} }}",
                theme_override_parts.join(", ")
            ));
        }

        if let Some(face) = &self.font.face {
            terminal_parts.push(format!("\"font_family\": \"{}\"", face));
            parts.push(format!("\"buffer_font_family\": \"{}\"", face));
        }
        if let Some(size) = self.font.size {
            terminal_parts.push(format!("\"font_size\": {}", size));
        }
        if let Some(weight) = self.font.weight {
            terminal_parts.push(format!("\"font_weight\": {}", weight));
            parts.push(format!("\"buffer_font_weight\": {}", weight));
        }
        if let Some(lh) = self.font.line_height {
            terminal_parts.push(format!("\"line_height\": {{ \"custom\": {} }}", lh));
        }
        if !self.font.features.is_empty() {
            let mut feature_parts: Vec<String> = Vec::new();
            for (tag, value) in &self.font.features {
                let is_valid_tag = tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric());
                if !is_valid_tag {
                    log::warn!("Ignoring invalid font feature tag: {tag}");
                    continue;
                }
                match value {
                    serde_json::Value::Bool(enabled) => {
                        feature_parts.push(format!("\"{tag}\": {}", if *enabled { 1 } else { 0 }));
                    }
                    serde_json::Value::Number(n) if n.is_u64() => {
                        feature_parts.push(format!("\"{tag}\": {n}"));
                    }
                    _ => {
                        log::warn!(
                            "Ignoring font feature \"{tag}\": value must be true/false or a non-negative integer"
                        );
                    }
                }
            }
            if !feature_parts.is_empty() {
                terminal_parts.push(format!(
                    "\"font_features\": {{ {} }}",
                    feature_parts.join(", ")
                ));
            }
        }
        if let Some(shape) = &self.cursor.shape {
            terminal_parts.push(format!("\"cursor_shape\": \"{}\"", shape));
        }
        if let Some(blinking) = &self.cursor.blinking {
            let val = match blinking.as_str() {
                "on" => "on",
                "off" => "off",
                _ => "terminal_controlled",
            };
            terminal_parts.push(format!("\"blinking\": \"{}\"", val));
        }
        if let Some(mult) = self.scroll.scroll_multiplier {
            terminal_parts.push(format!("\"scroll_multiplier\": {}", mult));
        }
        if let Some(max) = self.scroll.max_scroll_history {
            terminal_parts.push(format!("\"max_scroll_history_lines\": {}", max));
        }
        if let Some(alt) = &self.scroll.alternate_scroll {
            terminal_parts.push(format!("\"alternate_scroll\": \"{}\"", alt));
        }
        // Always on / always off — not user-configurable.
        terminal_parts.push("\"copy_on_select\": true".to_string());
        terminal_parts.push("\"bell\": \"off\"".to_string());

        if !terminal_parts.is_empty() {
            parts.push(format!("\"terminal\": {{ {} }}", terminal_parts.join(", ")));
        }

        // Always hide the pane tab bar — tabs live in the title bar
        parts.push("\"tab_bar\": { \"show\": false }".to_string());

        if parts.is_empty() {
            return;
        }

        let json = format!("{{ {} }}", parts.join(", "));
        SettingsStore::update_global(cx, |store, cx| {
            let _ = store.set_user_settings(&json, cx);
        });
    }

    fn parse(content: &str) -> Result<Self, String> {
        Self::parse_with_defaults(content, Self::embedded_defaults())
    }

    fn embedded_defaults() -> Self {
        #[cfg(target_os = "windows")]
        let asset_name = "windows.json";
        #[cfg(target_os = "macos")]
        let asset_name = "macos.json";
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let asset_name = "linux.json";
        assets::Assets::get(asset_name)
            .and_then(|f| serde_json::from_slice(&f.data).ok())
            .unwrap_or_default()
    }

    fn parse_with_defaults(content: &str, defaults: Self) -> Result<Self, String> {
        let user: Self = serde_json::from_str(content).map_err(|e| {
            let line = e.line();
            let col = e.column();
            let source_line = content
                .lines()
                .nth(line.saturating_sub(1))
                .unwrap_or("")
                .trim();
            format!(
                "settings.json line {line}, column {col}: {e}\n  → {source_line}"
            )
        })?;
        let default_count = user.tabs.iter().filter(|t| t.default).count();
        if default_count > 1 {
            return Err(format!(
                "settings.json: only one tab profile may have \"default\": true, found {default_count}"
            ));
        }
        let mut merged_keys = defaults.keys;
        let mut config = user;
        for (k, v) in std::mem::take(&mut config.keys) {
            merged_keys.insert(k, v);
        }
        config.keys = merged_keys;
        Ok(config)
    }

    pub fn watch(fs: Arc<dyn Fs>, cx: &mut App) {
        let settings_path = paths::config_dir().join("settings.json");
        let (mut rx, _watcher) = settings::watch_config_file(
            &cx.background_executor(),
            fs,
            settings_path,
        );
        cx.spawn(async move |cx: &mut AsyncApp| {
            let _watcher = _watcher;
            while let Some(content) = rx.next().await {
                match SomConfig::parse(&content) {
                    Ok(config) => {
                        cx.update(|cx| {
                            config.apply_settings(cx);
                            config.apply_keys(cx);

                            let padding = config.window.padding.clone().unwrap_or_default();
                            workspace::SomWindowSettings::set(
                                workspace::SomWindowSettings {
                                    padding_top: padding.top,
                                    padding_bottom: padding.bottom,
                                    padding_left: padding.left,
                                    padding_right: padding.right,
                                },
                                cx,
                            );
                            cx.refresh_windows();
                        });
                    }
                    Err(err) => {
                        log::error!("settings.json parse error: {err}");
                        cx.update(|cx| {
                            Self::show_parse_error(err, cx);
                        });
                    }
                }
            }
        })
        .detach();
    }

    fn show_parse_error(err: String, cx: &mut App) {
        use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};
        let id = NotificationId::Named("som-settings-parse-error".into());

        let show = move |ws: &mut workspace::Workspace, cx: &mut gpui::Context<workspace::Workspace>| {
            let err2 = err.clone();
            let msg = format!("Invalid settings.json\n{err2}");
            ws.show_notification(id.clone(), cx, move |cx| {
                let msg2 = msg.clone();
                let msg3 = msg.clone();
                cx.new(|cx| {
                    MessageNotification::new(msg2, cx)
                        .primary_message("Copy")
                        .primary_on_click(move |_window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(msg3.clone()));
                        })
                        .show_suppress_button(false)
                })
            });
        };

        for window in cx.windows() {
            if let Some(handle) = window.downcast::<workspace::MultiWorkspace>() {
                handle.update(cx, |mw, _, cx| {
                    mw.workspace().update(cx, |ws, cx| show(ws, cx));
                }).ok();
                return;
            }
        }

        cx.observe_new(move |mw: &mut workspace::MultiWorkspace, _, cx| {
            mw.workspace().update(cx, |ws, cx| show(ws, cx));
        })
        .detach();
    }
}
