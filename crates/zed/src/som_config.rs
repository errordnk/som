use gpui::{App, UpdateGlobal as _};
use serde::Deserialize;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use std::collections::HashMap;

fn som_action_to_gpui(action: &str) -> Option<(&'static str, bool)> {
    // Returns (gpui_action_name, needs_terminal_context)
    match action {
        "Copy"  => Some(("terminal::Copy",       true)),
        "Paste" => Some(("terminal::Paste",      true)),
        "New"   => Some(("workspace::NewTerminal", false)),
        "Quit"  => Some(("zed::Quit",            false)),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SomConfig {
    pub env: HashMap<String, String>,
    pub bell: Option<String>,
    pub copy_on_select: Option<bool>,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub scroll: ScrollConfig,
    pub tabs: Vec<TabProfile>,
    pub keys: HashMap<String, String>,
    pub theme: Option<String>,
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
        let data = assets::Assets::get("windows.json");
        #[cfg(target_os = "macos")]
        let data = assets::Assets::get("darwin.json");
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let data = assets::Assets::get("linux.json");

        data.and_then(|f| serde_json::from_slice(&f.data).ok())
            .unwrap_or_default()
    }

    pub fn apply(&self, cx: &mut App) {
        self.load_nord_theme(cx);
        self.apply_settings(cx);
        self.apply_keys(cx);
    }

    fn apply_keys(&self, cx: &mut App) {
        let mut entries: Vec<String> = Vec::new();

        // Built-in font size bindings
        let font_bindings = [
            ("ctrl-=", "zed::IncreaseBufferFontSize"),
            ("ctrl-+", "zed::IncreaseBufferFontSize"),
            ("ctrl--", "zed::DecreaseBufferFontSize"),
        ];
        for (keystroke, action) in &font_bindings {
            entries.push(format!("{{ \"bindings\": {{ \"{keystroke}\": \"{action}\" }} }}"));
        }

        // User-defined bindings from windows.json "keys"
        for (keystroke, action_name) in &self.keys {
            if let Some((gpui_action, needs_ctx)) = som_action_to_gpui(action_name) {
                if needs_ctx {
                    entries.push(format!(
                        "{{ \"context\": \"Terminal\", \"bindings\": {{ \"{keystroke}\": \"{gpui_action}\" }} }}"
                    ));
                } else {
                    entries.push(format!(
                        "{{ \"bindings\": {{ \"{keystroke}\": \"{gpui_action}\" }} }}"
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

    fn load_nord_theme(&self, cx: &mut App) {
        if let Some(data) = assets::Assets::get("nord.json") {
            let registry = theme::ThemeRegistry::global(cx);
            if let Err(e) = theme_settings::load_user_theme(&registry, &data.data) {
                log::warn!("Failed to load Nord theme: {e}");
            }
        }
    }

    fn apply_settings(&self, cx: &mut App) {
        let mut parts: Vec<String> = Vec::new();
        let mut terminal_parts: Vec<String> = Vec::new();

        if let Some(theme) = &self.theme {
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
        if let Some(copy) = self.copy_on_select {
            terminal_parts.push(format!("\"copy_on_select\": {}", copy));
        }

        if !terminal_parts.is_empty() {
            parts.push(format!("\"terminal\": {{ {} }}", terminal_parts.join(", ")));
        }

        if parts.is_empty() {
            return;
        }

        let json = format!("{{ {} }}", parts.join(", "));
        SettingsStore::update_global(cx, |store, cx| {
            let _ = store.set_user_settings(&json, cx);
        });
    }
}
