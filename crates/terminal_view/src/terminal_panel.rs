use std::{cmp, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    TerminalView, default_working_directory,
    persistence::{
        SerializedItems, SerializedTerminalPanel, deserialize_terminal_panel, serialize_pane_group,
    },
    som_tmux_session,
    som_tmux_view::SomTmuxView,
};
use db::kvp::KeyValueStore;
use gpui::{
    Action, Anchor, AnyView, App, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render, Styled, Task, TaskExt,
    WeakEntity, Window, actions,
};
use project::{Fs, Project};

use settings::{Settings, TerminalDockPosition};
use task::{RevealStrategy, Shell, ShellBuilder, SpawnInTerminal};
use terminal::{Terminal, terminal_settings::TerminalSettings};
use ui::{
    ButtonLike, Clickable, ContextMenu, FluentBuilder, PopoverMenu, SplitButton, Toggleable,
    Tooltip, prelude::*,
};
use util::{ResultExt, TryFutureExt};
use uuid::Uuid;
use workspace::{
    ActivateNextPane, ActivatePane, ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight,
    ActivatePaneUp, ActivatePreviousPane, DraggedTab, ItemId, MoveItemToPane,
    MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft, MovePaneRight, MovePaneUp, Pane,
    PaneGroup, SomTabsRestorer, SplitDirection, SplitDown, SplitLeft, SplitMode, SplitRight,
    SplitUp, SwapPaneDown, SwapPaneLeft, SwapPaneRight, SwapPaneUp, TabProfiles, ToggleZoom,
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    item::SerializableItem,
    move_active_item, pane,
};

use anyhow::{Result, anyhow};
use zed_actions::assistant::InlineAssist;

const TERMINAL_PANEL_KEY: &str = "TerminalPanel";

actions!(
    terminal_panel,
    [
        /// Toggles the terminal panel.
        Toggle,
        /// Toggles focus on the terminal panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.set_global(SomTabsRestorer(std::sync::Arc::new(
        |workspace, window, cx| TerminalPanel::restore_som_tabs(workspace, window, cx),
    )));
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
            workspace.register_action(TerminalPanel::open_terminal);
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    if !workspace.toggle_panel_focus::<TerminalPanel>(window, cx) {
                        workspace.close_panel::<TerminalPanel>(window, cx);
                    }
                }
            });
        },
    )
    .detach();
}

pub struct TerminalPanel {
    pub(crate) active_pane: Entity<Pane>,
    pub(crate) center: PaneGroup,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    pending_serialization: Task<Option<()>>,
    pending_terminals_to_add: usize,
    assistant_enabled: bool,
    assistant_tab_bar_button: Option<AnyView>,
    active: bool,
}

impl TerminalPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project();
        let pane = new_terminal_pane(workspace.weak_handle(), project.clone(), false, window, cx);
        let center = PaneGroup::new(pane.clone());
        let terminal_panel = Self {
            center,
            active_pane: pane,
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            pending_serialization: Task::ready(None),
            pending_terminals_to_add: 0,
            assistant_enabled: false,
            assistant_tab_bar_button: None,
            active: false,
        };
        terminal_panel.apply_tab_bar_buttons(&terminal_panel.active_pane, cx);
        terminal_panel
    }

    pub fn set_assistant_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.assistant_enabled = enabled;
        if enabled {
            let focus_handle = self
                .active_pane
                .read(cx)
                .active_item()
                .map(|item| item.item_focus_handle(cx))
                .unwrap_or(self.focus_handle(cx));
            self.assistant_tab_bar_button = Some(
                cx.new(move |_| InlineAssistTabBarButton { focus_handle })
                    .into(),
            );
        } else {
            self.assistant_tab_bar_button = None;
        }
        for pane in self.center.panes() {
            self.apply_tab_bar_buttons(pane, cx);
        }
    }

    pub(crate) fn apply_tab_bar_buttons(
        &self,
        terminal_pane: &Entity<Pane>,
        cx: &mut Context<Self>,
    ) {
        let assistant_tab_bar_button = self.assistant_tab_bar_button.clone();
        terminal_pane.update(cx, |pane, cx| {
            pane.set_render_tab_bar_buttons(cx, move |pane, window, cx| {
                let split_context = pane
                    .active_item()
                    .and_then(|item| item.downcast::<TerminalView>())
                    .map(|terminal_view| terminal_view.read(cx).focus_handle.clone());
                if !pane.has_focus(window, cx)
                    && !pane.context_menu_focused(window, cx)
                {
                    return (None, None);
                }
                let focus_handle = pane.focus_handle(cx);
                let right_children = h_flex()
                    .gap(DynamicSpacing::Base02.rems(cx))
                    .child(
                        PopoverMenu::new("terminal-tab-bar-popover-menu")
                            .trigger_with_tooltip(
                                IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
                                Tooltip::text("New…"),
                            )
                            .anchor(Anchor::TopRight)
                            .with_handle(pane.new_item_context_menu_handle.clone())
                            .menu(move |window, cx| {
                                let focus_handle = focus_handle.clone();
                                let menu = ContextMenu::build(window, cx, |menu, _, _| {
                                    menu.context(focus_handle.clone())
                                        .action(
                                            "New Terminal",
                                            workspace::NewTerminal::default().boxed_clone(),
                                        )
                                        // We want the focus to go back to terminal panel once task modal is dismissed,
                                        // hence we focus that first. Otherwise, we'd end up without a focused element, as
                                        // context menu will be gone the moment we spawn the modal.
                                        .action(
                                            "Spawn Task",
                                            zed_actions::Spawn::modal().boxed_clone(),
                                        )
                                });

                                Some(menu)
                            }),
                    )
                    .children(assistant_tab_bar_button.clone())
                    .child(
                        PopoverMenu::new("terminal-pane-tab-bar-split")
                            .trigger_with_tooltip(
                                IconButton::new("terminal-pane-split", IconName::Split)
                                    .icon_size(IconSize::Small),
                                Tooltip::text("Split Pane"),
                            )
                            .anchor(Anchor::TopRight)
                            .with_handle(pane.split_item_context_menu_handle.clone())
                            .menu({
                                move |window, cx| {
                                    ContextMenu::build(window, cx, |menu, _, _| {
                                        menu.when_some(
                                            split_context.clone(),
                                            |menu, split_context| menu.context(split_context),
                                        )
                                        .action("Split Right", SplitRight::default().boxed_clone())
                                        .action("Split Left", SplitLeft::default().boxed_clone())
                                        .action("Split Up", SplitUp::default().boxed_clone())
                                        .action("Split Down", SplitDown::default().boxed_clone())
                                    })
                                    .into()
                                }
                            }),
                    )
                    .child({
                        let zoomed = pane.is_zoomed();
                        IconButton::new("toggle_zoom", IconName::Maximize)
                            .icon_size(IconSize::Small)
                            .toggle_state(zoomed)
                            .selected_icon(IconName::Minimize)
                            .on_click(cx.listener(|pane, _, window, cx| {
                                pane.toggle_zoom(&workspace::ToggleZoom, window, cx);
                            }))
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action(
                                    if zoomed { "Zoom Out" } else { "Zoom In" },
                                    &ToggleZoom,
                                    cx,
                                )
                            })
                    })
                    .into_any_element()
                    .into();
                (None, right_children)
            });
        });
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{:?}-{:?}", TERMINAL_PANEL_KEY, id))
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let mut terminal_panel = None;

        if let Some((database_id, serialization_key, kvp)) = workspace
            .read_with(&cx, |workspace, cx| {
                workspace
                    .database_id()
                    .zip(TerminalPanel::serialization_key(workspace))
                    .map(|(id, key)| (id, key, KeyValueStore::global(cx)))
            })
            .ok()
            .flatten()
            && let Some(serialized_panel) = cx
                .background_spawn(async move { kvp.read_kvp(&serialization_key) })
                .await
                .log_err()
                .flatten()
                .map(|panel| serde_json::from_str::<SerializedTerminalPanel>(&panel))
                .transpose()
                .log_err()
                .flatten()
            && let Ok(serialized) = workspace
                .update_in(&mut cx, |workspace, window, cx| {
                    deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        database_id,
                        serialized_panel,
                        window,
                        cx,
                    )
                })?
                .await
        {
            terminal_panel = Some(serialized);
        }

        let terminal_panel = if let Some(panel) = terminal_panel {
            panel
        } else {
            workspace.update_in(&mut cx, |workspace, window, cx| {
                cx.new(|cx| TerminalPanel::new(workspace, window, cx))
            })?
        };

        // Since panels/docks are loaded outside from the workspace, we cleanup here, instead of through the workspace.
        if let Some(workspace) = workspace.upgrade() {
            let cleanup_task = workspace.update_in(&mut cx, |workspace, window, cx| {
                let alive_item_ids = terminal_panel
                    .read(cx)
                    .center
                    .panes()
                    .into_iter()
                    .flat_map(|pane| pane.read(cx).items())
                    .map(|item| item.item_id().as_u64() as ItemId)
                    .collect();
                workspace.database_id().map(|workspace_id| {
                    TerminalView::cleanup(workspace_id, alive_item_ids, window, cx)
                })
            })?;
            if let Some(task) = cleanup_task {
                task.await.log_err();
            }
        }

        if let Some(workspace) = workspace.upgrade() {
            let should_focus = workspace
                .update_in(&mut cx, |workspace, window, cx| {
                    workspace.active_item(cx).is_none()
                        && workspace
                            .is_dock_at_position_open(terminal_panel.position(window, cx), cx)
                })
                .unwrap_or(false);

            if should_focus {
                terminal_panel
                    .update_in(&mut cx, |panel, window, cx| {
                        panel.active_pane.update(cx, |pane, cx| {
                            pane.focus_active_item(window, cx);
                        });
                    })
                    .ok();
            }
        }
        Ok(terminal_panel)
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { .. } => self.serialize(cx),
            pane::Event::RemovedItem { .. } => self.serialize(cx),
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                let _removal_result = self.center.remove(pane, cx);
                if pane_count_before_removal == 1 {
                    self.center.first_pane().update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    });
                    cx.emit(PanelEvent::Close);
                } else if let Some(focus_on_pane) =
                    focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                {
                    focus_on_pane.focus_handle(cx).focus(window, cx);
                }
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(true, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                self.serialize(cx);
            }
            &pane::Event::Split { direction, mode } => {
                match mode {
                    SplitMode::ClonePane | SplitMode::EmptyPane => {
                        let clone = matches!(mode, SplitMode::ClonePane);
                        let new_pane = self.new_pane_with_active_terminal(clone, window, cx);
                        let pane = pane.clone();
                        cx.spawn_in(window, async move |panel, cx| {
                            let Some(new_pane) = new_pane.await else {
                                return;
                            };
                            panel
                                .update_in(cx, |panel, window, cx| {
                                    panel.center.split(&pane, &new_pane, direction, cx);
                                    window.focus(&new_pane.focus_handle(cx), cx);
                                })
                                .ok();
                        })
                        .detach();
                    }
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        let Ok(project) = self
                            .workspace
                            .update(cx, |workspace, _| workspace.project().clone())
                        else {
                            return;
                        };
                        let new_pane =
                            new_terminal_pane(self.workspace.clone(), project, false, window, cx);
                        new_pane.update(cx, |pane, cx| {
                            pane.add_item(item, true, true, None, window, cx);
                        });
                        self.center.split(&pane, &new_pane, direction, cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                };
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
            }
            pane::Event::ItemPinned | pane::Event::ItemUnpinned => {
                self.serialize(cx);
            }

            _ => {}
        }
    }

    fn new_pane_with_active_terminal(
        &mut self,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let workspace = workspace.read(cx);
        let database_id = workspace.database_id();
        let weak_workspace = self.workspace.clone();
        let project = workspace.project().clone();
        let active_pane = &self.active_pane;
        let terminal_view = if clone {
            active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
        } else {
            None
        };
        let working_directory = if clone {
            terminal_view
                .as_ref()
                .and_then(|terminal_view| {
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .working_directory()
                })
                .or_else(|| default_working_directory(workspace, cx))
        } else {
            default_working_directory(workspace, cx)
        };

        let is_zoomed = if clone {
            active_pane.read(cx).is_zoomed()
        } else {
            false
        };
        cx.spawn_in(window, async move |panel, cx| {
            let terminal = project
                .update(cx, |project, cx| match terminal_view {
                    Some(view) => project.clone_terminal(
                        &view.read(cx).terminal.clone(),
                        cx,
                        working_directory,
                    ),
                    None => project.create_terminal_shell(working_directory, cx),
                })
                .await
                .log_err()?;

            panel
                .update_in(cx, move |terminal_panel, window, cx| {
                    let terminal_view = Box::new(cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            weak_workspace.clone(),
                            database_id,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    }));
                    let pane = new_terminal_pane(weak_workspace, project, is_zoomed, window, cx);
                    terminal_panel.apply_tab_bar_buttons(&pane, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item(terminal_view, true, true, None, window, cx);
                    });
                    Some(pane)
                })
                .ok()
                .flatten()
        })
    }

    pub fn open_terminal(
        workspace: &mut Workspace,
        action: &workspace::OpenTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| {
                if action.local {
                    panel.add_local_terminal_shell(RevealStrategy::Always, window, cx)
                } else {
                    panel.add_terminal_shell(
                        Some(action.working_directory.clone()),
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                }
            })
            .detach_and_log_err(cx);
    }

    /// Create a new Terminal tab. This is the single entry point for opening a new
    /// tab in Som — the title bar `+` button, the tab-profile menu, and every
    /// keyboard shortcut all dispatch the same `workspace::NewTerminal` action,
    /// which always lands here. In Som every tab lives in the workspace's main
    /// (center) pane, so this always goes through `add_item_to_main_pane`, which
    /// appends the new tab at the end and clears any stale per-tab split state at
    /// that index. There is intentionally no other path: a second path (through
    /// `TerminalPanel`'s own side-panel pane) used to exist as a Zed-inherited
    /// fallback and could insert the tab next to the currently active one while
    /// skipping the split-state cleanup, causing new tabs to inherit a previous
    /// tab's split panes.
    fn new_terminal(
        workspace: &mut Workspace,
        action: &workspace::NewTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let default_tab_name = cx
            .try_global::<TabProfiles>()
            .and_then(|p| p.0.first().map(|profile| profile.name.clone()));
        let tab_name = action.tab_name.clone().or(default_tab_name);

        let profile = tab_name.as_deref().and_then(|name| workspace::TabProfiles::find_by_name(name, cx));
        // `action.shell` is populated even for the profile's OWN keybinding
        // (`Ctrl+Shift+N` — see `som_config.rs`'s keymap generation, which
        // bakes the profile's shell into the binding so it round-trips
        // through gpui's action-persistence), not just for a genuine
        // user override — so only treat this as an override (and skip the
        // tmux path) if it actually differs from the profile's own shell.
        let is_shell_override = action
            .shell
            .as_deref()
            .is_some_and(|shell| profile.as_ref().and_then(|p| p.shell.as_deref()) != Some(shell));
        if !is_shell_override && profile.as_ref().is_some_and(|p| p.tmux) {
            let profile = profile.unwrap();
            let profile_index = tab_name.as_deref().and_then(|name| TabProfiles::index_by_name(name, cx)).unwrap_or(0);
            let (program, args) = project::terminals::parse_shell_command(
                profile.shell.as_deref().unwrap_or(""),
            );
            let cwd = profile
                .working_dir
                .as_deref()
                .and_then(|dir| shellexpand::full(dir).ok())
                .map(|dir| dir.to_string())
                .or_else(|| {
                    default_working_directory(workspace, cx)
                        .map(|p| p.to_string_lossy().to_string())
                });
            Self::add_center_tmux_terminal_named(
                workspace,
                profile.name.clone(),
                profile.icon.clone(),
                profile_index,
                program,
                args,
                cwd,
                None,
                window,
                cx,
            )
            .detach_and_log_err(cx);
            return;
        }

        let (profile_shell, tab_icon) = tab_name.as_deref()
            .map(|name| TabProfiles::profile_by_name(name, cx))
            .unwrap_or((None, None));
        let profile_index = tab_name.as_deref().and_then(|name| TabProfiles::index_by_name(name, cx));
        let shell_override = action.shell.clone().or(profile_shell);
        let working_directory = default_working_directory(workspace, cx);
        let local = action.local;
        Self::add_center_terminal_named(workspace, tab_name, tab_icon, profile_index, window, cx, move |project, cx| {
            if local {
                project.create_local_terminal(cx)
            } else if let Some(cmd) = shell_override {
                project.create_terminal_with_shell(working_directory, cmd, cx)
            } else {
                project.create_terminal_shell(working_directory, cx)
            }
        })
        .detach_and_log_err(cx);
    }

    pub fn add_center_terminal(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let task = Self::add_center_terminal_named(workspace, None, None, None, window, cx, create_terminal);
        cx.background_spawn(async move { task.await.map(|(_, terminal)| terminal) })
    }

    /// Returns the newly-created tab item's `EntityId` alongside the
    /// terminal handle. Callers that need to find this specific tab's real
    /// position in the main pane later (e.g. `restore_som_tabs`, where
    /// several tabs are created concurrently and may finish out of order)
    /// must match on this id via `Pane::index_for_item` rather than assuming
    /// a fixed index.
    pub fn add_center_terminal_named(
        workspace: &mut Workspace,
        tab_name: Option<String>,
        tab_icon: Option<String>,
        profile_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<(gpui::EntityId, WeakEntity<Terminal>)>> {
        Self::add_center_terminal_named_at(
            workspace,
            tab_name,
            tab_icon,
            profile_index,
            None,
            window,
            cx,
            create_terminal,
        )
    }

    /// Like `add_center_terminal_named`, but pins the tab to `destination_index`
    /// in the main pane instead of always appending. Used by `restore_som_tabs`
    /// so tabs created concurrently still land in `db.json`'s order regardless
    /// of which terminal (local shell vs. ssh) finishes connecting first.
    pub fn add_center_terminal_named_at(
        workspace: &mut Workspace,
        tab_name: Option<String>,
        tab_icon: Option<String>,
        profile_index: Option<usize>,
        destination_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<(gpui::EntityId, WeakEntity<Terminal>)>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let project = workspace.project().downgrade();
        cx.spawn_in(window, async move |workspace, cx| {
            let terminal = project.update(cx, create_terminal)?.await?;

            let item_id = workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new_with_title_and_icon(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        tab_name.clone(),
                        tab_icon.clone(),
                        window,
                        cx,
                    )
                });
                let item_id = terminal_view.entity_id();
                workspace.add_item_to_main_pane_at(
                    Box::new(terminal_view),
                    profile_index,
                    destination_index,
                    window,
                    cx,
                );
                item_id
            })?;
            Ok((item_id, terminal.downgrade()))
        })
    }

    /// Creates a new tab backed by `som-tmux` instead of a plain local PTY —
    /// used when the tab's profile has `tmux: true`. Unlike
    /// `add_center_terminal_named_at`, this doesn't go through `Project` at
    /// all (there's no local `Terminal`/`alacritty_terminal::Term` — the PTY
    /// lives inside `som-tmux-server`, possibly on a different machine
    /// entirely, see `SOM_MUX_PLAN.md`). The connect-or-spawn/`NewSession`
    /// handshake is genuinely blocking network+process work, so it runs in
    /// `cx.background_spawn` first; only once that resolves do we touch
    /// `Workspace`/create the `SomTmuxView` (which needs `AsyncApp` — see
    /// `som_tmux_session`'s doc comments for why the two can't be mixed into
    /// one future).
    /// `existing_session_id` is `Some` on the restore path (`restore_som_tabs`,
    /// coming from db.json's `tmux_sessions`) — if given, tries `Attach` to
    /// that id first (the server may still be alive from before Som
    /// restarted, see the detach-vs-kill semantics in `project_som_tmux`
    /// memory), falling back to spawning a brand new session with
    /// `program`/`args`/`cwd` if the attach fails (server gone, or the
    /// session id no longer exists on it) — same fallback the rest of Som's
    /// restore path already uses when a saved terminal can't be resurrected.
    pub fn add_center_tmux_terminal_named(
        workspace: &mut Workspace,
        profile_name: String,
        tab_icon: Option<String>,
        profile_index: usize,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        existing_session_id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<gpui::EntityId>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!("terminal not yet supported for remote projects")));
        }
        let tab_name = profile_name.clone();
        cx.spawn_in(window, async move |workspace, cx| {
            let (session, snapshot) = {
                let cx = cx.clone();
                let profile_name = profile_name.clone();
                let attach_result = if let Some(session_id) = existing_session_id {
                    Some(
                        som_tmux_session::attach_session(
                            profile_name.clone(),
                            session_id,
                            program.clone(),
                            args.clone(),
                            cwd.clone(),
                            80,
                            24,
                            &cx,
                        )
                        .await,
                    )
                } else {
                    None
                };
                match attach_result {
                    Some(Ok(session_and_grid)) => Ok(session_and_grid),
                    Some(Err(err)) => {
                        log::info!(
                            "tmux restore: attach to saved session failed ({err:#}), starting a new session instead"
                        );
                        som_tmux_session::create_session(profile_name, program, args, cwd, 80, 24, &cx).await
                    }
                    None => {
                        som_tmux_session::create_session(profile_name, program, args, cwd, 80, 24, &cx).await
                    }
                }
            }?;

            let (item_id, view) = workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    SomTmuxView::new(
                        session.clone(),
                        snapshot,
                        Some(tab_name),
                        tab_icon,
                        workspace.weak_handle(),
                        workspace.database_id(),
                        cx,
                    )
                });
                let item_id = view.entity_id();
                workspace.add_item_to_main_pane(Box::new(view.clone()), Some(profile_index), window, cx);
                // Splits aren't implemented for tmux tabs yet (SomTmuxView::can_split()
                // is false), so this is always a single-session vec for now — see
                // `set_tmux_sessions_for_item`'s doc comment.
                workspace.set_tmux_sessions_for_item(item_id, vec![session.session_id()]);
                (item_id, view)
            })?;

            som_tmux_session::start_read_loop(&session, view.downgrade(), cx);

            Ok(item_id)
        })
    }

    /// Restores tabs and their split panes from `~/.config/som/db.json` at
    /// launch. Registered as the `workspace::SomTabsRestorer` global hook (see
    /// `init` below) since `workspace` can't call into `terminal_view`
    /// directly (dependency points the other way).
    ///
    /// Tabs' terminals are created *concurrently* in Phase 1 (a slow ssh login
    /// doesn't block a fast local shell from appearing), but each tab is
    /// pinned to its `db.json` array index up front via
    /// `add_center_terminal_named_at` — otherwise tabs would land in the main
    /// pane in whatever order their connections happen to finish, not the
    /// order the user left them in. Splits are then created in Phase 2, one
    /// tab at a time and fully sequentially within a tab (level 1 splits
    /// level 0, which must already exist) — `som_split_active_pane_awaited`
    /// works through `Workspace`'s single shared `active_pane`/
    /// `som_split_panes`, so splitting two tabs at once would race on that
    /// shared state and corrupt each other's layout.
    pub fn restore_som_tabs(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<()> {
        let window_handle = window.window_handle();
        cx.spawn(async move |cx| {
            let db_state = workspace::som_db::load_som_db();

            // Phase 1: create every tab's terminal concurrently — each tab is
            // an independent item in the main pane, so there's no shared
            // state to race on here. Where each tab actually lands in the
            // main pane depends on connection speed (fast local shells vs.
            // slow ssh logins), not `db.json`'s order, so `db_index` is kept
            // alongside each creation and used to fix up ordering explicitly
            // once every tab exists (see the reorder step at the start of
            // Phase 2) rather than trying to pin positions during insertion.
            let mut tab_creations = Vec::with_capacity(db_state.tabs.len());
            for (db_index, tab) in db_state.tabs.iter().enumerate() {
                let Some(profile) = window_handle
                    .update(cx, |_, _, cx| {
                        workspace::TabProfiles::profile_at(tab.profile_index, cx)
                    })
                    .ok()
                    .flatten()
                else {
                    // Profile index no longer exists in settings.json — skip
                    // this tab entirely rather than guessing.
                    continue;
                };

                let cwd = profile
                    .working_dir
                    .as_deref()
                    .and_then(|dir| shellexpand::full(dir).ok())
                    .map(|dir| PathBuf::from(dir.to_string()))
                    .filter(|dir| dir.is_dir());

                if profile.tmux {
                    // Splits aren't supported for tmux tabs yet (see
                    // `SomTmuxView::can_split`), so there's only ever the
                    // main session id to restore, never any extras.
                    let existing_session_id =
                        tab.tmux_sessions.as_ref().and_then(|ids| ids.first().copied());
                    let (program, args) =
                        project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
                    let cwd_string = cwd.as_ref().map(|p| p.to_string_lossy().to_string());
                    let created = window_handle
                        .update(cx, |_, window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                let cwd_string = cwd_string.clone().or_else(|| {
                                    default_working_directory(workspace, cx)
                                        .map(|p| p.to_string_lossy().to_string())
                                });
                                Self::add_center_tmux_terminal_named(
                                    workspace,
                                    profile.name.clone(),
                                    profile.icon.clone(),
                                    tab.profile_index,
                                    program,
                                    args,
                                    cwd_string,
                                    existing_session_id,
                                    window,
                                    cx,
                                )
                            })
                        })
                        .ok()
                        .and_then(|r| r.ok());
                    if let Some(created) = created {
                        tab_creations.push((db_index, tab.extra_splits, created));
                    }
                    continue;
                }

                let created = window_handle
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            let cwd = cwd.clone().or_else(|| default_working_directory(workspace, cx));
                            let shell = profile.shell.clone();
                            Self::add_center_terminal_named(
                                workspace,
                                Some(profile.name.clone()),
                                profile.icon.clone(),
                                Some(tab.profile_index),
                                window,
                                cx,
                                move |project, cx| {
                                    if let Some(shell) = shell {
                                        project.create_terminal_with_shell(cwd, shell, cx)
                                    } else {
                                        project.create_terminal_shell(cwd, cx)
                                    }
                                },
                            )
                        })
                    })
                    .ok()
                    .and_then(|r| r.ok());
                if let Some(created) = created {
                    let created = cx.background_spawn(async move {
                        created.await.map(|(item_id, _terminal)| item_id)
                    });
                    tab_creations.push((db_index, tab.extra_splits, created));
                }
            }

            // Await every tab's creation before doing anything else. Tabs
            // were created concurrently in Phase 1, so they can land in the
            // main pane in a different order than `db_state.tabs` (a local
            // shell resolves faster than an ssh connection) — fix that up
            // explicitly now, in one pass, rather than trying to pin
            // positions during insertion (which doesn't work: `add_item`'s
            // destination index gets clamped to however many items exist
            // *at that moment*, so an early-finishing tab meant for index 2
            // would just get inserted at 0).
            let mut tabs_in_db_order = Vec::with_capacity(tab_creations.len());
            for (db_index, extra_splits, created) in tab_creations.into_iter() {
                if let Some(item_id) = created.await.log_err() {
                    tabs_in_db_order.push((db_index, extra_splits, item_id));
                }
            }
            tabs_in_db_order.sort_by_key(|(db_index, _, _)| *db_index);
            window_handle
                .update(cx, |_, _, cx| {
                    workspace.update(cx, |workspace, cx| {
                        if let Some(main_pane) = workspace.panes().first().cloned() {
                            main_pane.update(cx, |pane, cx| {
                                for (position, (_, _, item_id)) in
                                    tabs_in_db_order.iter().enumerate()
                                {
                                    pane.reorder_item_to(*item_id, position);
                                }
                                cx.notify();
                            });
                        }
                        // The reorder above can silently move which array
                        // position the active tab sits at, without emitting
                        // `ActivateItem` — resync so the next real tab switch
                        // parks the correct tab instead of a stale one.
                        workspace.som_resync_active_tab_index(cx);
                    })
                })
                .ok();

            // Phase 2: for each tab, add its split panes. Splits within one
            // tab must stay sequential (level 1 splits level 0, which must
            // already exist), and tabs are handled one at a time here too —
            // `som_split_active_pane_awaited` works through `Workspace`'s
            // single shared `active_pane`/`som_split_panes`, so two tabs
            // creating splits at the same time would race on that shared
            // state and corrupt each other's layout.
            //
            // Each tab must be made the active tab before splitting it —
            // `som_split_active_pane_awaited` always splits whatever tab is
            // currently active. The reorder above already fixed up the main
            // pane's ordering, but locate each tab's index by `EntityId`
            // again anyway rather than assuming `position` still holds,
            // since reordering shifts indices around as each item moves.
            for (_db_index, extra_splits, item_id) in tabs_in_db_order.into_iter() {
                if extra_splits == 0 {
                    continue;
                }

                let real_index = window_handle
                    .update(cx, |_, _, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace
                                .panes()
                                .first()
                                .and_then(|p| p.read(cx).index_for_item_id(item_id))
                        })
                    })
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten();
                let Some(real_index) = real_index else {
                    // Item vanished (closed mid-restore?) — nothing to split.
                    continue;
                };

                window_handle
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            if let Some(main_pane) = workspace.panes().first().cloned() {
                                main_pane.update(cx, |pane, cx| {
                                    pane.activate_item(real_index, true, true, window, cx);
                                });
                            }
                        })
                    })
                    .ok();

                // `activate_item` above only *emits* `pane::Event::ActivateItem`;
                // GPUI dispatches that event (and the park/unpark handling that
                // updates `Workspace::active_pane`) on a later effect flush, not
                // synchronously within this `update` call. Without waiting for
                // that to land, the split below would clone whatever pane was
                // active *before* this activation (e.g. a previous tab), not
                // the tab we just switched to. Poll until the main pane's
                // active item actually reflects the tab we just activated.
                let mut activated = false;
                for _ in 0..50 {
                    let is_active = window_handle
                        .update(cx, |_, _, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace
                                    .panes()
                                    .first()
                                    .map(|p| p.read(cx).active_item_index() == real_index)
                                    .unwrap_or(false)
                            })
                        })
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or(false);
                    if is_active {
                        activated = true;
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(5))
                        .await;
                }
                if !activated {
                    // Something's very wrong (item removed mid-restore?) — skip
                    // this tab's splits rather than risk cloning the wrong pane.
                    continue;
                }

                for _ in 0..extra_splits {
                    let split_task = window_handle
                        .update(cx, |_, window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace.som_split_active_pane_awaited(window, cx)
                            })
                        })
                        .ok()
                        .and_then(|r| r.ok());
                    if let Some(split_task) = split_task {
                        split_task.await;
                    }
                }
            }

            // All tabs (and their splits) exist now — focus the tab db.json
            // marked active. Activating it also unparks its split panes via
            // the existing tab-switch handler if they aren't already live
            // (they are live only for the last tab created above).
            //
            // `som_park_current_split_panes` is called unconditionally first:
            // `pane::Event::ActivateItem` (and the parking it triggers) only
            // fires below if `current != db_state.active_tab`. If the last
            // split-creating tab in the loop above happens to already be the
            // one db.json marked active, `activate_item` would be a no-op and
            // that tab's splits would never be parked — leaving them visible
            // on screen while a *different* tab (per active_item_index) is
            // considered active.
            //
            // Once that tab is confirmed active, focus its `active_pane`
            // (0 = main pane, 1..=3 = a split level) explicitly — there is no
            // "saved active split" restoration elsewhere to fall back on
            // (`som_parked_splits`'s per-tab saved-active slot is always
            // written as `None` and never read back).
            let needs_activation = window_handle
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.som_park_current_split_panes(db_state.active_tab, window, cx);
                        let Some(main_pane) = workspace.panes().first().cloned() else {
                            return false;
                        };
                        let current = main_pane.read(cx).active_item_index();
                        if db_state.active_tab != current {
                            main_pane.update(cx, |pane, cx| {
                                pane.activate_item(db_state.active_tab, true, true, window, cx);
                            });
                            true
                        } else {
                            false
                        }
                    })
                })
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(false);

            // `activate_item` above only *emits* `ActivateItem`; GPUI dispatches
            // it (and the unpark it triggers, which populates `som_split_panes`
            // for this tab) on a later effect flush. Wait for it to land before
            // focusing a specific split pane below, or we'd focus a pane that
            // still belongs to the previous tab.
            if needs_activation {
                for _ in 0..50 {
                    let landed = window_handle
                        .update(cx, |_, _, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace
                                    .panes()
                                    .first()
                                    .map(|p| p.read(cx).active_item_index() == db_state.active_tab)
                                    .unwrap_or(false)
                            })
                        })
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or(false);
                    if landed {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(5))
                        .await;
                }
            }

            window_handle
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.som_focus_pane_by_index(db_state.active_pane, window, cx);
                    })
                })
                .ok();
        })
    }

    fn add_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_with_name(None, cwd, reveal_strategy, window, cx)
    }

    fn add_terminal_shell_named(
        &mut self,
        tab_name: Option<String>,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_internal(false, tab_name, cwd, None, reveal_strategy, window, cx)
    }

    fn add_local_terminal_shell(
        &mut self,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_internal(true, None, None, None, reveal_strategy, window, cx)
    }

    fn add_terminal_shell_with_name(
        &mut self,
        tab_name: Option<String>,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_internal(false, tab_name, cwd, None, reveal_strategy, window, cx)
    }

    fn add_terminal_shell_internal(
        &mut self,
        force_local: bool,
        tab_name: Option<String>,
        cwd: Option<PathBuf>,
        shell_override: Option<String>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for collaborative projects");
            }
            let pane = terminal_panel.update(cx, |terminal_panel, _| {
                terminal_panel.pending_terminals_to_add += 1;
                terminal_panel.active_pane.clone()
            })?;
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = if force_local {
                project
                    .update(cx, |project, cx| project.create_local_terminal(cx))
                    .await
            } else if let Some(cmd) = shell_override {
                project
                    .update(cx, |project, cx| project.create_terminal_with_shell(cwd, cmd, cx))
                    .await
            } else {
                project
                    .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                    .await
            };

            match terminal {
                Ok(terminal) => {
                    let result = workspace.update_in(cx, |workspace, window, cx| {
                        let terminal_view = Box::new(cx.new(|cx| {
                            TerminalView::new_with_title(
                                terminal.clone(),
                                workspace.weak_handle(),
                                workspace.database_id(),
                                workspace.project().downgrade(),
                                tab_name.clone(),
                                window,
                                cx,
                            )
                        }));

                        match reveal_strategy {
                            RevealStrategy::Always => {
                                workspace.focus_panel::<Self>(window, cx);
                            }
                            RevealStrategy::NoFocus => {
                                workspace.open_panel::<Self>(window, cx);
                            }
                            RevealStrategy::Never => {}
                        }

                        pane.update(cx, |pane, cx| {
                            let focus = matches!(reveal_strategy, RevealStrategy::Always);
                            pane.add_item(terminal_view, true, focus, None, window, cx);
                        });

                        Ok(terminal.downgrade())
                    })?;
                    terminal_panel.update(cx, |terminal_panel, cx| {
                        terminal_panel.pending_terminals_to_add =
                            terminal_panel.pending_terminals_to_add.saturating_sub(1);
                        terminal_panel.serialize(cx)
                    })?;
                    result
                }
                Err(error) => {
                    pane.update_in(cx, |pane, window, cx| {
                        let focus = pane.has_focus(window, cx);
                        let failed_to_spawn = cx.new(|cx| FailedToSpawnTerminal {
                            error: error.to_string(),
                            focus_handle: cx.focus_handle(),
                        });
                        pane.add_item(Box::new(failed_to_spawn), true, focus, None, window, cx);
                    })?;
                    Err(error)
                }
            }
        })
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                TerminalPanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.spawn(async move |terminal_panel, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let terminal_panel = terminal_panel.upgrade()?;
            let items = terminal_panel.update(cx, |terminal_panel, cx| {
                SerializedItems::WithSplits(serialize_pane_group(
                    &terminal_panel.center,
                    &terminal_panel.active_pane,
                    cx,
                ))
            });
            cx.background_spawn(
                async move {
                    kvp.write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedTerminalPanel {
                            items,
                            active_item_id: None,
                        })?,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    fn has_no_terminals(&self, cx: &App) -> bool {
        self.active_pane.read(cx).items_len() == 0 && self.pending_terminals_to_add == 0
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    /// Returns all panes in the terminal panel.
    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        self.center.panes()
    }

    /// Returns all non-empty terminal selections from all terminal views in all panes.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.center
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx).items().filter_map(|item| {
                    let terminal_view = item.downcast::<crate::TerminalView>()?;
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .selection_text
                        .clone()
                        .filter(|text| !text.is_empty())
                })
            })
            .collect()
    }

    fn is_enabled(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| is_enabled_in_workspace(workspace.read(cx), cx))
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
        {
            window.focus(&pane.focus_handle(cx), cx);
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_pane_in_direction(direction, window, cx)
                })
                .ok();
        }
    }

    fn swap_pane_in_direction(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if let Some(to) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
            .cloned()
        {
            self.center.swap(&self.active_pane, &to, cx);
            cx.notify();
        }
    }

    fn move_pane_to_border(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if self
            .center
            .move_to_border(&self.active_pane, direction, cx)
            .unwrap()
        {
            cx.notify();
        }
    }
}

/// Prepares a `SpawnInTerminal` by computing the command, args, and command_label
/// based on the shell configuration. This is a pure function that can be tested
/// without spawning actual terminals.
pub fn prepare_task_for_spawn(
    task: &SpawnInTerminal,
    shell: &Shell,
    is_windows: bool,
) -> SpawnInTerminal {
    let builder = ShellBuilder::new(shell, is_windows);
    let command_label = builder.command_label(task.command.as_deref().unwrap_or(""));
    let (command, args) = builder.build_no_quote(task.command.clone(), &task.args);

    SpawnInTerminal {
        command_label,
        command: Some(command),
        args,
        ..task.clone()
    }
}

fn is_enabled_in_workspace(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).supports_terminal(cx)
}

pub fn new_terminal_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    zoomed: bool,
    window: &mut Window,
    cx: &mut Context<TerminalPanel>,
) -> Entity<Pane> {
    let terminal_panel = cx.entity();
    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            None,
            false,
            window,
            cx,
        );
        pane.set_zoomed(zoomed, cx);
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_, _| false);
        pane.set_zoom_out_on_close(false);

        let split_closure_terminal_panel = terminal_panel.downgrade();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, _window, cx| {
            if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                let is_current_pane = tab.pane == cx.entity();
                let Some(can_drag_away) = split_closure_terminal_panel
                    .read_with(cx, |terminal_panel, _| {
                        let current_panes = terminal_panel.center.panes();
                        !current_panes.contains(&&tab.pane)
                            || current_panes.len() > 1
                            || (!is_current_pane || pane.items_len() > 1)
                    })
                    .ok()
                else {
                    return false;
                };
                if can_drag_away {
                    let item = if is_current_pane {
                        pane.item_for_index(tab.ix)
                    } else {
                        tab.pane.read(cx).item_for_index(tab.ix)
                    };
                    if let Some(item) = item {
                        return item.downcast::<TerminalView>().is_some();
                    }
                }
            }
            false
        })));

        let toolbar = pane.toolbar().clone();
        if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
            (callbacks.setup_search_bar)(&toolbar, window, cx);
        }

        pane
    });

    cx.subscribe_in(&pane, window, TerminalPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

struct FailedToSpawnTerminal {
    error: String,
    focus_handle: FocusHandle,
}

impl Focusable for FailedToSpawnTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FailedToSpawnTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover_menu = PopoverMenu::new("settings-popover")
            .trigger(
                IconButton::new("icon-button-popover", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |context_menu, _, _| {
                    context_menu
                        .action("Open Settings", zed_actions::OpenSettings.boxed_clone())
                        .action(
                            "Edit settings.json",
                            zed_actions::OpenSettingsFile.boxed_clone(),
                        )
                }))
            })
            .anchor(Anchor::TopRight)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(2.0),
            });

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w_112()
                    .items_center()
                    .justify_center()
                    .text_center()
                    .child(Label::new("Failed to spawn terminal"))
                    .child(
                        Label::new(self.error.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .mb_4(),
                    )
                    .child(SplitButton::new(
                        ButtonLike::new("open-settings-ui")
                            .child(Label::new("Edit Settings").size(LabelSize::Small))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(zed_actions::OpenSettings.boxed_clone(), cx);
                            }),
                        popover_menu.into_any_element(),
                    )),
            )
    }
}

impl EventEmitter<()> for FailedToSpawnTerminal {}

impl workspace::Item for FailedToSpawnTerminal {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::new_static("Failed to spawn terminal")
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let registrar = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane.clone())
            })
            .unwrap_or_else(div);
        self.workspace
            .update(cx, |workspace, cx| {
                registrar.size_full().child(self.center.render(
                    workspace.zoomed_item(),
                    &workspace::PaneRenderContext {
                        active_pane: &self.active_pane,
                        app_state: workspace.app_state(),
                        project: workspace.project(),
                        workspace: &workspace.weak_handle(),
                    },
                    window,
                    cx,
                ))
            })
            .ok()
            .map(|div| {
                div.on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneLeft, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Left, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneRight, window, cx| {
                        terminal_panel.activate_pane_in_direction(
                            SplitDirection::Right,
                            window,
                            cx,
                        );
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneUp, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Up, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneDown, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Down, window, cx);
                    })
                })
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivateNextPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let next_ix = (ix + 1) % panes.len();
                            window.focus(&panes[next_ix].focus_handle(cx), cx);
                        }
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, _action: &ActivatePreviousPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                            window.focus(&panes[prev_ix].focus_handle(cx), cx);
                        }
                    },
                ))
                .on_action(
                    cx.listener(|terminal_panel, action: &ActivatePane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(&pane) = panes.get(action.0) {
                            window.focus(&pane.read(cx).focus_handle(cx), cx);
                        } else {
                            let future =
                                terminal_panel.new_pane_with_active_terminal(true, window, cx);
                            cx.spawn_in(window, async move |terminal_panel, cx| {
                                if let Some(new_pane) = future.await {
                                    _ = terminal_panel.update_in(
                                        cx,
                                        |terminal_panel, window, cx| {
                                            terminal_panel.center.split(
                                                &terminal_panel.active_pane,
                                                &new_pane,
                                                SplitDirection::Right,
                                                cx,
                                            );
                                            let new_pane = new_pane.read(cx);
                                            window.focus(&new_pane.focus_handle(cx), cx);
                                        },
                                    );
                                }
                            })
                            .detach();
                        }
                    }),
                )
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneLeft, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneRight, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneUp, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneDown, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Down, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneLeft, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneRight, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneUp, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneDown, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Down, cx);
                }))
                .on_action(
                    cx.listener(|terminal_panel, action: &MoveItemToPane, window, cx| {
                        let Some(&target_pane) =
                            terminal_panel.center.panes().get(action.destination)
                        else {
                            return;
                        };
                        move_active_item(
                            &terminal_panel.active_pane,
                            target_pane,
                            action.focus,
                            true,
                            window,
                            cx,
                        );
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, action: &MoveItemToPaneInDirection, window, cx| {
                        let source_pane = &terminal_panel.active_pane;
                        if let Some(destination_pane) = terminal_panel
                            .center
                            .find_pane_in_direction(source_pane, action.direction, cx)
                        {
                            move_active_item(
                                source_pane,
                                destination_pane,
                                action.focus,
                                true,
                                window,
                                cx,
                            );
                        };
                    },
                ))
            })
            .unwrap_or_else(|| div())
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }
}

impl Panel for TerminalPanel {
    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        TerminalSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => TerminalDockPosition::Left,
                DockPosition::Bottom => TerminalDockPosition::Bottom,
                DockPosition::Right => TerminalDockPosition::Right,
            };
            settings.terminal.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = TerminalSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, _window: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).flexible
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.terminal.get_or_insert_default().flexible = Some(flexible);
        });
    }

    fn is_zoomed(&self, _window: &Window, cx: &App) -> bool {
        self.active_pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| {
                pane.set_zoomed(zoomed, cx);
            })
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let old_active = self.active;
        self.active = active;
        if !active || old_active == active || !self.has_no_terminals(cx) {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            let Ok(kind) = this
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
            else {
                return;
            };
            let first_profile_name = cx
                .try_global::<TabProfiles>()
                .and_then(|p| p.0.first().map(|profile| profile.name.clone()));

            this.add_terminal_shell_named(first_profile_name, kind, RevealStrategy::Always, window, cx)
                .detach_and_log_err(cx)
        })
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        if !TerminalSettings::get_global(cx).show_count_badge {
            return None;
        }
        let count = self
            .center
            .panes()
            .into_iter()
            .map(|pane| pane.read(cx).items_len())
            .sum::<usize>();
        if count == 0 {
            None
        } else {
            Some(count.to_string())
        }
    }

    fn persistent_name() -> &'static str {
        "TerminalPanel"
    }

    fn panel_key() -> &'static str {
        TERMINAL_PANEL_KEY
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        if (self.is_enabled(cx) || !self.has_no_terminals(cx))
            && TerminalSettings::get_global(cx).button
        {
            Some(IconName::TerminalAlt)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Terminal Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(Toggle)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn activation_priority(&self) -> u32 {
        2
    }

}

struct InlineAssistTabBarButton {
    focus_handle: FocusHandle,
}

impl Render for InlineAssistTabBarButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle.clone();
        IconButton::new("terminal_inline_assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(InlineAssist::default().boxed_clone(), cx);
            }))
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in("Inline Assist", &InlineAssist::default(), &focus_handle, cx)
            })
    }
}