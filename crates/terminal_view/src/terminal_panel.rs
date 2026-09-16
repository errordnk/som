use std::path::PathBuf;

use crate::{
    TerminalView, default_working_directory,
    pending_terminal_tab::PendingTerminalTab,
};
use gpui::{
    App, AppContext as _, AssetSource, Context, Entity, Task, TaskExt, WeakEntity, Window,
};
use project::Project;

use settings::Settings;
use task::{Shell, ShellBuilder, SpawnInTerminal};
use terminal::{Terminal, terminal_settings::{CursorShape, TerminalSettings}};
use util::ResultExt;
use workspace::{SomTabsRestorer, TabProfiles, Workspace};

use anyhow::{Context as _, Result, anyhow};

pub fn init(cx: &mut App) {
    cx.set_global(SomTabsRestorer(std::sync::Arc::new(
        |workspace, window, cx| TerminalPanel::restore_som_tabs(workspace, window, cx),
    )));
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
        },
    )
    .detach();
}

pub struct TerminalPanel;

impl TerminalPanel {
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
        if let Some(idx) = action.missing_profile_index {
            Self::show_missing_profile_error(workspace, idx, cx);
            return;
        }

        let default_tab_name = cx
            .try_global::<TabProfiles>()
            .and_then(|p| p.0.get(TabProfiles::default_index(cx)).map(|profile| profile.name.clone()));
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
        let profile_home = profile
            .as_ref()
            .and_then(|p| p.home.as_deref())
            .and_then(|dir| shellexpand::full(dir).ok())
            .map(|dir| PathBuf::from(dir.to_string()))
            .filter(|dir| dir.is_dir());
        let working_directory = profile_home.or_else(|| default_working_directory(workspace, cx));
        let local = action.local;

        if !is_shell_override && profile.as_ref().is_some_and(wants_srp) {
            let profile = profile.unwrap();
            let cwd = working_directory.clone().map(|p| p.to_string_lossy().to_string()).map(PathBuf::from);
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
            let (remote_program, host_args) = project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
            let remote_kind = classify_remote(&remote_program);
            let terminal_settings = TerminalSettings::get_global(cx);
            let cursor_shape = terminal_settings.cursor_shape;
            let scrollback = terminal_settings.max_scroll_history_lines;
            let cell_pixel_size = approximate_cell_pixel_size(terminal_settings);
            let window_handle = window.window_handle();
            // Drives the titlebar's drag-zone spinner (see `workspace::
            // SomRestoreActivity`'s doc comment) for a new tab's connect
            // attempt, same as `restore_som_tabs` already does for
            // restore-on-launch — `begin` here, `end` once the whole
            // inner future below resolves.
            workspace::SomRestoreActivity::begin(cx);
            cx.spawn_in(window, async move |workspace, cx| {
                let result: anyhow::Result<()> = async {
                    let profile_for_open = profile.clone();
                    let (item_id, srp_pane_id) = Self::open_srp_or_plain_ssh(
                        workspace.clone(),
                        window_handle,
                        cx,
                        placeholder_item_id,
                        profile_for_open,
                        pane_id,
                        cwd,
                        cursor_shape,
                        scrollback,
                        cell_pixel_size,
                        tab_name,
                        tab_icon,
                        profile_index,
                    )
                    .await?;
                    if let Some(pane_id) = srp_pane_id {
                        workspace.update(cx, |workspace, _cx| {
                            workspace.set_tmux_sessions_for_item(item_id, vec![pane_id]);
                        })?;
                    }

                    // Background mtime-based deploy/redeploy check — runs
                    // strictly AFTER the tab has already opened (SRP or
                    // fallback plain SSH, either way), never blocking it.
                    // Fire-and-forget: the spawned task still awaits its
                    // own background_spawn'd blocking work and surfaces a
                    // toast either way (see `show_somsrv_toast`'s doc
                    // comment).
                    if let RemoteKind::Ssh | RemoteKind::Wsl = remote_kind {
                        let host_args = host_args.clone();
                        let profile_name = profile.name.clone();
                        let profile_os = profile.os;
                        cx.spawn({
                            let workspace = workspace.clone();
                            async move |cx| {
                                let deploy_result = cx
                                    .background_spawn({
                                        let host_args = host_args.clone();
                                        async move { ensure_remote_binary_deployed(&host_args, remote_kind, profile_os) }
                                    })
                                    .await;
                                match deploy_result {
                                    Ok(true) => {
                                        let message = format!(
                                            "Redeployed somsrv on the \"{profile_name}\" server — restart this tab to use it, or keep working as-is."
                                        );
                                        workspace
                                            .update(cx, |workspace, cx| {
                                                show_somsrv_toast(
                                                    workspace,
                                                    placeholder_item_id,
                                                    "redeployed",
                                                    workspace::notifications::NotificationSeverity::Info,
                                                    message,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                    }
                                    Ok(false) => {}
                                    Err(err) => {
                                        log::warn!("background somsrv deploy check failed for {host_args:?}: {err:#}");
                                        let message = human_readable_deploy_error(&profile_name, &err);
                                        workspace
                                            .update(cx, |workspace, cx| {
                                                show_somsrv_error(workspace, placeholder_item_id, message, cx);
                                            })
                                            .ok();
                                    }
                                }
                            }
                        })
                        .detach();
                    }
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

    /// Shown when a `Ctrl+Shift+N` binding targets a `tabs[]` slot that
    /// settings.json doesn't actually have (see `som_config.rs`'s
    /// `apply_keys` and `workspace::NewTerminal::missing_profile_index`'s
    /// doc comment) — opens no tab at all rather than silently falling back
    /// to the default profile, which would be confusing (pressing "9" and
    /// getting profile 1 with no indication why).
    fn show_missing_profile_error(workspace: &mut Workspace, idx: usize, cx: &mut Context<Workspace>) {
        use workspace::notifications::{
            NotificationId, NotificationScope, NotificationSeverity, simple_message_notification::MessageNotification,
        };
        let tab_count = cx.try_global::<TabProfiles>().map(|p| p.0.len()).unwrap_or(0);
        let msg = format!(
            "Som: no profile #{idx} in settings.json's \"tabs\" — only {tab_count} profile(s) configured."
        );
        let id = NotificationId::Named(format!("som-missing-profile-{idx}").into());
        // Global (no tab exists to scope this to — the keybinding didn't
        // open one) + Warning (a missed keybinding, not a broken
        // application — doesn't get the window-border treatment reserved
        // for Global+Error).
        workspace.show_scoped_notification(id, NotificationScope::Global, NotificationSeverity::Warning, cx, move |cx| {
            let msg2 = msg.clone();
            let msg3 = msg.clone();
            cx.new(|cx| {
                MessageNotification::new(msg2, cx)
                    .severity(NotificationSeverity::Warning)
                    .primary_message("Copy")
                    .primary_on_click(move |_window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(msg3.clone()));
                    })
                    .show_suppress_button(false)
            })
        });
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
                Self::wrap_terminal_into_pane(workspace, placeholder_item_id, &terminal, tab_name, tab_icon, profile_index, window, cx)
            })?;
            Ok((item_id, terminal.downgrade()))
        })
    }

    /// The second half of `replace_center_terminal_named` — wraps an
    /// ALREADY-CREATED `Entity<Terminal>` into a `TerminalView` and swaps
    /// it into `placeholder_item_id`'s position. Split out so `open_srp_
    /// or_plain_ssh` can create the `Entity<Terminal>` itself first,
    /// subscribe to its events BEFORE any `TerminalView` exists (closing
    /// the race window where an instant `Event::SpawnFailed` could fire
    /// before anything is listening), and only wrap it into a real pane
    /// item once it's known whether to keep this one or fall back to a
    /// freshly-created plain-SSH terminal instead.
    fn wrap_terminal_into_pane(
        workspace: &mut Workspace,
        placeholder_item_id: gpui::EntityId,
        terminal: &Entity<Terminal>,
        tab_name: Option<String>,
        tab_icon: Option<String>,
        profile_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> gpui::EntityId {
        let terminal_view = cx.new(|cx| {
            TerminalView::new_with_title_and_icon(
                terminal.clone(),
                workspace.weak_handle(),
                workspace.database_id(),
                workspace.project().downgrade(),
                tab_name,
                tab_icon,
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
    }

    /// Opens an SSH/WSL `tmux`/`srp`/`lua` profile's tab, trying the real
    /// SRP-wrapped connection FIRST and transparently falling back to a
    /// plain SSH shell if it doesn't come up within 5 seconds — see this
    /// session's plan doc (`Som-srv deploy/SRP-connect redesign`) for the
    /// full policy this implements (2026-09-15): Som never pre-flights
    /// whether `somsrv` exists on the remote host before trying; it just
    /// tries the real thing and reacts to what actually happens.
    ///
    /// The SRP attempt's own `Entity<Terminal>` is created and subscribed
    /// to FIRST, before it's wrapped into any visible `TerminalView` —
    /// this closes the race window where an instant spawn failure (`ssh`
    /// connecting, trying to exec a `somsrv` that doesn't exist on the
    /// remote host, and exiting within milliseconds) could fire before
    /// anything is listening for it. `Terminal::register_task_finished`
    /// already emits `Event::SpawnFailed` for exactly this shape (a
    /// nonzero exit before any keyboard input — see that function's own
    /// doc comment in `crates/terminal/src/terminal.rs`) and `Event::
    /// CloseTerminal` for a clean-but-early exit; either one arriving
    /// within the 5s window is treated as "SRP didn't come up," anything
    /// else (including the timeout itself elapsing with the terminal
    /// still alive) is treated as success — a working SRP session simply
    /// never emits either of those events this early.
    ///
    /// On success, this returns the SRP terminal's own `(EntityId,
    /// pane_id)`- callers should record `pane_id` via `set_tmux_sessions_
    /// for_item`. On fallback, `wrap_terminal_into_pane` is called a
    /// SECOND time with a freshly-created plain-SSH terminal, replacing
    /// the (still technically live, if oddly behaving) SRP attempt's own
    /// item at the same tab position — no error toast for this path, this
    /// is expected, ordinary first-connection behavior. Returns `None`
    /// for `pane_id` in the fallback case, since a plain shell has no
    /// tmux session to record.
    async fn open_srp_or_plain_ssh(
        workspace: WeakEntity<Workspace>,
        window_handle: gpui::AnyWindowHandle,
        cx: &mut gpui::AsyncApp,
        placeholder_item_id: gpui::EntityId,
        profile: workspace::TabProfile,
        pane_id: String,
        cwd: Option<PathBuf>,
        cursor_shape: CursorShape,
        scrollback: Option<usize>,
        cell_pixel_size: Option<(u16, u16)>,
        tab_name: Option<String>,
        tab_icon: Option<String>,
        profile_index: Option<usize>,
    ) -> anyhow::Result<(gpui::EntityId, Option<String>)> {
        let (program, args) = tmux_wrapped_shell(&profile, &pane_id, cursor_shape, scrollback, cell_pixel_size)?;

        let project = window_handle.update(cx, |_, _, cx| {
            workspace.upgrade().map(|workspace| workspace.read(cx).project().downgrade())
        })?.ok_or_else(|| anyhow::anyhow!("workspace gone before SRP connect could start"))?;
        let cwd_for_srp = cwd.clone();
        let terminal = project
            .update(cx, |project, cx| {
                project.create_terminal_with_program_and_args(cwd_for_srp, program, args, cx)
            })?
            .await?;

        // Subscribed BEFORE this terminal is wrapped into any visible
        // `TerminalView` — see this function's own doc comment.
        let (failure_tx, failure_rx) = futures::channel::oneshot::channel::<terminal::Event>();
        let failure_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(failure_tx)));
        let subscription = window_handle.update(cx, |_, _, cx| {
            cx.subscribe(&terminal, move |_terminal, event, _cx| {
                if matches!(event, terminal::Event::SpawnFailed(_) | terminal::Event::CloseTerminal)
                    && let Some(tx) = failure_tx.borrow_mut().take()
                {
                    let _ = tx.send(event.clone());
                }
            })
        })?;

        let srp_came_up = match gpui::FutureExt::with_timeout(failure_rx, std::time::Duration::from_secs(5), cx.background_executor()).await {
            Ok(Ok(early_event)) => {
                log::debug!("SRP connect for pane {pane_id:?} failed early: {early_event:?}");
                false
            }
            Ok(Err(_)) => true, // sender dropped without firing — subscription outlived the terminal cleanly
            Err(gpui::Timeout) => true, // 5s elapsed with no early failure signal
        };
        drop(subscription);

        if srp_came_up {
            let item_id = window_handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    Self::wrap_terminal_into_pane(workspace, placeholder_item_id, &terminal, tab_name, tab_icon, profile_index, window, cx)
                })
            })??;
            return Ok((item_id, Some(pane_id)));
        }

        log::info!("SRP connect for pane {pane_id:?} did not come up within 5s, falling back to plain SSH");
        let (plain_program, plain_args) = plain_ssh_shell(&profile)?;
        let plain_terminal = project
            .update(cx, |project, cx| {
                project.create_terminal_with_program_and_args(cwd, plain_program, plain_args, cx)
            })?
            .await?;
        let item_id = window_handle.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                Self::wrap_terminal_into_pane(workspace, placeholder_item_id, &plain_terminal, tab_name, tab_icon, profile_index, window, cx)
            })
        })??;
        Ok((item_id, None))
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
            let placeholders: Vec<(usize, workspace::som_db::SomDbTab, Entity<PendingTerminalTab>)> = window_handle
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
                                let placeholder_for_list = placeholder.clone();
                                workspace.add_item_to_main_pane_at(
                                    Box::new(placeholder),
                                    Some(tab.profile_index),
                                    Some(db_index),
                                    window,
                                    cx,
                                );
                                Some((db_index, tab.clone(), placeholder_for_list))
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
            for (db_index, tab, placeholder) in placeholders {
                let placeholder_item_id = placeholder.entity_id();
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

                if wants_srp(&profile) {
                    // Splits aren't supported for tmux tabs yet, so there's
                    // only ever the one pane_id to restore, never extras —
                    // see `set_tmux_sessions_for_item`'s doc comment (still
                    // named for the OLD session_id-based design, now
                    // repurposed to store the pane_id used as this pane's
                    // `somsrv` pipe name — see `project_som_tmux`
                    // memory, "Обновление 17"/19).
                    let pane_id = tab
                        .tmux_sessions
                        .as_ref()
                        .and_then(|ids| ids.first())
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                    // The ENTIRE rest of this tab's setup — SRP connect
                    // attempt, deploy check, orphan cleanup, tmux_sessions
                    // bookkeeping — runs inside ONE `cx.spawn`, pushed into
                    // `tab_creations` immediately, so THIS tab's (possibly
                    // slow, real SSH round-trip) connect attempt can never
                    // block the `for` loop from moving on to the NEXT
                    // tab's restore.
                    let (remote_program, host_args) =
                        project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
                    let remote_kind = classify_remote(&remote_program);
                    let live_pane_ids = live_pane_ids.clone();
                    let workspace = workspace.clone();
                    let profile = profile.clone();
                    let cwd = cwd.clone();
                    let tab_profile_index = tab.profile_index;
                    let created = cx.spawn(async move |cx| {
                        let (cwd, cursor_shape, scrollback, cell_pixel_size) = window_handle
                            .update(cx, |_, _, cx| {
                                let settings = TerminalSettings::get_global(cx);
                                let cwd = cwd.clone().or_else(|| {
                                    workspace.upgrade().and_then(|workspace| default_working_directory(workspace.read(cx), cx))
                                });
                                (cwd, settings.cursor_shape, settings.max_scroll_history_lines, approximate_cell_pixel_size(settings))
                            })
                            .unwrap_or((cwd.clone(), CursorShape::default(), None, None));

                        let (item_id, srp_pane_id) = Self::open_srp_or_plain_ssh(
                            workspace.clone(),
                            window_handle,
                            cx,
                            placeholder_item_id,
                            profile.clone(),
                            pane_id,
                            cwd,
                            cursor_shape,
                            scrollback,
                            cell_pixel_size,
                            Some(profile.name.clone()),
                            profile.icon.clone(),
                            Some(tab_profile_index),
                        )
                        .await?;
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
                        // `somsrv` HOLDER survives Som closing
                        // (the whole point of the HOLDER/RELAY design —
                        // see `project_som_tmux` memory), but the tab
                        // that opens on the NEXT launch gets a
                        // brand-new `pane_id` instead of reattaching to
                        // that same still-alive HOLDER, since db.json
                        // never remembered which pane_id this tab was
                        // actually using. Only meaningful if this attempt
                        // actually came up via SRP — a plain-SSH fallback
                        // has no tmux session to record.
                        if let Some(pane_id) = srp_pane_id {
                            window_handle.update(cx, |_, _, cx| {
                                workspace.update(cx, |workspace, _cx| {
                                    workspace.set_tmux_sessions_for_item(item_id, vec![pane_id]);
                                })
                            })??;
                        }

                        // Background mtime-based deploy/redeploy check +
                        // orphan cleanup — both run in the background,
                        // detached, strictly AFTER this tab has already
                        // opened (SRP or fallback plain SSH). Neither
                        // blocks this tab's own open.
                        if let RemoteKind::Ssh | RemoteKind::Wsl = remote_kind {
                            let host_args = host_args.clone();
                            let live_pane_ids = live_pane_ids.clone();
                            let profile_name = profile.name.clone();
                            let profile_os = profile.os;
                            cx.spawn({
                                let workspace = workspace.clone();
                                async move |cx| {
                                    let deploy_result = cx
                                        .background_spawn({
                                            let host_args = host_args.clone();
                                            async move {
                                                let deploy_result = ensure_remote_binary_deployed(&host_args, remote_kind, profile_os);
                                                kill_orphaned_holders(&host_args, remote_kind, &live_pane_ids);
                                                deploy_result
                                            }
                                        })
                                        .await;
                                    match deploy_result {
                                        Ok(true) => {
                                            let message = format!(
                                                "Redeployed somsrv on the \"{profile_name}\" server — restart this tab to use it, or keep working as-is."
                                            );
                                            window_handle
                                                .update(cx, |_, _, cx| {
                                                    workspace.update(cx, |workspace, cx| {
                                                        show_somsrv_toast(
                                                            workspace,
                                                            placeholder_item_id,
                                                            "redeployed",
                                                            workspace::notifications::NotificationSeverity::Info,
                                                            message,
                                                            cx,
                                                        );
                                                    })
                                                })
                                                .ok();
                                        }
                                        Ok(false) => {}
                                        Err(err) => {
                                            log::warn!("background somsrv deploy check failed for {host_args:?}: {err:#}");
                                            let message = human_readable_deploy_error(&profile_name, &err);
                                            window_handle
                                                .update(cx, |_, _, cx| {
                                                    workspace.update(cx, |workspace, cx| {
                                                        show_somsrv_error(workspace, placeholder_item_id, message, cx);
                                                    })
                                                })
                                                .ok();
                                        }
                                    }
                                }
                            })
                            .detach();
                        }
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
            // Move the tab db.json marked active to the end of the queue.
            // Phase 2 below activates each tab it splits (a real, visible
            // focus change — see the comment above that loop), so whichever
            // tab is processed LAST is what's on screen once Phase 2 ends.
            // Without this, an earlier-in-db-order tab that also has splits
            // would flash into view after the active tab's real content
            // already landed, only for the final correction block further
            // down to snap focus back — a visible wrong-tab flicker on every
            // restore where a non-active tab happens to have splits too.
            if let Some(active_pos) = tabs_in_db_order
                .iter()
                .position(|(db_index, _, _)| *db_index == db_state.active_tab)
            {
                let active_entry = tabs_in_db_order.remove(active_pos);
                tabs_in_db_order.push(active_entry);
            }
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

            // `window.focus()` only sets GPUI's own logical focus target — it
            // does NOT make the OS actually deliver keyboard input to this
            // window. That's a separate, independently-async path: real OS
            // activation (`PlatformWindow::activate`, see
            // `gpui_windows::window::WindowsWindow::activate`) spawns its own
            // background task (`SetActiveWindow`/`SetFocus`, a synthetic-
            // keypress workaround for a real Windows quirk, THEN
            // `SetForegroundWindow` last) that races against this restore
            // task with no ordering guarantee between them. Calling
            // `som_focus_pane_by_index` before the OS has actually finished
            // activating the window is a real, intermittently-reproducing
            // bug (confirmed live multiple times: "Som window opens with a
            // tab but no input focus, not always") — GPUI ends up with the
            // right element logically focused, but the OS never routed real
            // keystrokes there because activation hadn't landed yet.
            //
            // Poll `window.is_window_foreground()` (NOT `is_window_active()`)
            // — `is_window_active` reflects `WM_ACTIVATE`, which Windows can
            // deliver as soon as `SetActiveWindow` runs, well before the
            // LATER `SetForegroundWindow` call in `activate()` actually wins
            // the real foreground/input-focus race against other processes.
            // A poll on `is_window_active` was confirmed live to still miss
            // the bug (focus lost again after this fix first shipped) —
            // `is_window_foreground` checks `GetForegroundWindow() == hwnd`
            // directly, which is what `SetForegroundWindow` itself settles.
            // If activation never lands within the timeout (e.g. Som started
            // minimized/in the background on purpose), fall through and
            // focus anyway — better than never focusing at all.
            for _ in 0..50 {
                let foreground =
                    window_handle.update(cx, |_, window, _cx| window.is_window_foreground()).unwrap_or(false);
                if foreground {
                    break;
                }
                cx.background_executor().timer(std::time::Duration::from_millis(5)).await;
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
/// (Som's own PTY child becomes `somsrv.exe` acting as the RELAY),
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

/// A ROUGH estimate of the terminal's real font cell size in pixels, from
/// `TerminalSettings` alone — NOT the exact figure `TerminalElement`
/// actually renders with (that only exists after a real GPUI layout pass,
/// which hasn't happened yet at the point a `tmux: true` tab is being
/// created). Used to seed a `tmux: true` local RELAY's `--cell-pixel-size`
/// flag (see `wrap_command_args`) — good enough to stop a HOLDER's `\x1b
/// [16t` ("report cell pixel size") answer from being the previous
/// hardcoded `1x1` (which made `yazi`'s own image-scaling logic downscale
/// every image to a handful of pixels before Som ever saw it — see
/// `somsrv::bounds::SessionBounds::cell_width`'s doc comment for the
/// full history), without the complexity/regression risk of threading a
/// live value through from the real render (that approach was tried and
/// reverted — see `project_som_tmux` memory).
///
/// `cell_height` uses `TerminalLineHeight::value()` (the exact multiplier
/// `TerminalElement` itself applies to font size). `cell_width` uses the
/// standard monospace advance-width heuristic (~0.6× the font's em size) —
/// no real text-shaping happens here, so this is necessarily approximate;
/// it exists to get `yazi`'s downscale target roughly right, not to be
/// pixel-perfect.
fn approximate_cell_pixel_size(settings: &TerminalSettings) -> Option<(u16, u16)> {
    let font_size = settings.font_size.unwrap_or(gpui::px(14.));
    let font_size = f32::from(font_size);
    let line_height = font_size * settings.line_height.value();
    let cell_width = font_size * 0.6;
    if line_height <= 0. || cell_width <= 0. {
        return None;
    }
    Some((cell_width.round() as u16, line_height.round() as u16))
}

/// Serializes Som's own `CursorShape` setting into the plain string
/// `somsrv` parses back out (`crate::session::parse_cursor_shape`
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

/// Substitutes a `somsrv`-wrapped command in for a `tmux: true`
/// profile's own shell — see `project_som_tmux` memory ("Обновление 16"-19)
/// for the full design. Som's own terminal creation path never learns
/// anything happened; it just gets a different program/args than the
/// profile's `shell` setting says, exactly the way `action.shell`/a
/// user-typed shell override already works for any other profile.
///
/// `pane_id` is generated by the CALLER (fresh `Uuid::new_v4()` for a new
/// tab, or a saved one from db.json for restore) — the daemon never
/// invents one; see `RelayInput::Register`'s doc comment
/// (`somsrv::protocol`) for how this identifies a session in the shared
/// daemon's registry.
///
/// `cursor_shape`/`scrollback` mirror Som's own `TerminalSettings` — passed
/// through explicitly to the server (which owns the actual `Term` these
/// configure now) rather than left for it to guess/default; see
/// `cursor_shape_arg`'s doc comment.
///
/// `cell_pixel_size` is `RemoteKind::Local`-only (see `wrap_command_args`'s
/// doc comment for why) — silently ignored for `Ssh`/`Wsl` since the HOLDER
/// there runs on a different machine, rendering nothing itself, where
/// Som's own local font metrics have no meaning.
/// Whether this profile wants an SRP-wrapped remote connection at all —
/// `tmux`/`srp`/`lua` are three independent feature flags (session
/// persistence, rich content, Lua scripting respectively) that all ride
/// on the SAME underlying SRP connection attempt, so any one of them
/// being set is enough to try it (2026-09-15).
fn wants_srp(profile: &workspace::TabProfile) -> bool {
    profile.tmux || profile.srp || profile.lua
}

/// Builds the SRP-wrapped `(program, args)` for an SSH/WSL profile that
/// wants SRP (`wants_srp` is `true`) — see `open_srp_or_plain_ssh`'s doc
/// comment for how the caller actually decides whether the resulting
/// connection succeeded or needs falling back to plain SSH; this
/// function only builds the command, it never probes anything first
/// (2026-09-15 redesign: no more pre-flight `remote_somsrv_exists`
/// check — SRP either comes up on this exact connection or it doesn't,
/// discovered live). Ignored for `RemoteKind::Local`, which always has
/// its own `somsrv_binary_path()` binary available (it ships inside Som
/// itself, nothing to deploy).
fn tmux_wrapped_shell(
    profile: &workspace::TabProfile,
    pane_id: &str,
    cursor_shape: CursorShape,
    scrollback: Option<usize>,
    cell_pixel_size: Option<(u16, u16)>,
) -> anyhow::Result<(String, Vec<String>)> {
    let (program, args) = project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
    match classify_remote(&program) {
        RemoteKind::Local => {
            let server_path = somsrv_binary_path()?;
            let wrapped_args =
                wrap_command_args(&profile.name, pane_id, program, args, cursor_shape, scrollback, cell_pixel_size);
            Ok((server_path.to_string_lossy().to_string(), wrapped_args))
        }
        kind @ (RemoteKind::Ssh | RemoteKind::Wsl) => {
            // Same MSYS-vs-real-OpenSSH `PATH` ambiguity as `run_remote_
            // command`/`scp_to_remote` — see `windows_openssh_binary`'s
            // own doc comment. This is the interactive PTY process
            // itself (not a one-shot probe/scp), so it matters just as
            // much here: an `ssh` resolved to Git for Windows' MSYS copy
            // would rewrite the remote-side `somsrv` invocation's own
            // POSIX-looking arguments the exact same way it rewrote the
            // deploy check's `chmod` path, before this fix.
            let program = if matches!(kind, RemoteKind::Ssh) {
                windows_openssh_binary(&program).to_string_lossy().into_owned()
            } else {
                program
            };
            if wants_srp(profile) {
                let wrapped_args = wrap_remote_command_args(&profile.name, pane_id, args, cursor_shape, scrollback, kind);
                Ok((program, wrapped_args))
            } else {
                // Plain `ssh host` (or `wsl [flags] --`) with NO explicit
                // remote command — sshd starts this client's own real
                // login shell directly (no `somsrv`/SRP involved at
                // all). `args` here is just `host_args` (`ssh`'s own
                // flags/hostname, or `wsl`'s own flags) since `parse_
                // shell_command` never had a `somsrv` wrapper to strip
                // in the first place for a profile that names a plain
                // shell.
                Ok((program, args))
            }
        }
    }
}

/// Same shape as `tmux_wrapped_shell`, but always builds the plain
/// (non-SRP) command regardless of `wants_srp(profile)` — used by
/// `open_srp_or_plain_ssh`'s fallback path once an SRP attempt has
/// already timed out or failed, where the caller needs the plain form
/// specifically rather than whatever the profile's own flags would
/// otherwise pick.
fn plain_ssh_shell(profile: &workspace::TabProfile) -> anyhow::Result<(String, Vec<String>)> {
    let (program, args) = project::terminals::parse_shell_command(profile.shell.as_deref().unwrap_or(""));
    match classify_remote(&program) {
        RemoteKind::Local => anyhow::bail!("plain_ssh_shell is only meaningful for SSH/WSL profiles"),
        kind @ (RemoteKind::Ssh | RemoteKind::Wsl) => {
            let program = if matches!(kind, RemoteKind::Ssh) {
                windows_openssh_binary(&program).to_string_lossy().into_owned()
            } else {
                program
            };
            Ok((program, args))
        }
    }
}

/// The actual argv construction, split out from `tmux_wrapped_shell` so it
/// can be unit-tested without touching the filesystem (`somsrv_
/// binary_path` looks up a real file next to `current_exe()`, which under
/// `cargo test` resolves to `target/debug/deps/`, not `target/debug/` —
/// same constraint other tests in this codebase have hit).
///
/// `--cursor-shape`/`--scrollback` are appended AFTER the program's own args
/// rather than before the positional `profile`/`pane-id`/`program` — order
/// doesn't matter to `somsrv`'s own arg parser (each flag consumes
/// its value via `iter.next()` regardless of what's already been seen), so
/// putting them last avoids having to touch the positional-fill logic at all.
fn wrap_command_args(
    profile_name: &str,
    pane_id: &str,
    program: String,
    args: Vec<String>,
    cursor_shape: CursorShape,
    scrollback: Option<usize>,
    cell_pixel_size: Option<(u16, u16)>,
) -> Vec<String> {
    let mut wrapped_args = vec![profile_name.to_string(), pane_id.to_string(), program];
    wrapped_args.extend(args);
    wrapped_args.push("--cursor-shape".to_string());
    wrapped_args.push(cursor_shape_arg(cursor_shape).to_string());
    if let Some(scrollback) = scrollback {
        wrapped_args.push("--scrollback".to_string());
        wrapped_args.push(scrollback.to_string());
    }
    if let Some((cell_width, cell_height)) = cell_pixel_size {
        wrapped_args.push("--cell-pixel-size".to_string());
        wrapped_args.push(format!("{cell_width};{cell_height}"));
    }
    wrapped_args
}

/// Appends the REMOTE-side `somsrv` invocation after an `ssh`/`wsl`
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
/// (see `somsrv::protocol::ssh_client_ip`'s doc comment), read entirely
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
    // <explicit command>` gives the remote `somsrv` RELAY plain pipes
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
    wrapped_args.push("~/.local/bin/somsrv".to_string());
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
    // (`somsrv`'s own arg parser treats everything after `--` as extra
    // args for the spawned program, see `main.rs`'s `Args` doc comment).
    // A plain (non-tmux) SSH profile gets this for free — Som runs a bare
    // `ssh host` with no explicit command, which makes sshd start a REAL
    // login shell itself (that's also what prints the MOTD banner). A
    // `tmux: true` profile's `ssh host ~/.local/bin/somsrv ...` is an
    // EXPLICIT remote command, which sshd never treats as a login session
    // for — so `$SHELL` was starting as a plain non-login shell, silently
    // skipping `.bash_profile`/`.profile` (only `.bashrc` still ran) and
    // never printing the MOTD a real login shell would. `-l` mirrors
    // what the ordinary profile gets automatically, restoring both.
    wrapped_args.push("--".to_string());
    wrapped_args.push("-l".to_string());
    wrapped_args
}

/// Detects whether `shell` is a `somsrv`-wrapped command (built by
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
            if !program.contains("somsrv") || args.len() < 2 {
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
            // wrap_remote_command_args: [...host_args, "~/.local/bin/somsrv", profile, pane_id, "$SHELL", ...]
            let server_pos = args.iter().position(|a| a.contains("somsrv"))?;
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
/// this gives `["192.168.50.5", "~/.local/bin/somsrv", "--version"]`,
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

/// Tears down every session the daemon on the far end of an SSH `tmux:
/// true` profile is holding for THIS SAME client machine (matched by
/// `client_id`, mirroring `somsrv::protocol::ssh_client_id`'s
/// `<user>@<ip>` shape) whose `pane_id` isn't in `live_pane_ids` — i.e. a
/// session for a pane that isn't (or is no longer) in THIS client's
/// `db.json` at all, so nothing running on this machine will EVER try to
/// reattach to it again. Called once per host during `restore_som_tabs`,
/// right alongside the existing deploy-check SSH round-trip (piggybacking
/// on that same "we're already paying for one SSH connection to this host
/// anyway" moment rather than adding a second one).
///
/// Uses `somsrv::admin`'s `--list-sessions`/`--kill-session` CLI
/// subcommands (run over SSH via `run_remote_command`, same as the
/// `--version` deploy-check probe) rather than a `ps`-grep — the OLD
/// per-pane-HOLDER architecture had one OS process per session, with
/// `--client-id`/`--pane-id` visible on ITS OWN command line for a `ps`
/// probe to grep; the shared daemon has exactly one process for every
/// session on the host, so session identity now lives in ITS registry,
/// not in any process's argv. Asking the daemon directly (via its own
/// admin protocol) is also simply more precise than a `ps`-grep ever was.
///
/// Deliberately keyed on `db.json` membership, not "how long has this
/// session been idle" or "is its shell process busy" — those need per-pane
/// state this function has no access to and, more importantly, would be
/// WRONG for a real live pane that's simply not the active tab right now (a
/// background tab's session is idle but absolutely not an orphan). A
/// pane_id db.json doesn't know about at all, by contrast, can never be
/// reattached to by anything — the ONLY way Som ever learns a pane_id to
/// reattach to is by reading it back out of db.json in the first place
/// (see `restore_som_tabs`'s own pane_id lookup) or by a currently-open tab
/// that already put it there, both of which `live_pane_ids` already
/// covers.
///
/// Scoping to `client_id` matters whenever more than one Som installation
/// SSHes into the same remote host (e.g. a Windows machine AND a Mac both
/// connect to `deb`) — a session the OTHER machine created is invisible to
/// THIS machine's `db.json` and would look orphaned by pane_id alone, even
/// though it's perfectly live from that other machine's point of view.
///
/// Best-effort: a failure here (host unreachable, `somsrv` too old to
/// understand `--list-sessions`) is logged and swallowed rather than
/// propagated — this is housekeeping, not something that should block a
/// tab from restoring.
fn kill_orphaned_holders(host_args: &[String], remote_kind: RemoteKind, live_pane_ids: &[String]) {
    // WSL has no long-lived cross-restart daemon session of its own the way
    // an SSH profile's remote host does (see `wrap_remote_command_args`'s
    // `-tt` doc comment) — nothing to clean up there.
    if !matches!(remote_kind, RemoteKind::Ssh) {
        return;
    }
    let Some(this_client_id) = read_this_client_id(host_args, remote_kind) else {
        return;
    };
    let this_client_id = this_client_id.as_str();

    let list_script = format!("~/.local/bin/somsrv --list-sessions {}", shell_quote(this_client_id));
    let quoted_list_script = shell_quote(&list_script);
    let list_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_list_script]);
    let sessions_json = match run_remote_command(remote_kind, &list_probe) {
        Ok(output) => output,
        Err(err) => {
            log::warn!("failed to list remote somsrv sessions for orphan cleanup, skipping: {err:#}");
            return;
        }
    };
    let sessions: Vec<somsrv::protocol::SessionInfo> = match serde_json::from_str(sessions_json.trim()) {
        Ok(sessions) => sessions,
        Err(err) => {
            log::warn!("failed to parse somsrv --list-sessions output, skipping: {err:#} (output: {sessions_json:?})");
            return;
        }
    };

    let orphaned_pane_ids = orphaned_pane_ids(&sessions, live_pane_ids);
    if orphaned_pane_ids.is_empty() {
        return;
    }
    log::info!("killing {} orphaned somsrv session(s) not in db.json: {orphaned_pane_ids:?}", orphaned_pane_ids.len());
    for pane_id in orphaned_pane_ids {
        let kill_script = format!("~/.local/bin/somsrv --kill-session {} {}", shell_quote(this_client_id), shell_quote(pane_id));
        let quoted_kill_script = shell_quote(&kill_script);
        let kill_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_kill_script]);
        if let Err(err) = run_remote_command(remote_kind, &kill_probe) {
            log::warn!("failed to kill orphaned somsrv session {pane_id:?}: {err:#}");
        }
    }
}

/// Filters `sessions` (already scoped to `this_client_id` by
/// `SrvRequest::ListSessions` itself) down to the `pane_id`s NOT present
/// in `live_pane_ids`. Pulled out of `kill_orphaned_holders` itself so
/// this filtering logic can be unit-tested without an actual SSH
/// round-trip.
fn orphaned_pane_ids<'a>(sessions: &'a [somsrv::protocol::SessionInfo], live_pane_ids: &[String]) -> Vec<&'a str> {
    sessions
        .iter()
        .map(|session| session.pane_id.as_str())
        .filter(|pane_id| !live_pane_ids.iter().any(|id| id == pane_id))
        .collect()
}

/// Shows a `somsrv` deploy/setup failure as a toast, bottom-right,
/// scoped to ONE tab — visible only while `tab_item_id` is the active
/// item, disappearing/reappearing as the user switches away from/back to
/// the broken tab, with a "Copy" button (full technical detail) and a
/// close (×) button, same shape as every other `MessageNotification` in
/// Som — NOT rendered as text baked into the tab's own body (an earlier
/// version of this fix did that; explicitly corrected per user feedback,
/// 2026-09-15: "все ошибки должны быть тостерами... с закрытием тостера
/// через крестик"). A `NotificationId` unique per tab so two different
/// broken profiles' errors don't clobber each other. See
/// `workspace::notifications::NotificationScope`'s own doc comment for
/// the full scoping rationale.
/// Turns `ensure_remote_binary_deployed`'s `anyhow::Error` chain (raw
/// `ssh`/`scp`/`chmod` process output — e.g. `ssh exited with Some(1):
/// chmod: cannot access '...': No such file or directory`) into one
/// plain-language sentence a non-Rust-developer user can actually act
/// on, instead of surfacing that technical chain verbatim. Pattern-
/// matches on the handful of failure shapes `ensure_remote_binary_
/// deployed`/`scp_to_remote`/`run_remote_command` actually produce
/// (confirmed by reading those functions directly, not guessed) rather
/// than attempting to cover every conceivable `ssh`/`scp` failure —
/// falls back to a generic "couldn't reach/set up the server" sentence
/// (with the raw detail still available via the notification's own
/// "Copy" button) for anything that doesn't match a known shape, so an
/// unrecognized error is never silently swallowed, just less specifically
/// worded.
fn human_readable_deploy_error(profile_name: &str, err: &anyhow::Error) -> String {
    let detail = format!("{err:#}");
    let reason = if detail.contains("No such file or directory") {
        "the somsrv program wasn't found on the server after copying it there"
    } else if detail.contains("Permission denied") {
        "the server refused permission while setting up somsrv"
    } else if detail.contains("no embedded somsrv binary for") {
        "Som doesn't have a somsrv build for that server's platform"
    } else if detail.contains("failed to spawn") {
        "couldn't even start ssh/scp — check that they're installed and on your PATH"
    } else if detail.contains("scp exited with") {
        "copying somsrv to the server failed"
    } else {
        "couldn't reach or set up the server"
    };
    format!("Couldn't set up the \"{profile_name}\" tab: {reason}.\n\nDetails:\n{detail}")
}

fn show_somsrv_error(workspace: &mut Workspace, tab_item_id: gpui::EntityId, message: String, cx: &mut Context<Workspace>) {
    use workspace::notifications::NotificationSeverity;
    show_somsrv_toast(workspace, tab_item_id, "error", NotificationSeverity::Error, message, cx);
}

/// Same toast shape as `show_somsrv_error` (Tab-scoped, "Copy" + close
/// button, per-tab `NotificationId` so it doesn't clobber a different
/// tab's toast), but for the background deploy check's SUCCESS case — see
/// `ensure_remote_binary_deployed`'s call sites (2026-09-15): a tab opens
/// immediately without waiting on this check at all, so its result (a
/// redeploy happened, or nothing needed doing) has nowhere else to
/// surface. `id_kind` (e.g. `"error"`/`"redeployed"`) keeps the two
/// notification IDs distinct per tab so a later error doesn't silently
/// replace an earlier success toast still on screen, or vice versa.
fn show_somsrv_toast(
    workspace: &mut Workspace,
    tab_item_id: gpui::EntityId,
    id_kind: &str,
    severity: workspace::notifications::NotificationSeverity,
    message: String,
    cx: &mut Context<Workspace>,
) {
    use workspace::notifications::{NotificationId, NotificationScope, simple_message_notification::MessageNotification};

    let message = format!("Som: {message}");
    let id = NotificationId::Named(format!("somsrv-deploy-{id_kind}-{}", tab_item_id.as_u64()).into());
    workspace.show_scoped_notification(id, NotificationScope::Tab(tab_item_id), severity, cx, move |cx| {
        let message2 = message.clone();
        let message3 = message.clone();
        cx.new(|cx| {
            MessageNotification::new(message2, cx)
                .severity(severity)
                .primary_message("Copy")
                .primary_on_click(move |_window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(message3.clone()));
                })
                .show_suppress_button(false)
        })
    });
}

/// Ensures `~/.local/bin/somsrv` on the far side of an `ssh`/`wsl`
/// tmux profile is present and matches THIS Som build's version — see
/// `project_som_tmux` memory for the full policy this implements. Runs
/// entirely on a background thread (blocking `ssh`/`wsl`/`scp` child
/// processes) — callers must not call this from GPUI's main thread.
///
/// Deploy mechanism is `scp` of a PRE-BUILT binary from `~/.config/som/
/// srv/{platform}/somsrv` (see `somsrv::protocol::platform_binaries_
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
/// UNLIKE the old approach, this never overwrites the LIVE `somsrv`
/// binary in place — `cp`/`scp`'s destination-truncate semantics fail
/// outright against a running executable (`ETXTBSY`, confirmed by direct
/// reproduction — ordinary Unix "safe to replace a running executable"
/// semantics only hold for an atomic rename onto a NEW inode, not an
/// in-place truncate-and-rewrite). Redesigned (2026-09-15) so Som's side
/// does no killing of any process at all: this function only `scp`s the
/// new binary to a SEPARATE path (`~/.local/bin/somsrv.new`) and drops a
/// marker file once it's fully in place — `somsrv` itself notices that
/// marker on its NEXT client connection and handles the entire cutover
/// (closing every live connection, renaming `.new` over its own running
/// binary — which Unix allows even while it's executing, since a
/// filename is just a directory entry pointing at an inode, not the
/// inode itself — deleting the marker, and restarting itself) — see
/// `crate::server::check_and_apply_pending_redeploy` on the `somsrv`
/// side for that half. Every live pane on every client currently
/// attached to that host (this account's own tabs AND, if sharing an
/// account across machines, any other client's) loses its connection and
/// reconnects to a brand new daemon on next use — an accepted, explicit
/// tradeoff (long-running remote sessions are exactly what tmux:true
/// exists to preserve across a LOCAL Som restart, but a version bump is
/// disruptive by nature here). An earlier version of this had Som itself
/// `kill -9` every `somsrv` process on the host before `scp`ing over the
/// live binary path directly — reverted the same day in favor of this
/// design specifically so Som's side never needs to decide when it's
/// "safe" to kill anything; `somsrv` alone knows when it's actually
/// between requests and safe to tear itself down.
/// Per-host mutexes so two tabs pointed at the SAME remote host never run
/// `ensure_remote_binary_deployed` concurrently — each entry is keyed on
/// `host_args.join(" ")` (the exact `ssh`/`wsl` argv, e.g. `"usa"`), so
/// different hosts still deploy fully in parallel. Without this, two tabs
/// restored from `db.json` for the same host (or a new tab opened while
/// another to the same host is still connecting) both independently see
/// "version mismatch", both kill the remote's `somsrv` processes, and
/// both `scp` the new binary to the SAME destination path AT THE SAME
/// TIME — `scp` is not atomic against a concurrent second `scp` to the
/// same destination, so this could in principle corrupt the resulting
/// file, and even when it doesn't, it needlessly doubles the wall-clock
/// cost of every affected tab's open (confirmed live: two `usa` tabs
/// open together both logged their own "redeploying" and "killing N
/// somsrv process(es)" lines seconds apart, race-restarting each
/// other's redeploy). A plain `Mutex` (not `RwLock`) is correct here —
/// every caller needs EXCLUSIVE access for the whole deploy-check-then-
/// maybe-redeploy sequence, there's no read-only variant of this
/// function to share.
static DEPLOY_LOCKS: std::sync::Mutex<Option<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>> =
    std::sync::Mutex::new(None);

fn deploy_lock_for_host(host_args: &[String]) -> std::sync::Arc<std::sync::Mutex<()>> {
    let key = host_args.join(" ");
    let mut locks = DEPLOY_LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .get_or_insert_with(std::collections::HashMap::new)
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// Fast, single-round-trip check of whether `~/.local/bin/somsrv` exists
/// (and is executable) on the far side of an SSH/WSL `tmux: true` profile
/// — deliberately NOT a version check against the CURRENT build (that's
/// Returns `Ok(true)` when a redeploy actually happened (so callers can
/// surface a "redeployed, restart the tab to use it" toast), `Ok(false)`
/// when the remote was already current and nothing was done.
///
/// 2026-09-15 redesign: no `--version` handshake anywhere in this
/// function anymore, and no `uname` probing either — this is deliberately
/// as dumb as possible now, per explicit user direction. `os` comes
/// straight from the profile's own explicit `settings.json` field
/// (`workspace::RemoteOs` — no heuristics, no asking the remote what it
/// is), and "does this need deploying" is answered purely by comparing
/// file mtimes: the remote `~/.local/bin/somsrv`'s mtime (via a `stat`
/// probe, `None` if the file doesn't exist or the probe fails for any
/// reason) against the LOCAL embedded/cached copy's own mtime. If the
/// local copy is newer, copy it over; otherwise do nothing. This function
/// runs strictly in the background, AFTER a tab has already opened
/// (successfully via SRP or falled back to plain SSH) — see
/// `open_srp_or_plain_ssh`'s doc comment — so it never blocks anything a
/// user is looking at.
fn ensure_remote_binary_deployed(host_args: &[String], remote_kind: RemoteKind, os: workspace::RemoteOs) -> anyhow::Result<bool> {
    // Held for this entire function's body (see `deploy_lock_for_host`'s
    // doc comment) — a second concurrent call for the SAME host blocks
    // here until the first one finishes, then re-runs its OWN mtime probe
    // and almost certainly finds the first call already brought the host
    // up to date, so it returns immediately via the early `Ok(false)`
    // below instead of redeploying a second time.
    let host_lock = deploy_lock_for_host(host_args);
    let _guard = host_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // WSL is the same machine/architecture Som itself was just built for —
    // no separate pre-built platform binary to `scp` in, so it keeps the
    // original remote-build path (still cheap: WSL's own repo clone,
    // usually already warm from Som's own dev use, and no network hop).
    // Always rebuilds unconditionally when it runs — it never depended on
    // a version/mtime comparison to decide whether to run in the first
    // place, before or after this redesign.
    if let RemoteKind::Wsl = remote_kind {
        let deploy_script =
            "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p somsrv) && mkdir -p ~/.local/bin && cp target/release/somsrv ~/.local/bin/somsrv";
        let quoted_deploy_script = shell_quote(deploy_script);
        let deploy_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_deploy_script]);
        run_remote_command(remote_kind, &deploy_probe)?;
        return Ok(true);
    }

    let (os, arch) = remote_os_to_platform(os);

    // "Local mtime" is THIS running `som.exe`'s own file mtime — not some
    // separately-cached extracted copy. There is no on-disk cache of the
    // embedded `somsrv` at all anymore (2026-09-15 simplification): the
    // embedded bytes are read directly from `assets::Assets` (already
    // resident in this process's own memory, part of `som.exe` itself)
    // and written straight to a throwaway temp file only when an actual
    // upload is about to happen, deleted right after — see `embedded_
    // somsrv_bytes`'s own doc comment. Som's own build/release process
    // already keeps `assets/srv/{platform}/somsrv` in lockstep with
    // Som's own version (`scripts/update_somsrv_binaries.sh`), so `som.exe`'s
    // own mtime is a faithful proxy for "how new is the somsrv build
    // embedded inside me" without needing a separate cached file's mtime
    // at all.
    let local_mtime = std::env::current_exe()
        .and_then(|exe| std::fs::metadata(exe))
        .and_then(|metadata| metadata.modified())
        .context("failed to read this running som.exe's own mtime")?;

    let remote_mtime = remote_binary_mtime(host_args, remote_kind, os);
    log::debug!("deploy-check: {host_args:?} local_mtime={local_mtime:?} remote_mtime={remote_mtime:?}");

    let needs_deploy = match remote_mtime {
        Some(remote_mtime) => local_mtime > remote_mtime,
        None => true, // nothing there (or unreadable) — always deploy
    };
    if !needs_deploy {
        log::debug!("deploy-check: {host_args:?} remote somsrv is already at least as new as the local copy, skipping");
        return Ok(false);
    }

    let staged_binary = stage_embedded_somsrv_to_temp_file(os, arch)?;

    // `remote_mtime.is_none()` means nothing usable is at `~/.local/bin/
    // somsrv` at all (missing, or the `stat` probe itself failed) — there
    // is no running daemon to hand a careful cutover to, so the `.new` +
    // marker + `somsrvupd` dance below would never get applied (nothing
    // would ever connect over SRP to notice the marker in the first
    // place). Write directly to the final path instead — safe precisely
    // because there is no running process that could be executing it out
    // from under this write.
    if remote_mtime.is_none() {
        let result = (|| -> anyhow::Result<()> {
            // A genuinely fresh host has no `~/.local/bin` at all yet —
            // `scp` doesn't create intermediate directories on its own,
            // so without this the very first-ever deploy to a brand new
            // host fails outright.
            let mkdir_probe = wrap_remote_probe_args(host_args, "mkdir", &["-p", "~/.local/bin"]);
            run_remote_command(remote_kind, &mkdir_probe).with_context(|| format!("failed to create ~/.local/bin on {host_args:?}"))?;
            scp_to_remote(host_args, &staged_binary, "~/.local/bin/somsrv")
                .with_context(|| format!("failed to upload somsrv to {host_args:?}"))?;
            let chmod_probe = wrap_remote_probe_args(host_args, "chmod", &["+x", "~/.local/bin/somsrv"]);
            run_remote_command(remote_kind, &chmod_probe).with_context(|| format!("failed to chmod somsrv on {host_args:?}"))?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&staged_binary);
        result?;
        log::debug!("deploy-check: {host_args:?} first-ever deploy completed directly (no running daemon to hand off from)");
        return Ok(true);
    }

    // Uploads to `~/.local/bin/somsrv.new` — a BRAND NEW path, never the
    // live `somsrv` binary itself — so this never touches (and can never
    // hit `ETXTBSY` against) whatever's currently executing on the
    // remote host. No kill of any kind happens on Som's side: the actual
    // cutover — closing every live connection, replacing the running
    // binary with `.new`, and restarting itself — is entirely `somsrv`'s
    // OWN responsibility, triggered the next time ANY client connects and
    // finds the marker file below. See `crate::server`'s `check_and_
    // apply_pending_redeploy` for that side.
    let result = (|| -> anyhow::Result<()> {
        scp_to_remote(host_args, &staged_binary, "~/.local/bin/somsrv.new")
            .with_context(|| format!("failed to upload somsrv.new to {host_args:?}"))?;

        let chmod_probe = wrap_remote_probe_args(host_args, "chmod", &["+x", "~/.local/bin/somsrv.new"]);
        run_remote_command(remote_kind, &chmod_probe).with_context(|| format!("failed to chmod somsrv.new on {host_args:?}"))?;

        // The marker file itself — `somsrv`'s connection-accept loop
        // checks for exactly this path (`server::REDEPLOY_MARKER_NAME`)
        // before handling each new connection. Written LAST, only after
        // `.new` is fully in place and executable, so `somsrv` never
        // observes "marker present, but `.new` still mid-transfer or not
        // yet +x" — the marker being there is the one-and-only signal
        // that a complete, ready-to-apply redeploy is waiting.
        let touch_marker_script = "touch ~/.local/bin/somsrv.redeploy-pending";
        let quoted_touch_script = shell_quote(touch_marker_script);
        let touch_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_touch_script]);
        run_remote_command(remote_kind, &touch_probe).with_context(|| format!("failed to write the redeploy marker on {host_args:?}"))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&staged_binary);
    result?;

    log::debug!("deploy-check: {host_args:?} deploy staged successfully (somsrv will apply it on next connection)");
    Ok(true)
}

/// Writes Som's own embedded `somsrv` copy for `(os, arch)` (see
/// `crates/assets/src/assets.rs`'s `#[include = "srv/..."]` entries, kept
/// current by `scripts/update_somsrv_binaries.sh`, a manually-run, pre-
/// release step) to a fresh file under the OS temp directory — `scp`
/// needs a real path on disk, but there is no reason for that path to
/// persist any longer than the single upload that needs it (2026-09-15
/// simplification: this used to extract into a permanent `~/.config/som/
/// srv/{platform}/` cache, re-used across calls and compared by version;
/// now that redeploy decisions are purely mtime-based, keeping a
/// permanent cache around serves no purpose — every call that actually
/// needs to upload writes a fresh temp copy and the caller deletes it
/// right after). Callers are responsible for deleting the returned path
/// once the upload attempt (success or failure) is done.
fn stage_embedded_somsrv_to_temp_file(os: somsrv::protocol::Os, arch: somsrv::protocol::Arch) -> anyhow::Result<std::path::PathBuf> {
    let exe_suffix = if let somsrv::protocol::Os::Windows = os { ".exe" } else { "" };
    let asset_path = format!("srv/{}/somsrv{exe_suffix}", somsrv::protocol::platform_dir_name(os, arch));
    let bytes = assets::Assets
        .load(&asset_path)
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("no embedded somsrv binary for {os:?}/{arch:?} at {asset_path:?} — unsupported platform"))?;

    let temp_path = std::env::temp_dir().join(format!("somsrv-deploy-{}{exe_suffix}", std::process::id()));
    std::fs::write(&temp_path, bytes.as_ref()).with_context(|| format!("failed to write embedded somsrv to {temp_path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to make {temp_path:?} executable"))?;
    }
    Ok(temp_path)
}

/// Maps the profile's own explicit `RemoteOs` (from `settings.json`'s
/// `os` field — no heuristics, see that type's own doc comment) onto the
/// `(Os, Arch)` pair `ensure_embedded_binary_available` needs — this
/// codebase's three supported platform combos, `Lnx` always meaning
/// `linux-amd` (`linux-arm` stays permanently unsupported).
fn remote_os_to_platform(os: workspace::RemoteOs) -> (somsrv::protocol::Os, somsrv::protocol::Arch) {
    match os {
        workspace::RemoteOs::Win => (somsrv::protocol::Os::Windows, somsrv::protocol::Arch::Amd64),
        workspace::RemoteOs::Mac => (somsrv::protocol::Os::Darwin, somsrv::protocol::Arch::Arm64),
        workspace::RemoteOs::Lnx => (somsrv::protocol::Os::Linux, somsrv::protocol::Arch::Amd64),
    }
}

/// Probes the remote `~/.local/bin/somsrv`'s mtime as Unix epoch seconds
/// via `stat` — `None` if the file doesn't exist or the probe fails for
/// any other reason (treated identically to "nothing deployed yet" by
/// `ensure_remote_binary_deployed`). `stat`'s flags for "mtime as epoch
/// seconds" differ between GNU/Linux (`-c %Y`) and BSD/macOS (`-f %m`) —
/// picked from `os` (the profile's own explicit setting, not a guess).
/// Prefixed with a marker line (same technique `read_this_client_id`
/// uses) since a login shell can print unrelated profile-script noise
/// ahead of the real output.
fn remote_binary_mtime(host_args: &[String], remote_kind: RemoteKind, os: somsrv::protocol::Os) -> Option<std::time::SystemTime> {
    let stat_flag = match os {
        somsrv::protocol::Os::Darwin => "-f %m",
        somsrv::protocol::Os::Windows | somsrv::protocol::Os::Linux => "-c %Y",
    };
    let script = format!(r#"echo "SOM_MTIME:$(stat {stat_flag} ~/.local/bin/somsrv 2>/dev/null)""#);
    let quoted_script = shell_quote(&script);
    let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_script]);
    let output = run_remote_command(remote_kind, &probe).ok()?;
    let marker_line = output.lines().find_map(|line| line.strip_prefix("SOM_MTIME:"))?;
    let epoch_seconds: u64 = marker_line.trim().parse().ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_seconds))
}

/// Reproduces the EXACT `<user>@<ip>` shape `somsrv::protocol::
/// ssh_client_id` builds on the RELAY side — `whoami` + `$SSH_CLIENT`
/// together, over THIS SAME SSH connection, so sshd reports the exact
/// same source IP any OTHER invocation from this same client machine
/// (i.e. a RELAY registering a session) already got and passed down as
/// its own `client_id`. Used by `kill_orphaned_holders` to scope itself
/// to only THIS client's own sessions.
fn read_this_client_id(host_args: &[String], remote_kind: RemoteKind) -> Option<String> {
    // Prefixed with a marker rather than trusting this to be the FIRST
    // line of output — a login shell (`sh -lc`, i.e. `-l`) can run profile
    // scripts (`.bashrc`/`.profile`/version-manager init like `fnm`/`nvm`)
    // that print their OWN unrelated lines to stdout first (confirmed live
    // against a real `ssh localhost` WSL2 setup).
    let client_id_script = r#"echo "SOM_CLIENT_ID:$(whoami)@$SSH_CLIENT""#;
    let quoted_client_id_script = shell_quote(client_id_script);
    let client_id_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_client_id_script]);
    let client_id_output = match run_remote_command(remote_kind, &client_id_probe) {
        Ok(output) => output,
        Err(err) => {
            log::warn!("failed to read this connection's client-id: {err:#}");
            return None;
        }
    };
    let marker_line = client_id_output.lines().find(|line| line.starts_with("SOM_CLIENT_ID:"))?;
    let this_client_id = marker_line["SOM_CLIENT_ID:".len()..].split_whitespace().next()?;
    // An empty `$SSH_CLIENT` collapses to a bare `user@` (whoami
    // succeeded, sshd didn't set the var) — just as unable to safely
    // identify this client's sessions as no `$SSH_CLIENT` at all.
    if this_client_id.ends_with('@') {
        return None;
    }
    Some(this_client_id.to_string())
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
/// On Windows, `PATH` very commonly resolves `ssh`/`scp` to Git for
/// Windows' own MSYS-based copies (`Program Files\Git\usr\bin\`) rather
/// than the real Windows OpenSSH client (`%WINDIR%\System32\OpenSSH\`) —
/// Git for Windows puts its own `usr\bin` ahead of `System32\OpenSSH` in
/// `PATH` by default. That MSYS build silently rewrites any argument
/// that LOOKS like a POSIX path (`~/.local/bin/somsrv`) into a Windows
/// path (`/c/Users/<user>/.local/bin/somsrv`) BEFORE it ever reaches the
/// remote host — the same automatic argv path-conversion MSYS2 programs
/// apply to make Unix-style paths work when calling native Windows
/// tools, applied here to an argument that was never meant to be
/// translated at all, since it's meant for the REMOTE machine's own
/// shell to expand, not this one. Confirmed live (2026-09-15) as the
/// root cause of a real deploy failure: `chmod +x ~/.local/bin/somsrv`
/// arrived on the remote host as `chmod +x /c/Users/dnk/.local/bin/
/// somsrv` — a path that only makes sense on the LOCAL Windows machine,
/// so `chmod` correctly reported it as not found. The real Windows
/// OpenSSH client (confirmed live, same repro, same host) does not do
/// this rewriting at all. Resolving the absolute path to `System32\
/// OpenSSH\ssh.exe`/`scp.exe` explicitly, rather than trusting whatever
/// `PATH` happens to resolve `"ssh"`/`"scp"` to, sidesteps the ambiguity
/// entirely instead of trying to suppress MSYS's rewriting behavior
/// (env vars like `MSYS_NO_PATHCONV`/`MSYS2_ARG_CONV_EXCL` were tried
/// live against the MSYS binary directly and did NOT suppress it for
/// this argument shape — not a reliable fix). Falls back to the bare
/// `"ssh"`/`"scp"` name (let `PATH` resolve it) if the expected
/// `System32\OpenSSH` binary isn't present, matching every other
/// platform's behavior unchanged.
#[cfg(target_os = "windows")]
fn windows_openssh_binary(name: &str) -> std::path::PathBuf {
    let candidate = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
        .join("OpenSSH")
        .join(format!("{name}.exe"));
    if candidate.is_file() { candidate } else { std::path::PathBuf::from(name) }
}

#[cfg(not(target_os = "windows"))]
fn windows_openssh_binary(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(name)
}

fn scp_to_remote(host_args: &[String], local_path: &std::path::Path, remote_path: &str) -> anyhow::Result<()> {
    let Some(host) = host_args.last() else {
        anyhow::bail!("scp_to_remote called with empty host_args");
    };
    let destination = format!("{host}:{remote_path}");
    let local_size = std::fs::metadata(local_path).map(|m| m.len());
    log::debug!("deploy-check: scp starting, local_path={local_path:?} (size={local_size:?}) -> {destination:?}");
    let output = util::command::new_std_command(windows_openssh_binary("scp"))
        .arg(local_path)
        .arg(&destination)
        .output()
        .with_context(|| format!("failed to spawn scp to copy {local_path:?} to {destination:?}"))?;
    log::debug!(
        "deploy-check: scp finished for {destination:?}: status={:?} stdout={:?} stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
    // `wsl.exe` has no MSYS-path-rewriting concern (it isn't an MSYS
    // binary at all) — only the `ssh` case needs `windows_openssh_
    // binary`'s explicit resolution, see that function's own doc comment
    // for why: Git for Windows' `ssh.exe` silently rewrites POSIX-looking
    // arguments (`~/.local/bin/somsrv`) into Windows paths before they
    // ever reach the remote host, which real Windows OpenSSH does not do.
    let program = match remote_kind {
        RemoteKind::Ssh => windows_openssh_binary("ssh"),
        RemoteKind::Wsl => std::path::PathBuf::from("wsl"),
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
    let output = util::command::new_std_command(&program)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn {program:?} for remote deploy check"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} exited with {:?}: {}",
            program.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Finds `somsrv`/`somsrv.exe` next to Som's own
/// executable — the same "binaries live side by side" assumption
/// `target/debug/` (and any packaged distribution) already guarantees for
/// Som's other bundled tools. Mirrors the old (now-removed)
/// `somsrv_client::server_binary_path`, which lived on the GPUI-client
/// side of the old JSON-protocol architecture; this is its natural home now
/// that the substitution happens directly in the shell command instead.
///
/// Cross-platform, not Windows-only — this is the `RemoteKind::Local` path
/// in `tmux_wrapped_shell`, which applies just as much to a Mac/Linux build
/// of Som with a plain local `zsh`/`bash` `tmux: true` profile as it does to
/// Windows' `pwsh.exe` one; only the `ssh`/`wsl` remote paths are Windows-only
/// today (those profiles' `shell` settings only make sense from a Windows
/// Som talking OUT to other machines).
fn somsrv_binary_path() -> anyhow::Result<PathBuf> {
    somsrv::daemon::binary_path_next_to_current_exe()
}

#[cfg(test)]
mod tmux_shell_wrapping_tests {
    use super::*;

    #[test]
    fn wraps_a_simple_program_with_no_args() {
        let args =
            wrap_command_args("dnk", "pane-uuid-1", "pwsh.exe".to_string(), vec![], CursorShape::Block, None, None);
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
            None,
        );
        assert_eq!(
            args,
            vec!["dnk", "pane-uuid-6", "pwsh.exe", "--cursor-shape", "underline", "--scrollback", "10000"]
        );
    }

    #[test]
    fn appends_cell_pixel_size_flag_when_set() {
        let args = wrap_command_args(
            "dnk",
            "pane-uuid-7",
            "pwsh.exe".to_string(),
            vec![],
            CursorShape::Block,
            None,
            Some((9, 18)),
        );
        assert_eq!(args, vec!["dnk", "pane-uuid-7", "pwsh.exe", "--cursor-shape", "block", "--cell-pixel-size", "9;18"]);
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
        // somsrv invocation goes on the REMOTE side, appended
        // after ssh's own arguments, since ssh hands everything past its
        // own flags/host to a shell on the far end.
        let args =
            wrap_remote_command_args("pi5", "pane-uuid-4", vec!["192.168.50.5".to_string()], CursorShape::Block, None, RemoteKind::Ssh);
        assert_eq!(
            args,
            vec![
                "-tt", "192.168.50.5", "~/.local/bin/somsrv", "pi5", "pane-uuid-4", "$SHELL", "--cursor-shape", "block",
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
                "--cd", "~", "~/.local/bin/somsrv", "wsl", "pane-uuid-5", "$SHELL", "--cursor-shape", "bar",
                "--scrollback", "5000", "--", "-l"
            ]
        );
    }

    #[test]
    fn builds_a_version_probe_using_the_same_host_args_as_the_real_invocation() {
        let args = wrap_remote_probe_args(
            &["192.168.50.5".to_string()],
            "~/.local/bin/somsrv",
            &["--version"],
        );
        assert_eq!(args, vec!["192.168.50.5", "~/.local/bin/somsrv", "--version"]);
    }

    fn session(profile_name: &str, pane_id: &str, client_id: &str) -> somsrv::protocol::SessionInfo {
        somsrv::protocol::SessionInfo {
            profile_name: profile_name.to_string(),
            pane_id: pane_id.to_string(),
            client_id: Some(client_id.to_string()),
        }
    }

    #[test]
    fn orphan_scan_flags_a_session_whose_pane_id_is_not_in_db_json() {
        // `SrvRequest::ListSessions` is already scoped to a single
        // `client_id` server-side, so this filter only needs to check
        // `live_pane_ids` membership — no `--client-id` string comparison
        // to get wrong the way the old `ps`-grep design once did (see
        // `kill_orphaned_holders`'s doc comment for that history).
        let sessions = vec![session("deb", "dead-pane", "dnk@192.168.50.2")];
        let orphaned = orphaned_pane_ids(&sessions, &["live-pane".to_string()]);
        assert_eq!(orphaned, vec!["dead-pane"]);
    }

    #[test]
    fn orphan_scan_leaves_a_session_whose_pane_id_is_in_db_json_alone() {
        let sessions = vec![session("deb", "live-pane", "dnk@192.168.50.2")];
        let orphaned = orphaned_pane_ids(&sessions, &["live-pane".to_string()]);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn orphan_scan_handles_multiple_sessions() {
        let sessions = vec![
            session("deb", "orphan-a", "dnk@192.168.50.2"),
            session("deb", "live-pane", "dnk@192.168.50.2"),
            session("deb", "orphan-b", "dnk@192.168.50.2"),
        ];
        let orphaned = orphaned_pane_ids(&sessions, &["live-pane".to_string()]);
        assert_eq!(orphaned, vec!["orphan-a", "orphan-b"]);
    }

    #[test]
    fn orphan_scan_with_no_sessions_returns_empty() {
        let orphaned = orphaned_pane_ids(&[], &[]);
        assert!(orphaned.is_empty());
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
            None,
        );
        let shell = Shell::WithArguments {
            program: "C:\\som\\somsrv.exe".to_string(),
            args,
            title_override: None,
        };
        let (rebuilt, fresh_pane_id) =
            rebuild_tmux_shell_with_fresh_pane_id(&shell).expect("should detect a local tmux-wrapped shell");
        let Shell::WithArguments { program, args, .. } = &rebuilt else { panic!("expected WithArguments") };
        assert_eq!(program, "C:\\som\\somsrv.exe");
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
        assert_eq!(args[2], "~/.local/bin/somsrv");
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
    /// `ensure_remote_binary_deployed` correctly STAGES a redeploy (2026-
    /// 09-15 redesign: Som's side no longer kills anything or applies the
    /// deploy directly — it only `scp`s the new binary to `somsrv.new`
    /// and drops a marker file; `somsrv` itself, via `somsrvupd`, applies
    /// the actual cutover asynchronously the next time ANY client
    /// connects — see `somsrv::daemon::check_and_apply_pending_redeploy`'s
    /// own doc comment for that half). This test therefore only checks
    /// the STAGING half: `somsrv.new` exists, is executable, and reports
    /// the current version when run directly (NOT via `~/.local/bin/
    /// somsrv`, which is deliberately untouched by this call) — and the
    /// marker file is present. Applying the staged redeploy end-to-end
    /// (actually cutting over `~/.local/bin/somsrv` itself) needs a real
    /// `somsrv` daemon connection cycle, covered separately, not by this
    /// test.
    ///
    /// Requires a LIVE, responding `somsrv` daemon on the host already
    /// (started here via `spawn_test_relay`, same as `test_redeploy_
    /// applies_end_to_end_via_somsrvupd` below) — `ensure_remote_binary_
    /// deployed` only takes the `.new` + marker staging path at all when
    /// `remote_info.is_some()` (see that function's own doc comment,
    /// 2026-09-15: with nothing live to hand a cutover to, staging would
    /// be a dead end no connection could ever apply, so it writes
    /// directly to the final path instead in that case — a DIFFERENT
    /// code path than the one this test exists to verify).
    ///
    /// KNOWN LIMITATION on a real dev machine: this manufactures a
    /// missing-binary case by deleting whatever's at `somsrv.new` before
    /// the call, which forces `ensure_remote_binary_deployed` down its
    /// `ensure_embedded_binary_available` -> `somsrv::protocol::
    /// ensure_embedded_binary_extracted` lookup — under `cfg!(test)`,
    /// `paths::config_dir()` resolves against a FAKE home directory
    /// (`C:\Users\zed\...`/`/home/zed\...`, see `util::paths::home_dir`'s
    /// own `cfg!(test)` branch) this process typically has no permission
    /// to create on a real Windows machine (confirmed: `New-Item -Path
    /// C:\Users\zed` → access denied, even from an otherwise fully-
    /// privileged dev account — creating an arbitrary top-level
    /// `C:\Users\<name>` directory needs real admin rights this test
    /// deliberately does NOT attempt to acquire). Skip this one unless
    /// that fake config dir is writable in this test environment.
    ///
    /// Regression test for the missing `mkdir -p ~/.local/bin` bug: a
    /// genuinely fresh host (no `~/.local/bin` at all, no `somsrv`
    /// process running) must still succeed on its FIRST-EVER deploy —
    /// `scp` doesn't create intermediate directories on its own, so
    /// without the `mkdir -p` this hits `remote_mtime.is_none()`'s
    /// direct-write branch and fails outright. Set `SOM_TEST_SSH_HOST`
    /// to a real host with `~/.local/bin` removed and no `somsrv`
    /// running before running this.
    #[test]
    #[ignore]
    fn test_first_ever_deploy_creates_local_bin_if_missing() {
        let host_args = integration_test_ssh_host();

        let staged = ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh, workspace::RemoteOs::Lnx)
            .expect("first-ever deploy to a directory-less host should succeed, not fail on a missing ~/.local/bin");
        assert!(staged, "a host with nothing deployed yet must report that a deploy happened");

        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/somsrv", &["--version"]);
        let output = run_remote_command(RemoteKind::Ssh, &version_probe)
            .expect("the freshly-deployed ~/.local/bin/somsrv should be executable and runnable");
        let info: somsrv::protocol::HandshakeInfo =
            serde_json::from_str(output.trim()).expect("--version should print valid HandshakeInfo JSON");
        assert_eq!(info.version, somsrv::protocol::HandshakeInfo::current().version);
    }

    /// `#[ignore]`d by default: needs a real reachable SSH host with a
    /// clone of this repo at `~/som` (so `git pull` succeeds, in the WSL
    /// branch of `ensure_remote_binary_deployed`). Run explicitly with:
    /// `cargo test -p terminal_view test_deploy_stages_a_redeploy -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_deploy_stages_a_redeploy() {
        let host_args = integration_test_ssh_host();

        // Clean slate: remove whatever might already be staged from a
        // previous test run, so a successful `scp` below is unambiguous.
        let cleanup_probe = wrap_remote_probe_args(
            &host_args,
            "rm",
            &["-f", "~/.local/bin/somsrv.new", "~/.local/bin/somsrv.redeploy-pending"],
        );
        run_remote_command(RemoteKind::Ssh, &cleanup_probe).expect("failed to clear out any stale staged files for this test");

        // Ensures a live, responding daemon exists on the host first —
        // see this test's own doc comment above for why the staging path
        // this test verifies is only reachable when one is.
        let warm_up_pane_id = format!("test-deploy-stages-warmup-{}", std::process::id());
        spawn_test_relay(&host_args, &warm_up_pane_id);
        std::thread::sleep(std::time::Duration::from_millis(500));

        let staged = ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh, workspace::RemoteOs::Lnx).expect("staging the deploy should succeed against a real reachable host");
        if !staged {
            eprintln!("remote host is already at the current version — nothing to stage, test has nothing to verify");
            return;
        }

        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/somsrv.new", &["--version"]);
        let output = run_remote_command(RemoteKind::Ssh, &version_probe).expect("the freshly-staged somsrv.new should be executable and runnable");
        let info: somsrv::protocol::HandshakeInfo =
            serde_json::from_str(output.trim()).expect("--version should print valid HandshakeInfo JSON");
        assert_eq!(info.version, somsrv::protocol::HandshakeInfo::current().version);

        let marker_probe = wrap_remote_probe_args(&host_args, "test", &["-f", "~/.local/bin/somsrv.redeploy-pending"]);
        run_remote_command(RemoteKind::Ssh, &marker_probe).expect("the redeploy marker file should have been written");

        // Cleanup: leave no staged redeploy behind for whatever real
        // daemon might be running on this test host.
        run_remote_command(RemoteKind::Ssh, &cleanup_probe).ok();
    }

    /// Real SSH round-trip covering the FULL redeploy cycle end to end —
    /// staging (`ensure_remote_binary_deployed`) AND application (`som-
    /// srv` noticing the marker via `somsrv::daemon::check_and_apply_
    /// pending_redeploy`, extracting/spawning `somsrvupd`, which kills the
    /// old daemon, renames `.new` into place, and starts a fresh one).
    /// Deliberately verifies every step through the SAME `--version`
    /// protocol probe `ensure_remote_binary_deployed` itself already
    /// uses (`~/.local/bin/somsrv --version`) rather than any raw shell
    /// inspection (`ps`/`ls`/etc) of the remote host — if that protocol
    /// round-trip fails or reports the wrong version, that IS the
    /// failure this test needs to catch, and probing it any other way
    /// would risk passing on a state the real deploy-check code path
    /// wouldn't actually trust either.
    ///
    /// Sequence:
    /// 1. Ensure SOME `somsrv` is already running as a daemon on the
    ///    test host (via `spawn_test_relay`, the same real-RELAY-
    ///    invocation helper other tests already use — this transitively
    ///    spawns the daemon if nothing was listening yet).
    /// 2. Stage a redeploy (`ensure_remote_binary_deployed`) — this
    ///    process's OWN current build is what gets staged, so if the
    ///    remote already happens to be at this exact version, staging
    ///    is skipped entirely and this test has nothing left to verify;
    ///    see the `Ok(false)`-early-return guard below.
    /// 3. Connect a SECOND real RELAY — this is the trigger:
    ///    `check_and_apply_pending_redeploy` only runs on the daemon's
    ///    connection-accept loop, so nothing applies the staged redeploy
    ///    until some client actually tries to connect.
    /// 4. Poll `~/.local/bin/somsrv --version` (the ordinary path, NOT
    ///    `.new`) until it reports the just-staged version — `somsrvupd`'s
    ///    own cutover (kill old daemon, rename, spawn new daemon) is
    ///    asynchronous relative to this test process, so this can't be a
    ///    single immediate assertion.
    ///
    /// `#[ignore]`d by default: needs a real reachable SSH host with a
    /// clone of this repo at `~/som`. Run explicitly with:
    /// `cargo test -p terminal_view test_redeploy_applies_end_to_end_via_somsrvupd -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_redeploy_applies_end_to_end_via_somsrvupd() {
        let host_args = integration_test_ssh_host();
        let warm_up_pane_id = format!("test-redeploy-warmup-{}", std::process::id());
        spawn_test_relay(&host_args, &warm_up_pane_id);
        std::thread::sleep(std::time::Duration::from_millis(500));

        let staged = ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh, workspace::RemoteOs::Lnx).expect("staging the deploy should succeed against a real reachable host");
        if !staged {
            eprintln!("remote host is already at the current version — nothing to redeploy, test has nothing to verify");
            return;
        }

        // Trigger: connect a fresh RELAY so the daemon's accept loop
        // runs `check_and_apply_pending_redeploy` and notices the
        // marker this call just staged.
        let trigger_pane_id = format!("test-redeploy-trigger-{}", std::process::id());
        spawn_test_relay(&host_args, &trigger_pane_id);

        let expected_version = somsrv::protocol::HandshakeInfo::current().version;
        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/somsrv", &["--version"]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Ok(output) = run_remote_command(RemoteKind::Ssh, &version_probe) {
                if let Ok(info) = serde_json::from_str::<somsrv::protocol::HandshakeInfo>(output.trim()) {
                    if info.version == expected_version {
                        break; // redeploy applied successfully
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "~/.local/bin/somsrv never reported version {expected_version:?} within the timeout — somsrvupd's takeover did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Real SSH round-trip: confirms `ensure_remote_binary_deployed` is a
    /// true no-op (no kill, no scp) when the remote is already at the
    /// current version (so its mtime is at least as new as the local
    /// embedded copy's own mtime — see `ensure_remote_binary_deployed`'s
    /// own doc comment for the 2026-09-15 mtime-based redesign this
    /// verifies) — the common case on every tab open/restore once a host
    /// has been deployed to once already. Verified by checking the
    /// binary's mtime is unchanged after the call, rather than asserting
    /// on internal call counts (this function has no test-seam for that
    /// and adding one purely for this would be over-engineering for a
    /// single assertion).
    ///
    /// Requires the remote to ALREADY be at the current version before
    /// this test runs (deliberately does NOT call `ensure_remote_binary_
    /// deployed` itself to set that up first, unlike `test_deploy_stages_
    /// a_redeploy` — the actual-deploy path needs `ensure_embedded_binary_
    /// available`, which resolves relative to `paths::config_dir()`,
    /// which under `cfg!(test)` resolves to a fake `C:\Users\zed\...`/
    /// `/home/zed/...` home directory this process has no permission to
    /// create on a real dev machine — see `util::paths::home_dir`'s own
    /// `cfg!(test)` branch. The ALREADY-current path returns early,
    /// before ever touching that lookup, so it's the one case this
    /// integration test CAN safely exercise without hitting that same
    /// wall; deploy the current build to the test host manually first —
    /// e.g. `scp ~/.config/som/srv/linux-amd/somsrv <host>:~/.local/
    /// bin/somsrv` — to set up the precondition.
    ///
    /// `#[ignore]`d by default — same reachability requirement as `test_
    /// deploy_stages_a_redeploy`. Run explicitly with:
    /// `cargo test -p terminal_view test_deploy_is_a_no_op_when_already_current -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn test_deploy_is_a_no_op_when_already_current() {
        let host_args = integration_test_ssh_host();

        let version_probe = wrap_remote_probe_args(&host_args, "~/.local/bin/somsrv", &["--version"]);
        let output = run_remote_command(RemoteKind::Ssh, &version_probe)
            .expect("remote must already have SOME somsrv at ~/.local/bin/somsrv for this test's precondition");
        let info: somsrv::protocol::HandshakeInfo =
            serde_json::from_str(output.trim()).expect("--version should print valid HandshakeInfo JSON");
        assert_eq!(
            info.version,
            somsrv::protocol::HandshakeInfo::current().version,
            "test precondition not met: deploy the current build to the test host manually first (see doc comment)"
        );

        let mtime_probe = wrap_remote_probe_args(&host_args, "stat", &["-c", "%Y", "~/.local/bin/somsrv"]);
        let mtime_before = run_remote_command(RemoteKind::Ssh, &mtime_probe).expect("stat should succeed");

        ensure_remote_binary_deployed(&host_args, RemoteKind::Ssh, workspace::RemoteOs::Lnx).expect("no-op deploy should still report success");

        let mtime_after = run_remote_command(RemoteKind::Ssh, &mtime_probe).expect("stat should succeed");
        assert_eq!(
            mtime_before, mtime_after,
            "binary's mtime must be unchanged — a matching version must never re-kill/re-scp"
        );
    }

    /// Real SSH round-trip: registers two sessions with the remote daemon
    /// (spawning it if not already running, via the real RELAY invocation
    /// — NOT a fabricated shortcut), one with a pane_id NOT in
    /// `live_pane_ids`, confirms `kill_orphaned_holders` kills it via
    /// `SrvRequest::KillSession`, and confirms a SEPARATE session whose
    /// pane_id IS in `live_pane_ids` survives — the two behaviors this
    /// function exists to balance (see its own doc comment). Uses the
    /// real `client_id` this account/host pair would actually get (read
    /// back via `$SSH_CLIENT` + `whoami`, the same way `somsrv::
    /// protocol::ssh_client_id` does on the remote side), not a
    /// fabricated one, so this exercises the SAME client-id matching path
    /// production code goes through, not a shortcut around it.
    ///
    /// `#[ignore]`d by default — same reachability requirement as the
    /// deploy tests above, plus a working `somsrv` binary already at
    /// `~/.local/bin/somsrv`. Run explicitly with:
    /// `cargo test -p terminal_view test_kill_orphaned_holders_only_kills_the_orphan -- --ignored --nocapture`

    /// Spawns a real RELAY (`somsrv <profile> <pane-id> <program>`,
    /// backgrounded via `nohup ... &`) — the exact same invocation shape
    /// `wrap_remote_command_args` builds for a real `tmux: true` tab. This
    /// registers a session with the shared daemon on the far end
    /// (spawning the daemon itself, detached, if this is the first
    /// session on that host — see `relay::connect_or_spawn_daemon`),
    /// exercising the REAL registration path end to end rather than
    /// reaching into the daemon's registry directly.
    fn spawn_test_relay(host_args: &[String], pane_id: &str) {
        let spawn_script = format!("nohup ~/.local/bin/somsrv test-orphan-cleanup {pane_id} /bin/sh >/dev/null 2>&1 &");
        let quoted_spawn_script = shell_quote(&spawn_script);
        let spawn_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_spawn_script]);
        run_remote_command(RemoteKind::Ssh, &spawn_probe).expect("failed to spawn a test relay");
    }

    /// Lists the remote daemon's sessions for `client_id` and reports
    /// whether `pane_id` is among them — the direct replacement for the
    /// old `ps`-grep-based `count_holders_with_pane_id` helper, now that
    /// session identity lives in the daemon's registry, not in any
    /// process's argv.
    fn session_exists(host_args: &[String], client_id: &str, pane_id: &str) -> bool {
        let list_script = format!("~/.local/bin/somsrv --list-sessions {}", shell_quote(client_id));
        let quoted = shell_quote(&list_script);
        let probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted]);
        let output = run_remote_command(RemoteKind::Ssh, &probe).expect("list-sessions probe should succeed");
        let sessions: Vec<somsrv::protocol::SessionInfo> =
            serde_json::from_str(output.trim()).expect("--list-sessions should print valid JSON");
        sessions.iter().any(|session| session.pane_id == pane_id)
    }

    fn read_back_this_connections_client_id(host_args: &[String]) -> String {
        // Same marker-prefix technique `kill_orphaned_holders` itself uses
        // (see its own doc comment) — a login shell can print unrelated
        // profile-script noise (e.g. a version manager's init banner) to
        // stdout BEFORE this echo ever runs, so blindly taking the first
        // line/word of output is unreliable on a real host (confirmed live
        // against `ssh localhost`'s WSL2 setup).
        let quoted_client_id_script = shell_quote(r#"echo "SOM_TEST_CLIENT_ID:$(whoami)@$SSH_CLIENT""#);
        let client_id_probe = wrap_remote_probe_args(host_args, "sh", &["-lc", &quoted_client_id_script]);
        let client_id_output =
            run_remote_command(RemoteKind::Ssh, &client_id_probe).expect("failed to read back this connection's client-id");
        client_id_output
            .lines()
            .find_map(|line| line.strip_prefix("SOM_TEST_CLIENT_ID:"))
            .and_then(|rest| rest.split_whitespace().next())
            .expect("client-id probe should print the marker line")
            .to_string()
    }

    #[test]
    #[ignore]
    fn test_kill_orphaned_holders_only_kills_the_orphan() {
        let host_args = integration_test_ssh_host();
        let orphan_pane_id = format!("test-orphan-{}", std::process::id());
        let live_pane_id = format!("test-live-{}", std::process::id());
        let client_id = read_back_this_connections_client_id(&host_args);

        spawn_test_relay(&host_args, &orphan_pane_id);
        spawn_test_relay(&host_args, &live_pane_id);
        // Give each RELAY a moment to actually register with the daemon
        // before the orphan-scan probe below lists sessions.
        std::thread::sleep(std::time::Duration::from_millis(500));

        kill_orphaned_holders(&host_args, RemoteKind::Ssh, std::slice::from_ref(&live_pane_id));

        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            !session_exists(&host_args, &client_id, &orphan_pane_id),
            "the orphaned session (not in live_pane_ids) should have been killed"
        );
        assert!(session_exists(&host_args, &client_id, &live_pane_id), "the live session (pane_id IS in live_pane_ids) must survive");

        // Cleanup: the live one was deliberately spared above, so kill it
        // now that the test is done with it — over SSH, same as the
        // production `--kill-session` call, NOT `somsrv::admin::
        // kill_session` (which would talk to a daemon on THIS machine,
        // not the remote one under test).
        let cleanup_script = format!("~/.local/bin/somsrv --kill-session {} {}", shell_quote(&client_id), shell_quote(&live_pane_id));
        let quoted_cleanup_script = shell_quote(&cleanup_script);
        let cleanup_probe = wrap_remote_probe_args(&host_args, "sh", &["-lc", &quoted_cleanup_script]);
        run_remote_command(RemoteKind::Ssh, &cleanup_probe).ok();
    }

}
