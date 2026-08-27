use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity};

use futures::{FutureExt, future::Shared};
use settings::{Settings, SettingsLocation};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use task::{Shell, ShellBuilder};
use terminal::{
    Terminal, TerminalBuilder,
    terminal_settings::TerminalSettings,
};
use util::{
    command::new_std_command, get_system_shell, rel_path::RelPath,
};

use crate::Project;

/// Parses a shell command string, handling paths with spaces.
/// If the string looks like a bare executable path (ends with .exe or has no flag-like args),
/// treat the whole string as the program. Otherwise split on first whitespace boundary
/// where the program part is quoted or has no spaces.
/// E.g. `C:\Program Files\pwsh.exe` → (`C:\Program Files\pwsh.exe`, [])
///      `wsl --cd ~` → (`wsl`, [`--cd`, `~`])
///
/// Public so `som-srv` client code (`terminal_view::som_tmux_client`) can
/// turn a `TabProfile::shell` string into the `program`/`args` its
/// `NewSession` protocol message needs, using the exact same parsing as the
/// regular (non-tmux) terminal-creation path — rather than duplicating this
/// logic and risking the two diverging.
pub fn parse_shell_command(cmd: &str) -> (String, Vec<String>) {
    let cmd = cmd.trim();
    // Quoted program: "path with spaces" [args...]
    if cmd.starts_with('"') {
        if let Some(end) = cmd[1..].find('"') {
            let program = cmd[1..end + 1].to_string();
            let rest = cmd[end + 2..].trim();
            let args = if rest.is_empty() {
                vec![]
            } else {
                rest.split_whitespace().map(|s| s.to_string()).collect()
            };
            return (program, args);
        }
    }
    // Check if the whole string is a path to an executable (no args)
    // Heuristic: if it ends with .exe (case-insensitive) treat whole string as program
    if cmd.to_ascii_lowercase().ends_with(".exe") {
        return (cmd.to_string(), vec![]);
    }
    // Otherwise split on whitespace
    let mut parts = cmd.split_whitespace();
    let program = parts.next().unwrap_or("").to_string();
    let args = parts.map(|s| s.to_string()).collect();
    (program, args)
}

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
}

impl Project {
    pub fn active_entry_directory(&self, cx: &App) -> Option<PathBuf> {
        let entry_id = self.active_entry()?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;

        let absolute_path = worktree.absolutize(entry.path.as_ref());
        if entry.is_dir() {
            Some(absolute_path)
        } else {
            absolute_path.parent().map(|p| p.to_path_buf())
        }
    }

    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        self.active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir())
    }

    pub fn first_project_directory(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.worktrees(cx).next()?;
        let worktree = worktree.read(cx);
        if worktree.root_entry()?.is_dir() {
            Some(worktree.abs_path().to_path_buf())
        } else {
            None
        }
    }


    pub fn create_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.create_terminal_shell_internal(cwd, None, cx)
    }

    pub fn create_terminal_with_shell(
        &mut self,
        cwd: Option<PathBuf>,
        shell_cmd: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let (program, args) = parse_shell_command(&shell_cmd);
        let shell = if args.is_empty() {
            Shell::Program(program)
        } else {
            Shell::WithArguments { program, args, title_override: None }
        };
        self.create_terminal_shell_internal(cwd, Some(shell), cx)
    }

    /// Like `create_terminal_with_shell`, but takes `program`/`args`
    /// directly instead of a single command string to parse — needed by
    /// `tmux:true` profiles (see `project_som_tmux` memory), which
    /// substitute `som-srv-server <profile> <program> [args...]` in for
    /// the profile's own shell. Round-tripping that substitution through a
    /// single string (build one, hand it to `create_terminal_with_shell`,
    /// have it get re-split by `parse_shell_command`) risks mangling
    /// arguments/paths containing spaces or quotes — this skips that
    /// entirely.
    pub fn create_terminal_with_program_and_args(
        &mut self,
        cwd: Option<PathBuf>,
        program: String,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let shell = if args.is_empty() {
            Shell::Program(program)
        } else {
            Shell::WithArguments { program, args, title_override: None }
        };
        self.create_terminal_shell_internal(cwd, Some(shell), cx)
    }

    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let working_directory = self.active_project_directory(cx).map(|p| p.to_path_buf());
        self.create_terminal_shell_internal(working_directory, None, cx)
    }

    fn create_terminal_shell_internal(
        &mut self,
        cwd: Option<PathBuf>,
        shell_override: Option<Shell>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path = cwd.map(|p| Arc::from(&*p));

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let env_shell = get_system_shell();

        let path_style = self.path_style(cx);

        let env_task = self.resolve_directory_environment(&env_shell, path.clone(), cx);

        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            let activation_script: Vec<String> = Vec::new();

            let shell = shell_override.unwrap_or(settings.shell);

            let builder = project
                .update(cx, move |_, cx| {
                    anyhow::Ok(TerminalBuilder::new(
                        path.map(|path| path.to_path_buf()),
                        None,
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        settings.path_hyperlink_timeout_ms,
                        false,
                        cx.entity_id().as_u64(),
                        None,
                        cx,
                        activation_script,
                        path_style,
                    ))
                })??
                .await?;
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn clone_terminal(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<'_, Project>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.clone_terminal_with_shell(terminal, None, cx, cwd)
    }

    /// Like `clone_terminal`, but with an optional `shell` substituted for
    /// whatever the source terminal was actually spawned with — see
    /// `terminal::Terminal::clone_builder_with_shell`'s doc comment for why
    /// (tmux-wrapped shells need a rebuilt command with a fresh pane id on
    /// split, not a byte-for-byte copy of the original).
    pub fn clone_terminal_with_shell(
        &mut self,
        terminal: &Entity<Terminal>,
        shell_override: Option<Shell>,
        cx: &mut Context<'_, Project>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        // We cannot clone the task's terminal, as it will effectively re-spawn the task, which might not be desirable.
        // For now, create a new shell instead.
        if terminal.read(cx).task().is_some() {
            return self.create_terminal_shell(cwd, cx);
        }
        let builder = match shell_override {
            Some(shell) => terminal.read(cx).clone_builder_with_shell(shell, cx, cwd),
            None => terminal.read(cx).clone_builder(cx, cwd),
        };
        cx.spawn(async |project, cx| {
            let terminal = builder.await?;
            project.update(cx, |project, cx| {
                let terminal_handle = cx.new(|cx| terminal.subscribe(cx));

                project
                    .terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn terminal_settings<'a>(
        &'a self,
        path: &'a Option<PathBuf>,
        cx: &'a App,
    ) -> &'a TerminalSettings {
        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        TerminalSettings::get(settings_location, cx)
    }

    pub fn exec_in_shell(
        &self,
        command: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<smol::process::Command>> {
        let path = self.first_project_directory(cx);
        let settings = self.terminal_settings(&path, cx).clone();
        let shell = Shell::System;
        let is_windows = self.path_style(cx).is_windows();
        let builder = ShellBuilder::new(&shell, is_windows).non_interactive();
        let (command, args) = builder.build(Some(command), &Vec::new());

        let env_task = self.resolve_directory_environment(
            &shell.program(),
            path.as_ref().map(|p| Arc::from(&**p)),
            cx,
        );

        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            project.update(cx, move |_, _cx| {
                let mut command = new_std_command(command);
                command.args(args);
                command.envs(env);
                if let Some(path) = path {
                    command.current_dir(path);
                }
                Ok(command)
                    .map(|mut process| {
                        util::set_pre_exec_to_start_new_session(&mut process);
                        smol::process::Command::from(process)
                    })
            })?
        })
    }

    pub fn local_terminal_handles(&self) -> &Vec<WeakEntity<terminal::Terminal>> {
        &self.terminals.local_handles
    }

    /// Resolves the environment for a brand-new terminal tab. Uses
    /// `refresh_directory_environment` (not `local_directory_environment`)
    /// so each new tab reflects the OS environment as of *now*, rather than
    /// silently reusing a snapshot cached from whenever the first terminal
    /// for this directory happened to be opened — see that method's doc
    /// comment for why the plain cached lookup is wrong here.
    fn resolve_directory_environment(
        &self,
        shell: &str,
        path: Option<Arc<Path>>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(path) = &path {
            let shell = Shell::Program(shell.to_string());
            self.environment
                .update(cx, |project_env, cx| {
                    project_env.refresh_directory_environment(&shell, path.clone(), cx)
                })
        } else {
            Task::ready(None).shared()
        }
    }
}
