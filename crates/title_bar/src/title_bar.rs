mod application_menu;
pub mod collab;
mod onboarding_banner;
mod title_bar_settings;

use crate::application_menu::ApplicationMenu;
pub use platform_title_bar::{
    self, DraggedWindowTab, MergeAllWindows, MoveTabToNewWindow, PlatformTitleBar,
    ShowNextWindowTab, ShowPreviousWindowTab,
};

#[cfg(not(target_os = "macos"))]
use crate::application_menu::{
    ActivateDirection, ActivateMenuLeft, ActivateMenuRight, OpenApplicationMenu,
};

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Subscription, WeakEntity, Window, actions, canvas, div,
};
use ui::PopoverMenuHandle;
use project::{
    Project,
    trusted_worktrees::TrustedWorktrees,
};
use settings::Settings as _;

use title_bar_settings::TitleBarSettings;
use ui::{
    TintColor, Tooltip, prelude::*, utils::platform_title_bar_height,
};
use util::ResultExt;
use workspace::{MultiWorkspace, ToggleWorktreeSecurity, Workspace};


pub use onboarding_banner::restore_banner;

/// A plain div-based button for use as PopoverMenu trigger, matching window control hover style.
struct TitleBarButton {
    id: gpui::ElementId,
    hover_bg: gpui::Hsla,
    active_bg: gpui::Hsla,
    icon: ui::IconName,
    click_handler: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    height: gpui::Pixels,
}

impl TitleBarButton {
    fn new(id: impl Into<gpui::ElementId>, hover_bg: gpui::Hsla, active_bg: gpui::Hsla, icon: ui::IconName, height: gpui::Pixels) -> Self {
        Self { id: id.into(), hover_bg, active_bg, icon, click_handler: None, height }
    }
}

impl ui::Clickable for TitleBarButton {
    fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.click_handler = Some(Box::new(handler));
        self
    }
    fn cursor_style(self, _: gpui::CursorStyle) -> Self { self }
}

impl ui::Toggleable for TitleBarButton {
    fn toggle_state(self, _selected: bool) -> Self { self }
}

impl IntoElement for TitleBarButton {
    type Element = gpui::AnyElement;
    fn into_element(self) -> Self::Element {
        let hover_bg = self.hover_bg;
        let active_bg = self.active_bg;
        let mut el = div()
            .id(self.id)
            .w(gpui::px(36.))
            .h(self.height)
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .hover(move |s| s.bg(hover_bg))
            .active(move |s| s.bg(active_bg))
            .child(ui::Icon::new(self.icon).size(ui::IconSize::Medium).color(ui::Color::Default));
        if let Some(handler) = self.click_handler {
            el = el.on_click(handler);
        }
        el.into_any_element()
    }
}

pub use workspace::TabProfiles;

struct TitlebarTabStrip {
    pane: gpui::Entity<workspace::Pane>,
    titlebar_height: gpui::Pixels,
    scroll_offset: f32,
    container_width: f32,
    content_width: f32,
    _subscription: Subscription,
}

impl TitlebarTabStrip {
    fn new(pane: gpui::Entity<workspace::Pane>, titlebar_height: gpui::Pixels, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&pane, |_, _, cx| cx.notify());
        Self { pane, titlebar_height, scroll_offset: 0.0, container_width: 0.0, content_width: 0.0, _subscription: sub }
    }
}

impl Render for TitlebarTabStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = self.titlebar_height;
        let tabs = self.pane.update(cx, |pane, cx| {
            pane.render_tabs_for_titlebar(height, window, cx)
        });
        let scroll_offset = self.scroll_offset;
        let max_scroll = (self.content_width - self.container_width).max(0.0);
        let clamped = scroll_offset.min(max_scroll);
        div()
            .id("titlebar-tabs")
            .flex()
            .flex_row()
            .h_full()
            .w_auto()
            .max_w_full()
            .overflow_x_hidden()
            .occlude()
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(gpui::px(24.0));
                let dx = if delta.x.abs() >= delta.y.abs() {
                    delta.x.as_f32()
                } else {
                    delta.y.as_f32()
                };
                let max = (this.content_width - this.container_width).max(0.0);
                this.scroll_offset = (this.scroll_offset + dx).clamp(0.0, max);
                cx.notify();
            }))
            .child({
                let entity = cx.entity().downgrade();
                canvas(
                    move |bounds, _, cx| {
                        entity.update(cx, |this, _| {
                            this.container_width = bounds.size.width.as_f32();
                        }).ok();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h_full()
                    .child({
                        let entity = cx.entity().downgrade();
                        canvas(
                            move |bounds, _, cx| {
                                entity.update(cx, |this, _| {
                                    this.content_width = bounds.size.width.as_f32();
                                }).ok();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .h_full()
                        .w_full()
                    })
                    .map(move |el| {
                        let mut el = el;
                        el.style().margin.left = Some(gpui::Length::Definite(
                            gpui::DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(
                                gpui::px(0.0 - clamped)
                            ))
                        ));
                        el
                    })
                    .children(tabs)
            )
    }
}

actions!(
    collab,
    [
        /// Toggles the user menu dropdown.
        ToggleUserMenu,
        /// Toggles the project menu dropdown.
        ToggleProjectMenu,
        /// Switches to a different git branch.
        SwitchBranch,
    ]
);

pub fn init(cx: &mut App) {
    platform_title_bar::PlatformTitleBar::init(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let multi_workspace = workspace.multi_workspace().cloned();
        let item = cx.new(|cx| TitleBar::new("title-bar", workspace, multi_workspace, window, cx));
        workspace.set_titlebar_item(item.into(), window, cx);

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, action: &OpenApplicationMenu, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| menu.open_menu(action, window, cx));
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuRight, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Right, window, cx)
                        });
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuLeft, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Left, window, cx)
                        });
                    }
                });
            }
        });
    })
    .detach();
}

pub struct TitleBar {
    platform_titlebar: Entity<PlatformTitleBar>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    application_menu: Option<Entity<ApplicationMenu>>,
    profiles_menu_handle: PopoverMenuHandle<ui::ContextMenu>,
    tab_strip: Option<Entity<TitlebarTabStrip>>,
    _subscriptions: Vec<Subscription>,
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.multi_workspace.is_none() {
            if let Some(mw) = self
                .workspace
                .upgrade()
                .and_then(|ws| ws.read(cx).multi_workspace().cloned())
            {
                self.multi_workspace = Some(mw.clone());
                self.platform_titlebar.update(cx, |titlebar, _cx| {
                    titlebar.set_multi_workspace(mw);
                });
            }
        }

        // Lazily create tab_strip once panes[0] is available
        if self.tab_strip.is_none() {
            if let Some(pane) = self.workspace.upgrade()
                .and_then(|ws| ws.read(cx).panes().first().cloned())
            {
                let height = ui::utils::platform_title_bar_height(window);
                self.tab_strip = Some(cx.new(|cx| TitlebarTabStrip::new(pane, height, cx)));
            }
        }

        let title_bar_settings = *TitleBarSettings::get_global(cx);
        let button_layout = title_bar_settings.button_layout;

        // Single row: [tabs...flex][drag zone][+][v][─][□][✕]
        let content = self.render_tab_controls(window, cx).into_any_element();

        self.platform_titlebar.update(cx, |this, _| {
            this.set_button_layout(button_layout);
            this.set_children([content]);
        });
        self.platform_titlebar.clone().into_any_element()
    }
}

impl TitleBar {
    pub fn new(
        id: impl Into<ElementId>,
        workspace: &Workspace,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let platform_style = PlatformStyle::platform();
        let application_menu = match platform_style {
            PlatformStyle::Mac => {
                if option_env!("ZED_USE_CROSS_PLATFORM_MENU").is_some() {
                    Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
                } else {
                    None
                }
            }
            PlatformStyle::Linux | PlatformStyle::Windows => {
                Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
            }
        };

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&workspace.weak_handle().upgrade().unwrap(), |_, _, cx| {
                cx.notify()
            }),
        );

        subscriptions.push(cx.observe_window_activation(window, Self::window_activation_changed));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(
                &workspace_entity,
                |_, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::WorktreeCreationChanged) {
                        cx.notify();
                    }
                },
            ));
        }
        subscriptions.push(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(&trusted_worktrees, |_, _, _, cx| {
                cx.notify();
            }));
        }

        let platform_titlebar = cx.new(|cx| {
            let mut titlebar = PlatformTitleBar::new(id, cx);
            if let Some(mw) = multi_workspace.clone() {
                titlebar = titlebar.with_multi_workspace(mw);
            }
            titlebar
        });

        let this = Self {
            platform_titlebar,
            application_menu,
            workspace: workspace.weak_handle(),
            multi_workspace,
            project,
            profiles_menu_handle: PopoverMenuHandle::default(),
            tab_strip: None,
            _subscriptions: subscriptions,
        };

        this
    }

    fn render_tab_controls(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let profiles = cx
            .try_global::<TabProfiles>()
            .cloned()
            .unwrap_or_default()
            .0;

        let profiles2 = profiles.clone();
        let hover_bg = cx.theme().colors().ghost_element_hover;
        let active_bg = cx.theme().colors().ghost_element_active;
        let titlebar_height = platform_title_bar_height(window);
        let workspace_focus = self.workspace.upgrade().map(|ws| ws.focus_handle(cx));

        let tabs_element: Option<gpui::AnyElement> = self.tab_strip.as_ref().map(|strip| {
            strip.clone().into_any_element()
        });

        h_flex()
            .h_full()
            .w_full()
            .when_some(tabs_element, |this, tabs| {
                this.child(tabs)
            })
            // Drag zone: fills remaining space, min width = 2 buttons (72px)
            .child(
                div()
                    .id("titlebar-drag-zone")
                    .flex_1()
                    .min_w(px(72.))
                    .h_full()
                    .on_click(|ev: &ClickEvent, window, _cx| {
                        if ev.click_count() == 2 {
                            window.zoom_window();
                        }
                    }),
            )
            // + button
            .child(
                div()
                    .id("new-terminal")
                    .flex_none()
                    .w(px(36.))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .occlude()
                    .hover(|s| s.bg(hover_bg))
                    .active(|s| s.bg(active_bg))
                    .child(Icon::new(IconName::Plus).size(IconSize::Medium).color(Color::Default))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(workspace::NewTerminal::default()), cx);
                    })),
            )
            // v button (single profile = opens that profile, multiple = popover menu)
            .when(profiles.len() == 1, |this| {
                let tab_name = profiles[0].0.clone();
                let tab_shell = profiles[0].1.clone();
                this.child(
                    div()
                        .id("new-terminal-profile")
                        .flex_none()
                        .w(px(36.))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .occlude()
                        .hover(|s| s.bg(hover_bg))
                        .active(|s| s.bg(active_bg))
                        .child(Icon::new(IconName::ChevronDown).size(IconSize::Medium).color(Color::Default))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            window.dispatch_action(Box::new(workspace::NewTerminal {
                                local: false,
                                tab_name: Some(tab_name.clone()),
                                shell: tab_shell.clone(),
                            }), cx);
                        })),
                )
            })
            .when(profiles.len() > 1, |this| {
                this.child(
                    div().flex_none().h_full().child(ui::PopoverMenu::new("terminal-profiles")
                        .with_handle(self.profiles_menu_handle.clone())
                        .anchor(gpui::Anchor::TopRight)
                        .attach(gpui::Anchor::BottomRight)
                        .offset(gpui::point(gpui::px(0.), gpui::px(0.)))
                        .trigger(
                            TitleBarButton::new("terminal-profiles-trigger", hover_bg, active_bg, IconName::ChevronDown, titlebar_height)
                        )
                        .menu(move |window, cx| {
                            let profiles = profiles2.clone();
                            let workspace_focus = workspace_focus.clone();
                            Some(ui::ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                                for (name, shell, keystroke, icon) in &profiles {
                                    let name = name.clone();
                                    let shell = shell.clone();
                                    let icon = icon.clone();
                                    let binding = keystroke.as_deref().and_then(|ks| {
                                        let keystrokes: std::rc::Rc<[gpui::KeybindingKeystroke]> = ks
                                            .split_whitespace()
                                            .filter_map(|chunk| gpui::Keystroke::parse(chunk).ok())
                                            .map(gpui::KeybindingKeystroke::from_keystroke)
                                            .collect::<Vec<_>>()
                                            .into();
                                        if keystrokes.is_empty() { None } else { Some(ui::KeyBinding::from_keystrokes(keystrokes, false)) }
                                    });
                                    let name_for_action = name.clone();
                                    let workspace_focus = workspace_focus.clone();
                                    menu = menu.custom_entry(
                                        {
                                            let name = name.clone();
                                            let binding = binding.clone();
                                            move |_window, _cx| {
                                                let icon_element: AnyElement = if let Some(ref ic) = icon {
                                                    div()
                                                        .font_family("FiraCode Nerd Font")
                                                        .child(ic.clone())
                                                        .into_any_element()
                                                } else {
                                                    Icon::new(IconName::Terminal).size(IconSize::Small).color(Color::Default).into_any_element()
                                                };
                                                let row = h_flex()
                                                    .w_full()
                                                    .justify_between()
                                                    .child(
                                                        h_flex()
                                                            .gap_2()
                                                            .child(icon_element)
                                                            .child(Label::new(name.clone()))
                                                    );
                                                if let Some(b) = binding.clone() {
                                                    row.child(div().ml_4().child(b)).into_any_element()
                                                } else {
                                                    row.into_any_element()
                                                }
                                            }
                                        },
                                        {
                                            let name_for_action = name_for_action.clone();
                                            move |window, cx| {
                                                if let Some(ref focus) = workspace_focus {
                                                    window.focus(focus, cx);
                                                }
                                                window.dispatch_action(
                                                    Box::new(workspace::NewTerminal {
                                                        local: false,
                                                        tab_name: Some(name_for_action.clone()),
                                                        shell: shell.clone(),
                                                    }),
                                                    cx,
                                                );
                                            }
                                        },
                                    );
                                }
                                menu
                            }))
                        }),
                    )
                )
            })
    }

    pub fn render_restricted_mode(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let has_restricted_worktrees =
            TrustedWorktrees::has_restricted_worktrees(&self.project.read(cx).worktree_store(), cx);
        if !has_restricted_worktrees {
            return None;
        }

        let button = Button::new("restricted_mode_trigger", "Restricted Mode")
            .style(ButtonStyle::Tinted(TintColor::Warning))
            .label_size(LabelSize::Small)
            .color(Color::Warning)
            .start_icon(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .tooltip(|_, cx| {
                Tooltip::with_meta(
                    "You're in Restricted Mode",
                    Some(&ToggleWorktreeSecurity),
                    "Mark this project as trusted and unlock all features",
                    cx,
                )
            })
            .on_click({
                cx.listener(move |this, _, window, cx| {
                    this.workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_worktree_trust_security_modal(true, window, cx)
                        })
                        .log_err();
                })
            });

        if cfg!(macos_sdk_26) {
            // Make up for Tahoe's traffic light buttons having less spacing around them
            Some(div().child(button).ml_0p5().into_any_element())
        } else {
            Some(button.into_any_element())
        }
    }

    fn window_activation_changed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
    }

}
