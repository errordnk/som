use gpui::{App, Menu, MenuItem};

pub fn app_menus(_cx: &mut App) -> Vec<Menu> {
    use zed_actions::Quit;

    let view_items = vec![
        MenuItem::action(
            "Zoom In",
            zed_actions::IncreaseUiFontSize { persist: false },
        ),
        MenuItem::action(
            "Zoom Out",
            zed_actions::DecreaseUiFontSize { persist: false },
        ),
        MenuItem::action(
            "Reset Zoom",
            zed_actions::ResetUiFontSize { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action("Toggle Left Dock", workspace::ToggleLeftDock),
        MenuItem::action("Toggle Right Dock", workspace::ToggleRightDock),
        MenuItem::action("Toggle Bottom Dock", workspace::ToggleBottomDock),
        MenuItem::action("Toggle All Docks", workspace::ToggleAllDocks),
    ];

    vec![
        Menu {
            name: "Som".into(),
            disabled: false,
            items: vec![
                MenuItem::action("About Som", zed_actions::About),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new("Settings").items([
                    MenuItem::action("Open Settings", zed_actions::OpenSettings),
                    MenuItem::action("Open Settings File", super::OpenSettingsFile),
                    MenuItem::action("Open Default Settings", super::OpenDefaultSettings),
                    MenuItem::separator(),
                    MenuItem::action("Open Keymap", zed_actions::OpenKeymap),
                    MenuItem::action("Open Keymap File", zed_actions::OpenKeymapFile),
                    MenuItem::action("Open Default Key Bindings", zed_actions::OpenDefaultKeymap),
                ])),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::action("Hide Som", super::Hide),
                #[cfg(target_os = "macos")]
                MenuItem::action("Hide Others", super::HideOthers),
                #[cfg(target_os = "macos")]
                MenuItem::action("Show All", super::ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Som", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New Window", workspace::NewWindow),
                MenuItem::separator(),
                #[cfg(not(target_os = "macos"))]
                MenuItem::action("Open File...", workspace::OpenFiles),
                MenuItem::action(
                    if cfg!(not(target_os = "macos")) {
                        "Open Folder..."
                    } else {
                        "Open…"
                    },
                    workspace::Open::default(),
                ),
                MenuItem::action(
                    "Open Recent...",
                    zed_actions::OpenRecent {
                        create_new_window: false,
                    },
                ),
                MenuItem::separator(),
                MenuItem::action("Add Folder to Project…", workspace::AddFolderToProject),
                MenuItem::separator(),
                MenuItem::action("Close Window", workspace::CloseWindow),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: "Go".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Back", workspace::GoBack),
                MenuItem::action("Forward", workspace::GoForward),
            ],
        },
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Minimize", super::Minimize),
                MenuItem::action("Zoom", super::Zoom),
                MenuItem::separator(),
            ],
        },
        Menu {
            name: "Help".into(),
            disabled: false,
            items: vec![
                MenuItem::action("View Dependency Licenses", zed_actions::OpenLicenses),
            ],
        },
    ]
}
