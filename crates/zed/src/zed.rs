mod app_menus;
pub(crate) mod open_listener;

pub use app_menus::*;
pub use open_listener::{
    OpenListener, OpenRequest, OpenRequestKind, RawOpenRequest,
    derive_paths_with_position, open_paths_with_positions,
    open_options_for_request,
};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use open_listener::listen_for_cli_connections;
use futures::{StreamExt, channel::mpsc, select_biased};
use gpui::{
    Action, App, Context, DismissEvent, Focusable, KeyBinding,
    PathPromptOptions, PromptLevel, ReadGlobal as _, SharedString,
    Window, WindowHandle, WindowKind, WindowOptions,
    actions, point, px,
};
use settings::{
    BaseKeymap, DEFAULT_KEYMAP_PATH, KeybindSource, KeymapFile,
    KeymapFileLoadResult, Settings, SettingsStore,
    update_settings_file,
};
use std::{
    path::Path,
    sync::Arc,
    sync::atomic::{self, AtomicBool},
};
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use ui::prelude::*;
use util::ResultExt;
use uuid::Uuid;
use workspace::notifications::{NotificationId, dismiss_app_notification, show_app_notification};
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{
    AppState, CloseWindow, MultiWorkspace, Workspace, WorkspaceSettings,
    with_active_or_new_workspace,
    CloseIntent, OpenLog,
};
use zed_actions::{About, OpenBrowser, OpenSettingsFile, OpenZedUrl, Quit};

actions!(
    zed,
    [
        /// Hides the application window.
        Hide,
        /// Hides all other application windows.
        HideOthers,
        /// Minimizes the current window.
        Minimize,
        /// Opens the default settings file.
        OpenDefaultSettings,
        /// Toggles fullscreen mode.
        ToggleFullScreen,
        /// Zooms the window.
        Zoom,
        /// Shows all hidden windows.
        ShowAll,
    ]
);

pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &Hide, cx| cx.hide());
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(quit);

    cx.on_action(|_: &OpenLog, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_log_file(workspace, window, cx);
        });
    })
    .on_action(|_: &workspace::RevealLogInFileManager, cx| {
        cx.reveal_path(paths::log_file().as_path());
    })
    .on_action(|&zed_actions::OpenKeymapFile, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::keymap_file(),
                || rope::Rope::from(settings::initial_keymap_content().as_ref()),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenSettingsFile, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::settings_file(),
                || rope::Rope::from(settings::initial_user_settings_content().as_ref()),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenDefaultSettings, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::settings_file(),
                || rope::Rope::from(settings::default_settings().as_ref()),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &About, _cx| {
        log::info!("Som v{}", env!("CARGO_PKG_VERSION"));
    });
}

static WAITING_QUIT_CONFIRMATION: AtomicBool = AtomicBool::new(false);

fn quit(_: &Quit, cx: &mut App) {
    if WAITING_QUIT_CONFIRMATION.load(atomic::Ordering::Acquire) {
        return;
    }

    let should_confirm = WorkspaceSettings::get_global(cx).confirm_quit;
    cx.spawn(async move |cx| {
        let mut workspace_windows: Vec<WindowHandle<workspace::MultiWorkspace>> = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<workspace::MultiWorkspace>())
                .collect::<Vec<_>>()
        });

        cx.update(|cx| {
            workspace_windows.sort_by_key(|window| window.is_active(cx) == Some(false));
        });

        if should_confirm {
            if let Some(multi_workspace) = workspace_windows.first() {
                let answer = multi_workspace
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Info,
                            "Are you sure you want to quit?",
                            None,
                            &["Quit", "Cancel"],
                            cx,
                        )
                    })
                    .log_err();

                if let Some(answer) = answer {
                    WAITING_QUIT_CONFIRMATION.store(true, atomic::Ordering::Release);
                    let answer = answer.await.ok();
                    WAITING_QUIT_CONFIRMATION.store(false, atomic::Ordering::Release);
                    if answer != Some(0) {
                        return Ok(());
                    }
                }
            }
        }

        for window in &workspace_windows {
            let window = *window;
            let workspaces = window
                .update(cx, |multi_workspace, _, _cx| {
                    multi_workspace.workspaces().cloned().collect::<Vec<_>>()
                })
                .log_err();

            let Some(workspaces) = workspaces else {
                continue;
            };

            for workspace in workspaces {
                if let Some(should_close) = window
                    .update(cx, |multi_workspace, window, cx| {
                        multi_workspace.activate(workspace.clone(), None, window, cx);
                        window.activate_window();
                        workspace.update(cx, |workspace, cx| {
                            workspace.prepare_to_close(CloseIntent::Quit, window, cx)
                        })
                    })
                    .log_err()
                {
                    if !should_close.await? {
                        return Ok(());
                    }
                }
            }
        }

        cx.update(|cx| cx.quit());
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn bind_on_window_closed(cx: &mut App) -> Option<gpui::Subscription> {
    #[cfg(target_os = "macos")]
    {
        WorkspaceSettings::get_global(cx)
            .on_last_window_closed
            .is_quit_app()
            .then(|| {
                cx.on_window_closed(|cx, _window_id| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }))
    }
}

pub fn build_window_options(display_uuid: Option<Uuid>, cx: &mut App) -> WindowOptions {
    let display = display_uuid.and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|display| display.uuid().ok() == Some(uuid))
    });
    let window_decorations = match std::env::var("ZED_WINDOW_DECORATIONS") {
        Ok(val) if val == "server" => gpui::WindowDecorations::Server,
        Ok(val) if val == "client" => gpui::WindowDecorations::Client,
        _ => match WorkspaceSettings::get_global(cx).window_decorations {
            settings::WindowDecorations::Server => gpui::WindowDecorations::Server,
            settings::WindowDecorations::Client => gpui::WindowDecorations::Client,
        },
    };

    let use_system_window_tabs = WorkspaceSettings::get_global(cx).use_system_window_tabs;

    // `window.mode` in settings.json (see `som_config::WindowMode`) governs
    // the window's placement on every launch — this is deliberately
    // authoritative (not just a first-run default), driven by Som's own
    // `db.json` for the `windowed` restore rect rather than Zed's inherited
    // SQLite window-bounds persistence, matching how tabs/splits already
    // work. See `som_db::SomWindowBounds`.
    let som_window_config = crate::som_config::SomConfig::load_embedded().window;
    let window_mode = som_window_config.mode();
    let restore_bounds = || {
        display
            .as_ref()
            .map(|d| d.default_bounds())
            .unwrap_or_else(|| {
                gpui::Bounds::new(point(px(0.), px(0.)), gpui::Size { width: px(1024.), height: px(768.) })
            })
    };
    let windowed_bounds = || -> gpui::Bounds<gpui::Pixels> {
        // `window.top`/`left`/`width`/`height` in settings.json — takes
        // priority over db.json's remembered geometry whenever all four are
        // set and non-zero (see `WindowConfig::explicit_windowed_bounds`'s
        // doc comment for the all-or-nothing rule). Otherwise, same as
        // before: db.json's remembered rect, or the no-geometry-yet default.
        if let Some((top, left, width, height)) = som_window_config.explicit_windowed_bounds() {
            return gpui::Bounds::new(
                point(px(left as f32), px(top as f32)),
                gpui::Size { width: px(width as f32), height: px(height as f32) },
            );
        }
        if let Some(b) = workspace::som_db::load_som_db().window_bounds {
            return gpui::Bounds::new(
                point(px(b.x as f32), px(b.y as f32)),
                gpui::Size { width: px(b.width as f32), height: px(b.height as f32) },
            );
        }
        // No remembered geometry yet — 100px smaller than the display in
        // each dimension, inset 50px from the top-left.
        let screen = display
            .as_ref()
            .map(|d| d.bounds())
            .unwrap_or_else(|| gpui::Bounds::new(point(px(0.), px(0.)), gpui::Size { width: px(1920.), height: px(1080.) }));
        gpui::Bounds::new(
            point(px(50.), px(50.)),
            gpui::Size {
                width: px((f32::from(screen.size.width) - 100.).max(360.)),
                height: px((f32::from(screen.size.height) - 100.).max(240.)),
            },
        )
    };
    let window_bounds = match window_mode {
        crate::som_config::WindowMode::Maximized => {
            Some(gpui::WindowBounds::Maximized(restore_bounds()))
        }
        crate::som_config::WindowMode::Fullscreen => {
            Some(gpui::WindowBounds::Fullscreen(restore_bounds()))
        }
        crate::som_config::WindowMode::Windowed => {
            Some(gpui::WindowBounds::Windowed(windowed_bounds()))
        }
        crate::som_config::WindowMode::Minimized => None,
    };
    let start_minimized = window_mode == crate::som_config::WindowMode::Minimized
        || crate::START_MINIMIZED.load(std::sync::atomic::Ordering::Relaxed);

    WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Som".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), px(9.0))),
        }),
        window_bounds,
        focus: false,
        show: false,
        kind: WindowKind::Normal,
        is_movable: true,
        display_id: display.map(|display| display.id()),
        window_background: cx.theme().window_background_appearance(),
        window_decorations: Some(window_decorations),
        window_min_size: Some(gpui::Size {
            width: px(360.0),
            height: px(240.0),
        }),
        tabbing_identifier: if use_system_window_tabs {
            Some(String::from("zed"))
        } else {
            None
        },
        app_id: Some("dev.som.Som".to_owned()),
        start_minimized,
        ..Default::default()
    }
}

pub fn initialize_workspace(app_state: Arc<AppState>, cx: &mut App) {
    let mut _on_close_subscription = bind_on_window_closed(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        _ = _on_close_subscription.is_some();
        _on_close_subscription = bind_on_window_closed(cx);
    })
    .detach();

    init_cursor_hide_mode(cx);

    cx.observe_new(|_multi_workspace: &mut MultiWorkspace, window, cx| {
        let Some(window) = window else { return };

        let multi_workspace_handle = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            multi_workspace_handle
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.close_window(&CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });
    })
    .detach();

    cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };

        #[cfg(not(any(test, target_os = "macos")))]
        initialize_file_watcher(window, cx);

        if let Some(specs) = window.gpu_specs() {
            log::info!("Using GPU: {:?}", specs);
            show_software_emulation_warning_if_needed(specs.clone(), window, cx);
        }

        register_actions(app_state.clone(), workspace, window, cx);

        if !workspace.has_active_modal(window, cx) {
            workspace.focus_handle(cx).focus(window, cx);
        }
    })
    .detach();
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[allow(unused)]
fn initialize_file_watcher(window: &mut Window, cx: &mut Context<Workspace>) {
    if let Err(e) = fs::fs_watcher::global(|_| {}) {
        let message = format!(
            indoc::indoc! {r#"
            inotify_init returned {}

            This may be due to system-wide limits on inotify instances.
            "#},
            e
        );
        let prompt = window.prompt(
            PromptLevel::Critical,
            "Could not start inotify",
            Some(&message),
            &["Quit"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if prompt.await == Ok(0) {
                cx.update(|cx| cx.quit());
            }
        })
        .detach()
    }
}

#[cfg(target_os = "windows")]
#[allow(unused)]
fn initialize_file_watcher(window: &mut Window, cx: &mut Context<Workspace>) {
    if let Err(e) = fs::fs_watcher::global(|_| {}) {
        let message = format!(
            indoc::indoc! {r#"
            ReadDirectoryChangesW initialization failed: {}
            "#},
            e
        );
        let prompt = window.prompt(
            PromptLevel::Critical,
            "Could not start ReadDirectoryChangesW",
            Some(&message),
            &["Quit"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if prompt.await == Ok(0) {
                cx.update(|cx| cx.quit())
            }
        })
        .detach()
    }
}

fn show_software_emulation_warning_if_needed(
    specs: gpui::GpuSpecs,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if specs.is_software_emulated && std::env::var("ZED_ALLOW_EMULATED_GPU").is_err() {
        let (graphics_api, docs_url, open_url) = if cfg!(target_os = "windows") {
            ("DirectX", "https://zed.dev/docs/windows", "https://zed.dev/docs/windows")
        } else {
            ("Vulkan", "https://zed.dev/docs/linux", "https://zed.dev/docs/linux#zed-fails-to-open-windows")
        };
        let message = format!(
            indoc::indoc! {r#"
            Som uses {} for rendering and requires a compatible GPU.

            Currently you are using a software emulated GPU ({}) which
            will result in awful performance.

            For troubleshooting see: {}
            Set ZED_ALLOW_EMULATED_GPU=1 env var to permanently override.
            "#},
            graphics_api, specs.device_name, docs_url
        );
        let prompt = window.prompt(
            PromptLevel::Critical,
            "Unsupported GPU",
            Some(&message),
            &["Skip", "Troubleshoot and Quit"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if prompt.await == Ok(1) {
                cx.update(|cx| {
                    cx.open_url(open_url);
                    cx.quit();
                });
            }
        })
        .detach()
    }
}

fn register_actions(
    app_state: Arc<AppState>,
    workspace: &mut Workspace,
    _: &mut Window,
    _cx: &mut Context<Workspace>,
) {
    workspace
        .register_action(|_, _: &Minimize, window, _| window.minimize_window())
        .register_action(|_, _: &Zoom, window, _| window.zoom_window())
        .register_action(|_, _: &ToggleFullScreen, window, _| window.toggle_fullscreen())
        .register_action(|_, action: &OpenZedUrl, _, cx| {
            OpenListener::global(cx).open(RawOpenRequest {
                urls: vec![action.url.clone()],
                ..Default::default()
            })
        })
        .register_action(|workspace, action: &OpenBrowser, _window, cx| {
            match url::Url::parse(&action.url) {
                Ok(parsed_url) => cx.open_url(parsed_url.as_str()),
                Err(e) => workspace.show_error(
                    &anyhow::anyhow!(
                        "Opening this URL failed: {}\n\nError: {e}",
                        action.url
                    ),
                    cx,
                ),
            }
        })
        .register_action(|workspace, action: &workspace::Open, window, cx| {
            workspace::prompt_for_open_path_and_open(
                workspace,
                workspace.app_state().clone(),
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: true,
                    prompt: None,
                },
                action.create_new_window,
                window,
                cx,
            );
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::IncreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let size = ThemeSettings::get_global(cx).ui_font_size(cx) + px(1.0);
                        let _ = settings.theme.ui_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(size)).into());
                    });
                } else {
                    theme_settings::adjust_ui_font_size(&mut **cx, |size| size + px(1.0));
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::DecreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let size = ThemeSettings::get_global(cx).ui_font_size(cx) - px(1.0);
                        let _ = settings.theme.ui_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(size)).into());
                    });
                } else {
                    theme_settings::adjust_ui_font_size(&mut **cx, |size| size - px(1.0));
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, _| {
                        settings.theme.ui_font_size = None;
                    });
                } else {
                    theme_settings::reset_ui_font_size(&mut **cx);
                }
            }
        })
        .register_action(
            |_, _: &zed_actions::IncreaseBufferFontSize, _window, cx| {
                theme_settings::increase_buffer_font_size(cx);
            },
        )
        .register_action(
            |_, _: &zed_actions::DecreaseBufferFontSize, _window, cx| {
                theme_settings::decrease_buffer_font_size(cx);
            },
        )
        .register_action(
            |_, _: &zed_actions::ResetBufferFontSize, _window, cx| {
                theme_settings::reset_buffer_font_size(cx);
            },
        )
        // NewWindow is deliberately not registered — Som only ever has one
        // window, and the action fires from inside that window anyway, so
        // there is nothing to do (see `Workspace::new_local`'s
        // `window_to_replace` logic, which every other window-opening path
        // already routes through the existing window rather than creating
        // a second one).
        ;
}

fn open_log_file(
    _workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.open_url(&format!("file://{}", paths::log_file().display()));
}

fn open_settings_file(
    abs_path: &'static Path,
    default_content: impl FnOnce() -> rope::Rope + Send + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.with_local_or_wsl_workspace(window, cx, move |workspace, window, cx| {
                    let project = workspace.project().clone();
                    cx.spawn_in(window, async move |workspace, cx| {
                        let config_dir = project
                            .update(cx, |project, cx| {
                                project.try_windows_path_to_wsl(paths::config_dir().as_path(), cx)
                            })
                            .await?;
                        let (_worktree, _) = project
                            .update(cx, |project, cx| {
                                project.find_or_create_worktree(&config_dir, false, cx)
                            })
                            .await?;
                        workspace
                            .update_in(cx, |_, window, cx| {
                                workspace::create_and_open_local_file(abs_path, window, cx, default_content)
                            })?
                            .await?;
                        anyhow::Ok(())
                    })
                })
            })?
            .await?
            .await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}


#[derive(Copy, Clone, Debug, settings::RegisterSetting)]
struct CursorHideModeSetting(gpui::CursorHideMode);

impl Settings for CursorHideModeSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(match content.hide_mouse.unwrap_or_default() {
            settings::HideMouseMode::Never => gpui::CursorHideMode::Never,
            settings::HideMouseMode::OnTyping => gpui::CursorHideMode::OnTyping,
            settings::HideMouseMode::OnTypingAndAction => gpui::CursorHideMode::OnTypingAndAction,
        })
    }
}

fn init_cursor_hide_mode(cx: &mut App) {
    let apply = |cx: &mut App| cx.set_cursor_hide_mode(CursorHideModeSetting::get_global(cx).0);
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

pub fn watch_settings_files(_fs: Arc<dyn fs::Fs>, _cx: &mut App) {
    // Som manages settings.json itself via SomConfig — SettingsStore must not
    // parse it, since the file contains Som-specific fields unknown to Zed.
}

pub fn handle_keymap_file_changes(
    mut user_keymap_file_rx: mpsc::UnboundedReceiver<String>,
    user_keymap_watcher: gpui::Task<()>,
    cx: &mut App,
) {
    let (base_keymap_tx, mut base_keymap_rx) = mpsc::unbounded();
    let (keyboard_layout_tx, mut keyboard_layout_rx) = mpsc::unbounded();
    let mut old_base_keymap = *BaseKeymap::get_global(cx);

    cx.observe_global::<SettingsStore>(move |cx| {
        let new_base_keymap = *BaseKeymap::get_global(cx);
        if new_base_keymap != old_base_keymap {
            old_base_keymap = new_base_keymap;
            base_keymap_tx.unbounded_send(()).unwrap();
        }
    })
    .detach();

    #[cfg(target_os = "windows")]
    {
        let mut current_layout_id = cx.keyboard_layout().id().to_string();
        cx.on_keyboard_layout_change(move |cx| {
            let next_layout_id = cx.keyboard_layout().id();
            if next_layout_id != current_layout_id {
                current_layout_id = next_layout_id.to_string();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut current_mapping = cx.keyboard_mapper().get_key_equivalents().cloned();
        cx.on_keyboard_layout_change(move |cx| {
            let next_mapping = cx.keyboard_mapper().get_key_equivalents();
            if current_mapping.as_ref() != next_mapping {
                current_mapping = next_mapping.cloned();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    load_default_keymap(cx);

    struct KeymapParseErrorNotification;
    let notification_id = NotificationId::unique::<KeymapParseErrorNotification>();

    cx.spawn(async move |cx| {
        let _user_keymap_watcher = user_keymap_watcher;
        let mut user_keymap_content = String::new();
        loop {
            select_biased! {
                _ = base_keymap_rx.next() => {},
                _ = keyboard_layout_rx.next() => {},
                content = user_keymap_file_rx.next() => {
                    if let Some(content) = content {
                        user_keymap_content = content;
                    }
                }
            };
            cx.update(|cx| {
                let load_result = KeymapFile::load(&user_keymap_content, cx);
                match load_result {
                    KeymapFileLoadResult::Success { key_bindings } => {
                        reload_keymaps(cx, key_bindings);
                        dismiss_app_notification(&notification_id.clone(), cx);
                    }
                    KeymapFileLoadResult::SomeFailedToLoad { key_bindings, error_message } => {
                        if !key_bindings.is_empty() {
                            reload_keymaps(cx, key_bindings);
                        }
                        show_keymap_file_load_error(notification_id.clone(), error_message, cx);
                    }
                    KeymapFileLoadResult::JsonParseFailure { error } => {
                        show_keymap_file_json_error(notification_id.clone(), &error, cx)
                    }
                }
            });
        }
    })
    .detach();
}

fn show_keymap_file_json_error(
    notification_id: NotificationId,
    error: &anyhow::Error,
    cx: &mut App,
) {
    let message: SharedString =
        format!("JSON parse error in keymap file. Bindings not reloaded.\n\n{error}").into();
    show_app_notification(notification_id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Keymap File")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenKeymapFile.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

fn show_keymap_file_load_error(
    notification_id: NotificationId,
    error_message: util::markdown::MarkdownString,
    cx: &mut App,
) {
    show_app_notification(notification_id, cx, move |cx| {
        let msg = error_message.0.clone();
        cx.new(|cx| {
            MessageNotification::new(format!("Invalid keymap file\n{msg}"), cx)
                .primary_message("Open Keymap File")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenKeymapFile.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

fn reload_keymaps(cx: &mut App, mut user_key_bindings: Vec<KeyBinding>) {
    cx.clear_key_bindings();
    load_default_keymap(cx);

    for key_binding in &mut user_key_bindings {
        key_binding.set_meta(KeybindSource::User.meta());
    }
    cx.bind_keys(user_key_bindings);

    // Re-apply som custom bindings after every keymap reload, since clear_key_bindings wipes them.
    crate::som_config::SomConfig::load_embedded().apply_keys(cx);

    let menus = app_menus(cx);
    cx.set_menus(menus);
}

pub fn load_default_keymap(cx: &mut App) {
    let base_keymap = *BaseKeymap::get_global(cx);
    if base_keymap == BaseKeymap::None || DEFAULT_KEYMAP_PATH.is_empty() {
        return;
    }
    cx.bind_keys(
        KeymapFile::load_asset(DEFAULT_KEYMAP_PATH, Some(KeybindSource::Default), cx).unwrap(),
    );
    if let Some(asset_path) = base_keymap.asset_path() {
        cx.bind_keys(
            KeymapFile::load_asset(asset_path, Some(KeybindSource::Base), cx).unwrap(),
        );
    }
}

