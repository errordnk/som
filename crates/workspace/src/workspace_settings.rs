use std::{num::NonZeroUsize, time::Duration};

use crate::DockPosition;
use collections::HashMap;
use serde::Deserialize;
pub use settings::{
    ActionName, AutosaveSetting, BottomDockLayout, InactiveOpacity,
    PaneSplitDirectionHorizontal, PaneSplitDirectionVertical, RegisterSetting,
    RestoreOnStartupBehavior, Settings,
};

#[derive(RegisterSetting)]
pub struct WorkspaceSettings {
    pub active_pane_modifiers: ActivePanelModifiers,
    pub bottom_dock_layout: settings::BottomDockLayout,
    pub pane_split_direction_horizontal: settings::PaneSplitDirectionHorizontal,
    pub pane_split_direction_vertical: settings::PaneSplitDirectionVertical,
    pub centered_layout: settings::CenteredLayoutSettings,
    pub confirm_quit: bool,
    pub show_call_status_icon: bool,
    pub autosave: AutosaveSetting,
    pub restore_on_startup: settings::RestoreOnStartupBehavior,
    pub cli_default_open_behavior: settings::CliDefaultOpenBehavior,
    pub restore_on_file_reopen: bool,
    pub drop_target_size: f32,
    pub use_system_path_prompts: bool,
    pub use_system_prompts: bool,
    pub command_aliases: HashMap<String, ActionName>,
    pub max_tabs: Option<NonZeroUsize>,
    pub when_closing_with_no_tabs: settings::CloseWindowWhenNoItems,
    pub on_last_window_closed: settings::OnLastWindowClosed,
    pub text_rendering_mode: settings::TextRenderingMode,
    pub resize_all_panels_in_dock: Vec<DockPosition>,
    pub close_on_file_delete: bool,
    pub close_panel_on_toggle: bool,
    pub use_system_window_tabs: bool,
    pub zoomed_padding: bool,
    pub window_decorations: settings::WindowDecorations,
    pub focus_follows_mouse: FocusFollowsMouse,
}

#[derive(Copy, Clone, Deserialize)]
pub struct FocusFollowsMouse {
    pub enabled: bool,
    pub debounce: Duration,
}

#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub struct ActivePanelModifiers {
    /// Size of the border surrounding the active pane.
    /// When set to 0, the active pane doesn't have any border.
    /// The border is drawn inset.
    ///
    /// Default: `0.0`
    // TODO: make this not an option, it is never None
    pub border_size: Option<f32>,
    /// Opacity of inactive panels.
    /// When set to 1.0, the inactive panes have the same opacity as the active one.
    /// If set to 0, the inactive panes content will not be visible at all.
    /// Values are clamped to the [0.0, 1.0] range.
    ///
    /// Default: `1.0`
    // TODO: make this not an option, it is never None
    pub inactive_opacity: Option<InactiveOpacity>,
}

#[derive(Deserialize, RegisterSetting)]
pub struct TabBarSettings {
    pub show: bool,
    pub show_nav_history_buttons: bool,
    pub show_tab_bar_buttons: bool,
    pub show_pinned_tabs_in_separate_row: bool,
}

impl Settings for WorkspaceSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let workspace = &content.workspace;
        let active_pane_modifiers = workspace.active_pane_modifiers.unwrap_or_default();
        let focus_follows_mouse = workspace.focus_follows_mouse.unwrap_or_default();
        Self {
            active_pane_modifiers: ActivePanelModifiers {
                border_size: active_pane_modifiers.border_size,
                inactive_opacity: active_pane_modifiers.inactive_opacity,
            },
            bottom_dock_layout: workspace.bottom_dock_layout.unwrap_or_default(),
            pane_split_direction_horizontal: workspace
                .pane_split_direction_horizontal
                .unwrap_or(PaneSplitDirectionHorizontal::Up),
            pane_split_direction_vertical: workspace
                .pane_split_direction_vertical
                .unwrap_or(PaneSplitDirectionVertical::Right),
            centered_layout: workspace.centered_layout.unwrap_or_default(),
            confirm_quit: workspace.confirm_quit.unwrap_or(false),
            show_call_status_icon: workspace.show_call_status_icon.unwrap_or(false),
            autosave: workspace.autosave.unwrap_or(AutosaveSetting::Off),
            restore_on_startup: workspace.restore_on_startup.unwrap_or_default(),
            cli_default_open_behavior: workspace.cli_default_open_behavior.unwrap_or_default(),
            restore_on_file_reopen: workspace.restore_on_file_reopen.unwrap_or(false),
            drop_target_size: workspace.drop_target_size.unwrap_or(0.2),
            use_system_path_prompts: workspace.use_system_path_prompts.unwrap_or(true),
            use_system_prompts: workspace.use_system_prompts.unwrap_or(true),
            command_aliases: workspace.command_aliases.clone(),
            max_tabs: workspace.max_tabs,
            when_closing_with_no_tabs: workspace.when_closing_with_no_tabs.unwrap_or_default(),
            on_last_window_closed: workspace.on_last_window_closed.unwrap_or_default(),
            text_rendering_mode: workspace.text_rendering_mode.unwrap_or_default(),
            resize_all_panels_in_dock: workspace
                .resize_all_panels_in_dock
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            close_on_file_delete: workspace.close_on_file_delete.unwrap_or(false),
            close_panel_on_toggle: workspace.close_panel_on_toggle.unwrap_or(false),
            use_system_window_tabs: workspace.use_system_window_tabs.unwrap_or(false),
            zoomed_padding: workspace.zoomed_padding.unwrap_or(false),
            window_decorations: workspace.window_decorations.unwrap_or_default(),
            focus_follows_mouse: FocusFollowsMouse {
                enabled: focus_follows_mouse.enabled.unwrap_or(false),
                debounce: Duration::from_millis(
                    focus_follows_mouse.debounce_ms.unwrap_or(250),
                ),
            },
        }
    }
}

impl Settings for TabBarSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let tab_bar = content.tab_bar.clone().unwrap_or_default();
        TabBarSettings {
            show: tab_bar.show.unwrap_or(true),
            show_nav_history_buttons: tab_bar.show_nav_history_buttons.unwrap_or(true),
            show_tab_bar_buttons: tab_bar.show_tab_bar_buttons.unwrap_or(true),
            show_pinned_tabs_in_separate_row: tab_bar
                .show_pinned_tabs_in_separate_row
                .unwrap_or(false),
        }
    }
}
