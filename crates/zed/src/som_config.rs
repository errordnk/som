use fs::Fs;
use futures::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, UpdateGlobal as _};
use serde::Deserialize;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use std::{collections::HashMap, sync::Arc};
use paths;

fn som_action_to_gpui(action: &str) -> Option<(&'static str, Option<&'static str>, bool)> {
    // Returns (gpui_action_name, optional_context, needs_object_syntax)
    match action {
        "Copy"         => Some(("terminal::Copy",                   Some("Terminal"), false)),
        "Paste"        => Some(("terminal::Paste",                  Some("Terminal"), false)),
        "CloseTab"     => Some(("pane::CloseActiveItem",            None,            false)),
        "SplitTab"     => Some(("workspace::SomSplitPane",          None,            false)),
        "UnSplitTab"   => Some(("workspace::SomUnsplitPane",        None,            false)),
        "NextPane"     => Some(("workspace::SomActivateNextPane",   None,            false)),
        "PrevPane"     => Some(("workspace::SomActivatePrevPane",   None,            false)),
        "NextTab"      => Some(("workspace::SomActivateNextTab",    None,            false)),
        "PrevTab"      => Some(("workspace::SomActivatePrevTab",    None,            false)),
        "Quit"         => Some(("zed::Quit",                        None,            false)),
        "FontIncrease" => Some(("zed::IncreaseBufferFontSize",      None,            true)),
        "FontDecrease" => Some(("zed::DecreaseBufferFontSize",      None,            true)),
        "FontReset"    => Some(("zed::ResetBufferFontSize",         None,            true)),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SomConfig {
    pub env: HashMap<String, String>,
    pub general: GeneralConfig,
    pub window: WindowConfig,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub scroll: ScrollConfig,
    pub log: LogConfig,
    pub tabs: Vec<TabProfile>,
    pub keys: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GeneralConfig {
    pub bell: Option<String>,
    pub copy_on_select: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowConfig {
    pub theme: Option<String>,
    pub mode: Option<String>,
    pub opacity: Option<f32>,
    pub padding: Option<PaddingConfig>,
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
    pub weight: Option<String>,
    pub line_height: Option<f32>,
    pub features: HashMap<String, bool>,
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
    pub working_dir: Option<String>,
}

impl SomConfig {
    pub fn load_embedded() -> Self {
        #[cfg(target_os = "windows")]
        let asset_name = "windows.json";
        #[cfg(target_os = "macos")]
        let asset_name = "darwin.json";
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

        let from_file = std::fs::read(&user_config_path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok());

        from_file.unwrap_or_else(|| {
            assets::Assets::get(asset_name)
                .and_then(|f| serde_json::from_slice(&f.data).ok())
                .unwrap_or_default()
        })
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
            // New / New1..New9 → open tabs[0]..tabs[8] by name
            if action_name == "New" || action_name.strip_prefix("New").map_or(false, |s| s.parse::<usize>().is_ok()) {
                let idx = if action_name == "New" {
                    1
                } else {
                    action_name["New".len()..].parse::<usize>().unwrap_or(1)
                };
                if idx >= 1 && idx <= 9 {
                    if let Some(tab) = self.tabs.get(idx - 1) {
                        let name = tab.name.replace('"', "\\\"");
                        entries.push(format!(
                            "{{ \"bindings\": {{ \"{keystroke}\": [\"workspace::NewTerminal\", {{ \"tab_name\": \"{name}\" }}] }} }}"
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

        // Build experimental.theme_overrides.players[0] from cursor.color
        let cursor_color = self.cursor.color.as_deref().unwrap_or("");
        if !cursor_color.is_empty() {
            parts.push(format!(
                "\"experimental.theme_overrides\": {{ \"players\": [{{ \"cursor\": \"{}\" }}] }}",
                cursor_color
            ));
        }

        if let Some(face) = &self.font.face {
            terminal_parts.push(format!("\"font_family\": \"{}\"", face));
            parts.push(format!("\"buffer_font_family\": \"{}\"", face));
        }
        if let Some(size) = self.font.size {
            terminal_parts.push(format!("\"font_size\": {}", size));
        }
        if let Some(lh) = self.font.line_height {
            terminal_parts.push(format!("\"line_height\": {{ \"custom\": {} }}", lh));
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
        if let Some(copy) = self.general.copy_on_select {
            terminal_parts.push(format!("\"copy_on_select\": {}", copy));
        }

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
        serde_json::from_str(content).map_err(|e| {
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
        })
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
