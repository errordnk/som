use std::{cmp, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    TerminalView, default_working_directory,
    pending_terminal_tab::PendingTerminalTab,
    persistence::{
        SerializedItems, SerializedTerminalPanel, deserialize_terminal_panel, serialize_pane_group,
    },
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
use terminal::{Terminal, terminal_settings::{CursorShape, TerminalSettings}};
use ui::{
    ButtonLike, Clickable, ContextMenu, FluentBuilder, PopoverMenu, SplitButton, Toggleable,
    Tooltip, prelude::*,
};
use util::{ResultExt, TryFutureExt};
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

use anyhow::{Context as _, Result, anyhow};
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
        let (profile_shell, tab_icon) = tab_name.as_deref()
            .map(|name| TabProfiles::profile_by_name(name, cx))
            .unwrap_or((None, None));
        let profile_index = tab_name.as_deref().and_then(|name| TabProfiles::index_by_name(name, cx));
        // `action.shell` is populated even for the profile's OWN keybinding
        // (`Ctrl+Shift+N` — see `som_config.rs`'s keymap generation, which
        // bakes the profile's shell into the binding so it round-trips
        // through gpui's action-persistence), not just for a genuine
        // user override — so only treat this as an override (and skip the
        // tmux substitution) if it actually differs from the profile's own
        // shell.
        let is_shell_override = action
            .shell
            .as_deref()
            .is_some_and(|shell| profile.as_ref().and_then(|p| p.shell.as_deref()) != Some(shell));
        let shell_override = action.shell.clone().or(profile_shell);
        let working_directory = default_working_directory(workspace, cx);
        let local = action.local;

        if !is_shell_override && profile.as_ref().is_some_and(|p| p.tmux) {
            let profile = profile.unwrap();
            let cwd = working_directory.clone().map(|p| p.to_string_lossy().to_string());
            let pane_id = uuid::Uuid::new_v4().to_string();
            // Same placeholder-tab approach as `restore_som_tabs`'s Phase
            // 0 (see that function's doc comment) — a new SSH `tmux: true`
            // tab used to sit on nothing at all (not even a tab in the tab
            // bar) for however long its deploy check + terminal creation
            // took, a real, reported pause on plain "new tab" clicks/
            // shortcuts, not just restore-on-launch. Inserting a
            // `PendingTerminalTab` synchronously, right now, means the tab
            // (name/icon already known) appears immediately, is activated
            // immediately (`add_item_to_main_pane` always focuses/
            // activates its new item), and the real terminal replaces it
            // in place once ready via `replace_center_terminal_named`.
            let placeholder = cx.new(|cx| PendingTerminalTab::new(tab_name.clone(), tab_icon.clone(), cx));
            let placeholder_item_id = placeholder.entity_id();
            workspace.add_item_to_main_pane(Box::new(placeholder), profile_index, window, cx);
            // Remote (ssh/wsl) profiles need a blocking deploy check (git
            // pull + rebuild on the far side if the version there doesn't
            // match this Som build — see `ensure_remote_binary_deployed`'s
            // doc comment) BEFORE the terminal is created at all, since
            // `tmux_wrapped_shell`'s remote path assumes the binary at
            // `~/.local/bin/som-tmux` is already current. That check
            // runs real blocking child processes, so it can't happen inline
            // here on GPUI's main thread — spawn it, then create the
            // terminal (same as the local path) once it's done. A local
            // profile has no remote binary to check at all, so `deploy_check`
            // below is just `None`, skipping straight to terminal creation.
            let (remote_program, host_args) = project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
            let remote_kind = classify_remote(&remote_program);
            let terminal_settings = TerminalSettings::get_global(cx);
            let cursor_shape = terminal_settings.cursor_shape;
            let scrollback = terminal_settings.max_scroll_history_lines;
            // Drives the titlebar's drag-zone spinner (see `workspace::
            // SomRestoreActivity`'s doc comment) for a new tab's deploy
            // check + terminal creation, same as `restore_som_tabs` already
            // does for restore-on-launch — `begin` here, `end` once the
            // whole inner future below resolves (success, deploy failure,
            // or tmux_wrapped_shell failure alike), via the wrapping
            // `async move` rather than duplicating `end()` at every one of
            // that future's several early-return points.
            workspace::SomRestoreActivity::begin(cx);
            cx.spawn_in(window, async move |workspace, cx| {
                let result: anyhow::Result<()> = async {
                    if let RemoteKind::Ssh | RemoteKind::Wsl = remote_kind {
                        let host_args = host_args.clone();
                        let deploy_result = cx
                            .background_spawn(async move { ensure_remote_binary_deployed(&host_args, remote_kind) })
                            .await;
                        if let Err(err) = deploy_result {
                            log::warn!("remote som-tmux deploy check failed, proceeding with whatever is already there: {err:#}");
                            // Best-effort: this profile's tab still gets
                            // created below with whatever binary is already on
                            // the remote host (which might work fine, or might
                            // not — but silently trying anyway beats refusing
                            // to open the tab at all). The banner is purely
                            // informational — a real deploy failure (e.g. the
                            // scp/kill retries in `ensure_remote_binary_
                            // deployed` both failing) is exactly the kind of
                            // "housekeeping went wrong, but you should know"
                            // case a banner exists for, rather than a silent
                            // log line the user has no way to notice.
                            workspace
                                .update(cx, |workspace, cx| {
                                    show_som_tmux_error(
                                        workspace,
                                        format!("som-tmux deploy check failed for this profile\n{err:#}"),
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    }

                    let (program, args) = match tmux_wrapped_shell(&profile, &pane_id, cursor_shape, scrollback) {
                        Ok(pair) => pair,
                        Err(err) => {
                            log::error!("failed to set up tmux profile {:?}: {err:#}", profile.name);
                            workspace
                                .update_in(cx, |workspace, window, cx| {
                                    show_som_tmux_error(
                                        workspace,
                                        format!("Failed to set up tmux profile {:?}\n{err:#}", profile.name),
                                        cx,
                                    );
                                    // The placeholder tab inserted synchronously
                                    // above never gets a real terminal now — close
                                    // it rather than leaving an empty, permanently
                                    // stuck tab behind.
                                    if let Some(main_pane) = workspace.panes().first().cloned() {
                                        main_pane.update(cx, |pane, cx| {
                                            pane.remove_item(placeholder_item_id, true, true, window, cx);
                                        });
                                    }
                                })
                                .ok();
                            return anyhow::Ok(());
                        }
                    };

                    let task = workspace.update_in(cx, |workspace, window, cx| {
                        Self::replace_center_terminal_named(
                            workspace,
                            placeholder_item_id,
                            tab_name,
                            tab_icon,
                            profile_index,
                            window,
                            cx,
                            move |project, cx| project.create_terminal_with_program_and_args(cwd.map(PathBuf::from), program, args, cx),
                        )
                    })?;
                    let (item_id, _terminal) = task.await?;
                    workspace.update(cx, |workspace, _cx| {
                        workspace.set_tmux_sessions_for_item(item_id, vec![pane_id]);
                    })?;
                    anyhow::Ok(())
                }
                .await;
                cx.update(|_, cx| workspace::SomRestoreActivity::end(cx)).ok();
                result
            })
            .detach_and_log_err(cx);
            return;
        }

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

    /// Like `add_center_terminal_named_at`, but SWAPS the finished
    /// `TerminalView` into `placeholder_item_id`'s existing position
    /// (`Pane::replace_item_at`) instead of inserting a new item — used by
    /// `restore_som_tabs`'s placeholder-tab flow, where a `PendingTerminalTab`
    /// already occupies this tab's final position in the tab bar (see that
    /// function's own doc comment for why) and just needs its content
    /// filled in once the real terminal is ready, not a brand new tab
    /// inserted alongside it.
    pub fn replace_center_terminal_named(
        workspace: &mut Workspace,
        placeholder_item_id: gpui::EntityId,
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
                if let Some(profile_index) = profile_index {
                    workspace.set_profile_index_for_item(item_id, profile_index);
                }
                if let Some(main_pane) = workspace.panes().first().cloned() {
                    main_pane.update(cx, |pane, cx| {
                        pane.replace_item_at(placeholder_item_id, Box::new(terminal_view), window, cx);
                    });
                }
                item_id
            })?;
            Ok((item_id, terminal.downgrade()))
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
        // Drives the titlebar's drag-zone spinner (see `workspace::
        // SomRestoreActivity`'s doc comment) — `begin` here, `end` right
        // before this whole task resolves, so the spinner covers this
        // ENTIRE restore (tab creation, splits, deploy checks, orphan
        // cleanup), not just some inner slice of it.
        workspace::SomRestoreActivity::begin(cx);
        cx.spawn(async move |cx| {
            let db_state = workspace::som_db::load_som_db();

            // Every tmux pane_id `db.json` currently knows about, across
            // every tab (including splits) — the live set `kill_orphaned_
            // holders` below cleans remote HOLDER processes against. Built
            // once, up front, from the SAME `db_state` the restore loop
            // below reads, before that loop can generate any FRESH pane_ids
            // of its own (a tab whose saved `tmux_sessions` is missing/
            // corrupt gets a brand new one) — an orphan-cleanup pass must
            // never race against pane_ids Som itself is about to start
            // relying on this run.
            let live_pane_ids: Vec<String> = db_state
                .tabs
                .iter()
                .flat_map(|tab| tab.tmux_sessions.iter().flatten())
                .cloned()
                .collect();

            // Phase 0: insert a `PendingTerminalTab` placeholder for EVERY
            // tab up front, synchronously, all in one `window_handle.
            // update` call before any `.await` gets in the way — this is
            // what makes the whole tab bar draw immediately, in db.json's
            // exact order, instead of tabs popping in one at a time as
            // each one's (possibly slow, real SSH) terminal connection
            // finishes. Only name/icon (from settings.json, via
            // `TabProfiles::profile_at`) are needed for this — no need to
            // wait for anything remote. A tab whose `profile_index` no
            // longer exists in settings.json is skipped here exactly like
            // Phase 1 already skipped it (same fallible lookup, same
            // "don't guess" reasoning) — no placeholder, no later fill-in.
            let placeholders: Vec<(usize, workspace::som_db::SomDbTab, gpui::EntityId)> = window_handle
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let placeholders: Vec<_> = db_state
                            .tabs
                            .iter()
                            .enumerate()
                            .filter_map(|(db_index, tab)| {
                                let profile = workspace::TabProfiles::profile_at(tab.profile_index, cx)?;
                                let placeholder = cx.new(|cx| {
                                    PendingTerminalTab::new(Some(profile.name.clone()), profile.icon.clone(), cx)
                                });
                                let item_id = placeholder.entity_id();
                                workspace.add_item_to_main_pane_at(
                                    Box::new(placeholder),
                                    Some(tab.profile_index),
                                    Some(db_index),
                                    window,
                                    cx,
                                );
                                Some((db_index, tab.clone(), item_id))
                            })
                            .collect();

                        // Switch to the tab db.json marked active RIGHT
                        // NOW, before any real terminal content exists at
                        // all — otherwise the user would see whatever tab
                        // happened to be at index 0 (or wherever the main
                        // pane's default active index lands) for as long as
                        // Phase 1/2 below take to finish, even though every
                        // tab's PLACEHOLDER already exists and could be
                        // switched to immediately. The real activation this
                        // shadows (`db_state.active_tab`, near the end of
                        // this function, after splits are restored) still
                        // needs to run too — it's what actually unparks
                        // this tab's split panes, which don't exist yet at
                        // this point in Phase 0.
                        if let Some(main_pane) = workspace.panes().first().cloned() {
                            main_pane.update(cx, |pane, cx| {
                                if db_state.active_tab < pane.items_len() {
                                    pane.activate_item(db_state.active_tab, true, true, window, cx);
                                }
                            });
                        }

                        placeholders
                    })
                })
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();

            // Give the tab db.json marked active a head start over the
            // rest — every tab's REAL content still gets created
            // concurrently below regardless of order, but `cx.spawn`/
            // `cx.background_spawn` ultimately schedule onto a shared
            // executor/thread pool, so which one gets dispatched FIRST can
            // still matter for which one visibly fills in first. `db_index
            // == db_state.active_tab` finds it directly (Phase 0 built
            // `placeholders` in that same db.json order) rather than
            // needing a separate lookup.
            let mut placeholders = placeholders;
            if let Some(active_position) = placeholders
                .iter()
                .position(|(db_index, ..)| *db_index == db_state.active_tab)
            {
                let active = placeholders.remove(active_position);
                placeholders.insert(0, active);
            }

            // Phase 1: create every tab's REAL terminal concurrently — each
            // tab already has a stable position and `EntityId` from Phase 0
            // above, so unlike before, there's no reordering needed once
            // these finish: each one just replaces its own placeholder in
            // place (`Pane::replace_item_at`, via `replace_center_terminal_
            // named`) whenever it happens to be ready, regardless of
            // whether a slower tab elsewhere is still connecting.
            let mut tab_creations = Vec::with_capacity(placeholders.len());
            for (db_index, tab, placeholder_item_id) in placeholders {
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
                    .home
                    .as_deref()
                    .and_then(|dir| shellexpand::full(dir).ok())
                    .map(|dir| PathBuf::from(dir.to_string()))
                    .filter(|dir| dir.is_dir());

                if profile.tmux {
                    // Splits aren't supported for tmux tabs yet, so there's
                    // only ever the one pane_id to restore, never extras —
                    // see `set_tmux_sessions_for_item`'s doc comment (still
                    // named for the OLD session_id-based design, now
                    // repurposed to store the pane_id used as this pane's
                    // `som-tmux` pipe name — see `project_som_tmux`
                    // memory, "Обновление 17"/19).
                    let pane_id = tab
                        .tmux_sessions
                        .as_ref()
                        .and_then(|ids| ids.first())
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                    // The ENTIRE rest of this tab's setup — deploy check,
                    // orphan cleanup, terminal creation, tmux_sessions
                    // bookkeeping — runs inside ONE `cx.spawn`, pushed into
                    // `tab_creations` immediately, so THIS tab's (possibly
                    // slow, real SSH round-trip) deploy check can never
                    // block the `for` loop from moving on to the NEXT
                    // tab's restore. Before this, the deploy check/orphan
                    // cleanup below were awaited directly in the loop body
                    // — with N SSH `tmux: true` tabs, total restore time
                    // was the SUM of every host's round-trip, not the
                    // slowest one, exactly the concurrency Phase 1's own
                    // doc comment already promised but this one branch
                    // didn't actually deliver.
                    let (remote_program, host_args) =
                        project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
                    let remote_kind = classify_remote(&remote_program);
                    let live_pane_ids = live_pane_ids.clone();
                    let workspace = workspace.clone();
                    let profile = profile.clone();
                    let cwd = cwd.clone();
                    let tab_profile_index = tab.profile_index;
                    let created = cx.spawn(async move |cx| {
                        if let RemoteKind::Ssh | RemoteKind::Wsl = remote_kind {
                            // `ensure_remote_binary_deployed`/`kill_orphaned_
                            // holders` are SYNCHRONOUS, blocking functions
                            // (real `ssh`/`scp` child processes via
                            // `std::process::Command::output()`) — running
                            // them directly on THIS `cx.spawn`'s foreground
                            // executor would block that executor's thread
                            // for the whole SSH round-trip, starving every
                            // OTHER tab's `cx.spawn` task of a chance to
                            // progress too (confirmed: without `background_
                            // spawn` here, two tabs' deploy-checks ran
                            // fully back-to-back, not concurrently, even
                            // though each already had its own `cx.spawn`).
                            // `cx.background_spawn` moves the blocking work
                            // onto a real background thread pool instead.
                            let deploy_result = cx
                                .background_spawn({
                                    let host_args = host_args.clone();
                                    let live_pane_ids = live_pane_ids.clone();
                                    async move {
                                        let deploy_result = ensure_remote_binary_deployed(&host_args, remote_kind);
                                        // Piggybacks on the SSH round-trip
                                        // the deploy check above already
                                        // pays for — see `kill_orphaned_
                                        // holders`'s doc comment.
                                        kill_orphaned_holders(&host_args, remote_kind, &live_pane_ids);
                                        deploy_result
                                    }
                                })
                                .await;
                            if let Err(err) = deploy_result {
                                log::warn!(
                                    "remote som-tmux deploy check failed on restore, proceeding with whatever is already there: {err:#}"
                                );
                                // See the identical banner in `new_terminal`
                                // for why this is worth surfacing, not just
                                // logging — a restore-time deploy failure
                                // is just as invisible to the user
                                // otherwise.
                                window_handle
                                    .update(cx, |_, _, cx| {
                                        workspace.update(cx, |workspace, cx| {
                                            show_som_tmux_error(
                                                workspace,
                                                format!(
                                                    "som-tmux deploy check failed for profile {:?}\n{err:#}",
                                                    profile.name
                                                ),
                                                cx,
                                            );
                                        })
                                    })
                                    .ok();
                            }
                        }

                        let (cursor_shape, scrollback) = window_handle
                            .update(cx, |_, _, cx| {
                                let settings = TerminalSettings::get_global(cx);
                                (settings.cursor_shape, settings.max_scroll_history_lines)
                            })
                            .unwrap_or((CursorShape::default(), None));
                        let (program, args) = tmux_wrapped_shell(&profile, &pane_id, cursor_shape, scrollback)?;
                        let created = window_handle.update(cx, |_, window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                let cwd = cwd.clone().or_else(|| default_working_directory(workspace, cx));
                                Self::replace_center_terminal_named(
                                    workspace,
                                    placeholder_item_id,
                                    Some(profile.name.clone()),
                                    profile.icon.clone(),
                                    Some(tab_profile_index),
                                    window,
                                    cx,
                                    move |project, cx| {
                                        project.create_terminal_with_program_and_args(cwd, program, args, cx)
                                    },
                                )
                            })
                        })??;
                        let (item_id, _terminal) = created.await?;
                        // Restored tabs never otherwise call
                        // `set_tmux_sessions_for_item` — without this,
                        // `pane_id` above (correctly reused from
                        // `db.json`, or freshly generated when this tab
                        // never had one) never makes it back into
                        // `Workspace::som_tab_tmux_sessions`, so the
                        // NEXT `som_persist_db_json` call writes `None`
                        // for this tab's `tmux_sessions` regardless of
                        // what pane it's actually backed by — silently
                        // forgetting the association on every restore.
                        // Confirmed root cause of a reported bug: a
                        // `htop`/`micro` process running in a
                        // `som-tmux` HOLDER survives Som closing
                        // (the whole point of the HOLDER/RELAY design —
                        // see `project_som_tmux` memory), but the tab
                        // that opens on the NEXT launch gets a
                        // brand-new `pane_id` instead of reattaching to
                        // that same still-alive HOLDER, since db.json
                        // never remembered which pane_id this tab was
                        // actually using. Uses `window_handle.update`
                        // (not `workspace.update` directly) since
                        // `Workspace::set_tmux_sessions_for_item` needs
                        // a `Context<Workspace>`, only obtainable
                        // through the window.
                        window_handle.update(cx, |_, _, cx| {
                            workspace.update(cx, |workspace, _cx| {
                                workspace.set_tmux_sessions_for_item(item_id, vec![pane_id]);
                            })
                        })??;
                        anyhow::Ok(item_id)
                    });
                    tab_creations.push((db_index, tab.extra_splits, created));
                    continue;
                }

                let created = window_handle
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            let cwd = cwd.clone().or_else(|| default_working_directory(workspace, cx));
                            let shell = profile.shell.clone();
                            Self::replace_center_terminal_named(
                                workspace,
                                placeholder_item_id,
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

            // Await every tab's REAL content to land. Unlike before this
            // whole placeholder-tab flow existed, no reordering is needed
            // here at all: Phase 0 already put every tab at its final
            // `db.json` position UP FRONT, and `replace_center_terminal_
            // named`'s `Pane::replace_item_at` swaps each placeholder for
            // its real `TerminalView` IN PLACE — the tab bar's order was
            // already correct the whole time, regardless of which tab's
            // (possibly slow, real SSH) connection happens to finish last.
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
                        // Nothing moved position, but `active_item_index`
                        // is still worth resyncing defensively here (e.g. a
                        // placeholder whose tab the user closed mid-restore
                        // before its real content ever arrived) — same
                        // safety net the old reorder-based flow already had.
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
            // currently active. Every tab's position has been stable since
            // Phase 0, but this still looks each one up by `EntityId`
            // rather than assuming `position` holds, since splitting an
            // EARLIER tab in this same loop shifts later ones' indices.
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

            window_handle.update(cx, |_, _, cx| workspace::SomRestoreActivity::end(cx)).ok();
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

/// Where a `tmux: true` profile's shell actually runs — determines which of
/// the two wrapping strategies in `tmux_wrapped_shell` applies. See
/// `project_som_tmux` memory ("Обновление 18"): a local shell gets WRAPPED
/// (Som's own PTY child becomes `som-tmux.exe` acting as the RELAY),
/// but `ssh`/`wsl` profiles get their REMOTE-SIDE command appended instead —
/// the `ssh`/`wsl` process itself, unmodified, IS the transport (its own
/// stdin/stdout tunnel bytes to/from wherever the shell actually runs), so
/// wrapping the local `ssh`/`wsl` invocation would be wrapping the wrong
/// end entirely.
#[derive(Clone, Copy)]
enum RemoteKind {
    Local,
    Ssh,
    Wsl,
}

fn classify_remote(program: &str) -> RemoteKind {
    match program {
        "ssh" => RemoteKind::Ssh,
        "wsl" | "wsl.exe" => RemoteKind::Wsl,
        _ => RemoteKind::Local,
    }
}

/// Serializes Som's own `CursorShape` setting into the plain string
/// `som-tmux` parses back out (`crate::session::parse_cursor_shape`
/// in that crate, deliberately NOT sharing this enum type — see that
/// function's doc comment for why) — passed via `--cursor-shape` so the
/// HOLDER's own `alacritty_terminal::Term` (which now OWNS the cursor shape
/// that gets baked into the ANSI this crate reads back, see
/// `project_som_tmux` memory's cursor-shape bug writeup) matches whatever
/// the user actually configured, instead of silently defaulting to Block.
fn cursor_shape_arg(shape: CursorShape) -> &'static str {
    match shape {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Bar => "bar",
        CursorShape::Hollow => "hollow",
    }
}

/// Substitutes a `som-tmux`-wrapped command in for a `tmux: true`
/// profile's own shell — see `project_som_tmux` memory ("Обновление 16"-19)
/// for the full design. Som's own terminal creation path never learns
/// anything happened; it just gets a different program/args than the
/// profile's `shell` setting says, exactly the way `action.shell`/a
/// user-typed shell override already works for any other profile.
///
/// `pane_id` is generated by the CALLER (fresh `Uuid::new_v4()` for a new
/// tab, or a saved one from db.json for restore) — the server never
/// invents one; see `som_tmux::protocol::pipe_name`'s doc comment
/// for why this changed from the old per-profile-multiplexed design.
///
/// `cursor_shape`/`scrollback` mirror Som's own `TerminalSettings` — passed
/// through explicitly to the server (which owns the actual `Term` these
/// configure now) rather than left for it to guess/default; see
/// `cursor_shape_arg`'s doc comment.
fn tmux_wrapped_shell(
    profile: &workspace::TabProfile,
    pane_id: &str,
    cursor_shape: CursorShape,
    scrollback: Option<usize>,
) -> anyhow::Result<(String, Vec<String>)> {
    let (program, args) = project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
    match classify_remote(&program) {
        RemoteKind::Local => {
            let server_path = som_tmux_binary_path()?;
            let wrapped_args = wrap_command_args(&profile.name, pane_id, program, args, cursor_shape, scrollback);
            Ok((server_path.to_string_lossy().to_string(), wrapped_args))
        }
        kind @ (RemoteKind::Ssh | RemoteKind::Wsl) => {
            let wrapped_args = wrap_remote_command_args(&profile.name, pane_id, args, cursor_shape, scrollback, kind);
            Ok((program, wrapped_args))
        }
    }
}

/// The actual argv construction, split out from `tmux_wrapped_shell` so it
/// can be unit-tested without touching the filesystem (`som_tmux_
/// binary_path` looks up a real file next to `current_exe()`, which under
/// `cargo test` resolves to `target/debug/deps/`, not `target/debug/` —
/// same constraint other tests in this codebase have hit).
///
/// `--cursor-shape`/`--scrollback` are appended AFTER the program's own args
/// rather than before the positional `profile`/`pane-id`/`program` — order
/// doesn't matter to `som-tmux`'s own arg parser (each flag consumes
/// its value via `iter.next()` regardless of what's already been seen), so
/// putting them last avoids having to touch the positional-fill logic at all.
fn wrap_command_args(
    profile_name: &str,
    pane_id: &str,
    program: String,
    args: Vec<String>,
    cursor_shape: CursorShape,
    scrollback: Option<usize>,
) -> Vec<String> {
    let mut wrapped_args = vec![profile_name.to_string(), pane_id.to_string(), program];
    wrapped_args.extend(args);
    wrapped_args.push("--cursor-shape".to_string());
    wrapped_args.push(cursor_shape_arg(cursor_shape).to_string());
    if let Some(scrollback) = scrollback {
        wrapped_args.push("--scrollback".to_string());
        wrapped_args.push(scrollback.to_string());
    }
    wrapped_args
}

/// Appends the REMOTE-side `som-tmux` invocation after an `ssh`/`wsl`
/// profile's own args (host, flags, `--cd ~`, etc.) — both `ssh host <cmd>`
/// and `wsl [flags] -- <cmd>` hand everything after their own arguments to
/// a shell on the far side, so this is what that far shell actually runs.
///
/// Deployment (copying/rebuilding the binary at `~/.local/bin`, checking its
/// version) is NOT this function's job — see `ensure_remote_binary_deployed`,
/// which callers run first, before ever building this command line.
///
/// `$SHELL` (expanded by the remote login shell that `ssh`/`wsl` hands this
/// command line to, NOT by Som itself) stands in for "the user's own
/// default shell" — the remote HOLDER spawns whatever that resolves to,
/// same zero-conf assumption a bare `ssh host` (no explicit command) already
/// makes today.
///
/// No client identity is threaded through here — a freshly-spawned
/// HOLDER's `--client-id` comes from the REMOTE RELAY's own `$SSH_CLIENT`
/// (see `som_tmux::protocol::ssh_client_ip`'s doc comment), read entirely
/// on the far side once `ssh` has already connected; there's nothing this
/// Windows-side command-line builder could usefully pass down instead (it
/// has no reliable view of what source IP sshd will see this connection
/// arrive from).
fn wrap_remote_command_args(
    profile_name: &str,
    pane_id: &str,
    args: Vec<String>,
    cursor_shape: CursorShape,
    scrollback: Option<usize>,
    remote_kind: RemoteKind,
) -> Vec<String> {
    let mut wrapped_args = args;
    // `-tt` (force pseudo-terminal allocation, doubled so it applies even
    // though this process's own stdin isn't a tty from ssh's point of
    // view) for SSH profiles specifically. Without a remote pty, `ssh host
    // <explicit command>` gives the remote `som-tmux` RELAY plain pipes
    // for stdin/stdout — no `TIOCGWINSZ` to read the real size from and no
    // SSH window-change channel — so the remote shell was permanently
    // stuck at the RELAY's 80x24 fallback regardless of the real pane
    // size, which mismatched what Som's own `TerminalElement` renders at
    // and showed up as systematic blank lines between prompts (the remote
    // shell wrapping/scrolling against a 24-row terminal while Som draws
    // ~40). Forcing a remote pty makes sshd allocate one, so the size
    // negotiated at connect AND every later window-change both propagate
    // natively over the SSH protocol — no custom in-band size channel
    // needed. WSL profiles don't go through sshd and have their own pty
    // handling, so this is SSH-only.
    if let RemoteKind::Ssh = remote_kind {
        wrapped_args.insert(0, "-tt".to_string());
    }
    wrapped_args.push("~/.local/bin/som-tmux".to_string());
    wrapped_args.push(profile_name.to_string());
    wrapped_args.push(pane_id.to_string());
    wrapped_args.push("$SHELL".to_string());
    wrapped_args.push("--cursor-shape".to_string());
    wrapped_args.push(cursor_shape_arg(cursor_shape).to_string());
    if let Some(scrollback) = scrollback {
        wrapped_args.push("--scrollback".to_string());
        wrapped_args.push(scrollback.to_string());
    }
    // `--` then `-l`: a login-shell flag passed through to `$SHELL` itself
    // (`som-tmux`'s own arg parser treats everything after `--` as extra
    // args for the spawned program, see `main.rs`'s `Args` doc comment).
    // A plain (non-tmux) SSH profile gets this for free — Som runs a bare
    // `ssh host` with no explicit command, which makes sshd start a REAL
    // login shell itself (that's also what prints the MOTD banner). A
    // `tmux: true` profile's `ssh host ~/.local/bin/som-tmux ...` is an
    // EXPLICIT remote command, which sshd never treats as a login session
    // for — so `$SHELL` was starting as a plain non-login shell, silently
    // skipping `.bash_profile`/`.profile` (only `.bashrc` still ran) and
    // never printing the MOTD a real login shell would. `-l` mirrors
    // what the ordinary profile gets automatically, restoring both.
    wrapped_args.push("--".to_string());
    wrapped_args.push("-l".to_string());
    wrapped_args
}

/// Detects whether `shell` is a `som-tmux`-wrapped command (built by
/// `wrap_command_args`/`wrap_remote_command_args`) and, if so, returns an
/// equivalent `Shell` with a FRESH `pane_id` substituted in place of the
/// original — everything else (profile, program/args, cursor-shape/
/// scrollback flags) copied through unchanged — ALONGSIDE that same fresh
/// pane_id as a plain `String`, so callers that need to record it (see
/// `TerminalView::clone_on_split`, which persists it into `Workspace::
/// som_tab_tmux_sessions` so a later restore can reattach to this split's
/// own HOLDER instead of losing track of it) don't have to re-parse the
/// rebuilt `Shell` a second time just to recover the value this function
/// already generated. Returns `None` for any other shell, which callers
/// treat as "not tmux-wrapped, clone normally".
///
/// Why this exists at all: `TerminalView::clone_on_split` (used for
/// Ctrl+\-style pane splitting) would otherwise copy this terminal's exact
/// shell command byte-for-byte into the new split pane — for a tmux-wrapped
/// shell that means the SAME `pane_id`, which connects the new split to the
/// SAME HOLDER/session as the pane it was split from (confirmed bug report:
/// starting `htop` in one split pane made it appear in every pane of the
/// tab, because they were all just RELAYs onto one shared session). This
/// gives the split its OWN independent HOLDER/session instead.
///
/// Deliberately re-parses the argv shape rather than threading a `pane_id`
/// through some parallel piece of state — `wrap_command_args`/
/// `wrap_remote_command_args` are the only places that know this exact
/// shape, so detecting/rewriting it here means any future change to that
/// shape only needs updating in one place to keep this in sync (the reverse
/// of maintaining a second, parallel encoding of the same information).
pub fn rebuild_tmux_shell_with_fresh_pane_id(shell: &Shell) -> Option<(Shell, String)> {
    let Shell::WithArguments { program, args, title_override } = shell else { return None };
    let fresh_pane_id = uuid::Uuid::new_v4().to_string();

    match classify_remote(program) {
        RemoteKind::Local => {
            // wrap_command_args: [profile, pane_id, original_program, ...]
            if !program.contains("som-tmux") || args.len() < 2 {
                return None;
            }
            let mut new_args = args.clone();
            new_args[1] = fresh_pane_id.clone();
            Some((
                Shell::WithArguments { program: program.clone(), args: new_args, title_override: title_override.clone() },
                fresh_pane_id,
            ))
        }
        RemoteKind::Ssh | RemoteKind::Wsl => {
            // wrap_remote_command_args: [...host_args, "~/.local/bin/som-tmux", profile, pane_id, "$SHELL", ...]
            let server_pos = args.iter().position(|a| a.contains("som-tmux"))?;
            let pane_id_pos = server_pos + 2;
            if pane_id_pos >= args.len() {
                return None;
            }
            let mut new_args = args.clone();
            new_args[pane_id_pos] = fresh_pane_id.clone();
            Some((
                Shell::WithArguments { program: program.clone(), args: new_args, title_override: title_override.clone() },
                fresh_pane_id,
            ))
        }
    }
}

/// Builds the argv for running `remote_program` on the far side of an
/// `ssh`/`wsl` profile's OWN connection args — e.g. for `ssh 192.168.50.5`
/// this gives `["192.168.50.5", "~/.local/bin/som-tmux", "--version"]`,
/// which `ssh` hands to a shell on the far end exactly like the real relay
/// invocation does (`wrap_remote_command_args`). Shared by the deploy-check
/// path so it builds the SAME kind of command line, not a parallel one that
/// could drift out of sync with what actually gets run for real.
///
/// CRITICAL: any element of `extra` containing spaces (a real `sh -lc
/// "<script>"` invocation, not a single-word flag like `--version`) MUST
/// be single-quoted here — OpenSSH's client concatenates every argv
/// element after the host into ONE space-joined command line and re-parses
/// THAT as a shell command on the remote side (see `ssh(1)`: "If command
/// is specified... arguments are concatenated together, separated by
/// spaces, to form a single command"), WITHOUT re-quoting each original
/// argv element. A bare multi-word `sh -lc echo hello world` (four
/// separate Rust `String`s at this level) reaches the remote shell as
/// `-c`'s argument being just `echo` (the first word after `-c`), with
/// `hello`/`world` becoming positional params instead of part of the
/// script — confirmed live with a plain Rust `Command::args(...)` test
/// (no wrapping shell involved at all) against BOTH a real deb host and
/// `ssh localhost`: `sh -lc "echo hi"` printed nothing. Every call site
/// passing a multi-word script through `extra` already wraps it in single
/// quotes for exactly this reason — this function does NOT do that
/// wrapping itself (it has no way to tell "a flag" from "a script" apart),
/// so it's each caller's responsibility.
fn wrap_remote_probe_args(host_args: &[String], remote_program: &str, extra: &[&str]) -> Vec<String> {
    let mut wrapped_args = host_args.to_vec();
    wrapped_args.push(remote_program.to_string());
    wrapped_args.extend(extra.iter().map(|s| s.to_string()));
    wrapped_args
}

/// Wraps `script` in single quotes so it survives OpenSSH's own argv-to-
/// one-command-line concatenation as ONE shell word instead of being
/// re-split on whitespace — see `wrap_remote_probe_args`'s doc comment for
/// why this matters. Escapes any single quote already in `script` using
/// the standard POSIX-shell trick (`'\''`: close the quote, an escaped
/// literal quote, reopen the quote) since a bare embedded `'` would
/// otherwise end the quoting early.
fn shell_quote(script: &str) -> String {
    format!("'{}'", script.replace('\'', r#"'\''"#))
}

/// Kills every `som-tmux --holder` process on the far end of an SSH `tmux:
/// true` profile that (a) was spawned by THIS SAME client machine (its
/// `--client-id` matches this cleanup connection's own `$SSH_CLIENT` IP —
/// see `som_tmux::protocol::ssh_client_ip`'s doc comment for why that's
/// the right scope) and (b) whose `--pane-id` isn't in `live_pane_ids` —
/// i.e. a HOLDER for a pane that isn't (or is no longer) in THIS client's
/// `db.json` at all, so nothing running on this machine will EVER try to
/// reattach to it again. Called once per host during `restore_som_tabs`,
/// right alongside the existing deploy-check SSH round-trip (piggybacking
/// on that same "we're already paying for one SSH connection to this host
/// anyway" moment rather than adding a second one).
///
/// Deliberately keyed on `db.json` membership, not "how long has this
/// HOLDER been idle" or "is its shell process busy" — those need per-pane
/// state this function has no access to and, more importantly, would be
/// WRONG for a real live pane that's simply not the active tab right now (a
/// background tab's HOLDER is idle but absolutely not an orphan). A pane_id
/// db.json doesn't know about at all, by contrast, can never be reattached
/// to by anything — the ONLY way Som ever learns a pane_id to reattach to
/// is by reading it back out of db.json in the first place (see
/// `restore_som_tabs`'s own pane_id lookup) or by a currently-open tab that
/// already put it there, both of which `live_pane_ids` already covers.
///
/// Scoping to `--client-id` matters whenever more than one Som installation
/// SSHes into the same remote host (e.g. a Windows machine AND a Mac both
/// connect to `deb`) — a HOLDER the OTHER machine spawned is invisible to
/// THIS machine's `db.json` and would look orphaned by pane_id alone, even
/// though it's perfectly live from that other machine's point of view.
/// Comparing `--client-id` against this very cleanup connection's own
/// `$SSH_CLIENT` needs no coordination between installations at all — sshd
/// independently reports the same source IP to every SSH connection from
/// the same client machine.
///
/// Best-effort: a failure here (host unreachable, no processes to list,
/// `ps`/`grep` missing) is logged and swallowed rather than propagated —
/// this is housekeeping, not something that should block a tab from
/// restoring.
fn kill_orphaned_holders(host_args: &[String], remote_kind: RemoteKind, live_pane_ids: &[String]) {
    // WSL has no long-lived cross-restart HOLDER of its own the way an SSH
    // profile's remote host does (see `wrap_remote_command_args`'s `-tt`
    // doc comment) — nothing to clean up there.
    if !matches!(remote_kind, RemoteKind::Ssh) {
        return;
    }
    // `ps -eo args` (not `ps aux`, which truncates the command line on
    // some `ps` implementations, and not `pgrep -a`, which isn't installed
    // everywhere) — one `pid\x1fargs` pair per line, `\x1f` (unit
    // separator, never a legal byte in a pid or an ordinary command line)
    // rather than whitespace so a `--program` argument containing spaces
    // doesn't shift the split. The leading `echo "$SSH_CLIENT"` line (read
    // separately below) piggybacks THIS SAME SSH connection's own
    // `$SSH_CLIENT` — sshd sets it to this cleanup connection's actual
    // source IP, which is by definition identical to what any OTHER
    // invocation from this same client machine (i.e. a RELAY spawning a
    // HOLDER) already got and passed down as `--client-id`.
    //
    // Matches on `--holder` (built from two concatenated awk string
    // literals, `"--hold" "er"`, rather than written as one) specifically
    // so the match pattern itself never appears as a literal substring
    // anywhere in THIS `sh -lc "..."` invocation's own command line — a
    // plain `/--holder/` (or `/--pane-id/`, or `/som-tmux.*--holder/`)
    // regex would find its own `awk` and the `sh -lc` wrapper that ran it
    // too, since the script text necessarily contains whatever substring
    // it's searching for. `index()` on the concatenation sidesteps that
    // self-match without needing a second process to post-filter results.
    // `whoami` + `$SSH_CLIENT` together reproduce the EXACT `<user>@<ip>`
    // shape `som_tmux::protocol::ssh_client_id` builds on the RELAY side
    // (see that function's doc comment) — comparing against a bare IP
    // here used to never match at all (a real regression: `--client-id`
    // is always `user@ip`, so `client_id != this_client_ip` was `true`
    // unconditionally once `--client-id` stopped being a bare IP,
    // silently disabling orphan cleanup entirely — confirmed live: 24
    // orphaned HOLDER processes had accumulated on a real deb host with
    // this bug in place, none of them ever cleaned up).
    // Prefixed with a marker (`SOM_CLIENT_ID:`) rather than trusting this
    // to be the FIRST line of output — a login shell (`sh -lc`, i.e. `-l`)
    // can run profile scripts (`.bashrc`/`.profile`/version-manager init
    // like `fnm`/`nvm`) that print their OWN unrelated lines to stdout
    // before this script's own `echo` ever runs (confirmed live against a
    // real `ssh localhost` WSL2 setup: `/usr/local/bin/fnm` appeared as
    // literal stdout noise ahead of everything else). Searching for the
    // marker instead of blindly taking the first line makes this immune to
    // that ordering, on any host, not just ones without such scripts.
    let script = r#"echo "SOM_CLIENT_ID:$(whoami)@$SSH_CLIENT"; ps -eo pid,args | awk 'index($0, "--hold" "er") {pid=$1; $1=""; printf "%s\x1f%s\n", pid, $0}'"#;
    let quoted_script = shell_quote(script);
    let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_script]);
    let output = match run_remote_command(remote_kind, &probe) {
        Ok(output) => output,
        Err(err) => {
            log::warn!("failed to list remote som-tmux holders for orphan cleanup, skipping: {err:#}");
            return;
        }
    };
    let Some(marker_line) = output.lines().find(|line| line.starts_with("SOM_CLIENT_ID:")) else {
        log::warn!("unexpected output from orphan-cleanup probe, skipping: {output:?}");
        return;
    };
    // `marker_line` (after the marker prefix) is `<user>@<ip> <port>
    // <server-port>` (whoami's output concatenated directly onto
    // $SSH_CLIENT's first field) — taking just the first whitespace-
    // separated token gives `<user>@<ip>`.
    let Some(this_client_id) = marker_line["SOM_CLIENT_ID:".len()..].split_whitespace().next() else {
        // Not actually an SSH connection from sshd's point of view (no
        // `$SSH_CLIENT`) — shouldn't happen for a real `tmux: true` SSH
        // profile, but if it ever does there's no safe way to tell which
        // HOLDERs belong to this client, so skip cleanup entirely rather
        // than guess.
        return;
    };
    // Every OTHER line is either the awk script's own `pid\x1fargs` output
    // or unrelated shell-profile noise — the `\u{1f}` unit separator only
    // ever appears in the former (see the awk script's own comment above
    // for why), so filtering on its presence discards the noise
    // regardless of whether it landed before or after the marker line.
    let ps_output: String = output
        .lines()
        .filter(|line| line.contains('\u{1f}'))
        .collect::<Vec<_>>()
        .join("\n");
    // An empty `$SSH_CLIENT` collapses to a bare `user@` (whoami succeeded,
    // sshd didn't set the var) — just as unable to safely identify this
    // client's HOLDERs as the "no $SSH_CLIENT at all" case above, so treat
    // it the same way rather than matching a `--client-id` that also
    // happens to end in a bare `@`.
    if this_client_id.ends_with('@') {
        return;
    }

    let orphaned_pids = parse_orphaned_holder_pids(&ps_output, this_client_id, live_pane_ids);

    if orphaned_pids.is_empty() {
        return;
    }
    log::info!("killing {} orphaned som-tmux holder(s) not in db.json: {orphaned_pids:?}", orphaned_pids.len());
    let kill_script = format!("kill {} 2>/dev/null || true", orphaned_pids.join(" "));
    let quoted_kill_script = shell_quote(&kill_script);
    let kill_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_kill_script]);
    if let Err(err) = run_remote_command(remote_kind, &kill_probe) {
        log::warn!("failed to kill orphaned som-tmux holders: {err:#}");
    }
}

/// Parses `pid\x1fargs` lines (the format `kill_orphaned_holders`'s `awk`
/// script emits) and returns the pids of every HOLDER that (a) was spawned
/// for `this_client_id` (its `--client-id` matches — the FULL `<user>@<ip>`
/// value, not just the ip half) and (b) whose `--pane-id` isn't in
/// `live_pane_ids`. Pulled out of `kill_orphaned_holders` itself so this
/// string-parsing logic can be unit-tested without an actual SSH
/// round-trip.
///
/// A HOLDER with NO `--client-id` at all (an old binary from before that
/// flag existed) is deliberately left alone rather than treated as a match
/// — silently killing pre-upgrade HOLDERs the first time this code ships
/// would be a surprising one-time mass-kill; they age out naturally once
/// each pane is reattached (which redeploys/relaunches with the new
/// binary, see `ensure_remote_binary_deployed`).
fn parse_orphaned_holder_pids(ps_output: &str, this_client_id: &str, live_pane_ids: &[String]) -> Vec<String> {
    fn find_flag_value<'a>(args: &'a str, flag: &str) -> Option<&'a str> {
        let pos = args.find(flag)?;
        args[pos + flag.len()..].split_whitespace().next()
    }

    let mut orphaned_pids = Vec::new();
    for line in ps_output.lines() {
        let Some((pid, args)) = line.split_once('\u{1f}') else { continue };
        let Some(client_id) = find_flag_value(args, "--client-id ") else { continue };
        if client_id != this_client_id {
            continue;
        }
        let Some(pane_id) = find_flag_value(args, "--pane-id ") else { continue };
        if !pane_id.is_empty() && !live_pane_ids.iter().any(|id| id == pane_id) {
            orphaned_pids.push(pid.trim().to_string());
        }
    }
    orphaned_pids
}

/// Shows a dismissible banner for a som-tmux deploy/setup failure — same
/// notification shape as `som_config.rs`'s `SomConfig::show_parse_error`
/// (a plain `MessageNotification` with a "Copy" button for the full error
/// text), reused here so ALL of Som's own user-facing error banners look
/// and behave the same way, not just settings.json parse errors. Unlike
/// `show_parse_error`, this doesn't need to search `cx.windows()` for a
/// `Workspace` — every caller already has a `WeakEntity<Workspace>` in
/// hand (the tab/restore context the failure happened in), so it takes
/// one directly instead.
fn show_som_tmux_error(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};

    let id = NotificationId::Named("som-tmux-deploy-error".into());
    workspace.show_notification(id, cx, move |cx| {
        let message2 = message.clone();
        let message3 = message.clone();
        cx.new(|cx| {
            MessageNotification::new(message2, cx)
                .primary_message("Copy")
                .primary_on_click(move |_window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(message3.clone()));
                })
                .show_suppress_button(false)
        })
    });
}

/// Ensures `~/.local/bin/som-tmux` on the far side of an `ssh`/`wsl`
/// tmux profile is present and matches THIS Som build's version — see
/// `project_som_tmux` memory for the full policy this implements. Runs
/// entirely on a background thread (blocking `ssh`/`wsl`/`scp` child
/// processes) — callers must not call this from GPUI's main thread.
///
/// Deploy mechanism is `scp` of a PRE-BUILT binary from `~/.config/som/
/// tmux/{platform}/som-tmux` (see `som_tmux::protocol::platform_binaries_
/// dir`) — NOT `git pull && cargo build` on the remote machine, which is
/// what this used to do. That approach needed a full clone of this
/// repository AND a working Rust toolchain already present on every single
/// remote host, and cost a real (sometimes multi-minute) compile on every
/// version bump; `scp` of an already-built file costs a few hundred
/// milliseconds regardless of host. The pre-built binaries themselves come
/// from Som's own installer for each platform (packaged once at release
/// time, not built here) — WSL keeps the OLD git-pull-and-build path since
/// it isn't really a separate "remote platform" needing its own packaged
/// binary (same machine, same architecture Som itself just built for).
///
/// UNLIKE the old approach, this one is NOT safe to run while an old
/// HOLDER is still alive on the remote host: overwriting a running Linux
/// binary in place via `cp`/`scp`'s destination-truncate semantics fails
/// outright (`ETXTBSY`, confirmed by direct reproduction — ordinary Unix
/// "safe to replace a running executable" semantics only hold for an
/// atomic rename onto a NEW inode, not an in-place truncate-and-rewrite).
/// So a version mismatch now means: kill every `som-tmux` process this
/// account owns on that host FIRST (`kill_all_holders_for_redeploy`),
/// THEN `scp` the new binary in. Every live pane on every client currently
/// attached to that host (this account's own tabs AND, if sharing an
/// account across machines, any other client's) loses its connection and
/// reconnects to a brand new HOLDER on next use — an accepted, explicit
/// tradeoff (long-running remote sessions are exactly what tmux:true
/// exists to preserve across a LOCAL Som restart, but a version bump is
/// disruptive by nature here; the alternative, a remote compile, was both
/// slower AND already had its own ETXTBSY-shaped failure mode against a
/// live HOLDER — see `project_bugs` memory).
fn ensure_remote_binary_deployed(host_args: &[String], remote_kind: RemoteKind) -> anyhow::Result<()> {
    let local_version = som_tmux::protocol::HandshakeInfo::current().version;

    let version_probe = wrap_remote_probe_args(host_args, "~/.local/bin/som-tmux", &["--version"]);
    let remote_info = run_remote_command(remote_kind, &version_probe)
        .ok()
        .and_then(|output| serde_json::from_str::<som_tmux::protocol::HandshakeInfo>(output.trim()).ok());

    if remote_info.as_ref().map(|info| info.version.as_str()) == Some(local_version.as_str()) {
        return Ok(()); // already up to date, nothing to do
    }

    log::info!(
        "som-tmux on remote host is {:?} (local build is {local_version:?}) — redeploying",
        remote_info.as_ref().map(|info| &info.version)
    );

    // WSL is the same machine/architecture Som itself was just built for —
    // no separate pre-built platform binary to `scp` in, so it keeps the
    // original remote-build path (still cheap: WSL's own repo clone,
    // usually already warm from Som's own dev use, and no network hop).
    if let RemoteKind::Wsl = remote_kind {
        let deploy_script =
            "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_tmux) && mkdir -p ~/.local/bin && cp target/release/som-tmux ~/.local/bin/som-tmux";
        let quoted_deploy_script = shell_quote(deploy_script);
        let deploy_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_deploy_script]);
        run_remote_command(remote_kind, &deploy_probe)?;
        return Ok(());
    }

    // The remote's own handshake tells us exactly which pre-built binary
    // to send when `~/.local/bin/som-tmux` already exists there (even an
    // old/incompatible one, since `HandshakeInfo` has been part of the
    // wire format since before this deploy mechanism existed). A genuinely
    // FIRST-ever deploy (nothing at that path yet — the actual `usa`
    // real-world case this fixes) has no `remote_info` at all: falling
    // back to THIS (Windows) machine's own `current_platform()` here was a
    // real, confirmed bug — it deployed a Windows .exe to a real Debian
    // `usa` host ("cannot execute binary file: Exec format error"), since
    // there's no reason at all a remote SSH server shares Som's own local
    // platform. `uname_platform` below asks the remote directly instead —
    // `uname` exists on every Unix `ssh` could plausibly be reaching (WSL
    // is handled separately, above, before this point is ever reached).
    let (os, arch) = match remote_info {
        Some(info) => (info.os, info.arch),
        None => uname_platform(host_args, remote_kind)?,
    };
    let Some(local_binary) = som_tmux::protocol::local_binary_path_for(os, arch) else {
        anyhow::bail!(
            "no pre-built som-tmux binary for {os:?}/{arch:?} under {:?} — nothing to deploy",
            som_tmux::protocol::platform_binaries_dir()
        );
    };

    // Kill every som-tmux process (RELAY and HOLDER alike) this account
    // owns on this host BEFORE overwriting the binary file in place — see
    // this function's own doc comment for why (`ETXTBSY` against a live
    // HOLDER). Every live pane on this host reconnects to a fresh HOLDER
    // on next use; an accepted, explicit tradeoff for a version bump.
    //
    // Retried up to `KILL_AND_SCP_ATTEMPTS` times as a whole (kill THEN
    // scp, not scp alone) — `kill_all_holders_for_redeploy` already kills
    // with SIGKILL and waits for every pid to actually disappear from `ps`
    // in the SAME ssh session, but that wait is itself best-effort/bounded
    // (2s) and best-effort on the LISTING step too (a failed `ps` probe
    // just skips cleanup rather than blocking), so a single kill+scp pass
    // can still occasionally lose the race against a slow-to-exit or
    // freshly-respawned process. Re-running the whole pair (not just scp)
    // means a second attempt also re-lists and re-kills anything still
    // alive, rather than retrying scp against a binary the first attempt
    // never actually got out of the way.
    const KILL_AND_SCP_ATTEMPTS: u32 = 2;
    let mut last_scp_err = None;
    for attempt in 1..=KILL_AND_SCP_ATTEMPTS {
        kill_all_holders_for_redeploy(host_args, remote_kind);
        match scp_to_remote(host_args, &local_binary, "~/.local/bin/som-tmux") {
            Ok(()) => {
                last_scp_err = None;
                break;
            }
            Err(err) => {
                log::warn!("scp attempt {attempt}/{KILL_AND_SCP_ATTEMPTS} failed for {host_args:?}: {err:#}");
                last_scp_err = Some(err);
            }
        }
    }
    if let Some(err) = last_scp_err {
        return Err(err.context(format!(
            "failed to deploy som-tmux to {host_args:?} after {KILL_AND_SCP_ATTEMPTS} kill+scp attempts"
        )));
    }

    let chmod_probe = wrap_remote_probe_args(host_args, "chmod", &["+x", "~/.local/bin/som-tmux"]);
    run_remote_command(remote_kind, &chmod_probe)?;
    Ok(())
}

/// Asks the remote host directly what platform it is via `uname -s`/`uname
/// -m`, for the one case `HandshakeInfo` can't help with: a host that has
/// NO `som-tmux` at `~/.local/bin/` yet at all (a genuinely first-ever
/// deploy). Every real SSH `tmux: true` profile target is some Unix (this
/// codebase's four supported platforms are Windows/macOS/Linux-amd64/
/// Linux-arm64, but a Windows SSH SERVER is exotic enough not to special-
/// case here — `uname` simply isn't present there, and that failure
/// surfaces as an ordinary `Err`, same as any other unreachable-probe
/// case).
fn uname_platform(host_args: &[String], remote_kind: RemoteKind) -> anyhow::Result<(som_tmux::protocol::Os, som_tmux::protocol::Arch)> {
    // Prefixed with a marker (same technique `kill_orphaned_holders` uses
    // — see that function's doc comment) rather than trusting `uname -s`/
    // `uname -m` to be the first two lines of output: a login shell (`-l`)
    // can run profile scripts that print their OWN unrelated lines to
    // stdout first (confirmed live: `/usr/local/bin/fnm` on a real `ssh
    // localhost` WSL2 setup), which would otherwise be silently
    // misinterpreted as the kernel name itself.
    let quoted_script = shell_quote(r#"echo "SOM_UNAME:$(uname -s) $(uname -m)""#);
    let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_script]);
    let output = run_remote_command(remote_kind, &probe)?;
    parse_uname_platform(&output)
}

/// Parses `uname_platform`'s marker-prefixed probe output (`SOM_UNAME:
/// <kernel> <machine>`, possibly preceded by unrelated shell-profile
/// noise) into `(Os, Arch)`. Pulled out of `uname_platform` itself so this
/// parsing logic can be unit-tested without an actual SSH round-trip.
fn parse_uname_platform(output: &str) -> anyhow::Result<(som_tmux::protocol::Os, som_tmux::protocol::Arch)> {
    let marker_line = output
        .lines()
        .find_map(|line| line.strip_prefix("SOM_UNAME:"))
        .ok_or_else(|| anyhow::anyhow!("uname probe output had no SOM_UNAME: marker line: {output:?}"))?;
    let mut fields = marker_line.split_whitespace();
    let kernel = fields.next().unwrap_or("").trim();
    let machine = fields.next().unwrap_or("").trim();

    let os = match kernel {
        "Linux" => som_tmux::protocol::Os::Linux,
        "Darwin" => som_tmux::protocol::Os::Darwin,
        other => anyhow::bail!("unsupported remote kernel {other:?} reported by uname -s"),
    };
    let arch = match machine {
        "x86_64" => som_tmux::protocol::Arch::Amd64,
        "aarch64" | "arm64" => som_tmux::protocol::Arch::Arm64,
        other => anyhow::bail!("unsupported remote machine architecture {other:?} reported by uname -m"),
    };
    Ok((os, arch))
}

/// Kills every `som-tmux` process (RELAY and HOLDER alike — a version bump
/// invalidates the RELAY side too, not just the detached HOLDER) on the far
/// end of an SSH `tmux: true` profile — called right before `ensure_remote_
/// binary_deployed` overwrites the binary file in place, since that fails
/// outright against any process still executing it (`ETXTBSY`, see that
/// function's doc comment).
///
/// Deliberately does NOT filter by `--client-id` the way `kill_orphaned_
/// holders` does — a version mismatch means the file on disk is about to
/// change out from under EVERY process executing it, this account's own
/// panes AND (if multiple client machines share this same SSH account,
/// see `som_tmux::protocol::ssh_client_id`'s doc comment) any other
/// client's too, so there's no "belongs to someone else, leave it" case to
/// preserve here the way there is for orphan cleanup. What `kill` alone
/// already can't reach — another OS account's processes — stays untouched
/// purely because Unix permissions forbid it regardless of any filtering
/// this function could add; see `ssh_client_id`'s doc comment for why this
/// is belt-and-suspenders there but not meaningful here.
///
/// UNLIKE `kill_orphaned_holders` (which only ever touches a HOLDER whose
/// pane_id is missing from `db.json`), this kills EVERY matching process
/// regardless of whether its pane is still live — a version mismatch means
/// every pane on this host needs a fresh HOLDER anyway, live or not, so
/// there's nothing to preserve by being selective here.
///
/// Best-effort, same as `kill_orphaned_holders`: a failure here is logged
/// and swallowed, and the caller proceeds to `scp` regardless — if the old
/// process really is still alive and blocking the overwrite, THAT step's
/// own error will surface and get logged there instead.
fn kill_all_holders_for_redeploy(host_args: &[String], remote_kind: RemoteKind) {
    if !matches!(remote_kind, RemoteKind::Ssh) {
        return;
    }
    // Same self-match-avoidance trick as `kill_orphaned_holders` — see that
    // function's doc comment for why `"--hold" "er"` is split like this.
    // Matches BOTH `--holder` (the HOLDER itself) and the bare RELAY
    // invocation, which never has `--holder` in its own args but always
    // has `som-tmux` in argv[0]'s path — `"som-tmu" "x"` splits THAT
    // pattern the same way, so the contiguous substring "som-tmux" never
    // appears verbatim in this probe's OWN `sh -lc` script text (which
    // `ps -eo args` would otherwise also match, since a process's command
    // line IS this script's literal source text).
    let script = r#"ps -eo pid,args | awk 'index($0, "som-tmu" "x") {pid=$1; $1=""; printf "%s\x1f%s\n", pid, $0}'"#;
    let quoted_script = shell_quote(script);
    let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_script]);
    let output = match run_remote_command(remote_kind, &probe) {
        Ok(output) => output,
        Err(err) => {
            log::warn!("failed to list remote som-tmux processes for redeploy cleanup, skipping: {err:#}");
            return;
        }
    };

    let pids: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_once('\u{1f}'))
        .map(|(pid, _args)| pid.trim().to_string())
        .collect();
    if pids.is_empty() {
        return;
    }
    log::info!("killing {} som-tmux process(es) on remote host ahead of a version-mismatch redeploy: {pids:?}", pids.len());
    // Plain `kill` sends SIGTERM, which a process can catch/delay/ignore —
    // and even a default handler's teardown isn't instantaneous. A `kill`
    // over one SSH connection followed by a SEPARATE `scp` connection
    // right after (as `ensure_remote_binary_deployed` does) is a real
    // race: the kernel hasn't necessarily finished tearing the process
    // down (and releasing its exec image) by the time the next
    // connection's `scp` tries to overwrite the same file, and `scp`/`cp`
    // fail outright against a still-executing binary (`ETXTBSY`,
    // confirmed live against `usa` — see `project_bugs` memory). `kill
    // -9` (SIGKILL) instead — uncatchable, unblockable, the kernel tears
    // the process down immediately with no user-space cleanup step to
    // wait out. Still followed by a short bounded `kill -0` poll (20 *
    // 100ms = 2s) IN THE SAME SSH session/script (not a second round-trip
    // from the Windows side, which would just move the same race to "did
    // THIS connection's kill finish before THAT connection's scp
    // started") as defense in depth — SIGKILL removes the process
    // immediately but the exec image's busy flag is still cleared by the
    // kernel's own (near-instant, but not appearing "before returning
    // from kill(2)") teardown, not synchronously with the signal call
    // itself.
    let pid_list = pids.join(" ");
    let kill_script = format!(
        r#"kill -9 {pid_list} 2>/dev/null || true
for _ in $(seq 1 20); do
  still_alive=0
  for pid in {pid_list}; do
    kill -0 "$pid" 2>/dev/null && still_alive=1
  done
  [ "$still_alive" = 0 ] && break
  sleep 0.1
done"#
    );
    let quoted_kill_script = shell_quote(&kill_script);
    let kill_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_kill_script]);
    if let Err(err) = run_remote_command(remote_kind, &kill_probe) {
        log::warn!("failed to kill remote som-tmux processes ahead of redeploy: {err:#}");
    }
}

/// Copies `local_path` to `host_args`' host at `remote_path` via `scp` —
/// separate from `run_remote_command` (which only ever runs `ssh`/`wsl`)
/// since `scp` has a completely different argv shape (`scp src host:dst`,
/// no shell command to hand off). Assumes `host_args` is exactly a single
/// hostname with no extra flags, same assumption `wrap_remote_probe_args`
/// already makes for the `ssh`-based probes — real profiles are always
/// `ssh <host>` with per-host quirks (ports, keys, users) living in `~/.
/// ssh/config`, never inline flags (see `terminal_panel.rs`'s test
/// fixtures / real `settings.json` profiles, all of this shape).
fn scp_to_remote(host_args: &[String], local_path: &std::path::Path, remote_path: &str) -> anyhow::Result<()> {
    let Some(host) = host_args.last() else {
        anyhow::bail!("scp_to_remote called with empty host_args");
    };
    let destination = format!("{host}:{remote_path}");
    let output = util::command::new_std_command("scp")
        .arg(local_path)
        .arg(&destination)
        .output()
        .with_context(|| format!("failed to spawn scp to copy {local_path:?} to {destination:?}"))?;
    if !output.status.success() {
        anyhow::bail!("scp exited with {:?}: {}", output.status.code(), String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// Runs one blocking `ssh`/`wsl` invocation to completion and returns its
/// stdout — shared plumbing for both the `--version` probe and the deploy
/// script in `ensure_remote_binary_deployed`. A non-zero exit or spawn
/// failure is surfaced as `Err` (e.g. "binary not found yet" for a
/// brand-new host); callers treat that as "assume out of date, deploy".
fn run_remote_command(remote_kind: RemoteKind, args: &[String]) -> anyhow::Result<String> {
    let program = match remote_kind {
        RemoteKind::Ssh => "ssh",
        RemoteKind::Wsl => "wsl",
        RemoteKind::Local => anyhow::bail!("run_remote_command called with RemoteKind::Local"),
    };
    // `util::command::new_std_command`, NOT a bare `std::process::Command::
    // new` — the plain version has no console window of its own on
    // Windows, but `ssh.exe`/`wsl.exe` are themselves console
    // subsystem binaries, so spawning them without `CREATE_NO_WINDOW`
    // (which this helper sets) makes Windows briefly flash a new console
    // window into existence for each one — confirmed as a real, visible
    // regression report (a `cmd`-looking window flashing on every new tab
    // for every `tmux: true` profile, local and remote alike, since this
    // deploy check's every-tab-open version probe is what's actually
    // spawning it).
    let output = util::command::new_std_command(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn {program:?} for remote deploy check"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{program} exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Finds `som-tmux`/`som-tmux.exe` next to Som's own
/// executable — the same "binaries live side by side" assumption
/// `target/debug/` (and any packaged distribution) already guarantees for
/// Som's other bundled tools. Mirrors the old (now-removed)
/// `som_tmux_client::server_binary_path`, which lived on the GPUI-client
/// side of the old JSON-protocol architecture; this is its natural home now
/// that the substitution happens directly in the shell command instead.
///
/// Cross-platform, not Windows-only — this is the `RemoteKind::Local` path
/// in `tmux_wrapped_shell`, which applies just as much to a Mac/Linux build
/// of Som with a plain local `zsh`/`bash` `tmux: true` profile as it does to
/// Windows' `pwsh.exe` one; only the `ssh`/`wsl` remote paths are Windows-only
/// today (those profiles' `shell` settings only make sense from a Windows
/// Som talking OUT to other machines).
fn som_tmux_binary_path() -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .context("failed to determine Som's own executable path")?
        .parent()
        .context("Som's executable path has no parent directory")?
        .to_path_buf();
    let binary_name = if cfg!(target_os = "windows") { "som-tmux.exe" } else { "som-tmux" };
    let candidate = exe_dir.join(binary_name);
    if !candidate.is_file() {
        anyhow::bail!("{binary_name} not found next to Som's own executable at {candidate:?}");
    }
    Ok(candidate)
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

#[cfg(test)]
mod tmux_shell_wrapping_tests {
    use super::*;

    #[test]
    fn wraps_a_simple_program_with_no_args() {
        let args = wrap_command_args("dnk", "pane-uuid-1", "pwsh.exe".to_string(), vec![], CursorShape::Block, None);
        assert_eq!(args, vec!["dnk", "pane-uuid-1", "pwsh.exe", "--cursor-shape", "block"]);
    }

    #[test]
    fn wraps_a_program_with_its_own_args_preserving_order() {
        // e.g. a "wsl --cd ~" profile: parse_shell_command already split
        // this into program="wsl", args=["--cd", "~"] upstream — this just
        // checks that gets appended after profile/pane-id/program, in
        // order, not re-parsed or re-joined into a single string (which
        // would risk mangling arguments containing spaces/quotes).
        let args = wrap_command_args(
            "wsl",
            "pane-uuid-2",
            "wsl".to_string(),
            vec!["--cd".to_string(), "~".to_string()],
            CursorShape::Block,
            None,
        );
        assert_eq!(args, vec!["wsl", "pane-uuid-2", "wsl", "--cd", "~", "--cursor-shape", "block"]);
    }

    #[test]
    fn preserves_a_program_path_containing_spaces_as_a_single_argument() {
        // The whole point of NOT round-tripping through a single joined
        // command string (see `create_terminal_with_program_and_args`'s
        // doc comment) — a path like this must stay one argv element, not
        // get split on its internal spaces.
        let args = wrap_command_args(
            "dnk",
            "pane-uuid-3",
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe".to_string(),
            vec![],
            CursorShape::Block,
            None,
        );
        assert_eq!(
            args,
            vec!["dnk", "pane-uuid-3", "C:\\Program Files\\PowerShell\\7\\pwsh.exe", "--cursor-shape", "block"]
        );
    }

    #[test]
    fn appends_cursor_shape_and_scrollback_flags_when_set() {
        let args = wrap_command_args(
            "dnk",
            "pane-uuid-6",
            "pwsh.exe".to_string(),
            vec![],
            CursorShape::Underline,
            Some(10000),
        );
        assert_eq!(
            args,
            vec!["dnk", "pane-uuid-6", "pwsh.exe", "--cursor-shape", "underline", "--scrollback", "10000"]
        );
    }

    #[test]
    fn classifies_ssh_and_wsl_programs_as_remote_but_everything_else_as_local() {
        assert!(matches!(classify_remote("ssh"), RemoteKind::Ssh));
        assert!(matches!(classify_remote("wsl"), RemoteKind::Wsl));
        assert!(matches!(classify_remote("wsl.exe"), RemoteKind::Wsl));
        assert!(matches!(classify_remote("pwsh.exe"), RemoteKind::Local));
        assert!(matches!(classify_remote("bash"), RemoteKind::Local));
    }

    #[test]
    fn wraps_an_ssh_profile_by_appending_the_remote_holder_invocation() {
        // profile.shell == "ssh 192.168.50.5" -> parse_shell_command splits
        // this into program="ssh", args=["192.168.50.5"] upstream. The
        // local program/args Som actually spawns must stay "ssh ..." — the
        // som-tmux invocation goes on the REMOTE side, appended
        // after ssh's own arguments, since ssh hands everything past its
        // own flags/host to a shell on the far end.
        let args =
            wrap_remote_command_args("pi5", "pane-uuid-4", vec!["192.168.50.5".to_string()], CursorShape::Block, None, RemoteKind::Ssh);
        assert_eq!(
            args,
            vec![
                "-tt", "192.168.50.5", "~/.local/bin/som-tmux", "pi5", "pane-uuid-4", "$SHELL", "--cursor-shape", "block",
                "--", "-l"
            ]
        );
    }

    #[test]
    fn wraps_a_wsl_profile_by_appending_the_remote_holder_invocation() {
        // profile.shell == "wsl --cd ~" -> program="wsl", args=["--cd", "~"].
        let args = wrap_remote_command_args(
            "wsl",
            "pane-uuid-5",
            vec!["--cd".to_string(), "~".to_string()],
            CursorShape::Bar,
            Some(5000),
            RemoteKind::Wsl,
        );
        // No `-tt` for WSL — that's SSH-only (WSL has its own pty handling
        // and doesn't go through sshd).
        assert_eq!(
            args,
            vec![
                "--cd", "~", "~/.local/bin/som-tmux", "wsl", "pane-uuid-5", "$SHELL", "--cursor-shape", "bar",
                "--scrollback", "5000", "--", "-l"
            ]
        );
    }

    #[test]
    fn builds_a_version_probe_using_the_same_host_args_as_the_real_invocation() {
        let args = wrap_remote_probe_args(
            &["192.168.50.5".to_string()],
            "~/.local/bin/som-tmux",
            &["--version"],
        );
        assert_eq!(args, vec!["192.168.50.5", "~/.local/bin/som-tmux", "--version"]);
    }

    #[test]
    fn orphan_scan_flags_a_holder_whose_pane_id_is_not_in_db_json() {
        // Regression coverage: `--client-id` is a FULL `<user>@<ip>` value
        // (see `som_tmux::protocol::ssh_client_id`), not a bare IP — every
        // one of these tests used to compare a bare IP against a bare IP
        // and pass, while the real `kill_orphaned_holders` compared a bare
        // IP against a real `user@ip` `--client-id` and NEVER matched,
        // silently disabling orphan cleanup entirely in production (see
        // `kill_orphaned_holders`'s doc comment for how this was confirmed
        // live). These fixtures use the realistic `user@ip` shape so this
        // test suite actually would have caught that.
        let ps_output =
            "12345\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id dead-pane --client-id dnk@192.168.50.2\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &["live-pane".to_string()]);
        assert_eq!(orphaned, vec!["12345"]);
    }

    #[test]
    fn orphan_scan_leaves_a_holder_whose_pane_id_is_in_db_json_alone() {
        let ps_output =
            "12345\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id live-pane --client-id dnk@192.168.50.2\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &["live-pane".to_string()]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn orphan_scan_handles_multiple_lines_and_ignores_malformed_ones() {
        let ps_output = concat!(
            "111\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id orphan-a --client-id dnk@192.168.50.2\n",
            "222\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id live-pane --client-id dnk@192.168.50.2\n",
            "not a valid line at all\n",
            "333\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id orphan-b --client-id dnk@192.168.50.2\n",
        );
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &["live-pane".to_string()]);
        assert_eq!(orphaned, vec!["111", "333"]);
    }

    #[test]
    fn orphan_scan_ignores_lines_with_no_pane_id_argument() {
        let ps_output = "999\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --client-id dnk@192.168.50.2\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &[]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn orphan_scan_ignores_lines_with_no_client_id_argument() {
        // An old pre-upgrade HOLDER binary, from before `--client-id`
        // existed — must be left alone, not treated as an automatic match.
        let ps_output = "888\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id dead-pane\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &[]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn orphan_scan_ignores_a_holder_spawned_by_a_different_client_machine() {
        // Simulates a second Som installation (e.g. a Mac) also SSHed into
        // the same host — its HOLDER must never look orphaned just because
        // this client's own db.json doesn't know its pane_id.
        let ps_output =
            "777\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id other-machines-pane --client-id dnk@192.168.50.9\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &[]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn orphan_scan_ignores_a_holder_spawned_by_a_different_user_on_the_same_machine() {
        // A shared build server: two different OS accounts, same client
        // machine IP — the ip half matching alone must NOT be enough.
        let ps_output =
            "555\u{1f} /home/dnk/.local/bin/som-tmux --holder --profile deb --pane-id other-users-pane --client-id otheruser@192.168.50.2\n";
        let orphaned = parse_orphaned_holder_pids(ps_output, "dnk@192.168.50.2", &[]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn parses_a_real_debian_x86_64_uname_report() {
        let (os, arch) = parse_uname_platform("SOM_UNAME:Linux x86_64\n").unwrap();
        assert_eq!(os, som_tmux::protocol::Os::Linux);
        assert_eq!(arch, som_tmux::protocol::Arch::Amd64);
    }

    #[test]
    fn parses_a_real_macos_arm64_uname_report() {
        let (os, arch) = parse_uname_platform("SOM_UNAME:Darwin arm64\n").unwrap();
        assert_eq!(os, som_tmux::protocol::Os::Darwin);
        assert_eq!(arch, som_tmux::protocol::Arch::Arm64);
    }

    #[test]
    fn parses_a_linux_aarch64_uname_report() {
        // `uname -m` on Linux reports "aarch64", NOT "arm64" (that's
        // macOS's spelling) — regression coverage for accepting both.
        let (os, arch) = parse_uname_platform("SOM_UNAME:Linux aarch64\n").unwrap();
        assert_eq!(os, som_tmux::protocol::Os::Linux);
        assert_eq!(arch, som_tmux::protocol::Arch::Arm64);
    }

    #[test]
    fn ignores_shell_profile_noise_ahead_of_the_marker_line() {
        // Regression coverage: a login shell (`sh -lc`) can run profile
        // scripts (version-manager init banners, etc.) that print
        // unrelated lines to stdout BEFORE this probe's own marker line —
        // confirmed live against a real `ssh localhost` WSL2 setup
        // (`/usr/local/bin/fnm` appeared as literal noise ahead of the
        // real uname output). The marker line must be found regardless of
        // what precedes it.
        let (os, arch) = parse_uname_platform("/usr/local/bin/fnm\nSOM_UNAME:Linux x86_64\n").unwrap();
        assert_eq!(os, som_tmux::protocol::Os::Linux);
        assert_eq!(arch, som_tmux::protocol::Arch::Amd64);
    }

    #[test]
    fn rejects_output_with_no_marker_line_at_all() {
        assert!(parse_uname_platform("Linux\nx86_64\n").is_err());
    }

    #[test]
    fn rejects_an_unrecognized_kernel_rather_than_guessing() {
        // Regression test for the real bug this whole function fixes: a
        // brand-new host with no som-tmux yet used to silently fall back
        // to THIS (Windows) machine's own platform instead of asking the
        // remote — confirmed live against a real Debian `usa` host, which
        // got a Windows .exe deployed to it ("Exec format error"). An
        // unparseable/unexpected uname report must be a hard error, never
        // a silent guess.
        assert!(parse_uname_platform("SOM_UNAME:SomeExoticKernel x86_64\n").is_err());
    }

    #[test]
    fn rejects_an_unrecognized_architecture_rather_than_guessing() {
        assert!(parse_uname_platform("SOM_UNAME:Linux riscv64\n").is_err());
    }

    #[test]
    fn rebuild_gives_a_local_tmux_shell_a_fresh_pane_id_and_nothing_else_changes() {
        let args = wrap_command_args(
            "dnk",
            "original-pane-id",
            "pwsh.exe".to_string(),
            vec![],
            CursorShape::Underline,
            Some(10000),
        );
        let shell = Shell::WithArguments {
            program: "C:\\som\\som-tmux.exe".to_string(),
            args,
            title_override: None,
        };
        let (rebuilt, fresh_pane_id) =
            rebuild_tmux_shell_with_fresh_pane_id(&shell).expect("should detect a local tmux-wrapped shell");
        let Shell::WithArguments { program, args, .. } = &rebuilt else { panic!("expected WithArguments") };
        assert_eq!(program, "C:\\som\\som-tmux.exe");
        assert_eq!(args[0], "dnk"); // profile unchanged
        assert_ne!(args[1], "original-pane-id"); // pane_id replaced
        assert_eq!(args[1], fresh_pane_id); // and matches the returned pane_id
        assert_eq!(&args[2..], &["pwsh.exe", "--cursor-shape", "underline", "--scrollback", "10000"]);
    }

    #[test]
    fn rebuild_gives_a_remote_tmux_shell_a_fresh_pane_id_and_nothing_else_changes() {
        let args = wrap_remote_command_args(
            "pi5",
            "original-pane-id",
            vec!["192.168.50.5".to_string()],
            CursorShape::Block,
            None,
            RemoteKind::Ssh,
        );
        let shell = Shell::WithArguments { program: "ssh".to_string(), args, title_override: None };
        let (rebuilt, fresh_pane_id) =
            rebuild_tmux_shell_with_fresh_pane_id(&shell).expect("should detect a remote tmux-wrapped shell");
        let Shell::WithArguments { program, args, .. } = &rebuilt else { panic!("expected WithArguments") };
        assert_eq!(program, "ssh");
        assert_eq!(args[0], "-tt");
        assert_eq!(args[1], "192.168.50.5");
        assert_eq!(args[2], "~/.local/bin/som-tmux");
        assert_eq!(args[3], "pi5"); // profile unchanged
        assert_ne!(args[4], "original-pane-id"); // pane_id replaced
        assert_eq!(args[4], fresh_pane_id); // and matches the returned pane_id
        assert_eq!(&args[5..], &["$SHELL", "--cursor-shape", "block", "--", "-l"]);
    }

    #[test]
    fn rebuild_leaves_a_non_tmux_shell_untouched() {
        let shell = Shell::WithArguments {
            program: "pwsh.exe".to_string(),
            args: vec![],
            title_override: None,
        };
        assert!(rebuild_tmux_shell_with_fresh_pane_id(&shell).is_none());
        assert!(rebuild_tmux_shell_with_fresh_pane_id(&Shell::System).is_none());
        assert!(rebuild_tmux_shell_with_fresh_pane_id(&Shell::Program("bash".to_string())).is_none());
    }

    /// Overridable via env var, same convention `terminal`'s own SSH
    /// integration tests use — defaults to `localhost` (a local sshd, e.g.
    /// WSL2's own on this dev machine) so the test suite doesn't depend on
    /// a specific machine on a specific network being reachable.
    fn integration_test_ssh_host() -> Vec<String> {
        vec![std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string())]
    }

    /// Real SSH round-trip against `integration_test_ssh_host()` — confirms
    /// `ensure_remote_binary_deployed` correctly detects a version mismatch
    /// and successfully redeploys, end to end (version probe → kill any
    /// live som-tmux processes this account owns → scp the right pre-built
    /// binary in → chmod +x → re-probe confirms the new version). Doesn't
    /// assert anything about WHICH processes get killed (see `kill_all_
    /// holders_for_redeploy`'s own doc comment) — this is specifically
    /// about the deploy mechanism itself succeeding against a real host.
    ///
    /// KNOWN LIMITATION on a real dev machine: this manufactures a version
    /// mismatch by deleting the remote binary, which forces `ensure_remote_
    /// binary_deployed` down its `local_binary_path_for` lookup — under
    /// `cfg!(test)`, `paths::config_dir()` resolves against a FAKE home
    /// directory (`C:\Users\zed\...`/`/home/zed\...`, see `util::paths::
    /// home_dir`'s own `cfg!(test)` branch) this process typically has no
    /// permission to create on a real Windows machine (confirmed:
    /// `New-Item -Path C:\Users\zed` → access denied, even from an
    /// otherwise fully-privileged dev account — creating an arbitrary
    /// top-level `C:\Users\<name>` directory needs real admin rights this
    /// test deliberately does NOT attempt to acquire). Skip this one (`test_
    /// deploy_is_a_no_op_when_already_current` below covers the OTHER,
    /// reachable branch) unless `C:\Users\zed\.config\som\tmux\linux-amd\
    /// som-tmux` (or the Unix equivalent) has been populated out of band.
    ///
    /// `#[ignore]`d by default: needs a real reachable SSH host with a
    /// clone of this repo at `~/som` (so `git pull` succeeds) — the
    /// version mismatch itself is manufactured by this test (deleting
    /// whatever's currently there), not a precondition. Run explicitly
    /// with: `cargo test -p terminal_view test_deploy_redeploys_a_version_mismatch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_deploy_redeploys_a_version_mismatch() {
        let host_args = integration_test_ssh_host();

        // Manufacture a version mismatch: remove whatever's currently
        // deployed (if anything) so ensure_remote_binary_deployed sees
        // `remote_info == None` and takes the real first-deploy path.
        let cleanup_probe = wrap_remote_probe_args(&host_args, "rm", &["-f", "~/.local/bin/som-tmux"]);
        run_remote_command(RemoteKind::Ssh, &cleanup_probe).expect("failed to clear out the remote binary for this test");

        ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh).expect("deploy should succeed against a real reachable host");

        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/som-tmux", &["--version"]);
        let output = run_remote_command(RemoteKind::Ssh, &version_probe).expect("the freshly-deployed binary should run");
        let info: som_tmux::protocol::HandshakeInfo =
            serde_json::from_str(output.trim()).expect("--version should print valid HandshakeInfo JSON");
        assert_eq!(info.version, som_tmux::protocol::HandshakeInfo::current().version);
    }

    /// Real SSH round-trip: confirms `ensure_remote_binary_deployed` is a
    /// true no-op (no kill, no scp) when the remote is already at the
    /// current version — the common case on every tab open/restore once a
    /// host has been deployed to once already. Verified by checking the
    /// binary's mtime is unchanged after the call, rather than asserting
    /// on internal call counts (this function has no test-seam for that
    /// and adding one purely for this would be over-engineering for a
    /// single assertion).
    ///
    /// Requires the remote to ALREADY be at the current version before
    /// this test runs (deliberately does NOT call `ensure_remote_binary_
    /// deployed` itself to set that up first, unlike `test_deploy_
    /// redeploys_a_version_mismatch` — a version MISMATCH path needs
    /// `local_binary_path_for`, which resolves relative to `paths::
    /// config_dir()`, which under `cfg!(test)` resolves to a fake `C:\
    /// Users\zed\...`/`/home/zed/...` home directory this process has no
    /// permission to create on a real dev machine — see `util::paths::
    /// home_dir`'s own `cfg!(test)` branch. The ALREADY-current path
    /// returns early, before ever touching `local_binary_path_for`, so
    /// it's the one case this integration test CAN safely exercise without
    /// hitting that same wall; deploy the current build to the test host
    /// manually first — e.g. `scp ~/.config/som/tmux/linux-amd/som-tmux
    /// <host>:~/.local/bin/som-tmux` — to set up the precondition.
    ///
    /// `#[ignore]`d by default — same reachability requirement as `test_
    /// deploy_redeploys_a_version_mismatch`. Run explicitly with:
    /// `cargo test -p terminal_view test_deploy_is_a_no_op_when_already_current -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_deploy_is_a_no_op_when_already_current() {
        let host_args = integration_test_ssh_host();

        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/som-tmux", &["--version"]);
        let output = run_remote_command(RemoteKind::Ssh, &version_probe)
            .expect("remote must already have SOME som-tmux at ~/.local/bin/som-tmux for this test's precondition");
        let info: som_tmux::protocol::HandshakeInfo =
            serde_json::from_str(output.trim()).expect("--version should print valid HandshakeInfo JSON");
        assert_eq!(
            info.version,
            som_tmux::protocol::HandshakeInfo::current().version,
            "test precondition not met: deploy the current build to the test host manually first (see doc comment)"
        );

        let mtime_probe = wrap_remote_probe_args(&host_args, "stat", &["-c", "%Y", "~/.local/bin/som-tmux"]);
        let mtime_before = run_remote_command(RemoteKind::Ssh, &mtime_probe).expect("stat should succeed");

        ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh).expect("no-op deploy should still report success");

        let mtime_after = run_remote_command(RemoteKind::Ssh, &mtime_probe).expect("stat should succeed");
        assert_eq!(
            mtime_before, mtime_after,
            "binary's mtime must be unchanged — a matching version must never re-kill/re-scp"
        );
    }

    /// Real SSH round-trip: creates a `som-tmux --holder` with a pane_id
    /// NOT in `live_pane_ids`, confirms `kill_orphaned_holders` kills it,
    /// and confirms a SEPARATE holder whose pane_id IS in `live_pane_ids`
    /// survives — the two behaviors this function exists to balance (see
    /// its own doc comment). Uses the real `--client-id` this account/host
    /// pair would actually get (read back via `$SSH_CLIENT` + `whoami`, the
    /// same way `som_tmux::protocol::ssh_client_id` does on the remote
    /// side), not a fabricated one, so this exercises the SAME client-id
    /// matching path production code goes through, not a shortcut around it.
    ///
    /// `#[ignore]`d by default — same reachability requirement as the
    /// deploy tests above, plus a working `som-tmux` binary already at
    /// `~/.local/bin/som-tmux` (this test spawns real HOLDER processes with
    /// it, it doesn't deploy). Run explicitly with:
    /// `cargo test -p terminal_view test_kill_orphaned_holders_only_kills_the_orphan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_kill_orphaned_holders_only_kills_the_orphan() {
        let host_args = integration_test_ssh_host();
        let orphan_pane_id = format!("test-orphan-{}", std::process::id());
        let live_pane_id = format!("test-live-{}", std::process::id());

        // `$(whoami)@$SSH_CLIENT`'s ip half — the EXACT `<user>@<ip>` shape
        // `kill_orphaned_holders` itself computes for THIS connection —
        // must be baked into each spawned HOLDER's own `--client-id` for
        // this test to mean anything: a HOLDER spawned with a mismatched
        // (or absent) `--client-id` is invisible to orphan cleanup by
        // design (see `parse_orphaned_holder_pids`'s doc comment), so
        // using anything else here would silently test nothing. Same
        // marker-prefix technique `kill_orphaned_holders` itself uses (see
        // its own doc comment) — a login shell can print unrelated profile-
        // script noise (e.g. a version manager's init banner) to stdout
        // BEFORE this echo ever runs, so blindly taking the first line/word
        // of output is unreliable on a real host (confirmed live against
        // `ssh localhost`'s WSL2 setup).
        let quoted_client_id_script = shell_quote(r#"echo "SOM_TEST_CLIENT_ID:$(whoami)@$SSH_CLIENT""#);
        let client_id_probe = wrap_remote_probe_args(&host_args, "sh", &["-lc", &quoted_client_id_script]);
        let client_id_output =
            run_remote_command(RemoteKind::Ssh, &client_id_probe).expect("failed to read back this connection's client-id");
        let client_id = client_id_output
            .lines()
            .find_map(|line| line.strip_prefix("SOM_TEST_CLIENT_ID:"))
            .and_then(|rest| rest.split_whitespace().next())
            .expect("client-id probe should print the marker line")
            .to_string();

        for pane_id in [&orphan_pane_id, &live_pane_id] {
            let spawn_script = format!(
                "nohup ~/.local/bin/som-tmux --holder --profile test-orphan-cleanup --pane-id {pane_id} --client-id {client_id} --program /bin/sh >/dev/null 2>&1 &"
            );
            let quoted_spawn_script = shell_quote(&spawn_script);
            let spawn_probe = wrap_remote_probe_args(&host_args, "sh", &["-lc", &quoted_spawn_script]);
            run_remote_command(RemoteKind::Ssh, &spawn_probe).expect("failed to spawn a test HOLDER");
        }
        // Give each HOLDER a moment to actually start listening before the
        // orphan-scan probe below greps for it.
        std::thread::sleep(std::time::Duration::from_millis(500));

        kill_orphaned_holders(&host_args, RemoteKind::Ssh, std::slice::from_ref(&live_pane_id));

        std::thread::sleep(std::time::Duration::from_millis(500));
        // A single `grep -c '<unique pane_id>'` (no intermediate `--pane-
        // id` filter) — an extra `grep -- --pane-id` stage would match ITS
        // OWN command line too (`ps -eo args` sees the `grep` process's
        // own argv, which literally contains the string `--pane-id` it's
        // searching for), double-counting every real match. The pane_id
        // itself is unique enough (embeds this test's own process id) to
        // not need that extra filter anyway.
        // Counts processes with `--holder` in argv AND this exact pane_id
        // — NOT a plain `grep -c '<pane_id>'`/`grep -- --holder`, either of
        // which would count its OWN `grep`/`sh -lc` process too (their
        // command line literally contains whatever plain substring they're
        // searching for). Same awk self-match-avoidance trick production
        // code already uses (`kill_orphaned_holders`'s own doc comment) —
        // splitting `--holder` across two concatenated awk string literals
        // so the contiguous substring never appears in this script's own
        // source text, which `ps -eo args` would otherwise also match.
        fn count_holders_with_pane_id(host_args: &[String], pane_id: &str) -> u32 {
            let script =
                format!(r#"ps -eo args | awk 'index($0, "--hold" "er") && index($0, "{pane_id}")' | wc -l"#);
            let quoted_script = shell_quote(&script);
            let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_script]);
            let output = run_remote_command(RemoteKind::Ssh, &probe).expect("ps probe should succeed");
            // Last non-empty line, not the whole trimmed output — a login
            // shell can print unrelated profile-script noise ahead of
            // `wc -l`'s own result (same real `ssh localhost` behavior
            // `kill_orphaned_holders`'s own doc comment describes).
            output.lines().rev().find(|line| !line.trim().is_empty()).and_then(|line| line.trim().parse().ok()).unwrap_or(999)
        }

        let orphan_count = count_holders_with_pane_id(&host_args, &orphan_pane_id);
        assert_eq!(orphan_count, 0, "the orphaned HOLDER (not in live_pane_ids) should have been killed");

        let live_count = count_holders_with_pane_id(&host_args, &live_pane_id);
        assert_eq!(live_count, 1, "the live HOLDER (pane_id IS in live_pane_ids) must survive");

        // Cleanup: the live one was deliberately spared above, so kill it
        // now that the test is done with it.
        let cleanup_script = format!("pkill -9 -f 'pane-id {live_pane_id}' 2>/dev/null || true");
        let quoted_cleanup_script = shell_quote(&cleanup_script);
        let cleanup_probe = wrap_remote_probe_args(&host_args, "sh", &["-lc", &quoted_cleanup_script]);
        run_remote_command(RemoteKind::Ssh, &cleanup_probe).ok();
    }
}