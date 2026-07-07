use gpui::WindowButtonLayout;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Copy, Clone, Debug, RegisterSetting)]
#[allow(dead_code)]
pub struct TitleBarSettings {
    pub show_branch_status_icon: bool,
    pub show_onboarding_banner: bool,
    pub show_user_picture: bool,
    pub show_branch_name: bool,
    pub show_project_items: bool,
    pub show_sign_in: bool,
    pub show_user_menu: bool,
    pub show_menus: bool,
    pub button_layout: Option<WindowButtonLayout>,
}

impl Settings for TitleBarSettings {
    fn from_settings(s: &SettingsContent) -> Self {
        let content = s.title_bar.clone().unwrap_or_default();
        TitleBarSettings {
            show_branch_status_icon: content.show_branch_status_icon.unwrap_or(false),
            show_onboarding_banner: content.show_onboarding_banner.unwrap_or(false),
            show_user_picture: content.show_user_picture.unwrap_or(false),
            show_branch_name: content.show_branch_name.unwrap_or(true),
            show_project_items: content.show_project_items.unwrap_or(true),
            show_sign_in: content.show_sign_in.unwrap_or(false),
            show_user_menu: content.show_user_menu.unwrap_or(false),
            show_menus: content.show_menus.unwrap_or(false),
            button_layout: content.button_layout.unwrap_or_default().into_layout(),
        }
    }
}
