pub mod mappings;

pub use alacritty_terminal;

pub mod kitty_graphics_placeholder;
mod pty_info;
pub mod rich_content_audio_player;
pub mod rich_content_cache;
pub mod rich_content_gif_player;
pub mod rich_content_player;
pub mod rich_content_static_image_player;
pub mod rich_content_transport;
mod terminal_hyperlinks;
pub mod terminal_settings;

use alacritty_terminal::{
    Term,
    event::{Event as AlacTermEvent, EventListener, Notify, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Grid, Row, Scroll as AlacScroll},
    index::{Boundary, Column, Direction as AlacDirection, Line, Point as AlacPoint},
    selection::{Selection, SelectionRange, SelectionType},
    sync::FairMutex,
    term::{
        Config, RenderableCursor, TermMode,
        cell::{Cell, Flags},
        search::{Match, RegexIter, RegexSearch},
    },
    tty::{self},
    vi_mode::{ViModeCursor, ViMotion},
    vte::ansi::{
        ClearMode, CursorStyle as AlacCursorStyle, Handler, NamedPrivateMode, PrivateMode,
    },
};
use anyhow::{Context as _, Result, bail};
use futures_lite::future::yield_now;
use log::trace;

use futures::{
    FutureExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};

use itertools::Itertools as _;
use mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

use async_channel::{Receiver, Sender};
use collections::{HashMap, VecDeque};
use futures::StreamExt;
use pty_info::{ProcessIdGetter, PtyProcessInfo};
use serde::{Deserialize, Serialize};
use settings::Settings;
use task::{HideStrategy, Shell, SpawnInTerminal};
use terminal_hyperlinks::RegexSearches;
use terminal_settings::{AlternateScroll, CursorShape, TerminalSettings};
use theme::{ActiveTheme, Theme};
use urlencoding;
use util::{paths::PathStyle, truncate_and_trailoff};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::{
    borrow::Cow,
    cmp::{self, min},
    fmt::Display,
    ops::{Deref, RangeInclusive},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;

use gpui::{
    App, AppContext as _, BackgroundExecutor, Bounds, ClipboardItem, Context, EventEmitter, Hsla,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Rgba, ScrollWheelEvent, Size, Task, TouchPhase, Window, actions, black, px,
};

use crate::mappings::{colors::to_alac_rgb, keys::to_esc_str};

actions!(
    terminal,
    [
        /// Clears the terminal screen.
        Clear,
        /// Copies selected text to the clipboard.
        Copy,
        /// Pastes from the clipboard.
        Paste,
        /// Pastes the text from the clipboard.
        PasteText,
        /// Shows the character palette for special characters.
        ShowCharacterPalette,
        /// Searches for text in the terminal.
        SearchTest,
        /// Scrolls up by one line.
        ScrollLineUp,
        /// Scrolls down by one line.
        ScrollLineDown,
        /// Scrolls up by one page.
        ScrollPageUp,
        /// Scrolls down by one page.
        ScrollPageDown,
        /// Scrolls up by half a page.
        ScrollHalfPageUp,
        /// Scrolls down by half a page.
        ScrollHalfPageDown,
        /// Scrolls to the top of the terminal buffer.
        ScrollToTop,
        /// Scrolls to the bottom of the terminal buffer.
        ScrollToBottom,
        /// Toggles vi mode in the terminal.
        ToggleViMode,
        /// Selects all text in the terminal.
        SelectAll,
    ]
);

const DEBUG_TERMINAL_WIDTH: Pixels = px(500.);
const DEBUG_TERMINAL_HEIGHT: Pixels = px(30.);
const DEBUG_CELL_WIDTH: Pixels = px(5.);
const DEBUG_LINE_HEIGHT: Pixels = px(5.);

/// How long after creation a terminal's PTY resize (SIGWINCH) is suppressed
/// for anything but its first size-set. See `created_at` on `Terminal`.
const RESIZE_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(1500);

/// Minimum spacing between consecutive `cx.force_redraw_windows()` calls
/// triggered by rich-content commands — see
/// `Terminal::rich_content_force_redraw_due`'s doc comment for the full
/// reasoning. ~33ms is roughly 30fps: fast enough that an animated GIF's
/// frames still visibly stream in as they arrive, slow enough that a
/// dozens-of-frames burst doesn't starve the window's message queue the
/// way calling this on every single frame did (confirmed live).
const FORCE_REDRAW_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// Inserts Zed-specific environment variables for terminal sessions.
/// Used by both local terminals and remote terminals (via SSH).
pub fn insert_zed_terminal_env(
    env: &mut HashMap<String, String>,
    version: &impl std::fmt::Display,
) {
    env.insert("ZED_TERM".to_string(), "true".to_string());
    env.insert("TERM_PROGRAM".to_string(), "zed".to_string());
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    env.insert("TERM_PROGRAM_VERSION".to_string(), version.to_string());
    // Capability signal for programs that want to detect SRP support
    // specifically (see SRP_INTEGRATION_GUIDE.md), independent of
    // TERM_PROGRAM (which stays "zed" for compatibility with tools that
    // already special-case it). Mirrors KITTY_WINDOW_ID's role for Kitty's
    // graphics protocol: presence alone means "SRP is available here",
    // matching the pattern documented for third-party integrators.
    env.insert("SOM_WINDOW_ID".to_string(), std::process::id().to_string());
}

///Upward flowing events, for changing the title and such
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    TitleChanged,
    BreadcrumbsChanged,
    CloseTerminal,
    Bell,
    Wakeup,
    BlinkChanged(bool),
    SelectionsChanged,
    NewNavigationTarget(Option<MaybeNavigationTarget>),
    Open(MaybeNavigationTarget),
    /// An interactive shell (no `task`) exited with a non-zero code before
    /// the user ever typed anything — almost always a spawn failure (bad
    /// `$SHELL`, an `ssh`/tmux wrapper binary that couldn't run, a
    /// connection refused, ...) rather than something the user did. The
    /// error text is already visible as terminal output (see
    /// `register_task_finished`'s doc comment for why the tab stays open),
    /// but nothing else calls it out — this drives a notification so it
    /// isn't silently missed in a background tab.
    SpawnFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLikeTarget {
    /// File system path, absolute or relative, existing or not.
    /// Might have line and column number(s) attached as `file.rs:1:23`
    pub maybe_path: String,
    /// Current working directory of the terminal
    pub terminal_dir: Option<PathBuf>,
}

/// A string inside terminal, potentially useful as a URI that can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaybeNavigationTarget {
    /// HTTP, git, etc. string determined by the `URL_REGEX` regex.
    Url(String),
    /// File system path, absolute or relative, existing or not.
    /// Might have line and column number(s) attached as `file.rs:1:23`
    PathLike(PathLikeTarget),
}

#[derive(Clone)]
enum InternalEvent {
    Resize(TerminalBounds),
    Clear,
    // FocusNextMatch,
    Scroll(AlacScroll),
    ScrollToAlacPoint(AlacPoint),
    SetSelection(Option<(Selection, AlacPoint)>),
    UpdateSelection(Point<Pixels>),
    FindHyperlink(Point<Pixels>, bool),
    ProcessHyperlink((String, bool, Match), bool),
    // Whether keep selection when copy
    Copy(Option<bool>),
    // Vi mode events
    ToggleViMode,
    ViMotion(ViMotion),
    MoveViCursorToAlacPoint(AlacPoint),
}

///A translation struct for Alacritty to communicate with us from their event loop
#[derive(Clone)]
pub struct ZedListener(pub UnboundedSender<AlacTermEvent>);

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(event).ok();
    }
}

/// Where `rich_content_cache::RichContentCache` writes progressively-
/// arriving files. In tests, `paths::config_dir()` would point at a real
/// user's `~/.config/som/` (it's a process-global `OnceLock`, not
/// test-aware — see `paths::config_dir`'s own implementation) — every
/// test run would otherwise write real files there and tests running in
/// parallel with the same session/file id would collide. `std::env::
/// temp_dir()` here mirrors the existing test-fixture pattern already
/// used for `somcat_*_test_fixture.png` elsewhere in this file's test
/// module.
fn rich_content_cache_dir() -> std::path::PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    {
        std::env::temp_dir().join("som_rich_content_cache_runtime")
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        paths::config_dir().join("media_cache")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalBounds {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub bounds: Bounds<Pixels>,
}

impl TerminalBounds {
    pub fn new(line_height: Pixels, cell_width: Pixels, bounds: Bounds<Pixels>) -> Self {
        TerminalBounds {
            cell_width,
            line_height,
            bounds,
        }
    }

    pub fn num_lines(&self) -> usize {
        // Tolerance to prevent f32 precision from losing a row:
        // `N * line_height / line_height` can be N-epsilon, which floor()
        // would round down, pushing the first line into invisible scrollback.
        let raw = self.bounds.size.height / self.line_height;
        raw.next_up().floor() as usize
    }

    pub fn num_columns(&self) -> usize {
        let raw = self.bounds.size.width / self.cell_width;
        raw.next_up().floor() as usize
    }

    pub fn height(&self) -> Pixels {
        self.bounds.size.height
    }

    pub fn width(&self) -> Pixels {
        self.bounds.size.width
    }

    pub fn cell_width(&self) -> Pixels {
        self.cell_width
    }

    pub fn line_height(&self) -> Pixels {
        self.line_height
    }
}

impl Default for TerminalBounds {
    fn default() -> Self {
        TerminalBounds::new(
            DEBUG_LINE_HEIGHT,
            DEBUG_CELL_WIDTH,
            Bounds {
                origin: Point::default(),
                size: Size {
                    width: DEBUG_TERMINAL_WIDTH,
                    height: DEBUG_TERMINAL_HEIGHT,
                },
            },
        )
    }
}

impl From<TerminalBounds> for WindowSize {
    fn from(val: TerminalBounds) -> Self {
        WindowSize {
            num_lines: val.num_lines() as u16,
            num_cols: val.num_columns() as u16,
            cell_width: f32::from(val.cell_width()) as u16,
            cell_height: f32::from(val.line_height()) as u16,
        }
    }
}

impl Dimensions for TerminalBounds {
    /// Note: this is supposed to be for the back buffer's length,
    /// but we exclusively use it to resize the terminal, which does not
    /// use this method. We still have to implement it for the trait though,
    /// hence, this comment.
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

#[derive(Error, Debug)]
pub struct TerminalError {
    pub directory: Option<PathBuf>,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub title_override: Option<String>,
    pub source: std::io::Error,
}

impl TerminalError {
    pub fn fmt_directory(&self) -> String {
        self.directory
            .clone()
            .map(|path| {
                match path
                    .into_os_string()
                    .into_string()
                    .map_err(|os_str| format!("<non-utf8 path> {}", os_str.to_string_lossy()))
                {
                    Ok(s) => s,
                    Err(s) => s,
                }
            })
            .unwrap_or_else(|| "<none specified>".to_string())
    }

    pub fn fmt_shell(&self) -> String {
        if let Some(title_override) = &self.title_override {
            format!(
                "{} {} ({})",
                self.program.as_deref().unwrap_or("<system defined shell>"),
                self.args.as_ref().into_iter().flatten().format(" "),
                title_override
            )
        } else {
            format!(
                "{} {}",
                self.program.as_deref().unwrap_or("<system defined shell>"),
                self.args.as_ref().into_iter().flatten().format(" ")
            )
        }
    }
}

impl Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir_string: String = self.fmt_directory();
        let shell = self.fmt_shell();

        write!(
            f,
            "Working directory: {} Shell command: `{}`, IOError: {}",
            dir_string, shell, self.source
        )
    }
}

// https://github.com/alacritty/alacritty/blob/cb3a79dbf6472740daca8440d5166c1d4af5029e/extra/man/alacritty.5.scd?plain=1#L207-L213
const DEFAULT_SCROLL_HISTORY_LINES: usize = 10_000;
pub const MAX_SCROLL_HISTORY_LINES: usize = 100_000;

pub struct TerminalBuilder {
    terminal: Terminal,
    events_rx: UnboundedReceiver<AlacTermEvent>,
}

impl TerminalBuilder {
    pub fn new_display_only(
        cursor_shape: CursorShape,
        alternate_scroll: AlternateScroll,
        max_scroll_history_lines: Option<usize>,
        window_id: u64,
        background_executor: &BackgroundExecutor,
        path_style: PathStyle,
    ) -> Result<TerminalBuilder> {
        // Create a display-only terminal (no actual PTY).
        let default_cursor_style = AlacCursorStyle::from(cursor_shape);
        let scrolling_history = max_scroll_history_lines
            .unwrap_or(DEFAULT_SCROLL_HISTORY_LINES)
            .min(MAX_SCROLL_HISTORY_LINES);
        let config = Config {
            scrolling_history,
            default_cursor_style,
            ..Config::default()
        };

        let (events_tx, events_rx) = unbounded();
        let mut term = Term::new(
            config.clone(),
            &TerminalBounds::default(),
            ZedListener(events_tx),
        );

        if let AlternateScroll::Off = alternate_scroll {
            term.unset_private_mode(PrivateMode::Named(NamedPrivateMode::AlternateScroll));
        }

        let term = Arc::new(FairMutex::new(term));

        let terminal = Terminal {
            task: None,
            terminal_type: TerminalType::DisplayOnly,
            completion_tx: None,
            term,
            term_config: config,
            write_output_parser: Default::default(),
            title_override: None,
            events: VecDeque::with_capacity(10),
            last_content: Default::default(),
            last_mouse: None,
            matches: Vec::new(),

            selection_head: None,
            breadcrumb_text: String::new(),
            scroll_px: px(0.),
            next_link_id: 0,
            selection_phase: SelectionPhase::Ended,
            hyperlink_regex_searches: RegexSearches::default(),
            vi_mode_enabled: false,
            is_remote_terminal: false,
            is_tmux_relay_shell: false,
            last_mouse_move_time: Instant::now(),
            last_hyperlink_search_position: None,
            mouse_down_hyperlink: None,
            rich_content_drag: None,
            #[cfg(windows)]
            shell_program: None,
            activation_script: Vec::new(),
            template: CopyTemplate {
                shell: Shell::System,
                env: HashMap::default(),
                cursor_shape,
                alternate_scroll,
                max_scroll_history_lines,
                path_hyperlink_regexes: Vec::default(),
                path_hyperlink_timeout_ms: 0,
                window_id,
            },
            child_exited: None,
            keyboard_input_sent: false,
            last_pty_grid_size: None,
            rich_content_cache: rich_content_cache::RichContentCache::new(rich_content_cache_dir()),
            rich_content_players: std::cell::RefCell::new(std::collections::HashMap::new()),
            rich_content_audio_players: std::cell::RefCell::new(std::collections::HashMap::new()),
            rich_content_placement_bounds: std::cell::RefCell::new(std::collections::HashMap::new()),
            rich_content_seek_bar_bounds: std::cell::RefCell::new(std::collections::HashMap::new()),
            next_query_request_id: std::cell::Cell::new(0),
            last_rich_content_force_redraw: std::cell::Cell::new(None),
            created_at: Instant::now(),
            event_loop_task: Task::ready(Ok(())),
            background_executor: background_executor.clone(),
            path_style,
            #[cfg(any(test, feature = "test-support"))]
            input_log: Vec::new(),
        };

        Ok(TerminalBuilder {
            terminal,
            events_rx,
        })
    }

    pub fn new(
        working_directory: Option<PathBuf>,
        task: Option<TaskState>,
        shell: Shell,
        mut env: HashMap<String, String>,
        cursor_shape: CursorShape,
        alternate_scroll: AlternateScroll,
        max_scroll_history_lines: Option<usize>,
        path_hyperlink_regexes: Vec<String>,
        path_hyperlink_timeout_ms: u64,
        is_remote_terminal: bool,
        window_id: u64,
        completion_tx: Option<Sender<Option<ExitStatus>>>,
        cx: &App,
        activation_script: Vec<String>,
        path_style: PathStyle,
    ) -> Task<Result<TerminalBuilder>> {
        let version = release_channel::AppVersion::global(cx);
        let background_executor = cx.background_executor().clone();
        #[cfg(not(windows))]
        let child_signal_mask = match tty::SignalMask::current()
            .context("failed to capture terminal child signal mask")
        {
            Ok(signal_mask) => Some(signal_mask),
            Err(error) => return Task::ready(Err(error)),
        };
        let fut = async move {
            // Remove SHLVL so the spawned shell initializes it to 1, matching
            // the behavior of standalone terminal emulators like iTerm2/Kitty/Alacritty.
            env.remove("SHLVL");

            // If the parent environment doesn't have a locale set
            // (As is the case when launched from a .app on MacOS),
            // and the Project doesn't have a locale set, then
            // set a fallback for our child environment to use.
            if std::env::var("LANG").is_err() {
                env.entry("LANG".to_string())
                    .or_insert_with(|| "en_US.UTF-8".to_string());
            }

            insert_zed_terminal_env(&mut env, &version);

            #[derive(Default)]
            struct ShellParams {
                program: String,
                args: Option<Vec<String>>,
                title_override: Option<String>,
            }

            impl ShellParams {
                fn new(
                    program: String,
                    args: Option<Vec<String>>,
                    title_override: Option<String>,
                ) -> Self {
                    log::debug!("Using {program} as shell");
                    Self {
                        program,
                        args,
                        title_override,
                    }
                }
            }

            let shell_params = match shell.clone() {
                Shell::System => {
                    if cfg!(windows) {
                        Some(ShellParams::new(
                            util::shell::get_windows_system_shell(),
                            None,
                            None,
                        ))
                    } else {
                        None
                    }
                }
                Shell::Program(program) => Some(ShellParams::new(program, None, None)),
                Shell::WithArguments {
                    program,
                    args,
                    title_override,
                } => Some(ShellParams::new(program, Some(args), title_override)),
            };
            let terminal_title_override =
                shell_params.as_ref().and_then(|e| e.title_override.clone());

            #[cfg(windows)]
            let shell_program = shell_params.as_ref().map(|params| {
                use util::ResultExt;
                // Try resolving with .exe extension first (avoids picking up wrong binaries
                // that share a name without extension, e.g. usbipd-win/wsl vs wsl.exe)
                let program = &params.program;
                let with_exe = if !program.contains('.') && !program.contains('\\') && !program.contains('/') {
                    format!("{}.exe", program)
                } else {
                    program.clone()
                };
                Self::resolve_path(&with_exe)
                    .or_else(|_| Self::resolve_path(program))
                    .log_err()
                    .unwrap_or(program.clone())
            });

            // Note: when remoting, this shell_kind will scrutinize `ssh` or
            // `wsl.exe` as a shell and fall back to posix or powershell based on
            // the compilation target. This is fine right now due to the restricted
            // way we use the return value, but would become incorrect if we
            // supported remoting into windows.
            let shell_kind = shell.shell_kind(cfg!(windows));

            let pty_options = {
                let alac_shell = shell_params.as_ref().map(|params| {
                    #[cfg(windows)]
                    let program = shell_program.as_deref().unwrap_or(&params.program);
                    #[cfg(not(windows))]
                    let program = params.program.as_str();
                    alacritty_terminal::tty::Shell::new(
                        program.to_string(),
                        params.args.clone().unwrap_or_default(),
                    )
                });

                alacritty_terminal::tty::Options {
                    shell: alac_shell,
                    working_directory: working_directory.clone(),
                    drain_on_exit: true,
                    env: env.clone().into_iter().collect(),
                    // We pass in the foreground thread's signal mask to the child process via pty_options,
                    // so terminal construction can run on a background thread without breaking Ctrl-C and other signals
                    // otherwise the terminal would inherit the background executor's signal mask which blocks
                    // some terminal signals
                    #[cfg(not(windows))]
                    child_signal_mask,
                    #[cfg(windows)]
                    escape_args: shell_kind.tty_escape_args(),
                }
            };

            let default_cursor_style = AlacCursorStyle::from(cursor_shape);
            let scrolling_history = if task.is_some() {
                // Tasks like `cargo build --all` may produce a lot of output, ergo allow maximum scrolling.
                // After the task finishes, we do not allow appending to that terminal, so small tasks output should not
                // cause excessive memory usage over time.
                MAX_SCROLL_HISTORY_LINES
            } else {
                max_scroll_history_lines
                    .unwrap_or(DEFAULT_SCROLL_HISTORY_LINES)
                    .min(MAX_SCROLL_HISTORY_LINES)
            };
            let config = Config {
                scrolling_history,
                default_cursor_style,
                ..Config::default()
            };

            //Setup the pty...
            let pty = match tty::new(&pty_options, TerminalBounds::default().into(), window_id) {
                Ok(pty) => pty,
                Err(error) => {
                    bail!(TerminalError {
                        directory: working_directory,
                        program: shell_params.as_ref().map(|params| params.program.clone()),
                        args: shell_params.as_ref().and_then(|params| params.args.clone()),
                        title_override: terminal_title_override,
                        source: error,
                    });
                }
            };

            //Spawn a task so the Alacritty EventLoop can communicate with us
            //TODO: Remove with a bounded sender which can be dispatched on &self
            let (events_tx, events_rx) = unbounded();
            //Set up the terminal...
            let mut term = Term::new(
                config.clone(),
                &TerminalBounds::default(),
                ZedListener(events_tx.clone()),
            );

            //Alacritty defaults to alternate scrolling being on, so we just need to turn it off.
            if let AlternateScroll::Off = alternate_scroll {
                term.unset_private_mode(PrivateMode::Named(NamedPrivateMode::AlternateScroll));
            }

            let term = Arc::new(FairMutex::new(term));

            let pty_info = PtyProcessInfo::new(&pty);

            // Detects a `som_tmux`-wrapped profile by shape — either a
            // remote (`ssh`/`wsl`) profile, where `wrap_remote_command_
            // args` always inserts the literal `~/.local/bin/som-tmux`
            // argument (the same key `rebuild_tmux_shell_with_fresh_pane_
            // id` uses), or a LOCAL `tmux: true` profile, where `tmux_
            // wrapped_shell`'s `RemoteKind::Local` branch makes `som-tmux
            // .exe` itself the PROGRAM (not one of its args) — checking
            // `args` alone misses this case entirely. Only these go
            // through the double-pty (`-tt` remote + Windows ConPTY)
            // output path that produces `\r\r\n` (see `collapse_cr_cr_lf`)
            // AND are the RELAY side of a HOLDER that already answers
            // capability queries on the real PTY itself (see `is_tmux_
            // relay_shell`'s second use below, in `process_event`).
            let is_tmux_relay_shell = shell_params.as_ref().is_some_and(|params| {
                params.program.contains("som-tmux")
                    || params.args.as_ref().is_some_and(|args| args.iter().any(|arg| arg.contains("som-tmux")))
            });

            //And connect them together
            let event_loop = EventLoop::new(
                term.clone(),
                ZedListener(events_tx),
                pty,
                pty_options.drain_on_exit,
                false,
            )
            .context("failed to create event loop")?
            // An SSH `tmux: true` profile's output crosses two pty layers
            // (a `-tt`-forced remote pty plus this Windows ConPTY), each
            // inserting a CR, so every real newline arrives as `\r\r\n` —
            // one extra blank line per newline. See `EventLoop::collapse_
            // cr_cr_lf` and `is_tmux_relay_shell`.
            .collapse_cr_cr_lf(is_tmux_relay_shell);

            let pty_tx = event_loop.channel();
            let _io_thread = event_loop.spawn(); // DANGER

            let no_task = task.is_none();
            let terminal = Terminal {
                task,
                terminal_type: TerminalType::Pty {
                    pty_tx: Notifier(pty_tx),
                    info: Arc::new(pty_info),
                },
                completion_tx,
                term,
                term_config: config,
                write_output_parser: Default::default(),
                title_override: terminal_title_override,
                events: VecDeque::with_capacity(10), //Should never get this high.
                last_content: Default::default(),
                last_mouse: None,
                matches: Vec::new(),

                selection_head: None,
                breadcrumb_text: String::new(),
                scroll_px: px(0.),
                next_link_id: 0,
                selection_phase: SelectionPhase::Ended,
                hyperlink_regex_searches: RegexSearches::new(
                    &path_hyperlink_regexes,
                    path_hyperlink_timeout_ms,
                ),
                vi_mode_enabled: false,
                is_remote_terminal,
                is_tmux_relay_shell,
                last_mouse_move_time: Instant::now(),
                last_hyperlink_search_position: None,
                mouse_down_hyperlink: None,
                rich_content_drag: None,
                #[cfg(windows)]
                shell_program,
                activation_script: activation_script.clone(),
                template: CopyTemplate {
                    shell,
                    env,
                    cursor_shape,
                    alternate_scroll,
                    max_scroll_history_lines,
                    path_hyperlink_regexes,
                    path_hyperlink_timeout_ms,
                    window_id,
                },
                child_exited: None,
                keyboard_input_sent: false,
                last_pty_grid_size: None,
                rich_content_cache: rich_content_cache::RichContentCache::new(rich_content_cache_dir()),
                rich_content_players: std::cell::RefCell::new(std::collections::HashMap::new()),
                rich_content_audio_players: std::cell::RefCell::new(std::collections::HashMap::new()),
                rich_content_placement_bounds: std::cell::RefCell::new(std::collections::HashMap::new()),
                rich_content_seek_bar_bounds: std::cell::RefCell::new(std::collections::HashMap::new()),
                next_query_request_id: std::cell::Cell::new(0),
                last_rich_content_force_redraw: std::cell::Cell::new(None),
                created_at: Instant::now(),
                event_loop_task: Task::ready(Ok(())),
                background_executor,
                path_style,
                #[cfg(any(test, feature = "test-support"))]
                input_log: Vec::new(),
            };

            if !activation_script.is_empty() && no_task {
                for activation_script in activation_script {
                    terminal.write_to_pty(activation_script.into_bytes());
                    // Simulate enter key press
                    // NOTE(PowerShell): using `\r\n` will put PowerShell in a continuation mode (infamous >> character)
                    // and generally mess up the rendering.
                    terminal.write_to_pty(b"\x0d");
                }
                // In order to clear the screen at this point, we have two options:
                // 1. We can send a shell-specific command such as "clear" or "cls"
                // 2. We can "echo" a marker message that we will then catch when handling a Wakeup event
                //    and clear the screen using `terminal.clear()` method
                // We cannot issue a `terminal.clear()` command at this point as alacritty is evented
                // and while we have sent the activation script to the pty, it will be executed asynchronously.
                // Therefore, we somehow need to wait for the activation script to finish executing before we
                // can proceed with clearing the screen.
                terminal.write_to_pty(shell_kind.clear_screen_command().as_bytes());
                // Simulate enter key press
                terminal.write_to_pty(b"\x0d");
            }

            Ok(TerminalBuilder {
                terminal,
                events_rx,
            })
        };
        cx.background_spawn(fut)
    }

    pub fn subscribe(mut self, cx: &Context<Terminal>) -> Terminal {
        //Event loop
        self.terminal.event_loop_task = cx.spawn(async move |terminal, cx| {
            while let Some(event) = self.events_rx.next().await {
                log::trace!("DIAGLOOP: events_rx yielded an event, entering process+drain cycle");
                terminal.update(cx, |terminal, cx| {
                    //Process the first event immediately for lowered latency
                    terminal.process_event(event, cx);
                })?;

                'outer: loop {
                    let mut events = Vec::new();

                    #[cfg(any(test, feature = "test-support"))]
                    let mut timer = cx.background_executor().simulate_random_delay().fuse();
                    #[cfg(not(any(test, feature = "test-support")))]
                    let mut timer = cx
                        .background_executor()
                        .timer(std::time::Duration::from_millis(4))
                        .fuse();

                    let mut wakeup = false;
                    loop {
                        futures::select_biased! {
                            _ = timer => break,
                            event = self.events_rx.next() => {
                                if let Some(event) = event {
                                    if matches!(event, AlacTermEvent::Wakeup) {
                                        wakeup = true;
                                    } else {
                                        events.push(event);
                                    }

                                    if events.len() > 100 {
                                        log::trace!("DIAGLOOP: drain loop hit the 100-event cap, breaking early");
                                        break;
                                    }
                                } else {
                                    log::trace!("DIAGLOOP: events_rx closed (child exited?)");
                                    break;
                                }
                            },
                        }
                    }

                    if events.is_empty() && !wakeup {
                        log::trace!("DIAGLOOP: drain cycle empty, yielding and breaking 'outer (back to waiting on events_rx.next())");
                        yield_now().await;
                        break 'outer;
                    }

                    log::trace!("DIAGLOOP: draining {} events (wakeup={wakeup})", events.len());
                    terminal.update(cx, |this, cx| {
                        if wakeup {
                            this.process_event(AlacTermEvent::Wakeup, cx);
                        }

                        for event in events {
                            this.process_event(event, cx);
                        }
                    })?;
                    yield_now().await;
                }
            }
            anyhow::Ok(())
        });
        self.terminal
    }

    #[cfg(windows)]
    fn resolve_path(path: &str) -> Result<String> {
        use windows::Win32::Storage::FileSystem::SearchPathW;
        use windows::core::HSTRING;

        let path = if path.starts_with(r"\\?\") || !path.contains(&['/', '\\']) {
            path.to_string()
        } else {
            r"\\?\".to_string() + path
        };

        let required_length = unsafe { SearchPathW(None, &HSTRING::from(&path), None, None, None) };
        let mut buf = vec![0u16; required_length as usize];
        let size = unsafe { SearchPathW(None, &HSTRING::from(&path), None, Some(&mut buf), None) };

        Ok(String::from_utf16(&buf[..size as usize])?)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexedCell {
    pub point: AlacPoint,
    pub cell: Cell,
}

impl Deref for IndexedCell {
    type Target = Cell;

    #[inline]
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

// TODO: Un-pub
#[derive(Clone)]
pub struct TerminalContent {
    pub cells: Vec<IndexedCell>,
    pub mode: TermMode,
    pub display_offset: usize,
    pub selection_text: Option<String>,
    pub selection: Option<SelectionRange>,
    pub cursor: RenderableCursor,
    pub cursor_char: char,
    pub terminal_bounds: TerminalBounds,
    pub last_hovered_word: Option<HoveredWord>,
    pub scrolled_to_top: bool,
    pub scrolled_to_bottom: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HoveredWord {
    pub word: String,
    pub word_match: RangeInclusive<AlacPoint>,
    pub id: usize,
}

impl Default for TerminalContent {
    fn default() -> Self {
        TerminalContent {
            cells: Default::default(),
            mode: Default::default(),
            display_offset: Default::default(),
            selection_text: Default::default(),
            selection: Default::default(),
            cursor: RenderableCursor {
                shape: alacritty_terminal::vte::ansi::CursorShape::Block,
                point: AlacPoint::new(Line(0), Column(0)),
            },
            cursor_char: Default::default(),
            terminal_bounds: Default::default(),
            last_hovered_word: None,
            scrolled_to_top: false,
            scrolled_to_bottom: false,
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum SelectionPhase {
    Selecting,
    Ended,
}

enum TerminalType {
    Pty {
        pty_tx: Notifier,
        info: Arc<PtyProcessInfo>,
    },
    DisplayOnly,
}

pub struct Terminal {
    terminal_type: TerminalType,
    completion_tx: Option<Sender<Option<ExitStatus>>>,
    term: Arc<FairMutex<Term<ZedListener>>>,
    term_config: Config,
    /// Escape-sequence parser for the `write_output` (display-only /
    /// injected-bytes) path. Must persist across calls: callers feed
    /// arbitrary slices of a byte stream, and a single escape sequence —
    /// notably a Kitty graphics APC string, which routinely runs to
    /// several kilobytes — can straddle any number of those slices. A
    /// fresh parser per call would silently drop everything before the
    /// boundary, turning a valid image transmission into a stream of
    /// orphaned continuation chunks.
    write_output_parser:
        alacritty_terminal::vte::ansi::Processor<alacritty_terminal::vte::ansi::StdSyncHandler>,
    events: VecDeque<InternalEvent>,
    /// This is only used for mouse mode cell change detection
    last_mouse: Option<(AlacPoint, AlacDirection)>,
    pub matches: Vec<RangeInclusive<AlacPoint>>,
    pub last_content: TerminalContent,
    pub selection_head: Option<AlacPoint>,

    pub breadcrumb_text: String,
    title_override: Option<String>,
    scroll_px: Pixels,
    next_link_id: usize,
    selection_phase: SelectionPhase,
    hyperlink_regex_searches: RegexSearches,
    task: Option<TaskState>,
    vi_mode_enabled: bool,
    is_remote_terminal: bool,
    /// True for a `tmux: true` profile's RELAY-side terminal (local or
    /// SSH/WSL) — see `is_tmux_relay_shell`'s doc comment at its
    /// definition site for how this is detected. Gates whether
    /// `process_event` answers capability queries (`PtyWrite`/
    /// `ColorRequest`) itself — see those match arms' doc comments for why
    /// a RELAY's own answering duplicates the HOLDER's, corrupting
    /// whatever the real client program (e.g. `yazi`) is currently
    /// rendering.
    is_tmux_relay_shell: bool,
    last_mouse_move_time: Instant,
    last_hyperlink_search_position: Option<Point<Pixels>>,
    mouse_down_hyperlink: Option<(String, bool, Match)>,
    /// Set by `mouse_down` when a click landed inside a rich-content
    /// widget's bounds (currently only audio play/pause/seek), so
    /// `mouse_drag`/`mouse_up` can skip terminal text-selection for the
    /// rest of this click — mirrors `mouse_down_hyperlink`'s identical
    /// role for hyperlink clicks. Without this, `mouse_move`'s handler
    /// in `terminal_element.rs` unconditionally calls `mouse_drag`
    /// whenever the cursor is hovered over the terminal at all (which a
    /// widget always is, since it's painted inside the terminal grid),
    /// starting a real text selection even for what the user experiences
    /// as a single click on the seek bar. `(session_id, file_id)` of the
    /// widget the click landed in, not just a bool — kept in case a
    /// future seek-drag needs to know which placement to keep seeking
    /// as the pointer moves, not just that "some" widget was clicked.
    rich_content_drag: Option<(u32, u32)>,
    #[cfg(windows)]
    shell_program: Option<String>,
    template: CopyTemplate,
    activation_script: Vec<String>,
    child_exited: Option<ExitStatus>,
    keyboard_input_sent: bool,
    /// Last (cols, rows) actually sent to the PTY. Used to suppress redundant
    /// SIGWINCH when only pixel metrics change but the grid size is identical.
    last_pty_grid_size: Option<(usize, usize)>,
    /// Progressive on-disk cache for Som's own rich-content protocol
    /// ([`rich_content_transport`]).
    rich_content_cache: rich_content_cache::RichContentCache,
    /// Paint-ready decode state for the same files `rich_content_cache`
    /// tracks bytes-on-disk for — keyed identically (`(session_id,
    /// file_id)`) and refreshed lazily at paint time via
    /// [`Self::rich_content_players`] rather than eagerly on every chunk
    /// arrival, since re-decoding a GIF prefix isn't free and most chunk
    /// arrivals don't need a fresh decode yet (see
    /// `rich_content_player::refresh_or_create`'s own doc comment on the
    /// reuse-when-unchanged fast path).
    rich_content_players:
        std::cell::RefCell<std::collections::HashMap<(u32, u32), rich_content_player::RichContentPlayer>>,
    /// Playback state for `ContentType::Audio` placements, keyed
    /// identically to `rich_content_players`. Kept in a SEPARATE map
    /// (not folded into `rich_content_players`) because a
    /// `RichContentAudioPlayer` owns a live `cpal` output stream whose
    /// lifetime must span many paints — unlike an image player, it
    /// can't be thrown away and rebuilt on every refresh (see that
    /// type's own doc comment). Once created for a given
    /// `(session_id, file_id)`, an entry here is never replaced, only
    /// mutated (play/pause/seek) — there is no "re-decode when more
    /// bytes arrive" story for audio the way there is for progressive
    /// GIF decoding.
    rich_content_audio_players:
        std::cell::RefCell<std::collections::HashMap<(u32, u32), rich_content_audio_player::RichContentAudioPlayer>>,
    /// Each rich-content placement's on-screen pixel bounds, as last
    /// computed by `terminal_element.rs`'s paint pass — nothing else
    /// persists this (the paint path itself only ever needs it as a
    /// local for the duration of one paint call). Written every paint
    /// via `Terminal::record_rich_content_placement_bounds`, read by
    /// `Terminal::mouse_down`'s click-interception check so a click can
    /// be tested against a placement's bounds outside of a paint pass.
    rich_content_placement_bounds: std::cell::RefCell<std::collections::HashMap<(u32, u32), Bounds<Pixels>>>,
    /// The audio widget's seek-bar bounds specifically — a strict
    /// sub-rectangle of the corresponding entry in
    /// `rich_content_placement_bounds` (the bar doesn't span the whole
    /// widget: it leaves room for the play/pause glyph on the left and
    /// the elapsed/total time text on the right, both variable-width).
    /// Click/drag fraction math (`handle_rich_content_click`/
    /// `mouse_drag`) MUST use this, not the widget's overall bounds —
    /// dividing by the full widget width previously made a click at the
    /// bar's visual midpoint compute a fraction far short of 0.5,
    /// confirmed live (clicking the middle of the bar seeked to roughly
    /// a third of the way through). Same write/read split as
    /// `rich_content_placement_bounds`: written every paint by
    /// `paint_rich_content_audio_widget`, read outside the paint pass
    /// for hit-testing.
    rich_content_seek_bar_bounds: std::cell::RefCell<std::collections::HashMap<(u32, u32), Bounds<Pixels>>>,
    /// Monotonically increasing counter for `request_audio_byte_range`'s
    /// `Query::request_id` — see that method's own doc comment for why
    /// correctness doesn't depend on global uniqueness here, just on
    /// being cheap and non-repeating enough to be useful for logging.
    next_query_request_id: std::cell::Cell<u32>,
    /// When `process_event`'s `ApcString` handler (or `terminal_element.rs`'s
    /// `paint()`, for animation-frame advancement) last called
    /// `cx.force_redraw_windows()` for a rich-content chunk/frame — throttles
    /// how often that forced native repaint actually fires (see
    /// `Terminal::rich_content_force_redraw_due`'s own doc comment for why
    /// `force_redraw` is needed at all on Windows). An animated GIF re-
    /// encoded by `crates/somcat` sends many chunks/frames in a burst —
    /// calling `force_redraw_windows()` on every single one back-to-back
    /// starves the window's message queue/PTY reader (confirmed live: a
    /// 47-frame GIF took 2+ minutes to finish uploading with no throttle).
    /// `None` means "never yet, redraw unconditionally on the next
    /// opportunity."
    last_rich_content_force_redraw: std::cell::Cell<Option<Instant>>,
    /// When this terminal was created. A sibling split pane can be resized
    /// (and thus SIGWINCH'd) purely as a side effect of `PaneGroup::split`
    /// restructuring the whole flex layout when a *new* split is added next
    /// to it — including while it's still mid-login. Many shells/servers
    /// react to SIGWINCH by repainting their prompt or MOTD, which then
    /// shows up as visibly duplicated banner text. Suppressing the PTY-side
    /// resize for a short window after creation avoids reaching a
    /// still-connecting remote at all; `last_pty_grid_size` is deliberately
    /// left unset while suppressed, so the next resize event sends the
    /// correct size once the window passes — `set_size` retries on its own
    /// (see the `pty_grid_stale` check there) rather than assuming `prepaint`
    /// will call it again soon, since that's false for a tab that isn't
    /// currently painted (parked, or simply not the active tab during
    /// restore). The local grid still resizes immediately either way, so
    /// rendering is never stale on the LOCAL side — only a remote PTY that
    /// hasn't yet been told about the real size can show stale content.
    created_at: Instant,
    event_loop_task: Task<Result<(), anyhow::Error>>,
    background_executor: BackgroundExecutor,
    path_style: PathStyle,
    #[cfg(any(test, feature = "test-support"))]
    input_log: Vec<Vec<u8>>,
}

struct CopyTemplate {
    shell: Shell,
    env: HashMap<String, String>,
    cursor_shape: CursorShape,
    alternate_scroll: AlternateScroll,
    max_scroll_history_lines: Option<usize>,
    path_hyperlink_regexes: Vec<String>,
    path_hyperlink_timeout_ms: u64,
    window_id: u64,
}

#[derive(Debug)]
pub struct TaskState {
    pub status: TaskStatus,
    pub completion_rx: Receiver<Option<ExitStatus>>,
    pub spawned_task: SpawnInTerminal,
}

/// A status of the current terminal tab's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// The task had been started, but got cancelled or somehow otherwise it did not
    /// report its exit code before the terminal event loop was shut down.
    Unknown,
    /// The task is started and running currently.
    Running,
    /// After the start, the task stopped running and reported its error code back.
    Completed { success: bool },
}

impl TaskStatus {
    fn register_terminal_exit(&mut self) {
        if self == &Self::Running {
            *self = Self::Unknown;
        }
    }

    fn register_task_exit(&mut self, error_code: i32) {
        *self = TaskStatus::Completed {
            success: error_code == 0,
        };
    }
}

const FIND_HYPERLINK_THROTTLE_PX: Pixels = px(5.0);

impl Terminal {
    fn process_event(&mut self, event: AlacTermEvent, cx: &mut Context<Self>) {
        match event {
            AlacTermEvent::Title(title) => {
                // ignore default shell program title change as windows always sends those events
                // and it would end up showing the shell executable path in breadcrumbs
                #[cfg(windows)]
                {
                    if self
                        .shell_program
                        .as_ref()
                        .map(|e| *e == title)
                        .unwrap_or(false)
                    {
                        return;
                    }
                }

                self.breadcrumb_text = title;
                cx.emit(Event::BreadcrumbsChanged);
            }
            AlacTermEvent::ResetTitle => {
                self.breadcrumb_text = String::new();
                cx.emit(Event::BreadcrumbsChanged);
            }
            AlacTermEvent::ClipboardStore(_, data) => {
                cx.write_to_clipboard(ClipboardItem::new_string(data))
            }
            AlacTermEvent::ClipboardLoad(_, format) => {
                self.write_to_pty(
                    match &cx.read_from_clipboard().and_then(|item| item.text()) {
                        // The terminal only supports pasting strings, not images.
                        Some(text) => format(text),
                        _ => format(""),
                    }
                    .into_bytes(),
                )
            }
            // A `tmux: true` RELAY's own `Term` sees the SAME raw PTY
            // bytes its HOLDER already parsed and answered on the real
            // PTY (som_tmux's `RawByteBroadcaster` mirrors bytes to every
            // connected RELAY before the HOLDER's own event loop finishes
            // handling them). If this RELAY's `Term` ALSO answers, the
            // client program (e.g. `yazi`) receives the query echoed back
            // to it a second time after it's already stopped expecting a
            // VT reply — which it then renders as literal on-screen text
            // instead of consuming as a control sequence. Confirmed root
            // cause of yazi's "Shell:"/"Find:" popups filling with raw
            // escape bytes in a `tmux: true` tab. This exactly mirrors
            // real tmux's own architecture — `input.c`'s `input_reply`
            // (DA1, DECRPM, cursor-position report, cell-pixel-size, etc.)
            // all answer on the SERVER side, against the real PTY, never
            // forwarded to the attaching client to answer a second time.
            // A HOLDER always answers these on the real PTY on its own
            // (see `som_tmux::session`'s permanent pump thread) whether or
            // not any RELAY is attached, so silently dropping them here
            // (rather than writing to a PTY som-tmux treats as a HOLDER-
            // facing pipe, not the real shell's PTY) is correct.
            //
            // This does NOT include mode-report bytes that only look like
            // ordinary `PtyWrite` output but actually carry SESSION STATE
            // (is mouse tracking on? is DECCKM on?) rather than a fixed
            // reply — those depend on the RELAY's own `Term`'s mode bits,
            // which a HOLDER's separate headless `Term` can't answer on
            // Som's behalf. Real tmux solves this the same way: `tty.c`'s
            // `tty_update_mode` re-emits the raw DECSET/DECRST bytes for
            // any mode bit that flipped, entirely SEPARATE from `input.c`'s
            // `input_reply`. `som_tmux::redraw::Redrawer` does the exact
            // same thing for `TermMode::APP_CURSOR`/mouse-tracking bits —
            // see its `last_app_cursor`/`last_mouse_mode` fields — so by
            // the time any capability query about THOSE modes could even
            // be parsed, the RELAY's own `Term` already has the right
            // mode bits from the Redrawer's explicit resend, independent
            // of this gate.
            AlacTermEvent::PtyWrite(out) => {
                if !self.is_tmux_relay_shell {
                    self.write_to_pty(out.into_bytes())
                }
            }
            // `\x1b[18t`/`\x1b[16t`-style "report text area size" queries
            // are DIFFERENT from `PtyWrite`'s general case: `alacritty_
            // terminal` doesn't stringify the reply itself (it can't know
            // the caller's actual GPUI pixel bounds) — instead `terminal::
            // Terminal` supplies its own `self.last_content.terminal_
            // bounds` here to build the answer, and a `tmux: true` HOLDER
            // has its own equally-authoritative real pane-size answer
            // ready via `som_tmux::session`'s `pump_last_bounds` (that's
            // what fixed a real `micro`-doesn't-fill-the-pane bug — see
            // that field's doc comment), so answering AGAIN here would
            // just be redundant, not wrong, but skipping it keeps this
            // one query type as one clean single reply instead of two.
            AlacTermEvent::TextAreaSizeRequest(format) => {
                if !self.is_tmux_relay_shell {
                    self.write_to_pty(format(self.last_content.terminal_bounds.into()).into_bytes())
                }
            }
            AlacTermEvent::CursorBlinkingChange => {
                let terminal = self.term.lock();
                let blinking = terminal.cursor_style().blinking;
                cx.emit(Event::BlinkChanged(blinking));
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit => self.register_task_finished(None, cx),
            AlacTermEvent::MouseCursorDirty => {
                //NOOP, Handled in render
            }
            AlacTermEvent::Wakeup => {
                cx.emit(Event::Wakeup);

                if let TerminalType::Pty { info, .. } = &self.terminal_type {
                    info.emit_title_changed_if_changed(cx);
                }
            }
            AlacTermEvent::ColorRequest(index, format) => {
                // Unlike `PtyWrite` (see its doc comment above), this
                // reply's CONTENT actually differs between Som and a
                // `tmux: true` HOLDER: this arm answers with the user's
                // real configured theme color (`cx.theme()`, right below),
                // while a HOLDER has no GPUI `Theme` to consult and falls
                // back to a hardcoded Nord Darker color (see `som_tmux::
                // session`'s `ColorRequest` arm). Always answering here —
                // same as the non-tmux path — means the client sees Som's
                // actual color even when a HOLDER's own (possibly
                // mismatched, if the user's theme isn't Nord Darker) reply
                // arrives first or second; a client that just uses
                // whichever reply lands first would otherwise sometimes
                // get the wrong one.
                // It's important that the color request is processed here to retain relative order
                // with other PTY writes. Otherwise applications might witness out-of-order
                // responses to requests. For example: An application sending `OSC 11 ; ? ST`
                // (color request) followed by `CSI c` (request device attributes) would receive
                // the response to `CSI c` first.
                // Instead of locking, we could store the colors in `self.last_content`. But then
                // we might respond with out of date value if a "set color" sequence is immediately
                // followed by a color request sequence.
                let color = self.term.lock().colors()[index]
                    .unwrap_or_else(|| to_alac_rgb(get_color_at_index(index, cx.theme().as_ref())));
                self.write_to_pty(format(color).into_bytes());
            }
            AlacTermEvent::ChildExit(exit_status) => {
                self.register_task_finished(Some(exit_status), cx);
            }
            AlacTermEvent::ApcString(bytes, _apc_cursor) => {
                // Som's own rich-content protocol (leading byte
                // `rich_content_transport::MARKER`, i.e. `S`) is currently
                // the only thing Som interprets APC strings as.
                if bytes.first().copied() == Some(rich_content_transport::MARKER) {
                    match rich_content_transport::parse_envelope(&bytes) {
                        Ok(chunk) => {
                            log::trace!(
                                "rich-content chunk parsed: content_type={:?} session={:#x} file={:#x} offset={} len={}",
                                chunk.content_type,
                                chunk.session_id,
                                chunk.file_id,
                                chunk.chunk_offset,
                                chunk.payload.len()
                            );
                            match self.rich_content_cache.apply_chunk(&chunk) {
                                Ok(_contiguous_len) => {
                                    // The Unicode-placeholder grid itself is
                                    // printed by the SENDING client (see
                                    // `somcat::print_placeholder_grid`), not
                                    // by Som — an earlier version had Som
                                    // inject the grid into its own terminal
                                    // model out-of-band once the transfer
                                    // completed, which left the real shell's
                                    // own cursor bookkeeping unaware of the
                                    // move (its line editor kept redrawing
                                    // from a stale remembered position,
                                    // visibly fighting the injected cursor
                                    // placement). Having the client print
                                    // through its own real stdout instead
                                    // means the shell sees ordinary child-
                                    // process output, with nothing to
                                    // reconcile. Som's only job here is
                                    // caching bytes and decoding frames for
                                    // whatever placeholder cells eventually
                                    // show up in the grid.
                                    cx.emit(Event::Wakeup);
                                },
                                Err(err) => {
                                    log::warn!("rich-content chunk write failed: {err}");
                                },
                            }
                        },
                        // A malformed envelope (checksum mismatch,
                        // truncated, wrong version) is dropped silently
                        // at trace level, not surfaced as a user-visible
                        // error — same tolerance principle as Kitty's own
                        // `parse_command` returning `None` for anything
                        // it doesn't recognize (see that function's doc
                        // comment): a single corrupted chunk shouldn't
                        // abort an otherwise-healthy progressive
                        // transfer, and a higher layer (not implemented
                        // yet) is expected to handle retransmission.
                        Err(err) => {
                            log::trace!("rich-content envelope failed to parse: {err}");
                        },
                    }
                    return;
                }
                // An unrecognized APC string (not SRP's marker byte) is
                // silently ignored — a program probing for protocol
                // support it thinks the terminal might answer is expected
                // behavior, not a bug, on a terminal that doesn't.
            }
        }
    }

    pub fn selection_started(&self) -> bool {
        self.selection_phase == SelectionPhase::Selecting
    }

    fn process_terminal_event(
        &mut self,
        event: &InternalEvent,
        term: &mut Term<ZedListener>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            &InternalEvent::Resize(mut new_bounds) => {
                trace!("Resizing: new_bounds={new_bounds:?}");
                new_bounds.bounds.size.height =
                    cmp::max(new_bounds.line_height, new_bounds.height());
                new_bounds.bounds.size.width = cmp::max(new_bounds.cell_width, new_bounds.width());

                self.last_content.terminal_bounds = new_bounds;

                // Only send a PTY resize (SIGWINCH) when the character grid actually
                // changes size. A resize with identical cols/rows but slightly different
                // pixel metrics would otherwise trigger a spurious SIGWINCH.
                let grid_size = (new_bounds.num_columns(), new_bounds.num_lines());
                let grid_size_changed = self.last_pty_grid_size != Some(grid_size);

                // Suppress SIGWINCH for a short window after creation — a
                // sibling split pane being added restructures the whole flex
                // layout and resizes every other pane in the tab, including
                // ones still mid-login. Many shells/servers repaint their
                // prompt or MOTD on SIGWINCH, which then shows up as visibly
                // duplicated banner text. The very first resize (establishing
                // the terminal's actual starting size) always goes through
                // regardless, since without it the shell negotiates with a
                // bogus default size.
                let suppress_for_grace_window =
                    self.last_pty_grid_size.is_some() && self.created_at.elapsed() < RESIZE_GRACE_PERIOD;

                if grid_size_changed
                    && !suppress_for_grace_window
                    && let TerminalType::Pty { pty_tx, .. } = &self.terminal_type
                {
                    self.last_pty_grid_size = Some(grid_size);
                    pty_tx.0.send(Msg::Resize(new_bounds.into())).ok();
                }

                // `reflow = false`: alacritty's built-in reflow (which
                // `Term::resize`'s default enables) splits any grid row
                // wider than the new column count into two rows BEFORE
                // this function's own resync below ever runs — SRP's
                // placeholder grids are exactly such wide, deliberately
                // unwrapped rows (one per pixel-row of the image), so
                // reflow silently grew the image's row count and
                // `history_size` inside alacritty itself on every narrow
                // resize, desyncing `resync_rich_content_placements`'s
                // tracked origin from the actual grid — confirmed live as
                // the image progressively sliding into scrollback
                // (`origin_line` growing more negative every resize step)
                // and, after enough steps, the cursor being walked to a
                // negative `Line`, panicking on the NEXT resize inside
                // alacritty's own `Grid::resize` (`self.lines -
                // cursor.point.line.0 as usize` underflowing). See
                // `Term::resize_with_reflow`'s doc comment (patched fork,
                // `errordnk/alacritty`) for the full tradeoff.
                term.resize_with_reflow(new_bounds, false);
                self.resync_rich_content_placements(term, new_bounds.cell_width, new_bounds.line_height);
                // If there are matches we need to emit a wake up event to
                // invalidate the matches and recalculate their locations
                // in the new terminal layout
                if !self.matches.is_empty() {
                    cx.emit(Event::Wakeup);
                }
            }
            InternalEvent::Clear => {
                trace!("Clearing");
                // Clear back buffer
                term.clear_screen(ClearMode::Saved);

                let cursor = term.grid().cursor.point;

                // Clear the lines above
                term.grid_mut().reset_region(..cursor.line);

                // Copy the current line up
                let line = term.grid()[cursor.line][..Column(term.grid().columns())]
                    .iter()
                    .cloned()
                    .enumerate()
                    .collect::<Vec<(usize, Cell)>>();

                for (i, cell) in line {
                    term.grid_mut()[Line(0)][Column(i)] = cell;
                }

                // Reset the cursor
                term.grid_mut().cursor.point =
                    AlacPoint::new(Line(0), term.grid_mut().cursor.point.column);
                let new_cursor = term.grid().cursor.point;

                // Clear the lines below the new cursor
                if (new_cursor.line.0 as usize) < term.screen_lines() - 1 {
                    term.grid_mut().reset_region((new_cursor.line + 1)..);
                }

                cx.emit(Event::Wakeup);
            }
            InternalEvent::Scroll(scroll) => {
                trace!("Scrolling: scroll={scroll:?}");
                term.scroll_display(*scroll);
                self.refresh_hovered_word(window);

                if self.vi_mode_enabled {
                    match *scroll {
                        AlacScroll::Delta(delta) => {
                            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, delta);
                        }
                        AlacScroll::PageUp => {
                            let lines = term.screen_lines() as i32;
                            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, lines);
                        }
                        AlacScroll::PageDown => {
                            let lines = -(term.screen_lines() as i32);
                            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, lines);
                        }
                        AlacScroll::Top => {
                            let point = AlacPoint::new(term.topmost_line(), Column(0));
                            term.vi_mode_cursor = ViModeCursor::new(point);
                        }
                        AlacScroll::Bottom => {
                            let point = AlacPoint::new(term.bottommost_line(), Column(0));
                            term.vi_mode_cursor = ViModeCursor::new(point);
                        }
                    }
                    if let Some(mut selection) = term.selection.take() {
                        let point = term.vi_mode_cursor.point;
                        selection.update(point, AlacDirection::Right);
                        term.selection = Some(selection);

                        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                        if let Some(selection_text) = term.selection_to_string() {
                            cx.write_to_primary(ClipboardItem::new_string(selection_text));
                        }

                        self.selection_head = Some(point);
                        cx.emit(Event::SelectionsChanged)
                    }
                }
            }
            InternalEvent::SetSelection(selection) => {
                trace!("Setting selection: selection={selection:?}");
                term.selection = selection.as_ref().map(|(sel, _)| sel.clone());

                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                if let Some(selection_text) = term.selection_to_string() {
                    cx.write_to_primary(ClipboardItem::new_string(selection_text));
                }

                if let Some((_, head)) = selection {
                    self.selection_head = Some(*head);
                }
                cx.emit(Event::SelectionsChanged)
            }
            InternalEvent::UpdateSelection(position) => {
                trace!("Updating selection: position={position:?}");
                if let Some(mut selection) = term.selection.take() {
                    let (point, side) = grid_point_and_side(
                        *position,
                        self.last_content.terminal_bounds,
                        term.grid().display_offset(),
                    );

                    selection.update(point, side);
                    term.selection = Some(selection);

                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    if let Some(selection_text) = term.selection_to_string() {
                        cx.write_to_primary(ClipboardItem::new_string(selection_text));
                    }

                    self.selection_head = Some(point);
                    cx.emit(Event::SelectionsChanged)
                }
            }

            InternalEvent::Copy(keep_selection) => {
                trace!("Copying selection: keep_selection={keep_selection:?}");
                if let Some(txt) = term.selection_to_string() {
                    cx.write_to_clipboard(ClipboardItem::new_string(txt));
                    if !keep_selection.unwrap_or_else(|| {
                        let settings = TerminalSettings::get_global(cx);
                        settings.keep_selection_on_copy
                    }) {
                        self.events.push_back(InternalEvent::SetSelection(None));
                    }
                }
            }
            InternalEvent::ScrollToAlacPoint(point) => {
                trace!("Scrolling to point: point={point:?}");
                term.scroll_to_point(*point);
                self.refresh_hovered_word(window);
            }
            InternalEvent::MoveViCursorToAlacPoint(point) => {
                trace!("Move vi cursor to point: point={point:?}");
                term.vi_goto_point(*point);
                self.refresh_hovered_word(window);
            }
            InternalEvent::ToggleViMode => {
                trace!("Toggling vi mode");
                self.vi_mode_enabled = !self.vi_mode_enabled;
                term.toggle_vi_mode();
            }
            InternalEvent::ViMotion(motion) => {
                trace!("Performing vi motion: motion={motion:?}");
                term.vi_motion(*motion);
            }
            InternalEvent::FindHyperlink(position, open) => {
                trace!("Finding hyperlink at position: position={position:?}, open={open:?}");

                let point = grid_point(
                    *position,
                    self.last_content.terminal_bounds,
                    term.grid().display_offset(),
                )
                .grid_clamp(term, Boundary::Grid);

                match terminal_hyperlinks::find_from_grid_point(
                    term,
                    point,
                    &mut self.hyperlink_regex_searches,
                    self.path_style,
                ) {
                    Some(hyperlink) => {
                        self.process_hyperlink(hyperlink, *open, cx);
                    }
                    None => {
                        self.last_content.last_hovered_word = None;
                        cx.emit(Event::NewNavigationTarget(None));
                    }
                }
            }
            InternalEvent::ProcessHyperlink(hyperlink, open) => {
                self.process_hyperlink(hyperlink.clone(), *open, cx);
            }
        }
    }

    fn process_hyperlink(
        &mut self,
        hyperlink: (String, bool, Match),
        open: bool,
        cx: &mut Context<Self>,
    ) {
        let (maybe_url_or_path, is_url, url_match) = hyperlink;
        let prev_hovered_word = self.last_content.last_hovered_word.take();

        let target = if is_url {
            if let Some(path) = maybe_url_or_path.strip_prefix("file://") {
                let decoded_path = urlencoding::decode(path)
                    .map(|decoded| decoded.into_owned())
                    .unwrap_or(path.to_owned());

                MaybeNavigationTarget::PathLike(PathLikeTarget {
                    maybe_path: decoded_path,
                    terminal_dir: self.working_directory(),
                })
            } else {
                MaybeNavigationTarget::Url(maybe_url_or_path.clone())
            }
        } else {
            MaybeNavigationTarget::PathLike(PathLikeTarget {
                maybe_path: maybe_url_or_path.clone(),
                terminal_dir: self.working_directory(),
            })
        };

        if open {
            cx.emit(Event::Open(target));
        } else {
            self.update_selected_word(prev_hovered_word, url_match, maybe_url_or_path, target, cx);
        }
    }

    fn update_selected_word(
        &mut self,
        prev_word: Option<HoveredWord>,
        word_match: RangeInclusive<AlacPoint>,
        word: String,
        navigation_target: MaybeNavigationTarget,
        cx: &mut Context<Self>,
    ) {
        if let Some(prev_word) = prev_word
            && prev_word.word == word
            && prev_word.word_match == word_match
        {
            self.last_content.last_hovered_word = Some(HoveredWord {
                word,
                word_match,
                id: prev_word.id,
            });
            return;
        }

        self.last_content.last_hovered_word = Some(HoveredWord {
            word,
            word_match,
            id: self.next_link_id(),
        });
        cx.emit(Event::NewNavigationTarget(Some(navigation_target)));
        cx.notify()
    }

    fn next_link_id(&mut self) -> usize {
        let res = self.next_link_id;
        self.next_link_id = self.next_link_id.wrapping_add(1);
        res
    }

    pub fn last_content(&self) -> &TerminalContent {
        &self.last_content
    }

    pub fn set_cursor_shape(&mut self, cursor_shape: CursorShape) {
        self.term_config.default_cursor_style = cursor_shape.into();
        self.term.lock().set_options(self.term_config.clone());
    }

    pub fn write_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        // Inject bytes directly into the terminal emulator and refresh the UI.
        // This bypasses the PTY/event loop for display-only terminals.
        //
        // We first convert LF to CRLF, to get the expected line wrapping in Alacritty.
        // When output comes from piped commands (not a PTY) such as codex-acp, and that
        // output only contains LF (\n) without a CR (\r) after it, such as the output
        // of the `ls` command when running outside a PTY, Alacritty moves the cursor
        // cursor down a line but does not move it back to the initial column. This makes
        // the rendered output look ridiculous. To prevent this, we insert a CR (\r) before
        // each LF that didn't already have one. (Alacritty doesn't have a setting for this.)
        //
        // NOT applied while inside an active APC string (`ESC _` ... `ESC \`)
        // — found the hard way (`test_rich_content_many_real_chunks_via_
        // write_output_reconstructs_file_exactly`, isolating Som's own
        // binary rich-content protocol, see `rich_content_transport`'s
        // module doc comment): this blind LF->CRLF rewrite corrupts ANY
        // binary APC payload containing a bare `0x0A` byte, inserting an
        // extra `0x0D` right in the middle of a conveyed byte stream that
        // has nothing to do with line wrapping at all — APC content is not
        // line-oriented text and must pass through completely unmodified.
        // A minimal state tracker (only `ESC _` entering, `ESC \` leaving)
        // is enough here — this is a text-injection helper for
        // display-only terminals, not a second VTE parser; it only needs
        // to know "am I inside an APC string right now," not parse
        // anything about its contents.
        let mut converted = Vec::with_capacity(bytes.len());
        let mut prev_byte = 0u8;
        let mut prev_prev_byte = 0u8;
        let mut in_apc_string = false;
        for &byte in bytes {
            if in_apc_string {
                if prev_byte == 0x1B && byte == b'\\' {
                    in_apc_string = false;
                }
            } else if prev_prev_byte == 0x1B && prev_byte == b'_' {
                in_apc_string = true;
            }

            if byte == b'\n' && prev_byte != b'\r' && !in_apc_string {
                converted.push(b'\r');
            }
            converted.push(byte);
            prev_prev_byte = prev_byte;
            prev_byte = byte;
        }

        {
            let mut term = self.term.lock();
            // Persistent parser (see `write_output_parser`) — a sequence
            // split across two calls has to resume, not restart.
            self.write_output_parser.advance(&mut *term, &converted);
        }
        cx.emit(Event::Wakeup);
    }

    /// Atomically checks-and-marks whether `FORCE_REDRAW_MIN_INTERVAL` has
    /// elapsed since the last forced rich-content redraw, via a `Cell` (not
    /// `Entity::update` — see `terminal_element.rs`'s `paint()` for why:
    /// `paint()` only ever has `&self`/read access to the `Entity<Terminal>`
    /// it's painting, and going through `Entity::update` from inside an
    /// in-progress paint call was tried and reverted — it broke even the
    /// FIRST frame from ever appearing, confirmed live). Shared across both
    /// chunk-arrival forced redraws and animation-frame-advance forced
    /// redraws, so there's exactly one throttle window instead of two
    /// independent budgets that could each individually stay "due" and
    /// double the effective redraw rate.
    pub fn rich_content_force_redraw_due(&self) -> bool {
        let now = Instant::now();
        let due = self
            .last_rich_content_force_redraw
            .get()
            .is_none_or(|last| now.duration_since(last) >= FORCE_REDRAW_MIN_INTERVAL);
        if due {
            self.last_rich_content_force_redraw.set(Some(now));
        }
        due
    }

    /// Every rich-content file with a currently paintable (at least one
    /// decoded frame) player, refreshing each against
    /// `rich_content_cache`'s latest `contiguous_len` first — a `&self`
    /// method (not `&mut self`) despite refreshing internal state, using
    /// `RefCell` the same way `kitty_graphics_store::DecodedImage`'s
    /// `animation: Cell<...>` does, for the identical reason: a paint pass
    /// only ever has read (`Entity::read`) access to the `Terminal` it's
    /// painting (see `paint_kitty_placements`'s own doc comment on why
    /// `Entity::update` from inside `paint()` was tried and reverted).
    ///
    /// No position/anchor here at all — unlike the earlier cursor-relative
    /// design, SRP placements are now real Unicode-placeholder grid cells
    /// (see `Terminal::print_rich_content_placeholder_grid`), so the
    /// caller (`terminal_element.rs`'s placeholder-scanning paint function,
    /// modeled on `paint_kitty_unicode_placeholders`) finds each
    /// placement's position by scanning the grid itself, the same way it
    /// already finds Kitty's own Stage 4 placements — this method only
    /// answers "does a decoded frame exist for this id, and what is it."
    ///
    /// Returns owned `(session_id, file_id, Arc<RenderImage>, current_frame,
    /// is_animating)` tuples rather than borrowed references into the
    /// `RefCell` — cloning an `Arc` is cheap (no pixel data copied) and
    /// sidesteps holding a `Ref` alive across the caller's own paint loop.
    pub fn rich_content_placements(
        &self,
    ) -> Vec<(u32, u32, std::sync::Arc<gpui::RenderImage>, usize, bool)> {
        let mut players = self.rich_content_players.borrow_mut();
        let mut out = Vec::new();
        for (session_id, file_id) in self.rich_content_cache.all_known_ids() {
            let Some(path) = self.rich_content_cache.path(session_id, file_id) else {
                continue;
            };
            let contiguous_len = self.rich_content_cache.contiguous_len(session_id, file_id);
            let Some(content_type) = self.rich_content_cache.content_type(session_id, file_id) else {
                continue;
            };
            let total_size = self.rich_content_cache.total_size(session_id, file_id);
            let key = (session_id, file_id);
            let existing = players.remove(&key);
            let Some(refreshed) =
                rich_content_player::refresh_or_create(existing, path, contiguous_len, content_type, total_size)
            else {
                continue;
            };
            let current_frame = refreshed.current_frame();
            let is_animating = refreshed.is_animating();
            let render_image = refreshed.render_image().clone();
            players.insert(key, refreshed);
            out.push((session_id, file_id, render_image, current_frame, is_animating));
        }
        out
    }

    /// See [`crate::rich_content_cache::RichContentCache::record_max_column_seen`].
    pub fn record_rich_content_max_column_seen(&self, session_id: u32, file_id: u32, column: u32) {
        self.rich_content_cache.record_max_column_seen(session_id, file_id, column);
    }

    /// Every `ContentType::Audio` placement with a currently open player,
    /// opening a new one (decoding the whole cached file) the first time
    /// a given `(session_id, file_id)` is fully cached — never re-decoded
    /// or replaced after that (see
    /// [`rich_content_audio_player::RichContentAudioPlayer`]'s own doc
    /// comment for why: it owns a live device stream, unlike an image
    /// player). Mirrors [`Self::rich_content_placements`]'s shape (scan
    /// `rich_content_cache.all_known_ids()`, refresh lazily at paint
    /// time) but returns playback state instead of a decoded frame, and
    /// deliberately does NOT remove-then-reinsert into the map the way
    /// `rich_content_placements` does — the player itself, not just a
    /// snapshot of it, needs to persist so play/pause/seek calls in
    /// between two paints (from `Terminal::mouse_down`) actually stick.
    pub fn rich_content_audio_placements(&self) -> Vec<(u32, u32, f32, bool, std::time::Duration, std::time::Duration)> {
        let mut players = self.rich_content_audio_players.borrow_mut();
        let mut out = Vec::new();
        for (session_id, file_id) in self.rich_content_cache.all_known_ids() {
            let Some(content_type) = self.rich_content_cache.content_type(session_id, file_id) else {
                continue;
            };
            if content_type != rich_content_transport::ContentType::Audio {
                continue;
            }
            let key = (session_id, file_id);
            if !players.contains_key(&key) {
                let Some(path) = self.rich_content_cache.path(session_id, file_id) else {
                    continue;
                };
                let total_size = self.rich_content_cache.total_size(session_id, file_id);
                let contiguous_len = self.rich_content_cache.contiguous_len(session_id, file_id);
                // Same "wait for the whole file" gate the JPEG/PNG branch
                // of `rich_content_player::refresh_or_create` uses — no
                // progressive-prefix decode story exists for mp3/flac
                // (see `rich_content_audio_player`'s module doc comment).
                if total_size == 0 || contiguous_len < total_size {
                    continue;
                }
                match rich_content_audio_player::RichContentAudioPlayer::open(path) {
                    Ok(player) => {
                        players.insert(key, player);
                    },
                    // A genuine decode/device error — silently skip, same
                    // tolerance principle every other rich-content decode
                    // path in this codebase already uses (a single
                    // unplayable file shouldn't need special paint-path
                    // error handling).
                    Err(err) => {
                        log::warn!("audio placement {session_id}:{file_id} failed to open: {err}");
                        continue;
                    },
                }
            }
            let player = players.get(&key).expect("just inserted or already present");
            out.push((
                session_id,
                file_id,
                player.position_fraction(),
                player.is_playing(),
                player.elapsed(),
                player.duration(),
            ));
        }
        out
    }

    /// Toggles play/pause for the audio placement at `(session_id,
    /// file_id)`, if one is currently open — a no-op otherwise (e.g. the
    /// click landed on a placeholder whose player hasn't been created
    /// yet because the file isn't fully cached). Called from
    /// `Terminal::mouse_down`'s click-interception check, never from the
    /// paint path itself.
    pub fn toggle_rich_content_audio_playback(&self, session_id: u32, file_id: u32) {
        if let Some(player) = self.rich_content_audio_players.borrow().get(&(session_id, file_id)) {
            player.toggle_play_pause();
        }
    }

    /// Seeks the audio placement at `(session_id, file_id)` to `fraction`
    /// (0.0..=1.0) of its total length, if a player is currently open.
    pub fn seek_rich_content_audio_playback(&self, session_id: u32, file_id: u32, fraction: f32) {
        if let Some(player) = self.rich_content_audio_players.borrow().get(&(session_id, file_id)) {
            player.seek_to_fraction(fraction);
        }
    }

    /// Asks the SENDING client for a specific byte range of a rich-
    /// content file it's already streaming (or has streamed) over this
    /// same `(session_id, file_id)` — used when playback needs bytes
    /// further into a large file than the sequential stream has reached
    /// yet (e.g. seeking forward in audio past what's currently cached).
    ///
    /// Uses the same low-level `write_to_pty` primitive Som already uses
    /// to ANSWER a client's own `CSI 16 t`/`CSI 18 t` size queries
    /// (`process_event`'s `TextAreaSizeRequest` arm) — that's a plain
    /// "write these bytes to the child process's stdin over the PTY"
    /// call, direction-agnostic; this is that same primitive used the
    /// other way around, Som asking the client something instead of
    /// answering it. See `rich_content_transport::Query`'s own doc
    /// comment for why the response doesn't need a dedicated reply
    /// marker: the client answers with ordinary chunk envelopes (the
    /// same kind `RichContentCache` already accepts out of sequential
    /// order) for the requested range, not a new envelope shape.
    ///
    /// `request_id` is caller-chosen and has no correctness requirement
    /// here (Som's own audio-range use doesn't need to correlate a
    /// specific response back to a specific request — the response
    /// lands in the same cache slot as the rest of the file regardless)
    /// — a monotonically increasing counter, good enough to be useful
    /// for logging/debugging without needing to be globally unique.
    pub fn request_audio_byte_range(&self, session_id: u32, file_id: u32, offset: u64, len: u64) {
        let request_id = self.next_query_request_id.get();
        self.next_query_request_id.set(request_id.wrapping_add(1));
        let query = rich_content_transport::Query { request_id, session_id, file_id, offset, len };
        let envelope = rich_content_transport::build_query_envelope(&query);
        let mut apc = Vec::with_capacity(2 + envelope.len() + 2);
        apc.extend_from_slice(&[0x1B, b'_']); // ESC _
        apc.extend_from_slice(&envelope);
        apc.extend_from_slice(&[0x1B, b'\\']); // ESC \
        self.write_to_pty(apc);
    }

    /// Persists `bounds` as the last-painted pixel bounds for placement
    /// `(session_id, file_id)` — see
    /// [`Self::rich_content_placement_bounds`]'s own doc comment for why
    /// this needs to outlive a single paint call. Called from
    /// `terminal_element.rs`'s paint pass for EVERY placement (not just
    /// audio ones), since hit-testing is a general mechanism even though
    /// audio is its first consumer.
    pub fn record_rich_content_placement_bounds(&self, session_id: u32, file_id: u32, bounds: Bounds<Pixels>) {
        self.rich_content_placement_bounds.borrow_mut().insert((session_id, file_id), bounds);
    }

    /// The last-painted pixel bounds for placement `(session_id,
    /// file_id)`, if it was visible on the most recent paint. `None`
    /// means either the placement doesn't exist or scrolled fully out of
    /// view since its bounds were last recorded (stale-but-absent is
    /// treated the same as never-existed — a click can't land on
    /// something that isn't currently on screen).
    pub fn rich_content_placement_bounds(&self, session_id: u32, file_id: u32) -> Option<Bounds<Pixels>> {
        self.rich_content_placement_bounds.borrow().get(&(session_id, file_id)).copied()
    }

    /// Persists `bounds` as the audio widget's actual seek-bar
    /// rectangle — see [`Self::rich_content_seek_bar_bounds`]'s own doc
    /// comment for why this is tracked separately from the widget's
    /// overall bounds.
    pub fn record_rich_content_seek_bar_bounds(&self, session_id: u32, file_id: u32, bounds: Bounds<Pixels>) {
        self.rich_content_seek_bar_bounds.borrow_mut().insert((session_id, file_id), bounds);
    }

    /// The last-painted seek-bar bounds for placement `(session_id,
    /// file_id)`, if recorded — `None` for a placement that has no
    /// seek bar at all (not audio) or hasn't painted one yet.
    pub fn rich_content_seek_bar_bounds(&self, session_id: u32, file_id: u32) -> Option<Bounds<Pixels>> {
        self.rich_content_seek_bar_bounds.borrow().get(&(session_id, file_id)).copied()
    }

    /// Every currently recorded placement's `(session_id, file_id,
    /// bounds)` — used by `Terminal::mouse_down`'s click-interception
    /// check to find which (if any) placement a click landed inside,
    /// without needing to know the id in advance.
    pub fn rich_content_placement_bounds_iter(&self) -> Vec<(u32, u32, Bounds<Pixels>)> {
        self.rich_content_placement_bounds.borrow().iter().map(|(&(s, f), &b)| (s, f, b)).collect()
    }

    /// Tests `position` (absolute window pixel coordinates, same space
    /// [`Self::record_rich_content_placement_bounds`] stores — see
    /// `mouse_down`'s call site) against every currently recorded audio
    /// placement's bounds. Returns `true` (and performs the click's
    /// effect locally) if it landed inside one, `false` otherwise — the
    /// `false` case lets `mouse_down` fall through to its normal
    /// selection/hyperlink/PTY-forwarding logic unchanged, exactly like
    /// the existing hyperlink early-return already does for a different
    /// kind of click.
    ///
    /// Only audio placements are hit-tested here (not every rich-content
    /// placement — an image placement has no interactive zones at all
    /// yet). The leftmost 2 cells of a widget are its play/pause zone
    /// (matches `paint_rich_content_audio_widget`'s glyph + 1-cell
    /// padding layout); anywhere else inside the bounds falls through to
    /// [`Self::rich_content_seek_bar_bounds`] to compute the seek
    /// fraction from the bar's OWN rectangle, not the whole widget's —
    /// the bar never spans the full widget width (it leaves room for
    /// the glyph on the left and the elapsed/total time text on the
    /// right), so using the widget's width as the fraction's denominator
    /// under-shot every click: clicking the visual middle of the bar
    /// seeked to roughly a third of the way through, confirmed live.
    fn handle_rich_content_click(&mut self, position: gpui::Point<Pixels>) -> bool {
        let cell_width = self.last_content.terminal_bounds.cell_width;
        for (session_id, file_id, bounds) in self.rich_content_placement_bounds_iter() {
            if !self.rich_content_audio_players.borrow().contains_key(&(session_id, file_id)) {
                continue;
            }
            if !bounds.contains(&position) {
                continue;
            }
            // Remembered for the rest of this click (`mouse_drag`/
            // `mouse_up`) — without this, GPUI's own mouse-move handler
            // (`terminal_element.rs`'s `register_mouse_listeners`) starts
            // an ordinary terminal text selection on ANY mouse movement
            // while hovered over the terminal at all, which a widget
            // always is (it's painted inside the terminal grid) — a
            // plain click on the seek bar was visibly highlighting text
            // underneath/around the widget before this field existed to
            // suppress it, confirmed live.
            self.rich_content_drag = Some((session_id, file_id));
            let offset_x = position.x - bounds.origin.x;
            if offset_x < cell_width * 2.0 {
                self.toggle_rich_content_audio_playback(session_id, file_id);
            } else if let Some(fraction) = self.seek_fraction_for_position(session_id, file_id, position) {
                self.seek_rich_content_audio_playback(session_id, file_id, fraction);
            }
            return true;
        }
        false
    }

    /// Computes a seek fraction (0.0..=1.0) from `position`'s horizontal
    /// offset within the recorded seek-bar bounds for `(session_id,
    /// file_id)` — shared by `handle_rich_content_click` and
    /// `mouse_drag` so both use the exact same geometry. `None` if no
    /// bar bounds have been recorded yet (widget painted this frame for
    /// the first time, before `record_rich_content_seek_bar_bounds` ran)
    /// — a click landing before the bar exists just does nothing, rather
    /// than guessing at a fraction from the wrong rectangle.
    fn seek_fraction_for_position(&self, session_id: u32, file_id: u32, position: gpui::Point<Pixels>) -> Option<f32> {
        let bar_bounds = self.rich_content_seek_bar_bounds(session_id, file_id)?;
        let offset_x = position.x - bar_bounds.origin.x;
        Some((f32::from(offset_x) / f32::from(bar_bounds.size.width)).clamp(0.0, 1.0))
    }

    /// Re-derives and rewrites, IN PLACE, every rich-content placeholder
    /// grid currently on screen so it matches the new cell pixel size
    /// (window resize, font size change, or a `lineHeight`/`padding`
    /// settings change — anything that changes `cell_width`/`line_height`
    /// takes this same `InternalEvent::Resize` path). Only rewrites the
    /// diacritics that encode each cell's (row, column) plus whatever
    /// plain grid CONTENT needs to move to make/close room (via
    /// `Grid::scroll_up`, the exact same mechanism ordinary scrolled text
    /// output already relies on) — never the placeholder character
    /// itself, never `term.grid_mut().cursor` directly, never any escape
    /// sequence sent anywhere.
    ///
    /// The cursor restriction is load-bearing, not just tidiness: the real
    /// child process behind the PTY (the shell) keeps its own, entirely
    /// separate model of where its cursor is, and is NEVER informed when
    /// this function edits the grid directly — ConPTY's API has no
    /// "shift my internal buffer's cursor row" operation, only resize
    /// (columns/rows). An earlier version of this function DID patch
    /// `cursor.point.line` directly to visually pin the prompt right after
    /// a resized image — this made the on-screen cursor glyph land in the
    /// correct spot immediately after a resize, but desynced Som's local
    /// idea of the cursor from the shell's own the moment the shell itself
    /// printed anything (e.g. echoing a keystroke): the shell wrote
    /// relative to ITS row, not Som's patched one, so typed characters
    /// visibly appeared on a different row than the cursor glyph —
    /// confirmed live, with the error compounding across repeated
    /// resizes as the two models drifted further apart on every growth/
    /// shrink cycle. This is deliberately NOT done by re-sending text
    /// through `write_output`/the PTY either: that path has the identical
    /// problem (see SRP_PROTOCOL.md's "Печать грида переехала из Som в
    /// somcat" section for the exact symptom that caused). Leaving the
    /// cursor wherever the real shell already believes it is means the
    /// prompt won't always visually snap to sit exactly below a freshly
    /// resized image — but it will never desync from what the shell
    /// itself is about to print next.
    ///
    /// Scope, deliberately limited (see SRP_PROTOCOL.md's "Пересчёт
    /// placement'ов при ресайзе окна/шрифта" section for the full
    /// reasoning):
    /// - Column count can shrink OR grow. Growth writes into whatever
    ///   cells are to the right of the old bounding box — this CAN
    ///   overwrite unrelated content that happened to share those rows (no
    ///   equivalent of "insert columns" exists for the horizontal
    ///   direction, unlike rows below).
    /// - Row count can also shrink OR grow — shrinking removes the extra
    ///   rows and pulls everything below the image up to close the gap;
    ///   growing inserts new rows and pushes everything below the image
    ///   down, via a manual row-by-row copy (see the inline comments
    ///   further down this function for why neither `Grid::scroll_up` nor
    ///   `scroll_down` alone can do this without moving the image itself
    ///   or losing content). Row growth is only attempted when the
    ///   placement starts on the currently visible screen
    ///   (`origin_line >= 0`) — a placement already partway into
    ///   scrollback is left at its old row count instead, since safely
    ///   inserting rows at an arbitrary point inside scrollback would need
    ///   much more involved buffer surgery than this function does.
    ///
    /// The scan itself covers the WHOLE buffer (scrollback included, not
    /// just the currently visible screen) — confirmed live that scanning
    /// only the viewport left cells still in scrollback with their OLD
    /// (pre-resize) diacritics, so scrolling to reveal them fed the paint
    /// path's `record_rich_content_max_column_seen` (monotonic, never
    /// shrinks) a stale, wider column count than this function had just
    /// narrowed things to — the on-screen image visibly jumped in size on
    /// every scroll after a resize until this was widened.
    fn resync_rich_content_placements(
        &self,
        term: &mut Term<ZedListener>,
        cell_width: gpui::Pixels,
        line_height: gpui::Pixels,
    ) {
        use kitty_graphics_placeholder::{PLACEHOLDER_CHAR, PlaceholderCell, decode_placeholder_cell, encode_cell};

        let terminal_columns = term.columns() as u32;
        let terminal_lines = term.screen_lines() as u32;
        if terminal_columns == 0 || terminal_lines == 0 {
            return;
        }

        // Scan the ENTIRE buffer (scrollback included), not just the
        // currently visible screen. A placement can straddle the
        // scrollback/viewport boundary (e.g. the window was scrolled up
        // when the resize happened), and every cell of the same group
        // MUST end up carrying consistent diacritics — rewriting only
        // whatever was on screen at the moment of resize left cells still
        // in scrollback with their OLD (pre-resize) row/column encoding,
        // so scrolling to reveal them fed the paint path's
        // `record_rich_content_max_column_seen` (monotonic, never
        // shrinks) a stale, wider column count than what this function
        // just narrowed things to — confirmed live as the image's on-
        // screen size visibly jumping every time the user scrolled after
        // a resize.
        let top = term.topmost_line().0;
        let bottom = term.bottommost_line().0;
        let columns = term.columns();

        // Keyed by (session_id, file_id, origin_line, origin_column) — NOT
        // just (session_id, file_id). The same image printed twice (e.g.
        // once, then again after the terminal was resized in between)
        // produces two physically separate blocks of placeholder cells
        // that happen to share the same id; every cell decodes its own
        // (row, column) OFFSET within its block, so two cells from
        // DIFFERENT blocks can easily decode to the same (row, column)
        // (both blocks' top-left corners decode as (0, 0)). Keying only
        // on id merged both blocks' cells into one incoherent group whose
        // `cells` mixed points from two unrelated screen locations —
        // confirmed live as a crash when resizing after printing the same
        // image twice, and reproduced in
        // `test_resize_growth_with_two_placement_groups_of_same_image_does_not_panic`.
        // Each cell's own (line - row, column - column) is exact and
        // cheap to compute up front, unlike a flood-fill over grid
        // adjacency, and correctly separates two blocks even if they sit
        // right next to each other with no gap.
        let mut groups: std::collections::HashMap<(u32, u32, i32, i32), (u32, u32, Vec<(AlacPoint, u32, u32)>)> =
            std::collections::HashMap::new();

        for line_idx in top..=bottom {
            let line = Line(line_idx);
            for col_idx in 0..columns {
                let point = AlacPoint::new(line, Column(col_idx));
                let cell = &term.grid()[point];
                if cell.c != PLACEHOLDER_CHAR {
                    continue;
                }
                let fg_rgb = match cell.fg {
                    alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                    _ => continue,
                };
                let underline_rgb = match cell.underline_color() {
                    Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                    Some(_) => continue,
                    None => None,
                };
                let diacritics = cell.zerowidth().unwrap_or(&[]);
                let Some(PlaceholderCell { image_id: session_id, placement_id: file_id, row, column }) =
                    decode_placeholder_cell(cell.c, fg_rgb, underline_rgb, diacritics)
                else {
                    continue;
                };

                let origin_line = line_idx - row as i32;
                let origin_column = col_idx as i32 - column as i32;
                let entry = groups
                    .entry((session_id, file_id, origin_line, origin_column))
                    .or_insert((0, 0, Vec::new()));
                entry.0 = entry.0.max(row);
                entry.1 = entry.1.max(column);
                entry.2.push((point, row, column));
            }
        }

        // Groups shrinking vertically are handled top-to-bottom, and each
        // one's row-deletion shifts every line below it up — later
        // groups' `origin_line` (computed fresh from THIS pass's own
        // scan, before any shifting) would go stale after an earlier
        // group's shift, so placements are sorted here to process from
        // the top of the buffer down, matching the order shifts actually
        // happen in.
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_key(|(_, (_, _, cells))| cells.iter().map(|(p, r, _)| p.line.0 - *r as i32).min());

        // Accumulates every `shift` applied by an EARLIER (higher on
        // screen, processed first per the sort above) group's row-growth
        // `scroll_up` of the whole screen. All of `groups`' cell
        // coordinates were captured in ONE scan before this loop started —
        // an earlier group's `scroll_up` physically moves every row on
        // screen, including a later, not-yet-processed group's cells, so
        // without correcting for it here, a later group's `origin_line`
        // would be computed from stale (pre-shift) coordinates. Confirmed
        // live as a crash (`Grid::compute_index`'s `requested.0 <
        // visible_lines` assertion) in
        // `test_resize_growth_with_two_placement_groups_of_same_image_does_not_panic`
        // once two same-id groups both needed row growth in the same
        // resize pass.
        let mut accumulated_shift = 0i32;

        for ((session_id, file_id, _, _), (old_max_row, old_max_column, cells)) in groups {
            let Some((width_px, height_px)) = self.rich_content_cache.image_size_px(session_id, file_id) else {
                continue;
            };
            if f32::from(cell_width) <= 0.0 || f32::from(line_height) <= 0.0 {
                continue;
            }

            let old_columns = old_max_column + 1;
            let old_rows = old_max_row + 1;
            let cells: Vec<_> = cells
                .into_iter()
                .map(|(point, row, column)| {
                    (AlacPoint::new(Line(point.line.0 - accumulated_shift), point.column), row, column)
                })
                .collect();
            // The placement's absolute top line — every cell's grid line
            // minus its own decoded row must agree (same reasoning as the
            // paint path's origin math), so the first cell is equally
            // authoritative.
            let Some(&(first_point, first_row, _)) = cells.first() else { continue };
            let origin_line = first_point.line.0 - first_row as i32;

            let mut new_columns = (width_px as f32 / f32::from(cell_width)).ceil().max(1.0) as u32;
            let mut new_rows = (height_px as f32 / f32::from(line_height)).ceil().max(1.0) as u32;

            // Never wider than the terminal itself (same reasoning as
            // `somcat::print_placeholder_grid`'s clamp — a grid wider than
            // the terminal gets silently wrapped mid-row by the terminal,
            // scrambling every cell's decoded position), scaling height
            // down to preserve aspect ratio if clamped.
            if new_columns > terminal_columns {
                let scale = terminal_columns as f32 / new_columns as f32;
                new_columns = terminal_columns.max(1);
                new_rows = ((new_rows as f32 * scale).floor() as u32).max(1);
            }

            // Also never taller than the SCREEN, counting from wherever
            // this placement actually starts — a placement must always
            // leave at least one real row free for whatever comes after
            // it (the shell's next prompt), never fill all the way to
            // `bottommost_line()`. Using a flat `terminal_lines - 1` here
            // (as if every placement started at `Line(0)`) undercounted
            // how tall a placement starting further down the screen
            // (`origin_line > 0`, the common case — e.g. the shell's own
            // banner/output printed before the image) is actually allowed
            // to grow: `origin_line + new_rows` could still land exactly
            // on `bottommost_line()`, leaving the "room for a prompt"
            // guarantee only true for placements starting at the very top
            // — confirmed live as the prompt ending up hidden BEHIND the
            // image (cursor stuck inside the placement's own row range)
            // once the image grew enough to reach the screen edge from a
            // nonzero starting row. A placement already in scrollback
            // (`origin_line < 0`) is left using the flat bound, since row
            // growth isn't attempted there anyway (see the `origin_line >=
            // 0` guard further down).
            let max_rows = if origin_line >= 0 {
                (terminal_lines as i32 - origin_line - 1).max(1) as u32
            } else {
                terminal_lines.saturating_sub(1).max(1)
            };
            if new_rows > max_rows {
                let scale = max_rows as f32 / new_rows as f32;
                new_rows = max_rows;
                new_columns = ((new_columns as f32 * scale).floor() as u32).max(1);
            }
            // Column growth writes into whatever cells are to the right of
            // the old bounding box — accepted risk of overwriting unrelated
            // content that happened to share those rows (no equivalent of
            // "insert columns" exists the way row insertion below works
            // for the vertical direction).
            let _ = old_columns;

            // Row growth needs actual new lines inserted into the grid —
            // `new_rows` cells beyond `old_rows` didn't exist in `cells`
            // at all (they were never part of the placement before this
            // resize). Neither `Grid::scroll_up` nor `scroll_down` alone
            // can do this without ALSO moving the image itself or losing
            // whatever's below it (every region-based rotation touches
            // its whole region, not just content below one specific
            // point) — done here as a manual, row-by-row copy instead:
            // read every row from the old bottom edge down to the
            // buffer's end, write it back `inserted_rows` further down
            // (iterating from the BOTTOM up so a row is never overwritten
            // before it's been read), leaving the freed rows blank for
            // the placement's new cells to fill in below. Only attempted
            // when the placement starts on the visible screen
            // (`origin_line >= 0`) — a placement already partway into
            // scrollback would need the same treatment applied there too,
            // which this doesn't handle (see SRP_PROTOCOL.md).
            let inserted_rows = new_rows.saturating_sub(old_rows);
            // Every remaining use of `origin_line`/`cells` below this point
            // must agree with whatever the grid ACTUALLY looks like right
            // now — `scroll_up` (used to make room for growth) shifts
            // EVERY row on screen, including the image's own, so both are
            // corrected here once, up front, rather than threading a
            // "did we already shift" flag through the rest of this
            // function.
            let mut origin_line = origin_line;
            let mut cells = cells;
            if inserted_rows > 0 && origin_line >= 0 {
                let bottom = term.bottommost_line().0;
                let old_bottom = origin_line + old_rows as i32;
                // Only shift the screen up by however many rows are
                // ACTUALLY missing below the image's old bottom edge — not
                // unconditionally by `inserted_rows`. A tall, mostly-empty
                // screen already has spare rows below the image; shifting
                // the whole screen up into scrollback in that case is
                // needless churn that also pushed the image further into
                // scrollback than necessary. Only the shortfall — whatever
                // doesn't fit in the existing spare rows — needs to make
                // room via `Grid::scroll_up` with `region.start == 0`
                // (which grows `history_size`, bounded by
                // `MAX_SCROLL_HISTORY_LINES`, the same mechanism ordinary
                // text printing already relies on — nothing is lost, it
                // safely becomes scrollback).
                let spare_rows_below = (bottom - old_bottom + 1).max(0);
                let shift = inserted_rows.saturating_sub(spare_rows_below as u32);
                if shift > 0 {
                    term.grid_mut().scroll_up(&(Line(0)..Line(bottom + 1)), shift as usize);
                    origin_line -= shift as i32;
                    cells = cells
                        .into_iter()
                        .map(|(point, row, column)| {
                            (AlacPoint::new(Line(point.line.0 - shift as i32), point.column), row, column)
                        })
                        .collect();
                    accumulated_shift += shift as i32;
                    // Deliberately NOT touching `term.grid_mut().cursor`
                    // anywhere in this function — see the doc comment
                    // above `resync_rich_content_placements` for why. This
                    // `scroll_up` call is safe regardless: it moves grid
                    // CONTENT the same way a normal full screen of output
                    // scrolling would, which the real shell already
                    // expects and handles on its own (it's watching the
                    // PTY's SIGWINCH-driven column/row count, not
                    // Som-internal placement bookkeeping).
                }
                let old_bottom = old_bottom - shift as i32;
                let new_bottom = origin_line + new_rows as i32;
                // How far the row-by-row copy below actually needs to move
                // content: the distance from the image's (already
                // screen-shift-corrected) old bottom edge to its new one —
                // NOT `inserted_rows` directly. Those two coincide only
                // when `shift == inserted_rows` (screen had no spare rows
                // at all); whenever the screen had SOME spare room below
                // the image, `shift < inserted_rows`, and copying by the
                // full `inserted_rows` would push content past
                // `bottommost_line()` needlessly — confirmed live as a
                // panic inside alacritty's own `Grid`
                // (`compute_index`'s `requested.0 < visible_lines`
                // assertion) once `shift` stopped always equaling
                // `inserted_rows`.
                let copy_distance = new_bottom - old_bottom;
                let mut line = bottom;
                while line >= old_bottom {
                    let target = line + copy_distance;
                    // Content at a `target` beyond `bottom` was already
                    // pushed into scrollback by the `scroll_up` above (if
                    // any) — nothing left on screen at `line` worth
                    // copying to a `target` that doesn't exist. Without
                    // this guard, `target` can exceed `bottom`, and
                    // indexing the grid at a nonexistent line panics —
                    // confirmed live as a real crash on a large resize.
                    if target <= bottom {
                        let row = term.grid()[Line(line)].clone();
                        term.grid_mut()[Line(target)] = row;
                    }
                    line -= 1;
                }
                // The freed rows (immediately below the image's OLD
                // bottom edge) must be blanked — otherwise they'd still
                // hold whatever content used to be there before being
                // copied further down above.
                let template = term.grid().cursor.template.clone();
                for line in old_bottom..(old_bottom + copy_distance) {
                    term.grid_mut()[Line(line)].reset(&template);
                }
            } else if inserted_rows > 0 {
                // Placement starts in scrollback — row growth not
                // attempted here (see doc comment above), clamp back down
                // to what already existed.
                new_rows = old_rows;
            }

            // A cell's own color (fg for session_id, underline for
            // file_id) is what identifies the placement's id — reused
            // below when synthesizing brand-new cells for grown rows/
            // columns that didn't exist in `cells` at all (nothing to
            // read an old cell's color back from for those).
            let Some((sample_point, ..)) = cells.first().copied() else { continue };
            let sample_cell = term.grid()[sample_point].clone();

            for (point, row, column) in &cells {
                let (point, row, column) = (*point, *row, *column);
                // `point` is still valid here even after the row-insertion
                // pass above: that pass only ever shifted rows AT OR BELOW
                // the image's OLD bottom edge (`old_bottom`) — cells
                // belonging to the image itself (`row < old_rows`) sit
                // strictly above that line and were never touched by it.
                if row >= new_rows || column >= new_columns {
                    // Outside the new, shrunk bounding box — blank it so no
                    // stale placeholder cell lingers past the image's new
                    // extent.
                    term.grid_mut()[point] = Cell::default();
                    continue;
                }
                let Some([_, row_diacritic, col_diacritic]) = encode_cell(row, column) else { continue };
                // `Cell` has no "clear just the zerowidth diacritics"
                // method (`CellExtra`'s fields are private, and
                // `clear_wide` also resets `c`/flags, which must stay
                // untouched here) — rebuild a fresh `Cell` carrying over
                // `c`/`fg`/`bg`/`flags`/underline color from the existing
                // one, with only the two diacritics freshly pushed.
                let old_cell = term.grid()[point].clone();
                let mut cell = Cell { c: old_cell.c, fg: old_cell.fg, bg: old_cell.bg, flags: old_cell.flags, extra: None };
                cell.set_underline_color(old_cell.underline_color());
                cell.push_zerowidth(row_diacritic);
                cell.push_zerowidth(col_diacritic);
                term.grid_mut()[point] = cell;
            }

            // New rows/columns beyond the OLD bounding box weren't in
            // `cells` at all — write brand-new placeholder cells for
            // them, reusing `sample_cell`'s color (the placement's id)
            // and character attributes.
            for row in 0..new_rows {
                for column in 0..new_columns {
                    if row < old_rows && column < old_columns {
                        continue; // Already handled above.
                    }
                    let Some([placeholder_char, row_diacritic, col_diacritic]) = encode_cell(row, column) else {
                        continue;
                    };
                    let point = AlacPoint::new(Line(origin_line + row as i32), Column(column as usize));
                    let mut cell = Cell {
                        c: placeholder_char,
                        fg: sample_cell.fg,
                        bg: sample_cell.bg,
                        flags: sample_cell.flags,
                        extra: None,
                    };
                    cell.set_underline_color(sample_cell.underline_color());
                    cell.push_zerowidth(row_diacritic);
                    cell.push_zerowidth(col_diacritic);
                    term.grid_mut()[point] = cell;
                }
            }

            self.rich_content_cache.reset_max_column_seen(session_id, file_id, new_columns.saturating_sub(1));

            // Height shrank — close the gap the removed rows would
            // otherwise leave behind (blank space between the image and
            // whatever was printed after it, e.g. the next prompt),
            // exactly like a real terminal's own `DL` (delete lines)
            // control function: shift everything below the image's new,
            // shorter extent up by the number of rows removed. Uses
            // `Grid::scroll_up` directly (not the `Handler::delete_lines`
            // trait method, which anchors its region to the CURSOR's
            // current line — wrong here, since resize can run with the
            // cursor anywhere, e.g. still sitting at the shell's prompt
            // well below the image) — the region is anchored to the
            // image's own new bottom edge instead. `saturating_sub`
            // because `new_rows` can now be LARGER than `old_rows` (row
            // growth, handled separately below) — this branch is simply a
            // no-op in that case.
            let removed_rows = old_rows.saturating_sub(new_rows);
            if removed_rows > 0 {
                let region_start = Line(origin_line + new_rows as i32);
                let region_end = Line(term.bottommost_line().0 + 1);
                if region_start < region_end {
                    term.grid_mut().scroll_up(&(region_start..region_end), removed_rows as usize);
                    // Deliberately NOT touching `term.grid_mut().cursor`
                    // here — see the doc comment above
                    // `resync_rich_content_placements` for why: the real
                    // child process behind the PTY keeps its own,
                    // completely separate idea of where the cursor is,
                    // and never learns about this direct grid edit.
                    // Patching Som's local `cursor.point` to "compensate"
                    // made the VISIBLE cursor land in the right spot right
                    // after a resize, but the moment the shell itself
                    // printed anything (e.g. echoing a keystroke), it did
                    // so relative to ITS OWN cursor row — which this
                    // function had silently moved out from under it —
                    // confirmed live as typed characters appearing on a
                    // different row than the visible cursor, with the
                    // error compounding across repeated resizes as the
                    // two models drifted further apart. Only touching the
                    // GRID CONTENT here (the same `scroll_up` a normal
                    // full screen of scrolled text output already relies
                    // on) keeps the picture-is-just-text invariant
                    // intact; the cursor now stays wherever the real
                    // shell already believes it is.
                }
            }
        }
    }

    /// See [`crate::rich_content_cache::RichContentCache::max_column_seen`].
    pub fn rich_content_max_column_seen(&self, session_id: u32, file_id: u32) -> Option<u32> {
        self.rich_content_cache.max_column_seen(session_id, file_id)
    }

    pub fn total_lines(&self) -> usize {
        self.term.lock_unfair().total_lines()
    }

    pub fn viewport_lines(&self) -> usize {
        self.term.lock_unfair().screen_lines()
    }

    /// Current `Term::history_size()`, read fresh — NOT the value cached in
    /// `last_content` (which only refreshes during a real paint pass via
    /// `sync()`; a headless test that never triggers one would otherwise see
    /// a stale/default snapshot). Mirrors `total_lines`/`viewport_lines`
    /// above for the same reason.
    pub fn history_size(&self) -> usize {
        self.term.lock_unfair().history_size()
    }

    //To test:
    //- Activate match on terminal (scrolling and selection)
    //- Editor search snapping behavior

    pub fn activate_match(&mut self, index: usize) {
        if let Some(search_match) = self.matches.get(index).cloned() {
            self.set_selection(Some((make_selection(&search_match), *search_match.end())));
            if self.vi_mode_enabled {
                self.events
                    .push_back(InternalEvent::MoveViCursorToAlacPoint(*search_match.end()));
            } else {
                self.events
                    .push_back(InternalEvent::ScrollToAlacPoint(*search_match.start()));
            }
        }
    }

    pub fn select_matches(&mut self, matches: &[RangeInclusive<AlacPoint>]) {
        let matches_to_select = self
            .matches
            .iter()
            .filter(|self_match| matches.contains(self_match))
            .cloned()
            .collect::<Vec<_>>();
        for match_to_select in matches_to_select {
            self.set_selection(Some((
                make_selection(&match_to_select),
                *match_to_select.end(),
            )));
        }
    }

    pub fn select_all(&mut self) {
        let term = self.term.lock();
        let start = AlacPoint::new(term.topmost_line(), Column(0));
        let end = AlacPoint::new(term.bottommost_line(), term.last_column());
        drop(term);
        self.set_selection(Some((make_selection(&(start..=end)), end)));
    }

    fn set_selection(&mut self, selection: Option<(Selection, AlacPoint)>) {
        self.events
            .push_back(InternalEvent::SetSelection(selection));
    }

    pub fn copy(&mut self, keep_selection: Option<bool>) {
        self.events.push_back(InternalEvent::Copy(keep_selection));
    }

    pub fn clear(&mut self) {
        self.events.push_back(InternalEvent::Clear)
    }

    pub fn scroll_line_up(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Delta(1)));
    }

    pub fn scroll_up_by(&mut self, lines: usize) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Delta(lines as i32)));
    }

    pub fn scroll_line_down(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Delta(-1)));
    }

    pub fn scroll_down_by(&mut self, lines: usize) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Delta(-(lines as i32))));
    }

    pub fn scroll_page_up(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::PageUp));
    }

    pub fn scroll_page_down(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::PageDown));
    }

    pub fn scroll_to_top(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Top));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Bottom));
    }

    pub fn scrolled_to_top(&self) -> bool {
        self.last_content.scrolled_to_top
    }

    pub fn scrolled_to_bottom(&self) -> bool {
        self.last_content.scrolled_to_bottom
    }

    ///Resize the terminal and the PTY.
    pub fn set_size(&mut self, new_bounds: TerminalBounds) {
        let mut new_bounds = new_bounds;
        new_bounds.bounds.size.height = cmp::max(new_bounds.line_height, new_bounds.height());
        new_bounds.bounds.size.width = cmp::max(new_bounds.cell_width, new_bounds.width());

        let old_bounds = self.last_content.terminal_bounds;
        self.last_content.terminal_bounds = new_bounds;

        // Avoid spamming PTY resizes on pixel-level size changes (e.g. while dragging edges),
        // since those can generate excessive SIGWINCH/reflows and cause visible flicker.
        let requires_resize = old_bounds.num_lines() != new_bounds.num_lines()
            || old_bounds.num_columns() != new_bounds.num_columns()
            || old_bounds.cell_width != new_bounds.cell_width
            || old_bounds.line_height != new_bounds.line_height;

        // A grid-size change during the creation grace period (see
        // `created_at`) intentionally skips sending SIGWINCH, banking on
        // "there's always another resize event soon" to flush the real size
        // once the grace period passes. That assumption breaks for a tab
        // that isn't currently painted (parked during restore, or simply not
        // the active tab) — `prepaint` never runs for it, so `set_size` is
        // never called again, and the PTY is left believing the terminal is
        // still whatever size it was BEFORE the grace-period resizes that
        // got suppressed. A remote program (e.g. `htop` over SSH) then never
        // repaints for its true size, leaving stale content on screen until
        // something else (a manual window resize) finally triggers another
        // `set_size` call. Retry here instead of only in the normal
        // requires_resize path: if the grid size no longer matches what was
        // last actually sent to the PTY, queue a resize regardless of
        // whether pixel bounds changed since the last call — `sync` re-checks
        // the grace period itself, so this is a no-op until it has elapsed.
        let grid_size = (new_bounds.num_columns(), new_bounds.num_lines());
        let pty_grid_stale = self.last_pty_grid_size.is_some_and(|s| s != grid_size);

        if !requires_resize && !pty_grid_stale {
            return;
        }

        match self.events.back_mut() {
            Some(InternalEvent::Resize(pending_bounds)) => *pending_bounds = new_bounds,
            _ => self.events.push_back(InternalEvent::Resize(new_bounds)),
        }
    }

    /// Write the Input payload to the PTY, if applicable.
    /// (This is a no-op for display-only terminals.)
    fn write_to_pty(&self, input: impl Into<Cow<'static, [u8]>>) {
        if let TerminalType::Pty { pty_tx, .. } = &self.terminal_type {
            let input = input.into();
            if log::log_enabled!(log::Level::Debug) {
                if let Ok(str) = str::from_utf8(&input) {
                    log::debug!("Writing to PTY: {:?}", str);
                } else {
                    log::debug!("Writing to PTY: {:?}", input);
                }
            }
            pty_tx.notify(input);
        }
    }

    pub fn input(&mut self, input: impl Into<Cow<'static, [u8]>>) {
        self.events
            .push_back(InternalEvent::Scroll(AlacScroll::Bottom));
        self.events.push_back(InternalEvent::SetSelection(None));

        self.keyboard_input_sent = true;
        let input = input.into();
        #[cfg(any(test, feature = "test-support"))]
        self.input_log.push(input.to_vec());

        self.write_to_pty(input);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn take_input_log(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.input_log)
    }

    pub fn toggle_vi_mode(&mut self) {
        self.events.push_back(InternalEvent::ToggleViMode);
    }

    pub fn vi_motion(&mut self, keystroke: &Keystroke) {
        if !self.vi_mode_enabled {
            return;
        }

        let key: Cow<'_, str> = if keystroke.modifiers.shift {
            Cow::Owned(keystroke.key.to_uppercase())
        } else {
            Cow::Borrowed(keystroke.key.as_str())
        };

        let motion: Option<ViMotion> = match key.as_ref() {
            "h" | "left" => Some(ViMotion::Left),
            "j" | "down" => Some(ViMotion::Down),
            "k" | "up" => Some(ViMotion::Up),
            "l" | "right" => Some(ViMotion::Right),
            "w" => Some(ViMotion::WordRight),
            "b" if !keystroke.modifiers.control => Some(ViMotion::WordLeft),
            "e" => Some(ViMotion::WordRightEnd),
            "%" => Some(ViMotion::Bracket),
            "$" => Some(ViMotion::Last),
            "0" => Some(ViMotion::First),
            "^" => Some(ViMotion::FirstOccupied),
            "H" => Some(ViMotion::High),
            "M" => Some(ViMotion::Middle),
            "L" => Some(ViMotion::Low),
            _ => None,
        };

        if let Some(motion) = motion {
            let cursor = self.last_content.cursor.point;
            let cursor_pos = Point {
                x: cursor.column.0 as f32 * self.last_content.terminal_bounds.cell_width,
                y: cursor.line.0 as f32 * self.last_content.terminal_bounds.line_height,
            };
            self.events
                .push_back(InternalEvent::UpdateSelection(cursor_pos));
            self.events.push_back(InternalEvent::ViMotion(motion));
            return;
        }

        let scroll_motion = match key.as_ref() {
            "g" => Some(AlacScroll::Top),
            "G" => Some(AlacScroll::Bottom),
            "b" if keystroke.modifiers.control => Some(AlacScroll::PageUp),
            "f" if keystroke.modifiers.control => Some(AlacScroll::PageDown),
            "d" if keystroke.modifiers.control => {
                let amount = self.last_content.terminal_bounds.line_height().to_f64() as i32 / 2;
                Some(AlacScroll::Delta(-amount))
            }
            "u" if keystroke.modifiers.control => {
                let amount = self.last_content.terminal_bounds.line_height().to_f64() as i32 / 2;
                Some(AlacScroll::Delta(amount))
            }
            _ => None,
        };

        if let Some(scroll_motion) = scroll_motion {
            self.events.push_back(InternalEvent::Scroll(scroll_motion));
            return;
        }

        match key.as_ref() {
            "v" => {
                let point = self.last_content.cursor.point;
                let selection_type = SelectionType::Simple;
                let side = AlacDirection::Right;
                let selection = Selection::new(selection_type, point, side);
                self.events
                    .push_back(InternalEvent::SetSelection(Some((selection, point))));
            }

            "escape" => {
                self.events.push_back(InternalEvent::SetSelection(None));
            }

            "y" => {
                self.copy(Some(false));
            }

            "i" => {
                self.scroll_to_bottom();
                self.toggle_vi_mode();
            }
            _ => {}
        }
    }

    pub fn try_keystroke(&mut self, keystroke: &Keystroke, option_as_meta: bool) -> bool {
        if self.vi_mode_enabled {
            self.vi_motion(keystroke);
            return true;
        }

        // Keep default terminal behavior
        let esc = to_esc_str(keystroke, &self.last_content.mode, option_as_meta);
        if let Some(esc) = esc {
            match esc {
                Cow::Borrowed(string) => self.input(string.as_bytes()),
                Cow::Owned(string) => self.input(string.into_bytes()),
            };
            true
        } else {
            false
        }
    }

    pub fn try_modifiers_change(
        &mut self,
        modifiers: &Modifiers,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .last_content
            .terminal_bounds
            .bounds
            .contains(&window.mouse_position())
            && modifiers.secondary()
        {
            self.refresh_hovered_word(window);
        }
        cx.notify();
    }

    ///Paste text into the terminal
    pub fn paste(&mut self, text: &str) {
        let paste_text = if self.last_content.mode.contains(TermMode::BRACKETED_PASTE) {
            format!("{}{}{}", "\x1b[200~", text.replace('\x1b', ""), "\x1b[201~")
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };

        self.input(paste_text.into_bytes());
    }

    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let term = self.term.clone();
        // `lock()` (fair), NOT `lock_unfair()` — this runs on the render
        // thread, on every single paint, and `FairMutex`'s whole fairness
        // guarantee (`lease()`/`lock()` queuing through the `next` mutex)
        // only holds between callers that BOTH go through `lock()`. A
        // `lock_unfair()` caller bypasses `next` entirely and can grab
        // `data` out from under a PTY reader thread that's already queued
        // via `lease()`, no matter how long that reader has been waiting —
        // confirmed as the actual cause of a real animated GIF's
        // transmission (`somcat` through Kitty's graphics protocol,
        // dozens of KB of base64 in a sustained burst) reliably stalling
        // partway through in a real Som window, while an equivalent
        // headless test (identical PTY/EventLoop code, but no real
        // render thread ever calling this method at all) completed
        // reliably: `pty_read`'s `try_lock_unfair()` failing while this
        // was held doesn't block — it `continue`s and reads MORE bytes
        // into its buffer instead of parking — so a render thread that
        // reliably wins the unfair race back-to-back can starve the
        // reader for however long it takes `unprocessed` to hit
        // `READ_BUFFER_SIZE` (1MB) and force a genuinely blocking
        // `lock_unfair()`, over and over, every single paint.
        let mut terminal = term.lock();
        //Note that the ordering of events matters for event processing
        while let Some(e) = self.events.pop_front() {
            self.process_terminal_event(&e, &mut terminal, window, cx)
        }

        self.last_content = Self::make_content(&terminal, &self.last_content);
    }

    fn make_content(term: &Term<ZedListener>, last_content: &TerminalContent) -> TerminalContent {
        let content = term.renderable_content();

        // Pre-allocate with estimated size to reduce reallocations
        let estimated_size = content.display_iter.size_hint().0;
        let mut cells = Vec::with_capacity(estimated_size);

        cells.extend(content.display_iter.map(|ic| IndexedCell {
            point: ic.point,
            cell: ic.cell.clone(),
        }));

        let selection_text = if content.selection.is_some() {
            term.selection_to_string()
        } else {
            None
        };

        TerminalContent {
            cells,
            mode: content.mode,
            display_offset: content.display_offset,
            selection_text,
            selection: content.selection,
            cursor: content.cursor,
            cursor_char: term.grid()[content.cursor.point].c,
            terminal_bounds: last_content.terminal_bounds,
            last_hovered_word: last_content.last_hovered_word.clone(),
            scrolled_to_top: content.display_offset == term.history_size(),
            scrolled_to_bottom: content.display_offset == 0,
        }
    }

    pub fn get_content(&self) -> String {
        let term = self.term.lock_unfair();
        let start = AlacPoint::new(term.topmost_line(), Column(0));
        let end = AlacPoint::new(term.bottommost_line(), term.last_column());
        term.bounds_to_string(start, end)
    }

    pub fn last_n_non_empty_lines(&self, n: usize) -> Vec<String> {
        let term = self.term.clone();
        let terminal = term.lock_unfair();
        let grid = terminal.grid();
        let mut lines = Vec::new();

        let mut current_line = grid.bottommost_line().0;
        let topmost_line = grid.topmost_line().0;

        while current_line >= topmost_line && lines.len() < n {
            let logical_line_start = self.find_logical_line_start(grid, current_line, topmost_line);
            let logical_line = self.construct_logical_line(grid, logical_line_start, current_line);

            if let Some(line) = self.process_line(logical_line) {
                lines.push(line);
            }

            // Move to the line above the start of the current logical line
            current_line = logical_line_start - 1;
        }

        lines.reverse();
        lines
    }

    fn find_logical_line_start(&self, grid: &Grid<Cell>, current: i32, topmost: i32) -> i32 {
        let mut line_start = current;
        while line_start > topmost {
            let prev_line = Line(line_start - 1);
            let last_cell = &grid[prev_line][Column(grid.columns() - 1)];
            if !last_cell.flags.contains(Flags::WRAPLINE) {
                break;
            }
            line_start -= 1;
        }
        line_start
    }

    fn construct_logical_line(&self, grid: &Grid<Cell>, start: i32, end: i32) -> String {
        let mut logical_line = String::new();
        for row in start..=end {
            let grid_row = &grid[Line(row)];
            logical_line.push_str(&row_to_string(grid_row));
        }
        logical_line
    }

    fn process_line(&self, line: String) -> Option<String> {
        let trimmed = line.trim_end().to_string();
        if !trimmed.is_empty() {
            Some(trimmed)
        } else {
            None
        }
    }

    pub fn focus_in(&self) {
        if self.last_content.mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[I".as_bytes());
        }
    }

    pub fn focus_out(&mut self) {
        if self.last_content.mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[O".as_bytes());
        }
    }

    pub fn mouse_changed(&mut self, point: AlacPoint, side: AlacDirection) -> bool {
        match self.last_mouse {
            Some((old_point, old_side)) => {
                if old_point == point && old_side == side {
                    false
                } else {
                    self.last_mouse = Some((point, side));
                    true
                }
            }
            None => {
                self.last_mouse = Some((point, side));
                true
            }
        }
    }

    pub fn mouse_mode(&self, shift: bool) -> bool {
        self.last_content.mode.intersects(TermMode::MOUSE_MODE) && !shift
    }

    pub fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if self.mouse_mode(e.modifiers.shift) {
            let (point, side) = grid_point_and_side(
                position,
                self.last_content.terminal_bounds,
                self.last_content.display_offset,
            );

            if self.mouse_changed(point, side)
                && let Some(bytes) =
                    mouse_moved_report(point, e.pressed_button, e.modifiers, self.last_content.mode)
            {
                self.write_to_pty(bytes);
            }
        } else {
            self.schedule_find_hyperlink(e.modifiers, e.position);
        }
        cx.notify();
    }

    fn schedule_find_hyperlink(&mut self, modifiers: Modifiers, position: Point<Pixels>) {
        if self.selection_phase == SelectionPhase::Selecting
            || !modifiers.secondary()
            || !self.last_content.terminal_bounds.bounds.contains(&position)
        {
            self.last_content.last_hovered_word = None;
            return;
        }

        // Throttle hyperlink searches to avoid excessive processing
        let now = Instant::now();
        if self
            .last_hyperlink_search_position
            .map_or(true, |last_pos| {
                // Only search if mouse moved significantly or enough time passed
                let distance_moved = ((position.x - last_pos.x).abs()
                    + (position.y - last_pos.y).abs())
                    > FIND_HYPERLINK_THROTTLE_PX;
                let time_elapsed = now.duration_since(self.last_mouse_move_time).as_millis() > 100;
                distance_moved || time_elapsed
            })
        {
            self.last_mouse_move_time = now;
            self.last_hyperlink_search_position = Some(position);
            self.events.push_back(InternalEvent::FindHyperlink(
                position - self.last_content.terminal_bounds.bounds.origin,
                false,
            ));
        }
    }

    pub fn select_word_at_event_position(&mut self, e: &MouseDownEvent) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        let (point, side) = grid_point_and_side(
            position,
            self.last_content.terminal_bounds,
            self.last_content.display_offset,
        );
        let selection = Selection::new(SelectionType::Semantic, point, side);
        self.events
            .push_back(InternalEvent::SetSelection(Some((selection, point))));
    }

    pub fn mouse_drag(
        &mut self,
        e: &MouseMoveEvent,
        region: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // A click that started inside a rich-content widget (see
        // `handle_rich_content_click`) never starts a text selection,
        // no matter how far the pointer moves afterward — same
        // "swallow the whole gesture" reasoning as the hyperlink-range
        // check just below for a different kind of click. If the drag
        // is still over the seek bar, keep seeking to follow the
        // pointer (a natural scrub gesture); once it isn't, just do
        // nothing rather than falling through to selection logic.
        if let Some((session_id, file_id)) = self.rich_content_drag {
            if let Some(bounds) = self.rich_content_placement_bounds(session_id, file_id)
                && bounds.contains(&e.position)
            {
                let cell_width = self.last_content.terminal_bounds.cell_width;
                let offset_x = e.position.x - bounds.origin.x;
                if offset_x >= cell_width * 2.0
                    && let Some(fraction) = self.seek_fraction_for_position(session_id, file_id, e.position)
                {
                    self.seek_rich_content_audio_playback(session_id, file_id, fraction);
                    cx.notify();
                }
            }
            return;
        }

        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if !self.mouse_mode(e.modifiers.shift) {
            if let Some((.., hyperlink_range)) = &self.mouse_down_hyperlink {
                let point = grid_point(
                    position,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if !hyperlink_range.contains(&point) {
                    self.mouse_down_hyperlink = None;
                } else {
                    return;
                }
            }

            self.selection_phase = SelectionPhase::Selecting;
            // Alacritty has the same ordering, of first updating the selection
            // then scrolling 15ms later
            self.events
                .push_back(InternalEvent::UpdateSelection(position));

            // Doesn't make sense to scroll the alt screen
            if !self.last_content.mode.contains(TermMode::ALT_SCREEN) {
                let scroll_lines = match self.drag_line_delta(e, region) {
                    Some(value) => value,
                    None => return,
                };

                self.events
                    .push_back(InternalEvent::Scroll(AlacScroll::Delta(scroll_lines)));
            }

            cx.notify();
        }
    }

    fn drag_line_delta(&self, e: &MouseMoveEvent, region: Bounds<Pixels>) -> Option<i32> {
        let top = region.origin.y;
        let bottom = region.bottom_left().y;

        let scroll_lines = if e.position.y < top {
            let scroll_delta = (top - e.position.y).pow(1.1);
            (scroll_delta / self.last_content.terminal_bounds.line_height).ceil() as i32
        } else if e.position.y > bottom {
            let scroll_delta = -((e.position.y - bottom).pow(1.1));
            (scroll_delta / self.last_content.terminal_bounds.line_height).floor() as i32
        } else {
            return None;
        };

        Some(scroll_lines.clamp(-3, 3))
    }

    pub fn mouse_down(&mut self, e: &MouseDownEvent, _cx: &mut Context<Self>) {
        // Rich-content placement hit-testing — checked BEFORE any
        // selection/hyperlink/PTY-forwarding logic below, mirroring the
        // existing hyperlink early-return pattern a few lines down. A
        // click landing inside an audio widget's play/pause zone or seek
        // bar is handled entirely locally and never reaches the (possibly
        // remote, over SSH) child process — see `rich_content_placement_
        // bounds`'s own doc comment for why these bounds are in the same
        // absolute-window pixel space as `e.position` (both come from
        // `terminal_element.rs`'s paint pass using the same `origin`), so
        // no coordinate translation is needed here, unlike the grid-point
        // math below.
        if e.button == MouseButton::Left && self.handle_rich_content_click(e.position) {
            return;
        }

        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        let point = grid_point(
            position,
            self.last_content.terminal_bounds,
            self.last_content.display_offset,
        );

        if e.button == MouseButton::Left
            && e.modifiers.secondary()
            && !self.mouse_mode(e.modifiers.shift)
        {
            let term_lock = self.term.lock();
            self.mouse_down_hyperlink = terminal_hyperlinks::find_from_grid_point(
                &term_lock,
                point,
                &mut self.hyperlink_regex_searches,
                self.path_style,
            );
            drop(term_lock);

            if self.mouse_down_hyperlink.is_some() {
                return;
            }
        }

        if self.mouse_mode(e.modifiers.shift) {
            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, true, self.last_content.mode)
            {
                self.write_to_pty(bytes);
            }
        } else {
            match e.button {
                MouseButton::Left => {
                    let (point, side) = grid_point_and_side(
                        position,
                        self.last_content.terminal_bounds,
                        self.last_content.display_offset,
                    );

                    let selection_type = match e.click_count {
                        0 => return, //This is a release
                        1 => Some(SelectionType::Simple),
                        2 => Some(SelectionType::Semantic),
                        3 => Some(SelectionType::Lines),
                        _ => None,
                    };

                    if selection_type == Some(SelectionType::Simple) && e.modifiers.shift {
                        self.events
                            .push_back(InternalEvent::UpdateSelection(position));
                        return;
                    }

                    let selection = selection_type
                        .map(|selection_type| Selection::new(selection_type, point, side));

                    if let Some(sel) = selection {
                        self.events
                            .push_back(InternalEvent::SetSelection(Some((sel, point))));
                    }
                }
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                MouseButton::Middle => {
                    if let Some(item) = _cx.read_from_primary() {
                        let text = item.text().unwrap_or_default();
                        self.paste(&text);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn mouse_up(&mut self, e: &MouseUpEvent, cx: &Context<Self>) {
        // Ends the click started in `handle_rich_content_click` — mirrors
        // `mouse_down_hyperlink.take()`'s identical cleanup a few lines
        // below for hyperlink clicks. Consuming it here (not just
        // reading it) means the NEXT click starts fresh, same as every
        // other per-click piece of state on this type.
        if self.rich_content_drag.take().is_some() {
            return;
        }

        let setting = TerminalSettings::get_global(cx);

        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if self.mouse_mode(e.modifiers.shift) {
            let point = grid_point(
                position,
                self.last_content.terminal_bounds,
                self.last_content.display_offset,
            );

            if let Some(bytes) =
                mouse_button_report(point, e.button, e.modifiers, false, self.last_content.mode)
            {
                self.write_to_pty(bytes);
            }
        } else {
            if e.button == MouseButton::Left && setting.copy_on_select {
                self.copy(Some(true));
            }

            if let Some(mouse_down_hyperlink) = self.mouse_down_hyperlink.take() {
                let point = grid_point(
                    position,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if let Some(mouse_up_hyperlink) = {
                    let term_lock = self.term.lock();
                    terminal_hyperlinks::find_from_grid_point(
                        &term_lock,
                        point,
                        &mut self.hyperlink_regex_searches,
                        self.path_style,
                    )
                } {
                    if mouse_down_hyperlink == mouse_up_hyperlink {
                        self.events
                            .push_back(InternalEvent::ProcessHyperlink(mouse_up_hyperlink, true));
                        self.selection_phase = SelectionPhase::Ended;
                        self.last_mouse = None;
                        return;
                    }
                }
            }

            //Hyperlinks
            if self.selection_phase == SelectionPhase::Ended {
                let mouse_cell_index =
                    content_index_for_mouse(position, &self.last_content.terminal_bounds);
                if let Some(link) = self.last_content.cells[mouse_cell_index].hyperlink() {
                    cx.open_url(link.uri());
                } else if e.modifiers.secondary() {
                    self.events
                        .push_back(InternalEvent::FindHyperlink(position, true));
                }
            }
        }

        self.selection_phase = SelectionPhase::Ended;
        self.last_mouse = None;
    }

    ///Scroll the terminal
    pub fn scroll_wheel(&mut self, e: &ScrollWheelEvent, scroll_multiplier: f32) {
        let mouse_mode = self.mouse_mode(e.shift);
        let scroll_multiplier = if mouse_mode { 1. } else { scroll_multiplier };

        if let Some(scroll_lines) = self.determine_scroll_lines(e, scroll_multiplier)
            && scroll_lines != 0
        {
            if mouse_mode {
                let point = grid_point(
                    e.position - self.last_content.terminal_bounds.bounds.origin,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if let Some(scrolls) = scroll_report(point, scroll_lines, e, self.last_content.mode)
                {
                    for scroll in scrolls {
                        self.write_to_pty(scroll);
                    }
                };
            } else if self
                .last_content
                .mode
                .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                && !e.shift
            {
                self.write_to_pty(alt_scroll(scroll_lines));
            } else {
                let scroll = AlacScroll::Delta(scroll_lines);

                self.events.push_back(InternalEvent::Scroll(scroll));
            }
        }
    }

    fn refresh_hovered_word(&mut self, window: &Window) {
        self.schedule_find_hyperlink(window.modifiers(), window.mouse_position());
    }

    fn determine_scroll_lines(
        &mut self,
        e: &ScrollWheelEvent,
        scroll_multiplier: f32,
    ) -> Option<i32> {
        let line_height = self.last_content.terminal_bounds.line_height;
        match e.touch_phase {
            /* Reset scroll state on started */
            TouchPhase::Started => {
                self.scroll_px = px(0.);
                None
            }
            /* Calculate the appropriate scroll lines */
            TouchPhase::Moved => {
                let old_offset = (self.scroll_px / line_height) as i32;

                self.scroll_px += e.delta.pixel_delta(line_height).y * scroll_multiplier;

                let new_offset = (self.scroll_px / line_height) as i32;

                // Whenever we hit the edges, reset our stored scroll to 0
                // so we can respond to changes in direction quickly
                self.scroll_px %= self.last_content.terminal_bounds.height();

                Some(new_offset - old_offset)
            }
            TouchPhase::Ended => None,
        }
    }

    pub fn find_matches(
        &self,
        mut searcher: RegexSearch,
        cx: &Context<Self>,
    ) -> Task<Vec<RangeInclusive<AlacPoint>>> {
        let term = self.term.clone();
        cx.background_spawn(async move {
            let term = term.lock();

            all_search_matches(&term, &mut searcher).collect()
        })
    }

    pub fn working_directory(&self) -> Option<PathBuf> {
        if self.is_remote_terminal {
            // We can't yet reliably detect the working directory of a shell on the
            // SSH host. Until we can do that, it doesn't make sense to display
            // the working directory on the client and persist that.
            None
        } else {
            self.client_side_working_directory()
        }
    }

    /// Normalizes the command name of the foreground process, if one is known.
    pub fn foreground_process_command_name(&self) -> Option<String> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info
                .current
                .read()
                .as_ref()
                .and_then(|process| foreground_process_command_from_argv(&process.argv)),
            TerminalType::DisplayOnly => None,
        }
    }

    /// Returns the working directory of the process that's connected to the PTY.
    /// That means it returns the working directory of the local shell or program
    /// that's running inside the terminal.
    ///
    /// This does *not* return the working directory of the shell that runs on the
    /// remote host, in case Zed is connected to a remote host.
    fn client_side_working_directory(&self) -> Option<PathBuf> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info
                .current
                .read()
                .as_ref()
                .map(|process| process.cwd.clone()),
            TerminalType::DisplayOnly => None,
        }
    }

    pub fn title(&self, truncate: bool) -> String {
        const MAX_CHARS: usize = 25;
        match &self.task {
            Some(task_state) => {
                if truncate {
                    truncate_and_trailoff(&task_state.spawned_task.label, MAX_CHARS)
                } else {
                    task_state.spawned_task.full_label.clone()
                }
            }
            None => self
                .title_override
                .as_ref()
                .map(|title_override| title_override.to_string())
                .unwrap_or_else(|| match &self.terminal_type {
                    TerminalType::Pty { info, .. } => info
                        .current
                        .read()
                        .as_ref()
                        .map(|fpi| {
                            let process_file = fpi
                                .cwd
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();

                            let argv = fpi.argv.as_slice();
                            let process_name = format!(
                                "{}{}",
                                fpi.name,
                                if !argv.is_empty() {
                                    format!(" {}", (argv[1..]).join(" "))
                                } else {
                                    "".to_string()
                                }
                            );
                            let (process_file, process_name) = if truncate {
                                (
                                    truncate_and_trailoff(&process_file, MAX_CHARS),
                                    truncate_and_trailoff(&process_name, MAX_CHARS),
                                )
                            } else {
                                (process_file, process_name)
                            };
                            format!("{process_file} — {process_name}")
                        })
                        .unwrap_or_else(|| "Terminal".to_string()),
                    TerminalType::DisplayOnly => "Terminal".to_string(),
                }),
        }
    }

    pub fn kill_active_task(&mut self) {
        if let Some(task) = self.task()
            && task.status == TaskStatus::Running
        {
            if let TerminalType::Pty { info, .. } = &self.terminal_type {
                // First kill the foreground process group (the command running in the shell)
                info.kill_current_process();
                // Then kill the shell itself so that the terminal exits properly
                // and wait_for_completed_task can complete
                info.kill_child_process();
            }
        }
    }

    pub fn pid(&self) -> Option<sysinfo::Pid> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info.pid(),
            TerminalType::DisplayOnly => None,
        }
    }

    pub fn pid_getter(&self) -> Option<&ProcessIdGetter> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => Some(info.pid_getter()),
            TerminalType::DisplayOnly => None,
        }
    }

    pub fn task(&self) -> Option<&TaskState> {
        self.task.as_ref()
    }

    pub fn wait_for_completed_task(&self, cx: &App) -> Task<Option<ExitStatus>> {
        if let Some(task) = self.task() {
            if task.status == TaskStatus::Running {
                let completion_receiver = task.completion_rx.clone();
                return cx.spawn(async move |_| completion_receiver.recv().await.ok().flatten());
            } else if let Ok(status) = task.completion_rx.try_recv() {
                return Task::ready(status);
            }
        }
        Task::ready(None)
    }

    fn register_task_finished(
        &mut self,
        exit_status: Option<ExitStatus>,
        cx: &mut Context<Terminal>,
    ) {
        if let Some(tx) = &self.completion_tx {
            tx.try_send(exit_status).ok();
        }
        if let Some(e) = exit_status {
            self.child_exited = Some(e);
        }
        let task = match &mut self.task {
            Some(task) => task,
            None => {
                // For interactive shells (no task), we need to differentiate:
                // 1. User-initiated exits (typed "exit", Ctrl+D, etc.) - always close,
                //    even if the shell exits with a non-zero code (e.g. after `false`).
                // 2. Shell spawn failures (bad $SHELL) - don't close, so the user sees
                //    the error. Spawn failures never receive keyboard input.
                let should_close = if self.keyboard_input_sent {
                    true
                } else {
                    self.child_exited.is_none_or(|e| e.code() == Some(0))
                };
                if should_close {
                    cx.emit(Event::CloseTerminal);
                } else if let Some(code) = exit_status.and_then(|e| e.code()) {
                    let title = self.title(false);
                    cx.emit(Event::SpawnFailed(format!(
                        "Som: terminal tab \"{title}\" exited immediately (code {code}) — the tab's own output has the full error text"
                    )));
                }
                return;
            }
        };
        if task.status != TaskStatus::Running {
            return;
        }
        match exit_status.and_then(|e| e.code()) {
            Some(error_code) => {
                task.status.register_task_exit(error_code);
            }
            None => {
                task.status.register_terminal_exit();
            }
        };

        let (finished_successfully, task_line, command_line) = task_summary(task, exit_status);
        let mut lines_to_show = Vec::new();
        if task.spawned_task.show_summary {
            lines_to_show.push(task_line.as_str());
        }
        if task.spawned_task.show_command {
            lines_to_show.push(command_line.as_str());
        }

        if !lines_to_show.is_empty() {
            // SAFETY: the invocation happens on non `TaskStatus::Running` tasks, once,
            // after either `AlacTermEvent::Exit` or `AlacTermEvent::ChildExit` events that are spawned
            // when Zed task finishes and no more output is made.
            // After the task summary is output once, no more text is appended to the terminal.
            unsafe { append_text_to_term(&mut self.term.lock(), &lines_to_show) };
        }

        match task.spawned_task.hide {
            HideStrategy::Never => {}
            HideStrategy::Always => {
                cx.emit(Event::CloseTerminal);
            }
            HideStrategy::OnSuccess => {
                if finished_successfully {
                    cx.emit(Event::CloseTerminal);
                }
            }
        }
    }

    pub fn vi_mode_enabled(&self) -> bool {
        self.vi_mode_enabled
    }

    /// This terminal's own shell command, as it was originally spawned —
    /// exposed so callers that need to inspect (not just blindly reuse) it
    /// before cloning can do so. Notably, `terminal_view`'s `clone_on_split`
    /// needs this to detect a `som-tmux`-wrapped shell (see
    /// `project_som_tmux` memory) and rebuild it with a fresh pane id rather
    /// than reusing this exact command — `clone_builder` alone always
    /// copies it byte-for-byte, which for a tmux-wrapped shell would
    /// connect the new split to the SAME HOLDER/session as the pane it was
    /// split from.
    pub fn shell(&self) -> &Shell {
        &self.template.shell
    }

    /// Like `clone_builder`, but lets the caller substitute a different
    /// `Shell` than the one this terminal was actually spawned with — used
    /// by `terminal_view`'s `clone_on_split` for tmux-wrapped shells (see
    /// `shell()`'s doc comment), where the SPLIT pane must run a rebuilt
    /// command (new pane id) rather than a byte-for-byte copy of the
    /// original.
    pub fn clone_builder_with_shell(&self, shell: Shell, cx: &App, cwd: Option<PathBuf>) -> Task<Result<TerminalBuilder>> {
        let working_directory = self.working_directory().or_else(|| cwd);
        TerminalBuilder::new(
            working_directory,
            None,
            shell,
            self.template.env.clone(),
            self.template.cursor_shape,
            self.template.alternate_scroll,
            self.template.max_scroll_history_lines,
            self.template.path_hyperlink_regexes.clone(),
            self.template.path_hyperlink_timeout_ms,
            self.is_remote_terminal,
            self.template.window_id,
            None,
            cx,
            self.activation_script.clone(),
            self.path_style,
        )
    }

    pub fn clone_builder(&self, cx: &App, cwd: Option<PathBuf>) -> Task<Result<TerminalBuilder>> {
        self.clone_builder_with_shell(self.template.shell.clone(), cx, cwd)
    }
}

// Helper function to convert a grid row to a string
pub fn row_to_string(row: &Row<Cell>) -> String {
    row[..Column(row.len())]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>()
}

const TASK_DELIMITER: &str = "⏵ ";
fn task_summary(task: &TaskState, exit_status: Option<ExitStatus>) -> (bool, String, String) {
    let escaped_full_label = task
        .spawned_task
        .full_label
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let task_label = |suffix: &str| format!("{TASK_DELIMITER}Task `{escaped_full_label}` {suffix}");
    let (success, task_line) = match exit_status {
        Some(status) => {
            let code = status.code();
            #[cfg(unix)]
            let signal = status.signal();
            #[cfg(not(unix))]
            let signal: Option<i32> = None;

            match (code, signal) {
                (Some(0), _) => (true, task_label("finished successfully")),
                (Some(code), _) => (
                    false,
                    task_label(&format!("finished with exit code: {code}")),
                ),
                (None, Some(signal)) => (
                    false,
                    task_label(&format!("terminated by signal: {signal}")),
                ),
                (None, None) => (false, task_label("finished")),
            }
        }
        None => (false, task_label("finished")),
    };
    let escaped_command_label = task
        .spawned_task
        .command_label
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let command_line = format!("{TASK_DELIMITER}Command: {escaped_command_label}");
    (success, task_line, command_line)
}

/// Appends a stringified task summary to the terminal, after its output.
///
/// SAFETY: This function should only be called after terminal's PTY is no longer alive.
/// New text being added to the terminal here, uses "less public" APIs,
/// which are not maintaining the entire terminal state intact.
///
///
/// The library
///
/// * does not increment inner grid cursor's _lines_ on `input` calls
///   (but displaying the lines correctly and incrementing cursor's columns)
///
/// * ignores `\n` and \r` character input, requiring the `newline` call instead
///
/// * does not alter grid state after `newline` call
///   so its `bottommost_line` is always the same additions, and
///   the cursor's `point` is not updated to the new line and column values
///
/// * ??? there could be more consequences, and any further "proper" streaming from the PTY might bug and/or panic.
///   Still, subsequent `append_text_to_term` invocations are possible and display the contents correctly.
///
/// Despite the quirks, this is the simplest approach to appending text to the terminal: its alternative, `grid_mut` manipulations,
/// do not properly set the scrolling state and display odd text after appending; also those manipulations are more tedious and error-prone.
/// The function achieves proper display and scrolling capabilities, at a cost of grid state not properly synchronized.
/// This is enough for printing moderately-sized texts like task summaries, but might break or perform poorly for larger texts.
unsafe fn append_text_to_term(term: &mut Term<ZedListener>, text_lines: &[&str]) {
    term.newline();
    term.grid_mut().cursor.point.column = Column(0);
    for line in text_lines {
        for c in line.chars() {
            term.input(c);
        }
        term.newline();
        term.grid_mut().cursor.point.column = Column(0);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let TerminalType::Pty { pty_tx, info } =
            std::mem::replace(&mut self.terminal_type, TerminalType::DisplayOnly)
        {
            pty_tx.0.send(Msg::Shutdown).ok();
            info.terminate_child_process();

            let timer = self.background_executor.timer(Duration::from_millis(100));
            self.background_executor
                .spawn(async move {
                    timer.await;
                    info.kill_child_process();
                })
                .detach();
        }
    }
}

impl EventEmitter<Event> for Terminal {}

fn normalize_path_command_name(command: &str) -> Option<String> {
    const MAX_COMMAND_NAME_LENGTH: usize = 64;

    let command = command.trim();
    if command.is_empty()
        || command.len() > MAX_COMMAND_NAME_LENGTH
        || command.starts_with('.')
        || command.starts_with('-')
        || command.contains('/')
        || command.contains('\\')
    {
        return None;
    }

    let mut command = command.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if command.ends_with(suffix) {
            command.truncate(command.len() - suffix.len());
            break;
        }
    }

    if command.is_empty()
        || !command.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }

    Some(command)
}

fn foreground_process_command_from_argv(argv: &[String]) -> Option<String> {
    let command = argv
        .first()
        .and_then(|command| normalize_path_command_name(command));

    if !matches!(
        command.as_deref(),
        Some("node" | "python" | "python3" | "bun" | "deno")
    ) {
        return command;
    }

    argv.iter()
        .skip(1)
        .filter_map(|argument| normalize_script_command_name(argument))
        .next()
        .or(command)
}

fn normalize_script_command_name(argument: &str) -> Option<String> {
    let path = Path::new(argument);
    let file_stem = path
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .and_then(normalize_path_command_name)?;

    if file_stem != "index" {
        return Some(file_stem);
    }

    path.parent()
        .and_then(|parent| parent.parent())
        .and_then(|package_path| package_path.file_name())
        .and_then(|package_name| package_name.to_str())
        .and_then(|package_name| package_name.strip_suffix("-cli").or(Some(package_name)))
        .and_then(normalize_path_command_name)
}

fn make_selection(range: &RangeInclusive<AlacPoint>) -> Selection {
    let mut selection = Selection::new(SelectionType::Simple, *range.start(), AlacDirection::Left);
    selection.update(*range.end(), AlacDirection::Right);
    selection
}

fn all_search_matches<'a, T>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
    let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
    RegexIter::new(start, end, AlacDirection::Right, term, regex)
}

fn content_index_for_mouse(pos: Point<Pixels>, terminal_bounds: &TerminalBounds) -> usize {
    let col = (pos.x / terminal_bounds.cell_width()).round() as usize;
    let clamped_col = min(col, terminal_bounds.columns() - 1);
    let row = (pos.y / terminal_bounds.line_height()).round() as usize;
    let clamped_row = min(row, terminal_bounds.screen_lines() - 1);
    clamped_row * terminal_bounds.columns() + clamped_col
}

/// Converts an 8 bit ANSI color to its GPUI equivalent.
/// Accepts `usize` for compatibility with the `alacritty::Colors` interface,
/// Other than that use case, should only be called with values in the `[0,255]` range
pub fn get_color_at_index(index: usize, theme: &Theme) -> Hsla {
    let colors = theme.colors();

    match index {
        // 0-15 are the same as the named colors above
        0 => colors.terminal_ansi_black,
        1 => colors.terminal_ansi_red,
        2 => colors.terminal_ansi_green,
        3 => colors.terminal_ansi_yellow,
        4 => colors.terminal_ansi_blue,
        5 => colors.terminal_ansi_magenta,
        6 => colors.terminal_ansi_cyan,
        7 => colors.terminal_ansi_white,
        8 => colors.terminal_ansi_bright_black,
        9 => colors.terminal_ansi_bright_red,
        10 => colors.terminal_ansi_bright_green,
        11 => colors.terminal_ansi_bright_yellow,
        12 => colors.terminal_ansi_bright_blue,
        13 => colors.terminal_ansi_bright_magenta,
        14 => colors.terminal_ansi_bright_cyan,
        15 => colors.terminal_ansi_bright_white,
        // 16-231 are a 6x6x6 RGB color cube, mapped to 0-255 using steps defined by XTerm.
        // See: https://github.com/xterm-x11/xterm-snapshots/blob/master/256colres.pl
        16..=231 => {
            let (r, g, b) = rgb_for_index(index as u8);
            rgba_color(
                if r == 0 { 0 } else { r * 40 + 55 },
                if g == 0 { 0 } else { g * 40 + 55 },
                if b == 0 { 0 } else { b * 40 + 55 },
            )
        }
        // 232-255 are a 24-step grayscale ramp from (8, 8, 8) to (238, 238, 238).
        232..=255 => {
            let i = index as u8 - 232; // Align index to 0..24
            let value = i * 10 + 8;
            rgba_color(value, value, value)
        }
        // For compatibility with the alacritty::Colors interface
        // See: https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/term/color.rs
        256 => colors.terminal_foreground,
        257 => colors.terminal_background,
        258 => theme.players().local().cursor,
        259 => colors.terminal_ansi_dim_black,
        260 => colors.terminal_ansi_dim_red,
        261 => colors.terminal_ansi_dim_green,
        262 => colors.terminal_ansi_dim_yellow,
        263 => colors.terminal_ansi_dim_blue,
        264 => colors.terminal_ansi_dim_magenta,
        265 => colors.terminal_ansi_dim_cyan,
        266 => colors.terminal_ansi_dim_white,
        267 => colors.terminal_bright_foreground,
        268 => colors.terminal_ansi_black, // 'Dim Background', non-standard color

        _ => black(),
    }
}

/// Generates the RGB channels in [0, 5] for a given index into the 6x6x6 ANSI color cube.
///
/// See: [8 bit ANSI color](https://en.wikipedia.org/wiki/ANSI_escape_code#8-bit).
///
/// Wikipedia gives a formula for calculating the index for a given color:
///
/// ```text
/// index = 16 + 36 × r + 6 × g + b (0 ≤ r, g, b ≤ 5)
/// ```
///
/// This function does the reverse, calculating the `r`, `g`, and `b` components from a given index.
fn rgb_for_index(i: u8) -> (u8, u8, u8) {
    debug_assert!((16..=231).contains(&i));
    let i = i - 16;
    let r = (i - (i % 36)) / 36;
    let g = ((i % 36) - (i % 6)) / 6;
    let b = (i % 36) % 6;
    (r, g, b)
}

pub fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: (r as f32 / 255.),
        g: (g as f32 / 255.),
        b: (b as f32 / 255.),
        a: 1.,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        IndexedCell, TerminalBounds, TerminalBuilder, TerminalContent, content_index_for_mouse,
        rgb_for_index,
    };
    use alacritty_terminal::{
        index::{Column, Line, Point as AlacPoint},
        term::cell::Cell,
    };
    use async_channel::Receiver;
    use collections::HashMap;
    use gpui::{
        Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
        Point, TestAppContext, VisualContext, bounds, point, size,
    };
    use parking_lot::Mutex;
    use rand::{Rng, distr, rngs::StdRng};
    use task::{Shell, ShellBuilder};

    #[test]
    fn test_normalize_path_command_name() {
        assert_eq!(normalize_path_command_name("claude"), Some("claude".into()));
        assert_eq!(normalize_path_command_name("Cargo"), Some("cargo".into()));
        assert_eq!(normalize_path_command_name("node.exe"), Some("node".into()));
        assert_eq!(
            normalize_path_command_name("my-agent_cli.1"),
            Some("my-agent_cli.1".into())
        );
        assert_eq!(normalize_path_command_name("./local-agent"), None);
        assert_eq!(normalize_path_command_name("../local-agent"), None);
        assert_eq!(normalize_path_command_name("/usr/local/bin/cargo"), None);
        assert_eq!(
            normalize_path_command_name("target\\debug\\agent.exe"),
            None
        );
        assert_eq!(normalize_path_command_name(".hidden-agent"), None);
        assert_eq!(normalize_path_command_name("agent with spaces"), None);
        assert_eq!(normalize_path_command_name("zsh"), Some("zsh".into()));
        assert_eq!(normalize_path_command_name("-zsh"), None);
        assert_eq!(normalize_path_command_name("pwsh.exe"), Some("pwsh".into()));
    }

    #[test]
    fn test_foreground_process_command_from_interpreter_wrapper() {
        assert_eq!(
            foreground_process_command_from_argv(&[
                "node".to_string(),
                "/opt/homebrew/lib/node_modules/@google/gemini-cli/dist/index.js".to_string(),
            ]),
            Some("gemini".to_string())
        );
        assert_eq!(
            foreground_process_command_from_argv(&[
                "python3".to_string(),
                "/Users/me/.local/bin/codex.py".to_string(),
            ]),
            Some("codex".to_string())
        );
        assert_eq!(
            foreground_process_command_from_argv(&[
                "node".to_string(),
                "/Users/me/private-project/scripts/customer-data-export.js".to_string(),
            ]),
            Some("customer-data-export".to_string())
        );
    }

    #[cfg(not(target_os = "windows"))]
    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// Helper to build a test terminal running a shell command.
    /// Returns the terminal entity and a receiver for the completion signal.
    async fn build_test_terminal(
        cx: &mut TestAppContext,
        command: &str,
        args: &[&str],
    ) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (program, args) =
            ShellBuilder::new(&Shell::System, false).build(Some(command.to_owned()), &args);
        build_test_terminal_with_arguments(cx, program, args).await
    }

    async fn build_test_terminal_with_arguments(
        cx: &mut TestAppContext,
        program: String,
        args: Vec<String>,
    ) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
        let (completion_tx, completion_rx) = async_channel::unbounded();
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    task::Shell::WithArguments {
                        program,
                        args,
                        title_override: None,
                    },
                    HashMap::default(),
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    vec![],
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));
        (terminal, completion_rx)
    }

    fn init_ctrl_click_hyperlink_test(cx: &mut TestAppContext, output: &[u8]) -> Entity<Terminal> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output, cx);
        });

        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let term_lock = terminal.term.lock();
            terminal.last_content = Terminal::make_content(&term_lock, &terminal.last_content);
            drop(term_lock);

            let terminal_bounds = TerminalBounds::new(
                px(20.0),
                px(10.0),
                bounds(point(px(0.0), px(0.0)), size(px(400.0), px(400.0))),
            );
            terminal.last_content.terminal_bounds = terminal_bounds;
            terminal.events.clear();
        });

        terminal
    }

    fn ctrl_mouse_down_at(
        terminal: &mut Terminal,
        position: Point<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_down = MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::secondary_key(),
            click_count: 1,
            first_mouse: true,
        };
        terminal.mouse_down(&mouse_down, cx);
    }

    fn ctrl_mouse_move_to(
        terminal: &mut Terminal,
        position: Point<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let terminal_bounds = terminal.last_content.terminal_bounds.bounds;
        let drag_event = MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::secondary_key(),
        };
        terminal.mouse_drag(&drag_event, terminal_bounds, cx);
    }

    fn ctrl_mouse_up_at(
        terminal: &mut Terminal,
        position: Point<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_up = MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::secondary_key(),
            click_count: 1,
        };
        terminal.mouse_up(&mouse_up, cx);
    }

    #[gpui::test]
    async fn test_basic_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (terminal, completion_rx) = build_test_terminal(cx, "echo", &["hello"]).await;
        assert_eq!(
            completion_rx.recv().await.unwrap(),
            Some(ExitStatus::default())
        );
        assert_content_eventually(&terminal, "hello", cx).await;

        // Inject additional output directly into the emulator (display-only path)
        terminal.update(cx, |term, cx| {
            term.write_output(b"\nfrom_injection", cx);
        });

        let content_after = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content_after.contains("from_injection"),
            "expected injected output to appear, got: {content_after}"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_foreground_process_command_tracks_path_command(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (terminal, completion_rx) =
            build_test_terminal_with_arguments(cx, "sleep".to_string(), vec!["1".to_string()])
                .await;

        assert_foreground_process_command_eventually(&terminal, "sleep", cx).await;

        assert!(
            completion_rx.recv().await.is_ok(),
            "expected terminal completion after sleep exits"
        );
    }

    // TODO should be tested on Linux too, but does not work there well
    #[cfg(target_os = "macos")]
    #[gpui::test(iterations = 10)]
    async fn test_terminal_eof(cx: &mut TestAppContext) {
        init_test(cx);

        cx.executor().allow_parking();

        let (completion_tx, completion_rx) = async_channel::unbounded();
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    task::Shell::System,
                    HashMap::default(),
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        // Build an empty command, which will result in a tty shell spawned.
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        cx.update(|cx| {
            cx.subscribe(&terminal, move |_, e, _| {
                event_tx.send_blocking(e.clone()).unwrap();
            })
        })
        .detach();
        cx.background_spawn(async move {
            assert_eq!(
                completion_rx.recv().await.unwrap(),
                Some(ExitStatus::default()),
                "EOF should result in the tty shell exiting successfully",
            );
        })
        .detach();

        let first_event = event_rx.recv().await.expect("No wakeup event received");

        terminal.update(cx, |terminal, _| {
            let success = terminal.try_keystroke(&Keystroke::parse("ctrl-c").unwrap(), false);
            assert!(success, "Should have registered ctrl-c sequence");
        });
        terminal.update(cx, |terminal, _| {
            let success = terminal.try_keystroke(&Keystroke::parse("ctrl-d").unwrap(), false);
            assert!(success, "Should have registered ctrl-d sequence");
        });

        let mut all_events = vec![first_event];
        while let Ok(new_event) = event_rx.recv().await {
            all_events.push(new_event.clone());
            if new_event == Event::CloseTerminal {
                break;
            }
        }
        assert!(
            all_events.contains(&Event::CloseTerminal),
            "EOF command sequence should have triggered a TTY terminal exit, but got events: {all_events:?}",
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[gpui::test(iterations = 10)]
    async fn test_terminal_closes_after_nonzero_exit(cx: &mut TestAppContext) {
        init_test(cx);

        cx.executor().allow_parking();

        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    task::Shell::System,
                    HashMap::default(),
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    None,
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        cx.update(|cx| {
            cx.subscribe(&terminal, move |_, e, _| {
                event_tx.send_blocking(e.clone()).unwrap();
            })
        })
        .detach();

        let first_event = event_rx.recv().await.expect("No wakeup event received");

        terminal.update(cx, |terminal, _| {
            terminal.input(b"false\r".to_vec());
        });
        cx.executor().timer(Duration::from_millis(500)).await;
        terminal.update(cx, |terminal, _| {
            terminal.input(b"exit\r".to_vec());
        });

        let mut all_events = vec![first_event];
        while let Ok(new_event) = event_rx.recv().await {
            all_events.push(new_event.clone());
            if new_event == Event::CloseTerminal {
                break;
            }
        }
        assert!(
            all_events.contains(&Event::CloseTerminal),
            "Shell exiting after `false && exit` should close terminal, but got events: {all_events:?}",
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_terminal_no_exit_on_spawn_failure(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (completion_tx, completion_rx) = async_channel::unbounded();
        let (program, args) = ShellBuilder::new(&Shell::System, false)
            .build(Some("asdasdasdasd".to_owned()), &["@@@@@".to_owned()]);
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    task::Shell::WithArguments {
                        program,
                        args,
                        title_override: None,
                    },
                    HashMap::default(),
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    Vec::new(),
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let all_events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        cx.update({
            let all_events = all_events.clone();
            |cx| {
                cx.subscribe(&terminal, move |_, e, _| {
                    all_events.lock().push(e.clone());
                })
            }
        })
        .detach();
        let completion_check_task = cx.background_spawn(async move {
            // The channel may be closed if the terminal is dropped before sending
            // the completion signal, which can happen with certain task scheduling orders.
            let exit_status = completion_rx.recv().await.ok().flatten();
            if let Some(exit_status) = exit_status {
                assert!(
                    !exit_status.success(),
                    "Wrong shell command should result in a failure"
                );
                #[cfg(target_os = "windows")]
                assert_eq!(exit_status.code(), Some(1));
                #[cfg(not(target_os = "windows"))]
                assert_eq!(exit_status.code(), Some(127)); // code 127 means "command not found" on Unix
            }
        });

        completion_check_task.await;
        cx.executor().timer(Duration::from_millis(500)).await;

        let events = all_events.lock();
        assert!(
            !events.iter().any(|event| event == &Event::CloseTerminal),
            "Wrong shell command should update the title but not should not close the terminal to show the error message, but got events: {events:?}",
        );
        assert!(
            events.iter().any(|event| matches!(event, Event::SpawnFailed(_))),
            "A spawn failure should emit Event::SpawnFailed so the UI can surface a notification, but got events: {events:?}",
        );
    }

    #[test]
    fn test_rgb_for_index() {
        // Test every possible value in the color cube.
        for i in 16..=231 {
            let (r, g, b) = rgb_for_index(i);
            assert_eq!(i, 16 + 36 * r + 6 * g + b);
        }
    }

    #[gpui::test]
    fn test_mouse_to_cell_test(mut rng: StdRng) {
        const ITERATIONS: usize = 10;
        const PRECISION: usize = 1000;

        for _ in 0..ITERATIONS {
            let viewport_cells = rng.random_range(15..20);
            let cell_size =
                rng.random_range(5 * PRECISION..20 * PRECISION) as f32 / PRECISION as f32;

            let size = crate::TerminalBounds {
                cell_width: Pixels::from(cell_size),
                line_height: Pixels::from(cell_size),
                bounds: bounds(
                    Point::default(),
                    size(
                        Pixels::from(cell_size * (viewport_cells as f32)),
                        Pixels::from(cell_size * (viewport_cells as f32)),
                    ),
                ),
            };

            let cells = get_cells(size, &mut rng);
            let content = convert_cells_to_content(size, &cells);

            for row in 0..(viewport_cells - 1) {
                let row = row as usize;
                for col in 0..(viewport_cells - 1) {
                    let col = col as usize;

                    let row_offset = rng.random_range(0..PRECISION) as f32 / PRECISION as f32;
                    let col_offset = rng.random_range(0..PRECISION) as f32 / PRECISION as f32;

                    let mouse_pos = point(
                        Pixels::from(col as f32 * cell_size + col_offset),
                        Pixels::from(row as f32 * cell_size + row_offset),
                    );

                    let content_index =
                        content_index_for_mouse(mouse_pos, &content.terminal_bounds);
                    let mouse_cell = content.cells[content_index].c;
                    let real_cell = cells[row][col];

                    assert_eq!(mouse_cell, real_cell);
                }
            }
        }
    }

    #[gpui::test]
    fn test_mouse_to_cell_clamp(mut rng: StdRng) {
        let size = crate::TerminalBounds {
            cell_width: Pixels::from(10.),
            line_height: Pixels::from(10.),
            bounds: bounds(
                Point::default(),
                size(Pixels::from(100.), Pixels::from(100.)),
            ),
        };

        let cells = get_cells(size, &mut rng);
        let content = convert_cells_to_content(size, &cells);

        assert_eq!(
            content.cells[content_index_for_mouse(
                point(Pixels::from(-10.), Pixels::from(-10.)),
                &content.terminal_bounds,
            )]
            .c,
            cells[0][0]
        );
        assert_eq!(
            content.cells[content_index_for_mouse(
                point(Pixels::from(1000.), Pixels::from(1000.)),
                &content.terminal_bounds,
            )]
            .c,
            cells[9][9]
        );
    }

    #[gpui::test]
    async fn test_set_size_coalesces_pixel_only_changes(cx: &mut TestAppContext) {
        let builder = cx.update(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::Block,
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
        });
        let mut terminal = builder.terminal;

        let base_bounds = TerminalBounds {
            cell_width: Pixels::from(10.),
            line_height: Pixels::from(10.),
            bounds: bounds(
                Point::default(),
                size(Pixels::from(100.), Pixels::from(100.)),
            ),
        };

        terminal.set_size(base_bounds);
        terminal.events.clear();
        assert_eq!(terminal.last_content.terminal_bounds, base_bounds);

        // Pixel-only change: height grows by 1px but still the same number of rows/cols.
        let mut pixel_changed = base_bounds;
        pixel_changed.bounds.size.height = Pixels::from(101.);
        terminal.set_size(pixel_changed);
        assert!(terminal.events.is_empty());
        assert_eq!(terminal.last_content.terminal_bounds, pixel_changed);

        // Grid change: height increases enough to add a row.
        let mut grid_changed = base_bounds;
        grid_changed.bounds.size.height = Pixels::from(110.);
        terminal.set_size(grid_changed);
        assert!(matches!(
            terminal.events.back(),
            Some(InternalEvent::Resize(_))
        ));
    }

    fn get_cells(size: TerminalBounds, rng: &mut StdRng) -> Vec<Vec<char>> {
        let mut cells = Vec::new();

        for _ in 0..size.num_lines() {
            let mut row_vec = Vec::new();
            for _ in 0..size.num_columns() {
                let cell_char = rng.sample(distr::Alphanumeric) as char;
                row_vec.push(cell_char)
            }
            cells.push(row_vec)
        }

        cells
    }

    fn convert_cells_to_content(
        terminal_bounds: TerminalBounds,
        cells: &[Vec<char>],
    ) -> TerminalContent {
        let mut ic = Vec::new();

        for (index, row) in cells.iter().enumerate() {
            for (cell_index, cell_char) in row.iter().enumerate() {
                ic.push(IndexedCell {
                    point: AlacPoint::new(Line(index as i32), Column(cell_index)),
                    cell: Cell {
                        c: *cell_char,
                        ..Default::default()
                    },
                });
            }
        }

        TerminalContent {
            cells: ic,
            terminal_bounds,
            ..Default::default()
        }
    }

    #[gpui::test]
    async fn test_write_output_converts_lf_to_crlf(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // Test simple LF conversion
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"line1\nline2\n", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            Terminal::make_content(&term, &terminal.last_content)
        });

        // If LF is properly converted to CRLF, each line should start at column 0
        // The diagonal staircase bug would cause increasing column positions

        // Get the cells and check that lines start at column 0
        let cells = &content.cells;
        let mut line1_col0 = false;
        let mut line2_col0 = false;

        for cell in cells {
            if cell.c == 'l' && cell.point.column.0 == 0 {
                if cell.point.line.0 == 0 && !line1_col0 {
                    line1_col0 = true;
                } else if cell.point.line.0 == 1 && !line2_col0 {
                    line2_col0 = true;
                }
            }
        }

        assert!(line1_col0, "First line should start at column 0");
        assert!(line2_col0, "Second line should start at column 0");
    }

    #[gpui::test]
    async fn test_rich_content_chunk_reaches_terminal_via_real_vte_parser(cx: &mut TestAppContext) {
        // Mirrors `test_kitty_graphics_apc_reaches_terminal` exactly, but
        // for Som's own rich-content protocol instead of Kitty's — proves
        // the routing added to `process_event`'s `ApcString` arm (marker
        // byte dispatch between `rich_content_transport::parse_envelope`
        // and `kitty_graphics::parse_command`) actually works end to end
        // through the REAL (patched) VTE parser, not just that
        // `rich_content_transport`'s own unit tests pass in isolation —
        // those never exercise `apc_hook`/`apc_put`/`apc_unhook` or the
        // patched full-8-bit passthrough at all.
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // Deliberately includes a raw `0x1B` (ESC) byte INSIDE `payload` —
        // proving the base91 wire encoding (see `rich_content_transport`'s
        // module doc comment) makes this a non-issue: every wire byte is
        // drawn from a 91-symbol printable-ASCII alphabet that never
        // contains `0x1B`, so the real (patched) VTE parser never sees
        // anything resembling its `ESC \` terminator until the genuine one
        // at the end of the APC string.
        let payload_with_esc = vec![b'a', b'b', b'c', 0x1B, b'd', b'e'];
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Gif,
            session_id: 0x1111_2222,
            file_id: 0x3333_4444,
            chunk_offset: 0,
            total_size: 6,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 0,
                height_px: 0,
                color_bits: 0,
                is_animated: false,
            },
            payload: payload_with_esc.clone(),
        };

        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']); // ESC _
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']); // ESC \

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&apc, cx);
        });

        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let contiguous = terminal.rich_content_cache.contiguous_len(chunk.session_id, chunk.file_id);
            assert_eq!(contiguous, 6, "the full 6-byte payload (including the embedded ESC byte) must land contiguously");
            let path = terminal
                .rich_content_cache
                .path(chunk.session_id, chunk.file_id)
                .expect("a cache file must exist after a chunk was applied");
            let on_disk = std::fs::read(path).expect("cache file must be readable");
            assert_eq!(on_disk, payload_with_esc, "the embedded ESC byte must survive on disk exactly as sent");
        });
    }

    #[gpui::test]
    async fn test_clear_command_hides_placeholder_grid_cells(cx: &mut TestAppContext) {
        // Requirement #1 from the user's own report ("clear должен прятать
        // картинку"). Since a placement is real grid text (the client —
        // `somcat` — prints the Unicode-placeholder grid itself through its
        // own real stdout, exactly like any other program's output; see
        // `crates/somcat/src/main.rs`'s `print_placeholder_grid` doc
        // comment for why Som itself no longer injects this text), a real
        // `clear` escape sequence must erase it exactly the same way it
        // erases any other line of text — no placement-specific handling
        // needed, which is the whole point of the placeholder-grid
        // architecture. This test writes the same placeholder text a real
        // client would print (through `write_output`, standing in for the
        // real PTY byte stream in this display-only test terminal) and
        // proves `clear` erases it end-to-end through the real VTE parser.
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        let mut placeholder_text = String::new();
        placeholder_text.push_str("\x1b[38;2;1;2;3m\x1b[58;2;4;5;6m");
        for row in 0..3u32 {
            for column in 0..3u32 {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            if row < 2 {
                placeholder_text.push_str("\r\n");
            }
        }
        placeholder_text.push_str("\x1b[0m\r\n");

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock();
            let has_placeholder = term
                .renderable_content()
                .display_iter
                .any(|indexed| indexed.cell.c == kitty_graphics_placeholder::PLACEHOLDER_CHAR);
            assert!(has_placeholder, "placeholder cells must be present before clear runs, or this test proves nothing");
        });

        // The real escape sequence a shell's `clear` builtin sends: home
        // cursor, then erase the whole screen.
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"\x1b[H\x1b[2J", cx);
        });
        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock();
            let has_placeholder = term
                .renderable_content()
                .display_iter
                .any(|indexed| indexed.cell.c == kitty_graphics_placeholder::PLACEHOLDER_CHAR);
            assert!(!has_placeholder, "clear must erase placeholder cells exactly like any other text");
        });
    }

    #[gpui::test]
    async fn test_resize_shrinks_placeholder_grid_to_match_new_cell_size(cx: &mut TestAppContext) {
        // Reproduces the resize/font-size-change scenario from
        // SRP_PROTOCOL.md's "Незавершённое: пересчёт placement'ов при
        // ресайзе окна/шрифта" section: a placeholder grid printed for one
        // `cell_width`/`line_height` must be re-derived (diacritics
        // rewritten in place, never the placeholder character or cursor
        // moved) when the terminal's cell pixel size changes, so the
        // on-screen image keeps its real aspect ratio instead of staying
        // frozen at whatever column/row count the original print happened
        // to use. `set_size` only QUEUES an `InternalEvent::Resize` — the
        // resync this test exercises only actually runs inside `sync()`
        // (called every real paint), which needs a real `Window`, hence
        // `cx.add_empty_window()` instead of the display-only builder
        // most other tests in this file use.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A 90x30px image at a 9x10px cell size occupies exactly 10
        // columns x 3 rows. Ids kept within 24 bits — the placeholder
        // grid encodes (session_id, file_id) in a cell's fg/underline
        // 24-bit RGB color (see `kitty_graphics_placeholder`'s module doc
        // comment), so a wider id would silently lose its top byte on
        // decode, same as the real bug this exact shape caught in
        // `somcat` earlier in this session.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Png,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 90,
                height_px: 30,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(10.),
                px(9.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        // Print the same placeholder grid `somcat` would for a 90x30px
        // image at a 9x10px cell (10 columns x 3 rows), with nothing
        // printed after it — the scope this function documents.
        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        for row in 0..3u32 {
            for column in 0..10u32 {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            if row < 2 {
                placeholder_text.push_str("\r\n");
            }
        }
        placeholder_text.push_str("\x1b[0m\r\n");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            let placeholder_count = term
                .renderable_content()
                .display_iter
                .filter(|indexed| indexed.cell.c == kitty_graphics_placeholder::PLACEHOLDER_CHAR)
                .count();
            assert_eq!(placeholder_count, 30, "10x3 grid must have printed all 30 cells before resize");
        });

        // Double the cell width (9px -> 18px), leaving line_height
        // unchanged: the SAME 90px-wide image now needs only 5 columns
        // instead of 10, but still 3 rows (30px tall / unchanged 10px
        // line_height) — `set_size` + `sync` with a coarser grid triggers
        // the resync.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(10.),
                px(18.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
        });

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();

            let mut decoded_row_columns: Vec<(u32, u32)> = Vec::new();
            for indexed in term.renderable_content().display_iter {
                if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                    continue;
                }
                let fg_rgb = match indexed.cell.fg {
                    alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                    _ => continue,
                };
                let underline_rgb = match indexed.cell.underline_color() {
                    Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                    _ => None,
                };
                let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                if let Some(kitty_graphics_placeholder::PlaceholderCell { row, column, .. }) =
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                {
                    decoded_row_columns.push((row, column));
                }
            }

            let max_row = decoded_row_columns.iter().map(|(r, _)| *r).max().unwrap_or(0);
            let max_column = decoded_row_columns.iter().map(|(_, c)| *c).max().unwrap_or(0);
            assert_eq!(max_column, 4, "5 columns (0..=4) expected after doubling cell width for a 90px-wide image");
            assert_eq!(max_row, 2, "still 3 rows (0..=2) — line_height didn't change, so row count shouldn't either");

            // Cells beyond the new, shrunk bounding box must be blanked,
            // not left with stale placeholder characters from the old
            // (wider) grid.
            let leftover_placeholder_count = term
                .renderable_content()
                .display_iter
                .filter(|indexed| indexed.cell.c == kitty_graphics_placeholder::PLACEHOLDER_CHAR)
                .count();
            assert_eq!(leftover_placeholder_count, 15, "5 columns x 3 rows = 15 cells, no stale cells beyond that");
        });

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let new_max = terminal.rich_content_max_column_seen(session_id, file_id);
            assert_eq!(new_max, Some(4), "max_column_seen must be reset to the new, smaller column count, not left stale");
        });
    }

    #[gpui::test]
    async fn test_resize_shrinking_placeholder_grid_height_pulls_up_content_printed_after_it(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real bug reported live after the width-only resize
        // fix above: shrinking a placement's ROW count left the removed
        // rows as dead blank space between the image and whatever was
        // printed after it (the shell's next prompt), because the old
        // fix only blanked the leftover cells in place instead of
        // actually closing the gap. `resync_rich_content_placements`
        // must additionally shift everything printed after the image up
        // by however many rows were removed — exactly like a real
        // terminal's own delete-lines (`DL`) escape sequence would.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A 90x60px image at a 9x10px cell occupies 10 columns x 6 rows.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Png,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 90,
                height_px: 60,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(10.),
                px(9.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        // Print the 10x6 placeholder grid, then a real prompt line right
        // after it — the exact shape of the live bug report ("между
        // картинкой и приглашением получается большое свободное место").
        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        for row in 0..6u32 {
            for column in 0..10u32 {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            placeholder_text.push_str("\r\n");
        }
        placeholder_text.push_str("\x1b[0m~ >> ");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        let prompt_line_before_resize = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            term.grid().cursor.point.line.0
        });
        assert_eq!(prompt_line_before_resize, 6, "prompt must land immediately after the 6-row image, before resize");

        // Triple line_height (10px -> 30px), leaving cell_width unchanged:
        // the SAME 60px-tall image now needs only 2 rows instead of 6 —
        // the 4 removed rows' worth of blank space must NOT appear
        // between the image and the prompt.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(30.),
                px(9.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
        });

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();

            let max_row = term
                .renderable_content()
                .display_iter
                .filter_map(|indexed| {
                    if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                        return None;
                    }
                    let fg_rgb = match indexed.cell.fg {
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                        _ => return None,
                    };
                    let underline_rgb = match indexed.cell.underline_color() {
                        Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                        _ => None,
                    };
                    let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                    .map(|d| d.row)
                })
                .max();
            assert_eq!(max_row, Some(1), "2 rows (0..=1) expected — 60px tall / 30px line_height");

            // The real, live-reported symptom: the prompt's own printed
            // text must be pulled up to sit immediately after the SHRUNK
            // image, not left where it was printed (which would leave a
            // gap of blank rows where the removed image rows used to be).
            // Checking the actual grid content here, not
            // `term.grid().cursor.point` — `resync_rich_content_
            // placements` deliberately never touches the cursor directly
            // (see that function's doc comment: the real shell behind the
            // PTY has its own separate cursor model a direct grid edit
            // can't inform, so patching it just desyncs the two the
            // moment the shell itself prints anything).
            let prompt_line = (term.topmost_line().0..=term.bottommost_line().0)
                .find(|&line_idx| term.grid()[Line(line_idx)][Column(0)].c == '~');
            assert_eq!(
                prompt_line,
                Some(2),
                "the prompt's own printed text must be pulled up to immediately follow the now-2-row image, no gap left behind"
            );
        });
    }

    #[gpui::test]
    async fn test_resize_growing_placeholder_grid_height_pushes_down_content_printed_after_it(
        cx: &mut TestAppContext,
    ) {
        // The inverse of the shrink test above — growing a placement's row
        // count (window/font made LARGER, not smaller) must INSERT new
        // rows and push whatever was printed after the image (the prompt)
        // further down, without losing or overwriting it. Uses
        // `Grid`'s row-by-row copy path in `resync_rich_content_placements`
        // (there's no ready-made `Grid::scroll_up`/`scroll_down` call that
        // does this alone — see that function's inline comment for why).
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A 90x60px image at an 18x30px cell occupies 5 columns x 2 rows.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Png,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 90,
                height_px: 60,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(30.),
                px(18.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        // Print the 5x2 placeholder grid, then a real prompt line right
        // after it.
        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        for row in 0..2u32 {
            for column in 0..5u32 {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            placeholder_text.push_str("\r\n");
        }
        placeholder_text.push_str("\x1b[0m~ >> ");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        let prompt_line_before_resize = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            term.grid().cursor.point.line.0
        });
        assert_eq!(prompt_line_before_resize, 2, "prompt must land immediately after the 2-row image, before resize");

        // Shrink line_height by a third (30px -> 10px), leaving cell_width
        // unchanged: the SAME 60px-tall image now needs 6 rows instead of
        // 2 — the 4 new rows must push the prompt down, not overwrite it
        // or leave it in the middle of the image.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(10.),
                px(18.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(300.) } },
            ));
            terminal.sync(window, cx);
        });

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();

            let max_row = term
                .renderable_content()
                .display_iter
                .filter_map(|indexed| {
                    if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                        return None;
                    }
                    let fg_rgb = match indexed.cell.fg {
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                        _ => return None,
                    };
                    let underline_rgb = match indexed.cell.underline_color() {
                        Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                        _ => None,
                    };
                    let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                    .map(|d| d.row)
                })
                .max();
            assert_eq!(max_row, Some(5), "6 rows (0..=5) expected — 60px tall / 10px line_height");

            // `resync_rich_content_placements` deliberately never touches
            // `term.grid_mut().cursor` (see that function's doc comment —
            // the real shell behind the PTY has its own, separate cursor
            // model that direct grid edits can't inform, so patching
            // Som's local cursor to "compensate" only desyncs the two the
            // moment the shell itself prints anything). What DOES need to
            // hold: the row-by-row copy must have physically carried the
            // prompt's own printed characters down along with everything
            // else below the image's old bottom edge, landing them
            // immediately after the image's new (taller) extent — not
            // left in the middle of the grown image, not lost, not
            // stuck at their old row leaving a gap.
            let term_locked = term;
            let prompt_line = (term_locked.topmost_line().0..=term_locked.bottommost_line().0)
                .find(|&line_idx| term_locked.grid()[Line(line_idx)][Column(0)].c == '~');
            assert_eq!(
                prompt_line,
                Some(6),
                "the prompt's own printed text must have been carried down to immediately follow the now-6-row image"
            );
        });
    }

    #[gpui::test]
    async fn test_resize_growing_placeholder_grid_height_past_screen_bottom_does_not_panic(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real crash reported live: growing a placement's row
        // count on a SMALL screen, where the growth pushes rows past the
        // bottom of the visible screen, panicked (`Grid` indexed at a
        // nonexistent line) instead of safely scrolling the overflow into
        // history — confirmed via the log ending abruptly right after the
        // `Resizing:`/`New num_cols...` trace lines, with no further
        // output, on a real resize-to-maximized-window action after
        // showing an image. This test deliberately uses a screen just
        // tall enough for the image's OLD size but not its NEW (grown)
        // size, so the insertion path's row-by-row copy must push
        // overflowing rows into scrollback rather than write past
        // `bottommost_line()`.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A 90x60px image at an 18x30px cell occupies 5 columns x 2 rows —
        // a 5-line-tall screen (3 rows spare below the image) is enough
        // for the OLD size but not the NEW one below.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Png,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 90,
                height_px: 60,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(30.),
                px(18.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(150.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        for row in 0..2u32 {
            for column in 0..5u32 {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            placeholder_text.push_str("\r\n");
        }
        placeholder_text.push_str("\x1b[0m~ >> ");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        // Shrink line_height a lot (30px -> 5px): the SAME 60px-tall image
        // now needs 12 rows instead of 2 — 10 new rows on a screen that's
        // only 5 lines tall, which used to overflow the row-by-row copy's
        // write target past `bottommost_line()` and panic. Success here is
        // simply that this call returns without crashing.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(5.),
                px(18.),
                Bounds { origin: Point::default(), size: Size { width: px(900.), height: px(150.) } },
            ));
            terminal.sync(window, cx);
        });
    }

    #[gpui::test]
    async fn test_resize_growth_with_two_placement_groups_of_same_image_does_not_panic(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real crash reported live: the SAME image printed
        // twice at two different terminal widths (once while the window
        // was wide, then again after the user manually shrank it) leaves
        // TWO placeholder-cell groups on screen simultaneously sharing the
        // same (session_id, file_id) — decoded as two separate entries in
        // `resync_rich_content_placements`'s `groups` map purely because
        // their grid positions/decoded (row, column) differ, not because
        // decoding is wrong. Growing the terminal again afterward crashed:
        // the first group processed (topmost by `origin_line`) can insert
        // rows via `Grid::scroll_up`, which shifts the WHOLE visible
        // screen — including any later, not-yet-processed group's cells,
        // whose coordinates were captured once in the initial scan and
        // never refreshed. Success here is simply that resizing after
        // both prints completes without panicking.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Png,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 90,
                height_px: 60,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        let placeholder_text = |rows: u32, columns: u32| {
            let mut text = String::new();
            let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
            let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
            text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
            for row in 0..rows {
                for column in 0..columns {
                    if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                        text.extend(cell);
                    }
                }
                text.push_str("\r\n");
            }
            text.push_str("\x1b[0m");
            text
        };

        // First print: 8px-wide cells, ~888px wide terminal (matches the
        // real crash's cell_width=8/first Resizing bounds), image fits at
        // its natural 5x2 grid.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(15.6),
                px(8.),
                Bounds { origin: Point::default(), size: Size { width: px(888.), height: px(624.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();
        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text(2, 5).as_bytes(), cx);
            terminal.write_output(b"~ >> first\r\n", cx);
        });
        window.run_until_parked();

        // Print the SAME image again — creates a brand-new group on
        // screen (below the prompt) sharing the same session_id/file_id
        // as the first, already-printed one now sitting higher up.
        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();
        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text(2, 5).as_bytes(), cx);
            terminal.write_output(b"~ >> second\r\n", cx);
        });
        window.run_until_parked();

        // Grow the terminal back in several small, single-column steps,
        // mirroring a real mouse-drag resize (which fires one resize
        // event per pixel of movement, not one event for the whole
        // gesture) — must not panic at any step, regardless of which of
        // the two same-id groups gets processed first.
        for width_px in [888, 897, 905, 912, 921, 928, 936] {
            window.update_window_entity(&terminal, |terminal, window, cx| {
                terminal.set_size(TerminalBounds::new(
                    px(15.6),
                    px(8.),
                    Bounds { origin: Point::default(), size: Size { width: px(width_px as f32), height: px(624.) } },
                ));
                terminal.sync(window, cx);
            });
            window.run_until_parked();
        }
    }

    #[gpui::test]
    async fn test_resize_shrinking_terminal_width_in_many_small_steps_keeps_prompt_immediately_after_image(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real bug reported live: a wide image shown on a
        // large window, then the window narrowed by DRAGGING (which fires
        // one resize event per pixel of movement, not one for the whole
        // gesture — same as the growth-side regression test above) opened
        // up a large, growing gap between the image's new (shorter, since
        // narrowing a wide image scales its row count down via the
        // aspect-ratio-preserving clamp) bottom edge and the prompt
        // printed after it, instead of the prompt following it up.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A wide 1920x1080px image (same aspect ratio as the real
        // giphy.gif frame that triggered the live report) at an 8x15.6px
        // cell — matches the real crash log's numbers from earlier in
        // this session.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Jpeg,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 1920,
                height_px: 1080,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(15.6),
                px(8.),
                Bounds { origin: Point::default(), size: Size { width: px(1800.), height: px(1200.) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        // Print the placeholder grid at the terminal's initial, wide
        // column count (clamped, same math `somcat` uses), then a prompt
        // right after it.
        let initial_columns = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            terminal.term.lock().columns() as u32
        });
        let natural_columns = (1920f32 / 8.0).ceil() as u32;
        let natural_rows = (1080f32 / 15.6).ceil() as u32;
        let (columns, rows) = if natural_columns > initial_columns {
            let scale = initial_columns as f32 / natural_columns as f32;
            (initial_columns, ((natural_rows as f32 * scale).floor() as u32).max(1))
        } else {
            (natural_columns, natural_rows)
        };

        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        for row in 0..rows {
            for column in 0..columns {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            placeholder_text.push_str("\r\n");
        }
        placeholder_text.push_str("\x1b[0m~ >> ");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        let prompt_line_before_resize = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            terminal.term.lock().grid().cursor.point.line.0
        });
        assert_eq!(
            prompt_line_before_resize, rows as i32,
            "prompt must land immediately after the {rows}-row image, before any resize"
        );

        // Narrow the window in several small, single-column steps,
        // mirroring a real mouse-drag resize — after EVERY step, the
        // prompt (cursor) must remain immediately after the image's
        // current row count, never leaving a gap.
        let mut width_px = 1800;
        while width_px > 900 {
            width_px -= 8;
            window.update_window_entity(&terminal, |terminal, window, cx| {
                terminal.set_size(TerminalBounds::new(
                    px(15.6),
                    px(8.),
                    Bounds { origin: Point::default(), size: Size { width: px(width_px as f32), height: px(1200.) } },
                ));
                terminal.sync(window, cx);
            });
            window.run_until_parked();
        }

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            let max_row = term
                .renderable_content()
                .display_iter
                .filter_map(|indexed| {
                    if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                        return None;
                    }
                    let fg_rgb = match indexed.cell.fg {
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                        _ => return None,
                    };
                    let underline_rgb = match indexed.cell.underline_color() {
                        Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                        _ => None,
                    };
                    let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                    .map(|d| d.row)
                })
                .max();
            let final_rows = max_row.map(|r| r + 1).unwrap_or(0);
            // Checking the prompt's own printed text, not
            // `term.grid().cursor.point` — `resync_rich_content_
            // placements` deliberately never touches the cursor directly
            // (see that function's doc comment).
            let prompt_line = (term.topmost_line().0..=term.bottommost_line().0)
                .find(|&line_idx| term.grid()[Line(line_idx)][Column(0)].c == '~');
            assert_eq!(
                prompt_line,
                Some(final_rows as i32),
                "the prompt's own printed text must sit immediately after the image's final row count ({final_rows}) — no gap left behind by the many small resize steps"
            );
        });
    }

    #[gpui::test]
    async fn test_resize_growing_terminal_width_after_image_printed_in_a_small_window_does_not_panic(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real crash reported live, with the EXACT order of
        // events the user described: shrink the window FIRST, THEN show
        // the image (so it's printed directly into the already-small
        // window, never having existed at a larger size), THEN grow the
        // window's width back out by dragging the right edge — panicked
        // inside alacritty's own `RenderableCursor::new`
        // (`grid/storage.rs:221`, `requested.0 < visible_lines`) during
        // the very next `Terminal::sync()`. Root cause: as the terminal's
        // column count grows, the image's clamped row count scales up
        // with it (preserving aspect ratio) — on a short-enough screen,
        // that row count can reach the screen's FULL height, leaving zero
        // rows free for a prompt below the image. This function's cursor-
        // pinning logic (`if cursor_at_old_bottom { cursor.line =
        // new_bottom }`) didn't clamp `new_bottom` to the screen's last
        // real line, so it happily pointed the cursor at a `Line` one
        // past the end of the grid. The exact live-reported image
        // dimensions weren't known precisely (the log only captured cell
        // counts, not source pixel size), so this sweeps two real
        // scenarios reported live rather than guessing one combination.
        for (image_width_px, image_height_px, start_width_px, end_width_px, screen_height_px) in [
            // Matches an earlier crash log's exact numbers: cell_width=8,
            // line_height=15.6, 43 visible lines (670.8px tall), starting
            // at 126 columns (1008px) and growing past 141 (1128px).
            (1920, 1080, 1008, 1200, 670.8),
            // Matches the LATEST live crash report exactly: real
            // 1920x1080 JPEG, Som snapped to the left/right HALF of a
            // 1536x864 (logical, 200% DPI scale) primary display via
            // Win+Left/Right, then dragged wider from there.
            (1920, 1080, 768, 1536, 864.),
        ] {
            let window = cx.add_empty_window();
            let terminal = window.new(|cx| {
                TerminalBuilder::new_display_only(
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    PathStyle::local(),
                )
                .unwrap()
                .subscribe(cx)
            });

            let session_id = 0x0203_04;
            let file_id = 0x0607_08;
            let chunk = rich_content_transport::Chunk {
                content_type: rich_content_transport::ContentType::Jpeg,
                session_id,
                file_id,
                chunk_offset: 0,
                total_size: 1,
                metadata: rich_content_transport::ContentMetadata::Image {
                    width_px: image_width_px,
                    height_px: image_height_px,
                    color_bits: 32,
                    is_animated: false,
                },
                payload: vec![0u8],
            };
            let mut apc = Vec::new();
            apc.extend_from_slice(&[0x1B, b'_']);
            apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
            apc.extend_from_slice(&[0x1B, b'\\']);

            // Start ALREADY small (the window was shrunk before the image
            // was ever shown).
            window.update_window_entity(&terminal, |terminal, window, cx| {
                terminal.set_size(TerminalBounds::new(
                    px(15.6),
                    px(8.),
                    Bounds {
                        origin: Point::default(),
                        size: Size { width: px(start_width_px as f32), height: px(screen_height_px) },
                    },
                ));
                terminal.sync(window, cx);
                terminal.write_output(&apc, cx);
            });
            window.run_until_parked();

            let initial_columns = window.update_window_entity(&terminal, |terminal, _window, _cx| {
                terminal.term.lock().columns() as u32
            });
            let natural_columns = (image_width_px as f32 / 8.0).ceil() as u32;
            let natural_rows = (image_height_px as f32 / 15.6).ceil() as u32;
            let (columns, rows) = if natural_columns > initial_columns {
                let scale = initial_columns as f32 / natural_columns as f32;
                (initial_columns, ((natural_rows as f32 * scale).floor() as u32).max(1))
            } else {
                (natural_columns, natural_rows)
            };

            let mut placeholder_text = String::new();
            let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
            let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
            placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
            for row in 0..rows {
                for column in 0..columns {
                    if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                        placeholder_text.extend(cell);
                    }
                }
                placeholder_text.push_str("\r\n");
            }
            placeholder_text.push_str("\x1b[0m~ >> ");

            window.update_window_entity(&terminal, |terminal, _window, cx| {
                terminal.write_output(placeholder_text.as_bytes(), cx);
            });
            window.run_until_parked();

            // Grow the width back out in many small (single-pixel) steps,
            // mirroring a real drag-the-right-edge-wider gesture — this is
            // the exact sequence that crashed live. Success is simply
            // that every `sync()` call here completes without panicking,
            // even through the step(s) where the image's scaled-up row
            // count reaches (or exceeds) the screen's full height.
            let mut width_px = start_width_px;
            while width_px < end_width_px {
                width_px += 1;
                window.update_window_entity(&terminal, |terminal, window, cx| {
                    terminal.set_size(TerminalBounds::new(
                        px(15.6),
                        px(8.),
                        Bounds {
                            origin: Point::default(),
                            size: Size { width: px(width_px as f32), height: px(screen_height_px) },
                        },
                    ));
                    terminal.sync(window, cx);
                });
                window.run_until_parked();
            }
        }
    }

    #[gpui::test]
    async fn test_resize_placement_never_grows_taller_than_screen_minus_one_row(cx: &mut TestAppContext) {
        // Direct assertion of the requirement behind the fix in
        // `test_resize_growing_terminal_width_after_image_printed_in_a_
        // small_window_does_not_panic`: a placement must ALWAYS leave at
        // least one real row free below it for the prompt, on both axes —
        // never grow to fill the screen edge-to-edge, even when its
        // width-clamped natural row count (scaled to preserve aspect
        // ratio) would otherwise reach exactly `screen_lines()`. A short,
        // wide-aspect-ratio image on a short screen is the case most
        // likely to hit this: clamping only by column count still lets
        // the row count reach the full screen height.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // A 1920x1080px image at an 8x15.6px cell, on a screen only 43
        // lines tall (670.8px) — matches the real crash numbers. Natural
        // row count (1080/15.6 ≈ 70) clamped by width alone would still
        // land at 43 rows (the screen's full height) once the terminal is
        // wide enough, per the earlier live crash.
        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Jpeg,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 1920,
                height_px: 1080,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(15.6),
                px(8.),
                Bounds { origin: Point::default(), size: Size { width: px(1184.), height: px(670.8) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        let (initial_columns, screen_lines) = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            (term.columns() as u32, term.screen_lines() as u32)
        });

        let mut placeholder_text = String::new();
        let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
        let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
        placeholder_text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
        // Print at the terminal's OLD natural clamp — same two-axis clamp
        // `somcat::print_placeholder_grid` now applies (width first, then
        // height capped to `screen_lines - 1` so a row is always left for
        // the prompt) — so `resync_rich_content_placements` has a
        // `cells`-derived origin/bounding box matching what a real client
        // would actually send.
        let natural_columns = (1920f32 / 8.0).ceil() as u32;
        let natural_rows = (1080f32 / 15.6).ceil() as u32;
        let (mut columns, mut rows) = if natural_columns > initial_columns {
            let scale = initial_columns as f32 / natural_columns as f32;
            (initial_columns, ((natural_rows as f32 * scale).floor() as u32).max(1))
        } else {
            (natural_columns, natural_rows)
        };
        let max_rows = screen_lines.saturating_sub(1).max(1);
        if rows > max_rows {
            let scale = max_rows as f32 / rows as f32;
            rows = max_rows;
            columns = ((columns as f32 * scale).floor() as u32).max(1);
        }
        for row in 0..rows {
            for column in 0..columns {
                if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                    placeholder_text.extend(cell);
                }
            }
            placeholder_text.push_str("\r\n");
        }
        placeholder_text.push_str("\x1b[0m~ >> ");

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text.as_bytes(), cx);
        });
        window.run_until_parked();

        // Widen by one more pixel — the exact step that, before this
        // fix's height clamp, grew the image's row count to precisely
        // `screen_lines()`.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(15.6),
                px(8.),
                Bounds { origin: Point::default(), size: Size { width: px(1185.), height: px(670.8) } },
            ));
            terminal.sync(window, cx);
        });
        window.run_until_parked();

        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            let max_row = term
                .renderable_content()
                .display_iter
                .filter_map(|indexed| {
                    if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                        return None;
                    }
                    let fg_rgb = match indexed.cell.fg {
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                        _ => return None,
                    };
                    let underline_rgb = match indexed.cell.underline_color() {
                        Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                        _ => None,
                    };
                    let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                    .map(|d| d.row)
                })
                .max();
            let placement_rows = max_row.map(|r| r + 1).unwrap_or(0);
            assert!(
                placement_rows < screen_lines,
                "placement must leave at least one row free for the prompt: placement_rows={placement_rows} screen_lines={screen_lines}"
            );
            let cursor_line = term.grid().cursor.point.line.0;
            assert!(
                cursor_line < screen_lines as i32,
                "cursor must stay within the real screen: cursor_line={cursor_line} screen_lines={screen_lines}"
            );
        });
    }

    #[gpui::test]
    async fn test_resize_prompt_text_follows_image_after_growing_to_fill_screen_then_shrinking(
        cx: &mut TestAppContext,
    ) {
        // Reproduces a real bug shown on video: a small image printed in a
        // small window, the window WIDENED until the image's (aspect-
        // ratio-preserving) row count reached the screen's full height,
        // then narrowed back down over many small steps — the PROMPT TEXT
        // itself (the literal `~ >>` characters already printed into the
        // grid, not just the cursor) stayed frozen at its old row while a
        // separate blinking cursor glyph appeared several rows below it.
        // Neither `test_resize_shrinking_terminal_width_in_many_small_
        // steps_keeps_prompt_immediately_after_image` (grows only within
        // the height clamp, never fills the screen) nor `test_resize_
        // growing_terminal_width_after_image_printed_in_a_small_window_
        // does_not_panic` (grows then never shrinks back) cover this
        // specific grow-to-max-then-shrink sequence.
        let window = cx.add_empty_window();
        let terminal = window.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        let session_id = 0x0203_04;
        let file_id = 0x0607_08;
        let chunk = rich_content_transport::Chunk {
            content_type: rich_content_transport::ContentType::Jpeg,
            session_id,
            file_id,
            chunk_offset: 0,
            total_size: 1,
            metadata: rich_content_transport::ContentMetadata::Image {
                width_px: 1920,
                height_px: 1080,
                color_bits: 32,
                is_animated: false,
            },
            payload: vec![0u8],
        };
        let mut apc = Vec::new();
        apc.extend_from_slice(&[0x1B, b'_']);
        apc.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
        apc.extend_from_slice(&[0x1B, b'\\']);

        // Start small, on a tall-enough screen that the image never has to
        // clamp by height at its initial (narrow) width.
        window.update_window_entity(&terminal, |terminal, window, cx| {
            terminal.set_size(TerminalBounds::new(
                px(15.6),
                px(8.),
                Bounds { origin: Point::default(), size: Size { width: px(400.), height: px(733.2) } },
            ));
            terminal.sync(window, cx);
            terminal.write_output(&apc, cx);
        });
        window.run_until_parked();

        let (initial_columns, screen_lines) = window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            (term.columns() as u32, term.screen_lines() as u32)
        });

        let placeholder_text = |columns: u32, rows: u32| {
            let mut text = String::new();
            let (sr, sg, sb) = ((session_id >> 16) as u8, (session_id >> 8) as u8, session_id as u8);
            let (fr, fg, fb) = ((file_id >> 16) as u8, (file_id >> 8) as u8, file_id as u8);
            text.push_str(&format!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m"));
            for row in 0..rows {
                for column in 0..columns {
                    if let Some(cell) = kitty_graphics_placeholder::encode_cell(row, column) {
                        text.extend(cell);
                    }
                }
                text.push_str("\r\n");
            }
            text.push_str("\x1b[0m~ >> ");
            text
        };

        let natural_columns = (1920f32 / 8.0).ceil() as u32;
        let natural_rows = (1080f32 / 15.6).ceil() as u32;
        let (mut columns, mut rows) = if natural_columns > initial_columns {
            let scale = initial_columns as f32 / natural_columns as f32;
            (initial_columns, ((natural_rows as f32 * scale).floor() as u32).max(1))
        } else {
            (natural_columns, natural_rows)
        };
        let max_rows = screen_lines.saturating_sub(1).max(1);
        if rows > max_rows {
            let scale = max_rows as f32 / rows as f32;
            rows = max_rows;
            columns = ((columns as f32 * scale).floor() as u32).max(1);
        }

        window.update_window_entity(&terminal, |terminal, _window, cx| {
            terminal.write_output(placeholder_text(columns, rows).as_bytes(), cx);
        });
        window.run_until_parked();

        // Grow the width in many small (single-pixel) steps until the
        // image's row count reaches the screen's full height (clamped),
        // then shrink back down over just as many steps — the exact
        // sequence shown on video.
        let mut width_px = 400;
        while width_px < 2400 {
            width_px += 4;
            window.update_window_entity(&terminal, |terminal, window, cx| {
                terminal.set_size(TerminalBounds::new(
                    px(15.6),
                    px(8.),
                    Bounds { origin: Point::default(), size: Size { width: px(width_px as f32), height: px(733.2) } },
                ));
                terminal.sync(window, cx);
            });
            window.run_until_parked();
        }
        while width_px > 400 {
            width_px -= 4;
            window.update_window_entity(&terminal, |terminal, window, cx| {
                terminal.set_size(TerminalBounds::new(
                    px(15.6),
                    px(8.),
                    Bounds { origin: Point::default(), size: Size { width: px(width_px as f32), height: px(733.2) } },
                ));
                terminal.sync(window, cx);
            });
            window.run_until_parked();
        }

        // After all that, the cursor AND the actual printed prompt text
        // (`~ >> `) must be on the SAME row, immediately after the
        // image's final (shrunk-back) row count — not desynced, with the
        // literal characters frozen on some earlier row.
        window.update_window_entity(&terminal, |terminal, _window, _cx| {
            let term = terminal.term.lock();
            let max_row = term
                .renderable_content()
                .display_iter
                .filter_map(|indexed| {
                    if indexed.cell.c != kitty_graphics_placeholder::PLACEHOLDER_CHAR {
                        return None;
                    }
                    let fg_rgb = match indexed.cell.fg {
                        alacritty_terminal::vte::ansi::Color::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
                        _ => return None,
                    };
                    let underline_rgb = match indexed.cell.underline_color() {
                        Some(alacritty_terminal::vte::ansi::Color::Spec(rgb)) => Some((rgb.r, rgb.g, rgb.b)),
                        _ => None,
                    };
                    let diacritics = indexed.cell.zerowidth().unwrap_or(&[]);
                    kitty_graphics_placeholder::decode_placeholder_cell(
                        indexed.cell.c,
                        fg_rgb,
                        underline_rgb,
                        diacritics,
                    )
                    .map(|d| d.row)
                })
                .max();
            let final_placement_rows = max_row.map(|r| r + 1).unwrap_or(0);
            let cursor_line = term.grid().cursor.point.line.0;

            // Find the row the literal `~` prompt character actually sits
            // on, by scanning the grid directly (not `renderable_content`,
            // which would just report the cursor's own line back to us).
            let tilde_line = (term.topmost_line().0..=term.bottommost_line().0).find(|&line_idx| {
                term.grid()[Line(line_idx)][Column(0)].c == '~'
            });

            assert_eq!(
                cursor_line, final_placement_rows as i32,
                "cursor must sit immediately after the image's final row count ({final_placement_rows})"
            );
            assert_eq!(
                tilde_line,
                Some(cursor_line),
                "the actual printed prompt text ('~') must be on the SAME row as the cursor, not frozen on an earlier row from before the resize sequence"
            );
        });
    }

    #[gpui::test]
    async fn test_rich_content_many_real_chunks_via_write_output_reconstructs_file_exactly(cx: &mut TestAppContext) {
        // Regression-style test isolating the transport from the
        // real-process/real-ConPTY variable entirely — feeds ALL ~3000+
        // chunks a real giphy.gif produces (via the exact same
        // `split_into_chunks` a real `somcat --stream` invocation uses)
        // straight through `write_output` (synchronous injection into
        // the VTE parser, no child process, no PTY) in one shot. Written
        // to isolate whether `bench_rich_content_stream_progressive_
        // playback_starts_before_transfer_completes`'s mystery failure
        // (all envelopes individually confirmed reaching `process_event`
        // successfully via `log::trace!`, yet `term.get_content()` shows
        // garbled binary-as-text and the GIF never decodes even
        // partially) is a transport/parser-scale problem (this test
        // would also fail) or is specific to the real subprocess/ConPTY
        // path that test uses (this test would pass, narrowing the bug
        // to process/PTY-specific plumbing instead of the APC parsing
        // itself). Reconstructs the on-disk file from all chunks and
        // asserts it's byte-for-byte identical to the source GIF —
        // catches ANY corruption, not just "did some frames decode."
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        let giphy_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif");
        let source_bytes = std::fs::read(&giphy_path).unwrap_or_else(|e| panic!("reading {giphy_path:?}: {e}"));

        let session_id = 0x5EED_0001u32;
        let file_id = 0x5EED_0002u32;
        let pieces = rich_content_transport::split_into_chunks(&source_bytes, 4096);

        let mut all_apc_bytes: Vec<u8> = Vec::new();
        let mut offset = 0u64;
        for payload in &pieces {
            let chunk = rich_content_transport::Chunk {
                content_type: rich_content_transport::ContentType::Gif,
                session_id,
                file_id,
                chunk_offset: offset,
                total_size: source_bytes.len() as u64,
                metadata: rich_content_transport::ContentMetadata::Image {
                    width_px: 480,
                    height_px: 480,
                    color_bits: 32,
                    is_animated: true,
                },
                payload: payload.clone(),
            };
            all_apc_bytes.extend_from_slice(&[0x1B, b'_']);
            all_apc_bytes.extend_from_slice(&rich_content_transport::build_envelope(&chunk));
            all_apc_bytes.extend_from_slice(&[0x1B, b'\\']);
            offset += payload.len() as u64;
        }

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&all_apc_bytes, cx);
        });
        // `write_output` only enqueues `Event::ApcString` onto the async
        // `events_tx`/`events_rx` channel (see `ZedListener::send_event`);
        // the actual `process_event` handler that parses envelopes and
        // writes into `rich_content_cache` runs on the event-loop task
        // spawned by `subscribe`, which only makes progress once the test
        // executor is given a chance to drive it.
        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let contiguous = terminal.rich_content_cache.contiguous_len(session_id, file_id);
            eprintln!("TEST: contiguous_len after write_output = {contiguous} (source was {} bytes)", source_bytes.len());
            assert_eq!(
                contiguous,
                source_bytes.len() as u64,
                "contiguous watermark must reach the full source file length — a gap here means some \
                 chunk failed to parse/apply even though this test bypasses the real subprocess/ConPTY entirely"
            );
            let path = terminal.rich_content_cache.path(session_id, file_id).expect("cache file must exist");
            let on_disk = std::fs::read(path).expect("cache file must be readable");
            assert_eq!(
                on_disk, source_bytes,
                "reconstructed file must be byte-for-byte identical to the source GIF — any mismatch \
                 here (even one byte) means the APC transport corrupted data somewhere in the stream"
            );

            // Also confirm the reconstructed file actually decodes as a
            // real, complete GIF — the strongest possible proof the
            // transport-level byte-identity check above isn't missing
            // something a naive comparison could paper over.
            let decoded = rich_content_gif_player::try_decode_progressive(path, contiguous)
                .expect("decode must not error")
                .expect("a full, valid GIF must decode into at least one frame");
            assert!(decoded.complete, "the full reconstructed file must decode as complete, not partial");
            assert_eq!(decoded.frames.len(), 47, "giphy.gif fixture is known to have 47 frames");
        });
    }

    #[cfg(windows)]

    #[cfg(windows)]

    #[cfg(windows)]

    #[gpui::test]
    async fn bench_rich_content_stream_progressive_playback_starts_before_transfer_completes(cx: &mut TestAppContext) {
        // The core property that distinguishes the rich-content protocol
        // (`somcat --stream`, `rich_content_transport`/`rich_content_cache`/
        // `rich_content_gif_player`) from the OLD Kitty-animation path this
        // whole redesign replaced: a decoder must be able to start showing
        // frames from a file that is STILL being written to disk, not only
        // once the entire transfer has finished. This test proves that
        // property directly — not just "it eventually works end to end"
        // (see the various `bench_somcat_*` tests above for that, on the
        // OLD Kitty path) but specifically that SOME frames are decodable
        // from a growing file BEFORE the underlying `somcat --stream`
        // process has exited, by racing the two: poll
        // `rich_content_cache::contiguous_len` + `try_decode_progressive`
        // on a fixed cadence while independently watching for process exit,
        // and assert the frame count was already nonzero on an iteration
        // where the process was confirmed still running.
        cx.executor().allow_parking();

        // Same patched-conpty.dll setup `test_somcat_displays_a_realistic_large_photo_sized_image`
        // uses — without it, this test falls back to Windows' own
        // baseline ConPTY implementation, which has materially different
        // (worse, per `alacritty_terminal`'s own doc comment on
        // `conpty.rs`) buffering behavior than the patched one this
        // project ships/expects in production.
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
            use windows::core::HSTRING;
            let conpty_dir =
                dirs::home_dir().expect("home dir must resolve").join(".config").join("som").join("conpty");
            if conpty_dir.join("conpty.dll").is_file() {
                SetDllDirectoryW(&HSTRING::from(conpty_dir.as_os_str())).ok();
            }
        }

        let giphy_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif");
        assert!(giphy_path.is_file(), "giphy.gif not found at {giphy_path:?}");

        let test_exe = std::env::current_exe().expect("current_exe must resolve in a test binary");
        let target_debug_dir = test_exe.parent().and_then(|p| p.parent()).expect("target/<profile>/deps/.. shape");
        let somcat_path = target_debug_dir.join(if cfg!(windows) { "somcat.exe" } else { "somcat" });
        assert!(somcat_path.is_file(), "somcat bin target not found at {somcat_path:?} — run `cargo build -p somcat`");

        let (terminal, completion_rx) = build_test_terminal_with_arguments(
            cx,
            somcat_path.to_string_lossy().into_owned(),
            vec![giphy_path.to_string_lossy().into_owned()],
        )
        .await;

        let spawn_start = Instant::now();
        let mut saw_partial_frames_while_process_still_running = false;
        let mut first_partial_frame_count = 0usize;
        let mut elapsed_at_first_partial_frames = None;
        let mut final_frame_count = 0usize;

        // Same bounded-poll shape as the other `bench_somcat_*` tests
        // (200ms * 300 = 60s ceiling) — see `feedback_bounded_test_loops`
        // memory for why an unbounded loop here would be a real hazard,
        // not just style.
        for i in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cx.run_until_parked();

            let still_running = completion_rx.try_recv().is_err();

            let (session_id, file_id, path) = terminal.update(cx, |term, _| {
                // Only one file is ever streamed in this test — grab
                // whatever (session_id, file_id) the cache has recorded
                // so far, if any chunk has arrived yet.
                term.rich_content_cache
                    .all_known_ids()
                    .into_iter()
                    .next()
                    .map(|(s, f)| (s, f, term.rich_content_cache.path(s, f).map(|p| p.to_path_buf())))
                    .unwrap_or((0, 0, None))
            });

            if let Some(path) = path {
                let available = terminal.update(cx, |term, _| term.rich_content_cache.contiguous_len(session_id, file_id));
                if let Ok(Some(decoded)) = rich_content_gif_player::try_decode_progressive(&path, available) {
                    let fc = decoded.frames.len();
                    if fc > 0 && first_partial_frame_count == 0 {
                        first_partial_frame_count = fc;
                        elapsed_at_first_partial_frames = Some(spawn_start.elapsed());
                        eprintln!(
                            "BENCH(stream): first partial decode ({fc} frames, complete={}) after {:?} (poll {i}, still_running={still_running})",
                            decoded.complete,
                            elapsed_at_first_partial_frames.unwrap()
                        );
                    }
                    if fc > 0 && still_running {
                        saw_partial_frames_while_process_still_running = true;
                    }
                    final_frame_count = fc;
                    if decoded.complete {
                        eprintln!("BENCH(stream): transfer complete, {fc} total frames, after {:?}", spawn_start.elapsed());
                        break;
                    }
                }
            }

            if !still_running && final_frame_count > 0 {
                // Process exited and we've already decoded whatever the
                // final state is — no point polling further.
                break;
            }
        }

        eprintln!(
            "BENCH(stream) SUMMARY: first_partial={:?} at {:?}, final_frame_count={final_frame_count}, \
             saw_partial_while_running={saw_partial_frames_while_process_still_running}",
            first_partial_frame_count, elapsed_at_first_partial_frames
        );

        assert!(first_partial_frame_count > 0, "somcat --stream never produced even a partial decode within the 15s budget");
        assert_eq!(final_frame_count, 47, "giphy.gif fixture is known to have 47 frames — the final decode must reach all of them");
        assert!(
            saw_partial_frames_while_process_still_running,
            "never observed a nonzero partial frame count while the somcat --stream process was still running — \
             this is the core progressive-playback property this benchmark exists to prove, and it did not hold"
        );
    }

    #[gpui::test]
    async fn test_rich_content_placements_reach_the_paint_path_via_a_real_process(cx: &mut TestAppContext) {
        // Proves the LAST link in the SRP pipeline that
        // `bench_rich_content_stream_progressive_playback_starts_before_
        // transfer_completes` above doesn't itself check: that
        // `Terminal::rich_content_placements()` — the exact method
        // `terminal_view::terminal_element::paint_rich_content_placements`
        // calls at paint time — returns a real anchor plus a non-empty,
        // advancing `RenderImage` once a real `somcat --stream` child
        // process (real ConPTY, not `write_output`) has sent enough of
        // giphy.gif. Doesn't paint anything itself (no `Window`/`App`
        // paint context available in a `#[gpui::test]`) — this is the
        // data-availability half of the paint path, not a pixel-level
        // rendering check.
        cx.executor().allow_parking();

        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
            use windows::core::HSTRING;
            let conpty_dir =
                dirs::home_dir().expect("home dir must resolve").join(".config").join("som").join("conpty");
            if conpty_dir.join("conpty.dll").is_file() {
                SetDllDirectoryW(&HSTRING::from(conpty_dir.as_os_str())).ok();
            }
        }

        let giphy_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../giphy.gif");
        assert!(giphy_path.is_file(), "giphy.gif not found at {giphy_path:?}");

        let test_exe = std::env::current_exe().expect("current_exe must resolve in a test binary");
        let target_debug_dir = test_exe.parent().and_then(|p| p.parent()).expect("target/<profile>/deps/.. shape");
        let somcat_path = target_debug_dir.join(if cfg!(windows) { "somcat.exe" } else { "somcat" });
        assert!(somcat_path.is_file(), "somcat bin target not found at {somcat_path:?} — run `cargo build -p somcat`");

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            somcat_path.to_string_lossy().into_owned(),
            vec![giphy_path.to_string_lossy().into_owned()],
        )
        .await;

        let mut saw_nonempty_placements = false;
        let mut saw_frame_advance = false;
        let mut first_frame_seen: Option<usize> = None;

        for _ in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cx.run_until_parked();

            let placements = terminal.update(cx, |term, _| term.rich_content_placements());
            if let Some((session_id, _file_id, render_image, current_frame, _is_animating)) =
                placements.into_iter().next()
            {
                saw_nonempty_placements = true;
                assert!(render_image.frame_count() > 0, "a placement must always have at least one decoded frame");
                // No position to assert here anymore — placements have no
                // separate anchor now that they're real Unicode-placeholder
                // grid cells (see `Terminal::print_rich_content_placeholder_grid`).
                // Just confirm a real (nonzero) session_id was captured.
                assert!(session_id > 0, "session_id must be a real, nonzero id");

                match first_frame_seen {
                    None => first_frame_seen = Some(current_frame),
                    Some(first) if current_frame != first => saw_frame_advance = true,
                    Some(_) => {},
                }

                if saw_nonempty_placements && saw_frame_advance {
                    break;
                }
            }
        }

        assert!(saw_nonempty_placements, "rich_content_placements() never returned a placement within the poll budget");
        assert!(
            saw_frame_advance,
            "current_frame never advanced across polls — animation playback isn't ticking through the paint-path accessor"
        );
    }

    #[gpui::test]
    async fn test_rich_content_audio_placement_decodes_and_plays_via_a_real_process(cx: &mut TestAppContext) {
        // Audio counterpart to
        // `test_rich_content_placements_reach_the_paint_path_via_a_real_
        // process` above — proves the WHOLE pipeline end-to-end through a
        // real `somcat` child process (real ConPTY) and a real `cpal`
        // output device on this machine: transport -> cache -> symphonia
        // decode -> cpal playback -> position tracking. This is
        // deliberately NOT a mock: `RichContentAudioPlayer::open` opens
        // the actual default output device, so this test only proves
        // anything on a machine that has one (skips gracefully otherwise,
        // matching `rich_content_audio_player`'s own test tolerance).
        cx.executor().allow_parking();

        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
            use windows::core::HSTRING;
            let conpty_dir =
                dirs::home_dir().expect("home dir must resolve").join(".config").join("som").join("conpty");
            if conpty_dir.join("conpty.dll").is_file() {
                SetDllDirectoryW(&HSTRING::from(conpty_dir.as_os_str())).ok();
            }
        }

        let tone_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_fixtures/tone.flac");
        assert!(tone_path.is_file(), "test_fixtures/tone.flac not found at {tone_path:?}");

        let test_exe = std::env::current_exe().expect("current_exe must resolve in a test binary");
        let target_debug_dir = test_exe.parent().and_then(|p| p.parent()).expect("target/<profile>/deps/.. shape");
        let somcat_path = target_debug_dir.join(if cfg!(windows) { "somcat.exe" } else { "somcat" });
        assert!(somcat_path.is_file(), "somcat bin target not found at {somcat_path:?} — run `cargo build -p somcat`");

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            somcat_path.to_string_lossy().into_owned(),
            vec![tone_path.to_string_lossy().into_owned()],
        )
        .await;

        let mut saw_placement = false;
        let mut session_and_file_id: Option<(u32, u32)> = None;

        for _ in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cx.run_until_parked();

            let placements = terminal.update(cx, |term, _| term.rich_content_audio_placements());
            if let Some((session_id, file_id, _position_fraction, _is_playing, _elapsed, duration)) =
                placements.into_iter().next()
            {
                saw_placement = true;
                session_and_file_id = Some((session_id, file_id));
                assert!(duration.as_secs_f64() > 0.0, "a decoded audio placement must report a nonzero duration");
                break;
            }
        }

        if !saw_placement {
            // No default output device in this environment (matches
            // `rich_content_audio_player`'s own tests' tolerance) — the
            // transport/decode side still ran (the file streamed and
            // `RichContentAudioPlayer::open` was attempted), just no
            // device to open. Not a failure of this feature's own logic.
            eprintln!("skipping playback assertions: no audio placement/device available in this environment");
            return;
        }

        let (session_id, file_id) = session_and_file_id.expect("set alongside saw_placement");

        // Toggle playback on (starts paused, matching the module's own
        // "don't autoplay" doc comment) and confirm the reported position
        // actually advances — proves the `cpal` callback thread is really
        // consuming samples, not just that a player object exists.
        terminal.update(cx, |term, _| term.toggle_rich_content_audio_playback(session_id, file_id));

        let mut first_fraction: Option<f32> = None;
        let mut saw_advance = false;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cx.run_until_parked();
            let placements = terminal.update(cx, |term, _| term.rich_content_audio_placements());
            let Some((_, _, position_fraction, is_playing, _, _)) =
                placements.iter().find(|(sid, fid, ..)| *sid == session_id && *fid == file_id)
            else {
                continue;
            };
            assert!(*is_playing, "playback must report as playing after toggling it on");
            match first_fraction {
                None => first_fraction = Some(*position_fraction),
                Some(first) if *position_fraction > first => {
                    saw_advance = true;
                    break;
                },
                Some(_) => {},
            }
        }
        assert!(saw_advance, "playback position never advanced after toggling play on");
    }

    #[gpui::test]
    async fn test_unrecognized_apc_string_is_silently_ignored(cx: &mut TestAppContext) {
        // Degradation check: APC is a general-purpose escape mechanism —
        // some OTHER program could legitimately use it for something this
        // implementation knows nothing about (no leading marker byte
        // Som's own rich-content protocol recognizes, or a recognizable
        // marker but garbage envelope data). Som's handling of anything it
        // doesn't understand must be a true no-op (no panic, no corrupted
        // terminal state, no stray cache entries), not a special
        // "recognized but broken" path. Ordinary text sent immediately
        // before/after the unrecognized APC string must render completely
        // unaffected, proving the byte stream itself wasn't disrupted.
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // Three different "not a Kitty graphics command this implementation
        // recognizes" shapes: no leading G at all, a leading G with
        // malformed control data, and a leading G with a well-formed but
        // unsupported action — all wrapped in real ANSI text before/after
        // to prove the surrounding stream survives intact.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"before");
        bytes.extend_from_slice(b"\x1b_some_other_protocols_apc_data\x1b\\");
        bytes.extend_from_slice(b"\x1b_Ggarbage,no,equals,signs\x1b\\");
        bytes.extend_from_slice(b"\x1b_Ga=zzz\x1b\\");
        bytes.extend_from_slice(b"after");

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&bytes, cx);
        });
        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            // `last_content` only refreshes via `sync()` — read straight off
            // the real `Term` instead, same pattern
            // `test_write_output_preserves_existing_crlf` already uses.
            let content = {
                let term = terminal.term.lock_unfair();
                Terminal::make_content(&term, &terminal.last_content)
            };
            let text: String = content.cells.iter().map(|cell| cell.c).collect();
            assert!(
                text.contains("before") && text.contains("after"),
                "ordinary text around an unrecognized APC string must render unaffected — got: {text:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_write_output_preserves_existing_crlf(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // Test that existing CRLF doesn't get doubled
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"line1\r\nline2\r\n", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            Terminal::make_content(&term, &terminal.last_content)
        });

        let cells = &content.cells;

        // Check that both lines start at column 0
        let mut found_lines_at_column_0 = 0;
        for cell in cells {
            if cell.c == 'l' && cell.point.column.0 == 0 {
                found_lines_at_column_0 += 1;
            }
        }

        assert!(
            found_lines_at_column_0 >= 2,
            "Both lines should start at column 0"
        );
    }

    #[gpui::test]
    async fn test_write_output_preserves_bare_cr(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .unwrap()
            .subscribe(cx)
        });

        // Test that bare CR (without LF) is preserved
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"hello\rworld", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            Terminal::make_content(&term, &terminal.last_content)
        });

        let cells = &content.cells;

        // Check that we have "world" at the beginning of the line
        let mut text = String::new();
        for cell in cells.iter().take(5) {
            if cell.point.line.0 == 0 {
                text.push(cell.c);
            }
        }

        assert!(
            text.starts_with("world"),
            "Bare CR should allow overwriting: got '{}'",
            text
        );
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_same_position(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            let click_position = point(px(80.0), px(10.0));
            ctrl_mouse_down_at(terminal, click_position, cx);
            ctrl_mouse_up_at(terminal, click_position, cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, true))),
                "Should have ProcessHyperlink event when ctrl+clicking on same hyperlink position"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_drag_outside_bounds(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(
            cx,
            b"Visit https://zed.dev/ for more\r\nThis is another line\r\n",
        );

        terminal.update(cx, |terminal, cx| {
            let down_position = point(px(80.0), px(10.0));
            let up_position = point(px(10.0), px(50.0));

            ctrl_mouse_down_at(terminal, down_position, cx);
            ctrl_mouse_move_to(terminal, up_position, cx);
            ctrl_mouse_up_at(terminal, up_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "Should NOT have ProcessHyperlink event when dragging outside the hyperlink"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_drag_within_bounds(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            let down_position = point(px(70.0), px(10.0));
            let up_position = point(px(130.0), px(10.0));

            ctrl_mouse_down_at(terminal, down_position, cx);
            ctrl_mouse_move_to(terminal, up_position, cx);
            ctrl_mouse_up_at(terminal, up_position, cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, true))),
                "Should have ProcessHyperlink event when dragging within hyperlink bounds"
            );
        });
    }

    /// Polls the terminal content until `expected` appears, or panics after ~1s.
    /// The PTY IO thread writes into the terminal grid independently of the
    /// GPUI executor, so we need a real-time polling loop to synchronize.
    async fn assert_content_eventually(
        terminal: &Entity<Terminal>,
        expected: &str,
        cx: &mut TestAppContext,
    ) {
        let mut content = String::new();
        for _ in 0..100 {
            content = terminal.update(cx, |term, _| term.get_content());
            if content.contains(expected) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        panic!("Expected terminal content to contain {expected:?}, got: {content}");
    }

    #[cfg(unix)]
    async fn assert_foreground_process_command_eventually(
        terminal: &Entity<Terminal>,
        expected: &str,
        cx: &mut TestAppContext,
    ) {
        let mut command_name = None;
        for _ in 0..100 {
            terminal.update(cx, |terminal, _| {
                if let TerminalType::Pty { info, .. } = &terminal.terminal_type {
                    info.load_for_test();
                }
            });
            command_name =
                terminal.update(cx, |terminal, _| terminal.foreground_process_command_name());
            if command_name.as_deref() == Some(expected) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let process_info = terminal.update(cx, |terminal, _| match &terminal.terminal_type {
            TerminalType::Pty { info, .. } => format!(
                "pid={:?}, fallback_pid={:?}, has_current_info={}",
                info.pid(),
                info.pid_getter().fallback_pid(),
                info.current.read().is_some()
            ),
            TerminalType::DisplayOnly => "display-only".to_string(),
        });
        panic!(
            "Expected foreground process command name to be {expected:?}, got {command_name:?}; process info: {process_info:?}"
        );
    }

    /// Test that kill_active_task properly terminates both the foreground process
    /// and the shell, allowing wait_for_completed_task to complete and output to be captured.
    #[cfg(unix)]
    #[gpui::test]
    async fn test_kill_active_task_completes_and_captures_output(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        // Run a command that prints output then sleeps for a long time
        // The echo ensures we have output to capture before killing
        let (terminal, completion_rx) =
            build_test_terminal(cx, "echo", &["test_output_before_kill; sleep 60"]).await;

        assert_content_eventually(&terminal, "test_output_before_kill", cx).await;

        // Kill the active task
        terminal.update(cx, |term, _cx| {
            term.kill_active_task();
        });

        // wait_for_completed_task should complete within a reasonable time (not hang)
        let completion_result = completion_rx.recv().await;
        assert!(
            completion_result.is_ok(),
            "wait_for_completed_task should complete after kill_active_task, but it timed out"
        );

        // The exit status should indicate the process was killed (not a clean exit)
        let exit_status = completion_result.unwrap();
        assert!(
            exit_status.is_some(),
            "Should have received an exit status after killing"
        );

        // Verify that output captured before killing is still available
        let content = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content.contains("test_output_before_kill"),
            "Output from before kill should be captured, got: {content}"
        );
    }

    /// Test that kill_active_task on a task that's not running is a no-op
    #[gpui::test]
    async fn test_kill_active_task_on_completed_task_is_noop(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        // Run a command that exits immediately
        let (terminal, completion_rx) = build_test_terminal(cx, "echo", &["done"]).await;

        // Wait for the command to complete naturally
        let exit_status = completion_rx
            .recv()
            .await
            .expect("Should receive exit status");
        assert_eq!(exit_status, Some(ExitStatus::default()));

        assert_content_eventually(&terminal, "done", cx).await;

        // Now try to kill - should be a no-op since task already completed
        terminal.update(cx, |term, _cx| {
            term.kill_active_task();
        });

        // Content should still be there
        let content = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content.contains("done"),
            "Output should still be present after no-op kill, got: {content}"
        );
    }

    /// Finds `som-tmux(.exe)` next to whichever `target/debug` this
    /// test binary itself was built into — mirrors
    /// `terminal_view::terminal_panel::som_tmux_binary_path`'s own
    /// resolution logic (that one looks next to `som.exe`; this one looks
    /// next to `target/debug/deps/terminal-<hash>.exe`, one directory
    /// shallower, since `cargo test` binaries live in `deps/` while
    /// `cargo build`'s bin targets land directly in `target/debug/`).
    /// Returns `None` (causing the test to skip, not fail) if the binary
    /// hasn't been built yet — this test exercises the REAL som-tmux
    /// binary as an external process (deliberately, since the whole point is
    /// testing the wire protocol between a real `Terminal`'s PTY and it),
    /// not something `cargo test` builds automatically as a dependency.
    fn find_som_tmux_binary() -> Option<std::path::PathBuf> {
        let test_exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let binary_name = if cfg!(target_os = "windows") { "som-tmux.exe" } else { "som-tmux" };
        for candidate_dir in [test_exe_dir.clone(), test_exe_dir.parent()?.to_path_buf()] {
            let candidate = candidate_dir.join(binary_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// Reproduces the exact mechanism `TerminalView::on_removed`
    /// (`crates/terminal_view/src/terminal_view.rs`) relies on to make a
    /// tmux-wrapped tab's HOLDER actually die when the user explicitly
    /// closes that tab — entirely headless, no window, no OS-level
    /// keystrokes, no screenshots (see `project_som_tmux`/
    /// `feedback_gpui_test_framework_priority` memory for why this is the
    /// preferred way to test Som's UI-adjacent logic).
    ///
    /// Spawns a REAL `som-tmux` RELAY (as this `Terminal`'s own
    /// shell command, exactly like a `tmux: true` profile does), lets it
    /// spawn its own detached HOLDER, then writes a single NUL byte via
    /// `Terminal::input` — the same call `on_removed` makes — and asserts
    /// that the RELAY's own log ends up showing it forwarded
    /// `RelayInput::Close` to the HOLDER, which is the signal that actually
    /// kills the real shell process for good (as opposed to the HOLDER
    /// surviving forever, which was the original bug report).
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn test_nul_byte_signals_tmux_relay_to_close_for_good(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let Some(server_path) = find_som_tmux_binary() else {
            eprintln!("skipping: som-tmux binary not built, run `cargo build -p som_tmux` first");
            return;
        };

        let pane_id = format!("test-nul-close-{}", std::process::id());
        let profile_name = "test-nul-close";

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            server_path.to_string_lossy().into_owned(),
            vec![
                profile_name.to_string(),
                pane_id.clone(),
                "C:\\Windows\\System32\\cmd.exe".to_string(),
            ],
        )
        .await;

        // Give the RELAY a moment to spawn its detached HOLDER and complete
        // the handshake. Deliberately a REAL `std::thread::sleep`, not
        // `cx.executor().timer(...)` — this test's `TestAppContext` uses
        // GPUI's deterministic test executor, which fast-forwards its own
        // timers instantly rather than sleeping wall-clock time (there's
        // nothing else scheduled on it to make waiting on it meaningful).
        // The real `som-tmux` RELAY/HOLDER pair are genuine external
        // OS processes running on real wall-clock time regardless, so
        // waiting on them requires actually blocking this thread — safe
        // here only because `cx.executor().allow_parking()` was called
        // above.
        std::thread::sleep(std::time::Duration::from_secs(2));

        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![0u8]);
        });

        // Give the async PTY I/O thread time to actually deliver the byte
        // before killing the RELAY — see `TerminalView::on_removed`'s own
        // doc comment for why this same delay exists in production code.
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(terminal); // triggers Terminal::drop, killing the RELAY process

        // `session.kill()` (server.rs's `RelayInput::Close` branch) kills
        // the real shell process; the HOLDER's own change-watcher thread
        // then observes that exit and logs the line asserted on below right
        // before calling `std::process::exit(0)` — see
        // `server::run`'s doc comment. That log line is the ONLY
        // unambiguous proof the HOLDER actually died for good, as opposed
        // to merely seeing its one RELAY connection drop (which also
        // happens on every ordinary disconnect-without-Close, i.e. the
        // pre-fix buggy behavior this test exists to catch). Poll (with
        // real sleeps, same reasoning as above) rather than a single fixed
        // wait since real process teardown time isn't bounded tightly
        // enough for one guess to be reliable.
        // NOT `paths::logs_dir()` — under `cfg!(test)`,
        // `util::paths::home_dir()` is hardcoded to a fake `C:\Users\zed`
        // fixture home (test isolation, so tests never touch the real
        // user's actual home directory), but the `som-tmux.exe`
        // child process spawned above is a separate, non-test binary that
        // has no such override and logs to the REAL home directory. This
        // test needs to read what that real child process actually wrote,
        // so it computes the log path the same way `som_tmux::main`
        // does when not built under `cfg!(test)` — via `dirs::home_dir()`
        // directly.
        let real_home = dirs::home_dir().expect("failed to determine home directory");
        let holder_log_path = real_home
            .join(".config")
            .join("som")
            .join("logs")
            .join(format!("som-tmux-{profile_name}-{pane_id}-holder.log"));
        let mut holder_log = String::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            holder_log = std::fs::read_to_string(&holder_log_path).unwrap_or_default();
            if holder_log.contains("shell process exited, holder shutting down") {
                break;
            }
        }
        assert!(
            holder_log.contains("shell process exited, holder shutting down"),
            "HOLDER should have died after the NUL byte was forwarded as RelayInput::Close — \
             this is the exact bug this test guards against (HOLDER outliving an explicitly \
             closed tab). Got holder log: {holder_log:?}"
        );

        let log_path = real_home
            .join(".config")
            .join("som")
            .join("logs")
            .join(format!("som-tmux-{profile_name}-{pane_id}-relay.log"));
        let log_contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log_contents.contains("holder handshake"),
            "sanity check: relay should have at least completed its handshake — got log: {log_contents:?}"
        );

        std::fs::remove_file(&log_path).ok();
        std::fs::remove_file(&holder_log_path).ok();
    }

    /// Same mechanism as `test_nul_byte_signals_tmux_relay_to_close_for_good`,
    /// but exercising the `RemoteKind::Ssh` path
    /// (`terminal_view::terminal_panel::wrap_remote_command_args`) instead
    /// of the local one — this is what a `tmux: true` profile whose `shell`
    /// is `ssh <host>` actually runs (see `project_som_tmux` memory,
    /// "Обновление 30": adding macOS support turned out to need zero changes
    /// to this crate or `terminal_view`, since `RemoteKind::Ssh` was already
    /// handled identically to `RemoteKind::Wsl` — this test is what actually
    /// proves that claim against a real machine instead of just reading the
    /// code and assuming).
    ///
    /// `#[ignore]`d by default: needs a real, reachable SSH host with
    /// `~/.local/bin/som-tmux` already built there (this test does
    /// NOT deploy it — see `ensure_remote_binary_deployed` in
    /// `terminal_panel.rs` for that, which is Som's own production path, not
    /// this test's job) and its log files live on THAT machine, not
    /// locally, so this reads them back over a second `ssh` invocation
    /// rather than `std::fs`. Run explicitly with:
    /// `cargo test -p terminal test_ssh_remote_tmux_close_kills_the_holder -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_remote_tmux_close_kills_the_holder(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        // Overridable via env var so this same test can be pointed at any
        // reachable SSH host with `~/.local/bin/som-tmux` already built
        // there (Mac, deb, pi5, ...) without editing this file each time —
        // defaults to `localhost` (a local sshd, e.g. WSL2's own on this
        // dev machine) so the test suite doesn't depend on a specific
        // machine on a specific network being reachable.
        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-mac-close-{}", std::process::id());
        let profile_name = "test-mac-close";

        // Mirrors `wrap_remote_command_args`'s exact argv shape: the ssh
        // host/flags first, then the remote-side command appended after —
        // `ssh <host> ~/.local/bin/som-tmux <profile> <pane-id>
        // $SHELL --cursor-shape ...`, letting the remote login shell expand
        // `$SHELL` to whatever the remote user's default shell is.
        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "ssh".to_string(),
            vec![
                ssh_host.clone(),
                "~/.local/bin/som-tmux".to_string(),
                profile_name.to_string(),
                pane_id.clone(),
                "$SHELL".to_string(),
                "--cursor-shape".to_string(),
                "block".to_string(),
            ],
        )
        .await;

        // SSH handshake + remote process spawn is slower than the local
        // case's own RELAY spawn — same real-wall-clock-sleep reasoning as
        // the local test, just a longer budget for the extra network hop.
        std::thread::sleep(std::time::Duration::from_secs(4));

        // Mirrors `TerminalView::on_removed` exactly (see that function's
        // doc comment): a lone NUL byte, THEN a separate `input()` call
        // with an ordinary `\r`. That second call isn't cosmetic — while
        // diagnosing this test against the real Mac, a bare NUL byte (even
        // sent twice in a row) reliably never arrived at the RELAY at all
        // over this ssh-tunneled path, while following it with a second,
        // separate call containing ordinary bytes reliably made the first
        // one arrive too. Exact root cause not pinned down further than
        // "somewhere between this ConPTY and the far side's RELAY", but the
        // workaround is what `on_removed` now does in production.
        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![0u8]);
        });
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(300));
        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![b'\r']);
        });
        cx.run_until_parked();

        // Deliberately NOT `drop(terminal)` here (unlike the local test) —
        // over a real SSH tunnel, killing the LOCAL `ssh.exe` the instant
        // after queuing the NUL byte races the byte's actual delivery
        // across the network (SSH channel flow control/buffering can hold
        // a few bytes for longer than any fixed local sleep reliably
        // covers) against `ssh.exe`'s own death tearing down the TCP
        // connection first — confirmed by reproducing exactly that "pipe
        // closed" (no `RelayInput::Close` ever logged) failure mode
        // against the real Mac while diagnosing this test. Instead: let
        // `ssh.exe` keep running and wait for the REAL RELAY (on the far
        // side) to receive the byte, forward `RelayInput::Close`, and the
        // HOLDER to log its own death — same as it does the moment a
        // real user closes a real tab (`TerminalView::on_removed` doesn't
        // kill anything either; it only ever queues the NUL byte and lets
        // `Pane::_remove_item`'s later `Terminal::drop` happen on its own
        // natural schedule).
        std::thread::sleep(std::time::Duration::from_secs(2));

        // `paths::logs_dir()` (`crates/paths/src/paths.rs`) resolves
        // differently per platform: macOS gets `~/Library/Logs/Som/`;
        // plain Linux (confirmed against a real Debian box, NOT just
        // assumed) goes through `dirs::data_local_dir()`, i.e.
        // `$XDG_DATA_HOME/som/logs` — typically `~/.local/share/som/logs/`.
        // A particular WSL setup was ALSO observed using
        // `~/.config/som/logs/` at one point, so that's included too even
        // though it isn't `paths.rs`'s literal Linux branch — evidently
        // some environment's `$XDG_DATA_HOME` was actually set to
        // `~/.config`. Since this test doesn't know the remote host's exact
        // setup ahead of time, it just tries all three.
        let candidate_log_dirs = ["~/Library/Logs/Som", "~/.local/share/som/logs", "~/.config/som/logs"];
        let mut holder_log = String::new();
        let mut holder_log_dir = candidate_log_dirs[0];
        'outer: for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            for dir in candidate_log_dirs {
                let holder_log_path = format!("{dir}/som-tmux-{profile_name}-{pane_id}-holder.log");
                let output = std::process::Command::new("ssh").args([&ssh_host, "cat", &holder_log_path]).output();
                let content = output.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
                if content.contains("shell process exited, holder shutting down") {
                    holder_log = content;
                    holder_log_dir = dir;
                    break 'outer;
                }
            }
        }
        assert!(
            holder_log.contains("shell process exited, holder shutting down"),
            "the remote HOLDER should have died after the NUL byte was forwarded over ssh as \
             RelayInput::Close — got remote holder log: {holder_log:?}"
        );

        let holder_log_path = format!("{holder_log_dir}/som-tmux-{profile_name}-{pane_id}-holder.log");
        std::process::Command::new("ssh").args([&ssh_host, "rm", "-f", &holder_log_path]).output().ok();
        let relay_log_path = format!("{holder_log_dir}/som-tmux-{profile_name}-{pane_id}-relay.log");
        std::process::Command::new("ssh").args([&ssh_host, "rm", "-f", &relay_log_path]).output().ok();
    }

    /// Regression test for a real user report ("не работают F-клавиши в
    /// терминалах wsl/ssh" — F-keys and other special keys don't work in
    /// tmux-wrapped wsl/ssh terminals): sends the EXACT byte sequences
    /// `mappings::keys::to_esc_str` generates for a representative set of
    /// special keys (F1-F12, arrows, Home/End, Insert/Delete, Tab,
    /// Backspace, a Ctrl-letter combo) through a real tmux RELAY/HOLDER
    /// pair over a real SSH tunnel, and confirms each one arrives at the
    /// remote shell byte-for-byte unmangled.
    ///
    /// Uses `cat -v` (with the remote pty put into raw, non-canonical mode
    /// via `stty raw -echo` first, so `cat` sees each byte immediately
    /// rather than only after a line's worth arrives) as the "remote
    /// shell" — `-v` prints control/escape bytes in caret notation (e.g.
    /// `\x1bOP` becomes the literal text `^[OP`), which is easy to match
    /// against without needing to parse real ANSI back out of the
    /// terminal's own already-parsed screen grid.
    ///
    /// This test doesn't (yet) pin down WHY any particular key fails if it
    /// does — see `strip_cr_induced_lf`/the NUL-byte-as-Close-signal check
    /// in `som_tmux::relay::run`'s stdin loop, both of which
    /// inspect raw bytes and are the most likely place an escape sequence
    /// could get mangled — but it gives a fast, repeatable way to check
    /// the full RELAY/HOLDER pipeline against every key at once, on a real
    /// remote host, without a human touching a real keyboard.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_remote_tmux_forwards_special_keys_unmangled(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-keys-{}", std::process::id());
        let profile_name = "test-keys";

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "ssh".to_string(),
            vec![
                ssh_host.clone(),
                "~/.local/bin/som-tmux".to_string(),
                profile_name.to_string(),
                pane_id.clone(),
                "bash".to_string(),
                "-c".to_string(),
                "'stty raw -echo;exec stdbuf -o0 cat -v'".to_string(),
                "--cursor-shape".to_string(),
                "block".to_string(),
            ],
        )
        .await;

        std::thread::sleep(std::time::Duration::from_secs(4));

        // (key name for the failure message, the exact bytes `to_esc_str`
        // produces, what `cat -v` prints for those bytes). Covers every
        // "manual" special-key mapping family in `mappings::keys::
        // to_esc_str` — F-keys via both the `\x1bO_` (F1-F4) and `\x1b[N~`
        // (F5+) shapes, arrows, Home/End, Insert/Delete, Tab, Backspace,
        // and a Ctrl-letter combo — not an exhaustive list of every single
        // key it maps, which would make this test unwieldy without adding
        // real coverage beyond what these representative shapes already
        // exercise.
        let cases: &[(&str, &[u8], &str)] = &[
            ("F1", b"\x1bOP", "^[OP"),
            ("F4", b"\x1bOS", "^[OS"),
            ("F5", b"\x1b[15~", "^[[15~"),
            ("F12", b"\x1b[24~", "^[[24~"),
            ("Up (app cursor off)", b"\x1b[A", "^[[A"),
            ("Home (app cursor off)", b"\x1b[H", "^[[H"),
            ("Insert", b"\x1b[2~", "^[[2~"),
            ("Delete", b"\x1b[3~", "^[[3~"),
            ("Tab", b"\x09", "^I"),
            ("Backspace", b"\x7f", "^?"),
            ("Ctrl-A", b"\x01", "^A"),
        ];

        let mut failures = Vec::new();
        for (name, bytes, expected_marker) in cases {
            // NOT a `before.len() < seen.len()` comparison — `get_content()`
            // returns the WHOLE fixed-size screen grid (padded with blank
            // spaces/newlines to the pane's actual rows/cols), so printing
            // new characters typically overwrites existing blank cells IN
            // PLACE rather than growing the string at all. The only
            // reliable check is whether the expected marker text showed up
            // somewhere that wasn't there before.
            let before = terminal.update(cx, |term, _| term.get_content());
            terminal.update(cx, |terminal, _cx| {
                terminal.input(bytes.to_vec());
            });
            cx.run_until_parked();

            let mut seen = String::new();
            let mut arrived = false;
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                seen = terminal.update(cx, |term, _| term.get_content());
                if seen.contains(expected_marker) && !before.contains(expected_marker) {
                    arrived = true;
                    break;
                }
            }
            if !arrived {
                failures.push(format!(
                    "{name}: sent {bytes:?}, expected {expected_marker:?} to show up via `cat -v` — got screen: {seen:?}"
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "special keys failed to arrive unmangled through the ssh-remote tmux RELAY/HOLDER pipeline:\n{}",
            failures.join("\n")
        );
    }

    /// Regression test for a real user report ("F-клавиши не работают в
    /// терминалах wsl" — F2 specifically, confirmed against a real `htop`
    /// on WSL: pressing F2 printed the literal text `^[OQ` onto the screen
    /// instead of opening htop's Setup screen). Runs the EXACT same
    /// `wsl.exe`-fronted tmux path a real `tmux: true` "wsl" profile uses
    /// (`RemoteKind::Wsl` in `terminal_view::terminal_panel`), with `htop`
    /// itself as the wrapped program — as close to the real, reported
    /// failure as a headless test gets.
    ///
    /// `#[ignore]`d by default: needs WSL installed with
    /// `~/.local/bin/som-tmux` already built there, and `htop`
    /// installed inside that WSL distro. Run explicitly with:
    /// `cargo test -p terminal test_wsl_htop_f2_opens_setup_screen -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_wsl_htop_f2_opens_setup_screen(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let pane_id = format!("test-wsl-f2-{}", std::process::id());
        let profile_name = "test-wsl-f2";

        // Mirrors `wrap_remote_command_args`'s exact argv shape for a
        // `wsl`-classified profile: `wsl.exe --cd ~ -- ~/.local/bin/
        // som-tmux <profile> <pane-id> htop ...` — `RemoteKind::Wsl`
        // is matched on the program name being `wsl`/`wsl.exe` (see
        // `classify_remote`), same remote-command-appending path as ssh.
        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "wsl.exe".to_string(),
            vec![
                "--cd".to_string(),
                "~".to_string(),
                "--".to_string(),
                "~/.local/bin/som-tmux".to_string(),
                profile_name.to_string(),
                pane_id.clone(),
                "htop".to_string(),
                "--cursor-shape".to_string(),
                "block".to_string(),
            ],
        )
        .await;

        // Wait for htop's own real screen to render — its header shows
        // "Tasks:" unconditionally.
        let mut ready = false;
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let content = terminal.update(cx, |term, _| term.get_content());
            if content.contains("Tasks:") {
                ready = true;
                break;
            }
        }
        assert!(ready, "htop's own header never appeared after 15s through the wsl tmux pipeline — test setup problem");

        // F2, exactly as `mappings::keys::to_esc_str` generates it.
        terminal.update(cx, |terminal, _cx| {
            terminal.input(b"\x1bOQ".to_vec());
        });
        cx.run_until_parked();

        let mut seen = String::new();
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            seen = terminal.update(cx, |term, _| term.get_content());
            if seen.contains("Setup") || seen.contains("OQ") {
                break;
            }
        }

        assert!(
            !seen.contains("OQ"),
            "F2's escape sequence leaked onto the screen as literal text instead of being consumed \
             by htop as the F2 keypress through the real wsl tmux RELAY/HOLDER pipeline — this is \
             the exact reported bug. Got screen: {seen:?}"
        );
        assert!(
            seen.contains("Setup") || seen.contains("Display options") || seen.contains("Meters"),
            "F2 should have opened htop's Setup screen through the wsl tmux pipeline — got screen: {seen:?}"
        );
    }

    /// Same as `test_wsl_htop_f2_opens_setup_screen`, but over a real SSH
    /// connection instead of `wsl.exe` — verifies the new `tmux_backend`
    /// module (see `SOM_MUX_PLAN.md`'s "som-tmux v2" section) against the
    /// `RemoteKind::Ssh` path specifically, on a real reachable host with
    /// the new tmux-wrapping `som-tmux` binary already built there.
    ///
    /// `#[ignore]`d by default. Run explicitly with:
    /// `SOM_TEST_SSH_HOST=<host> cargo test -p terminal test_ssh_tmux_backend_htop_f2_opens_setup_screen -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_tmux_backend_htop_f2_opens_setup_screen(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-ssh-tmuxv2-f2-{}", std::process::id());
        let profile_name = "test-ssh-tmuxv2-f2";

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "ssh".to_string(),
            vec![
                ssh_host.clone(),
                "~/.local/bin/som-tmux".to_string(),
                profile_name.to_string(),
                pane_id.clone(),
                "htop".to_string(),
                "--cursor-shape".to_string(),
                "block".to_string(),
            ],
        )
        .await;

        let mut ready = false;
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let content = terminal.update(cx, |term, _| term.get_content());
            if content.contains("Tasks:") {
                ready = true;
                break;
            }
        }
        assert!(ready, "htop's own header never appeared after 15s through the ssh tmux_backend pipeline — test setup problem");

        // A LONE escape sequence in one isolated `input()` call has been
        // confirmed (separately, for the old architecture, but the
        // underlying transport is identical here) to sometimes vanish
        // between Som's ConPTY and a remote RELAY's stdin — a harmless
        // follow-up byte in a SEPARATE call reliably makes the first one
        // arrive too. Sending both defensively here since this test's job
        // is to check tmux_backend's OWN correctness, not re-litigate that
        // already-understood transport quirk.
        terminal.update(cx, |terminal, _cx| {
            terminal.input(b"\x1bOQ".to_vec());
        });
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(300));
        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![b' ']);
        });
        cx.run_until_parked();

        let mut seen = String::new();
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            seen = terminal.update(cx, |term, _| term.get_content());
            if seen.contains("Setup") || seen.contains("OQ") {
                break;
            }
        }

        assert!(
            !seen.contains("OQ"),
            "F2's escape sequence leaked onto the screen as literal text instead of being consumed \
             by htop through the new tmux_backend pipeline — got screen: {seen:?}"
        );
        assert!(
            seen.contains("Setup") || seen.contains("Display options") || seen.contains("Meters"),
            "F2 should have opened htop's Setup screen through the new tmux_backend pipeline — got screen: {seen:?}"
        );
    }

    /// Fast sanity check for `tmux_backend` — deliberately simpler than
    /// the F2/htop test, to isolate whether basic text I/O through the
    /// new pipeline is clean BEFORE chasing htop/F2-specific symptoms.
    /// Checks: (1) a plain typed command echoes back correctly with no
    /// corruption, (2) the NUL-byte tab-close signal actually reaches
    /// `tmux_backend::run` and kills the real tmux session (confirmed by
    /// polling the remote host's own `tmux has-session` over a second ssh
    /// call, not just trusting the local `Terminal` dropped cleanly).
    ///
    /// `#[ignore]`d by default. Run explicitly with:
    /// `SOM_TEST_SSH_HOST=<host> cargo test -p terminal test_ssh_tmux_backend_basic_io_and_close -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_tmux_backend_basic_io_and_close(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-tmuxv2-basic-{}", std::process::id());
        let profile_name = "test-tmuxv2-basic";

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "ssh".to_string(),
            vec![ssh_host.clone(), "~/.local/bin/som-tmux".to_string(), profile_name.to_string(), pane_id.clone(), "bash".to_string()],
        )
        .await;

        // Wait for a real bash prompt — NOT a `$`-suffix check (this host's
        // shell has a custom PS1, confirmed via `tmux capture-pane`: just
        // `~` with no `$`), so instead wait for ANY non-blank content to
        // show up at all, which is enough to know the shell is actually
        // ready to receive input.
        let mut ready = false;
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let content = terminal.update(cx, |term, _| term.get_content());
            if content.trim().len() > 1 {
                ready = true;
                break;
            }
        }
        assert!(ready, "bash prompt never appeared after 15s through the tmux_backend pipeline — test setup problem");

        // Plain echo command, byte by byte then Enter — same style as this
        // file's other "does typed text arrive uncorrupted" tests.
        let command = "echo SANITY_CHECK_MARKER_12345\r";
        for byte in command.bytes() {
            terminal.update(cx, |terminal, _cx| {
                terminal.input(vec![byte]);
            });
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let mut echoed = String::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            echoed = terminal.update(cx, |term, _| term.get_content());
            if echoed.matches("SANITY_CHECK_MARKER_12345").count() >= 2 {
                break; // once for the typed command itself, once for its own echoed output
            }
        }
        assert!(
            echoed.matches("SANITY_CHECK_MARKER_12345").count() >= 2,
            "plain typed text should echo back uncorrupted (typed once, echoed by `echo` once) \
             through the tmux_backend pipeline — got screen: {echoed:?}"
        );

        // Tab-close signal: a lone NUL byte, mirroring `TerminalView::
        // on_removed` exactly (including its follow-up-byte workaround —
        // see that function's doc comment for why the second call exists).
        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![0u8]);
        });
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(300));
        terminal.update(cx, |terminal, _cx| {
            terminal.input(vec![b'\r']);
        });
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(terminal);

        let mut session_gone = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let output = std::process::Command::new("ssh")
                .args([&ssh_host, "tmux", "-f", "/home/dnk/.config/som/som-tmux.conf", "has-session", "-t", &pane_id])
                .output();
            if output.map(|o| !o.status.success()).unwrap_or(false) {
                session_gone = true;
                break;
            }
        }
        assert!(
            session_gone,
            "tab-close (NUL byte) should have killed the real tmux session {pane_id:?} on the remote host, \
             but `tmux has-session` still reports it alive after 10s"
        );
    }

    /// Regression test for a real, repeatedly reproduced bug: pressing
    /// Enter on an SSH `tmux: true` profile's remote shell shows an extra
    /// blank line between every prompt after the first — the user's own
    /// report, confirmed against the real deployed `som.exe`, is "first
    /// Enter is clean, every one after duplicates a blank line". This test
    /// exists specifically because two prior isolated repros (a
    /// `som_tmux::Session`-only test spawning `ssh` directly, and a
    /// same-shape test against a LOCAL `bash` with no ssh at all) BOTH
    /// stayed clean even after the `alacritty_terminal::EventLoop::
    /// pty_read` per-syscall dedup fix, while the bug still reproduced in
    /// the real Som UI — the working theory is that the race depends on
    /// GPUI's own render/executor contending for the SAME `Term` lock the
    /// `pty_read` thread needs, which a headless `som_tmux::Session` test
    /// (no GPUI at all) can never recreate. This test goes through the
    /// REAL `crates/terminal::Terminal`/`TerminalBuilder` (same code path
    /// `terminal_view` uses for an actual tab, including its own GPUI
    /// executor/subscription plumbing), which is the closest a `#[gpui::
    /// test]` can get to the real UI without an actual window/keystrokes.
    ///
    /// `#[cfg(target_os = "windows")]` deliberately — like the fix itself,
    /// this is specifically about the Windows-side ConPTY reader, and
    /// would prove nothing run against a Unix `ssh` client.
    ///
    /// `#[ignore]`d by default. Run explicitly with:
    /// `SOM_TEST_SSH_HOST=<host> cargo test -p terminal test_ssh_tmux_backend_enter_does_not_double_crlf -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_tmux_backend_enter_does_not_double_crlf(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-tmuxv2-enter-{}", std::process::id());
        let profile_name = "test-tmuxv2-enter";

        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "ssh".to_string(),
            vec![ssh_host.clone(), "~/.local/bin/som-tmux".to_string(), profile_name.to_string(), pane_id.clone(), "bash".to_string()],
        )
        .await;

        let mut ready = false;
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let content = terminal.update(cx, |term, _| term.get_content());
            if content.trim().len() > 1 {
                ready = true;
                break;
            }
        }
        assert!(ready, "bash prompt never appeared after 15s through the tmux_backend pipeline — test setup problem");

        // Press Enter 10 times, same as the real reproduction — each one
        // its own `terminal.input` call, with a real sleep in between so
        // GPUI's own render/executor has room to actually run (and
        // contend for the `Term` lock) between keystrokes, not just
        // `run_until_parked` immediately after each one.
        for _ in 0..10 {
            terminal.update(cx, |terminal, _cx| {
                terminal.input(vec![b'\r']);
            });
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        let content = terminal.update(cx, |term, _| term.get_content());
        let all_lines: Vec<&str> = content.lines().map(|l| l.trim_end()).collect();
        // Only the prompt-repeat region actually exercises the bug —
        // everything before the FIRST prompt is the real MOTD text (kernel/
        // distro banner, `/etc/motd`), which legitimately contains blank
        // lines between its own paragraphs (confirmed by direct comparison
        // against a real `ssh -tt` session's own MOTD output). Counting
        // those would produce false positives unrelated to Enter-triggered
        // duplication — skip everything up to and including the first
        // line that's just the prompt's own trailing glyph.
        let first_prompt_index = all_lines
            .iter()
            .position(|line| line.trim() == "\u{e77d} ~ \u{f101}")
            .unwrap_or(0);
        let lines = &all_lines[first_prompt_index..];
        let max_consecutive_blank = lines
            .iter()
            .fold((0usize, 0usize), |(max, cur), line| {
                let cur = if line.is_empty() { cur + 1 } else { 0 };
                (max.max(cur), cur)
            })
            .0;
        assert!(
            max_consecutive_blank <= 1,
            "found {max_consecutive_blank} consecutive blank lines after 10 Enter presses through the real \
             crates/terminal::Terminal SSH tmux_backend pipeline — this is exactly the reported doubled-CRLF-\
             between-prompts bug. Screen:\n{content:?}"
        );
    }

    /// Regression test for a real user report that the fix above (rolling-
    /// window dedup, `test_ssh_tmux_backend_enter_does_not_double_crlf`)
    /// did NOT fully resolve: pressing Enter is clean on a `tmux: true`
    /// SSH profile's FIRST connection to a freshly-spawned HOLDER, but
    /// after Som is closed and reopened — reattaching to the SAME still-
    /// running remote HOLDER, which `wrap_remote_command_args` does simply
    /// by reusing the same pane ID — the duplicated-blank-line bug comes
    /// back. This drops one `Terminal` (closing its RELAY connection, the
    /// same way closing Som does) and spawns a SECOND one with the SAME
    /// pane_id, which `server.rs::handle_relay` treats as a reattach to
    /// the still-alive HOLDER (see its `force_resize`/`Snapshot` handling)
    /// rather than a fresh spawn — then repeats the exact same Enter-
    /// press check on the second connection.
    ///
    /// `#[cfg(target_os = "windows")]` deliberately — same reasoning as
    /// the fresh-spawn version above.
    ///
    /// `#[ignore]`d by default. Run explicitly with:
    /// `SOM_TEST_SSH_HOST=<host> cargo test -p terminal test_ssh_tmux_backend_enter_after_reattach -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn test_ssh_tmux_backend_enter_does_not_double_crlf_after_reattach(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let ssh_host = std::env::var("SOM_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".to_string());
        let pane_id = format!("test-tmuxv2-reattach-{}", std::process::id());
        let profile_name = "test-tmuxv2-reattach";
        // Exact `wrap_remote_command_args` shape (terminal_panel.rs), not
        // just "close enough" — `$SHELL` (not a literal `bash`) and the
        // trailing `-- -l` login-shell flag matter here specifically: `-l`
        // is what makes the remote shell run as a LOGIN shell, which is
        // what triggers `Session::spawn`'s `motd_wrapped_command` MOTD
        // emulation in the first place. A test spawning a plain `bash`
        // with no `-l` never exercises that code path at all, and the
        // real user report showed the bug's SECOND connection still
        // printing a correct MOTD (confirmed via screenshot) — so if
        // there's an interaction between the MOTD wrapper's own output
        // and reattach specifically, only the exact real arg shape can
        // catch it.
        let args = || {
            vec![
                ssh_host.clone(),
                "~/.local/bin/som-tmux".to_string(),
                profile_name.to_string(),
                pane_id.clone(),
                "$SHELL".to_string(),
                "--cursor-shape".to_string(),
                "underline".to_string(),
                "--scrollback".to_string(),
                "10000".to_string(),
                "--".to_string(),
                "-l".to_string(),
            ]
        };

        async fn wait_for_prompt(terminal: &Entity<Terminal>, cx: &mut TestAppContext) {
            let mut ready = false;
            for _ in 0..150 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let content = terminal.update(cx, |term, _| term.get_content());
                if content.trim().len() > 1 {
                    ready = true;
                    break;
                }
            }
            assert!(ready, "prompt never appeared — test setup problem, not the bug under test");
        }

        fn max_consecutive_blank_after_first_prompt(content: &str) -> usize {
            let all_lines: Vec<&str> = content.lines().map(|l| l.trim_end()).collect();
            let first_prompt_index =
                all_lines.iter().position(|line| line.trim() == "\u{e77d} ~ \u{f101}").unwrap_or(0);
            all_lines[first_prompt_index..]
                .iter()
                .fold((0usize, 0usize), |(max, cur), line| {
                    let cur = if line.is_empty() { cur + 1 } else { 0 };
                    (max.max(cur), cur)
                })
                .0
        }

        // First connection: fresh HOLDER spawn.
        let (terminal, _completion_rx) = build_test_terminal_with_arguments(cx, "ssh".to_string(), args()).await;
        wait_for_prompt(&terminal, cx).await;
        for _ in 0..5 {
            terminal.update(cx, |terminal, _cx| terminal.input(vec![b'\r']));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        let first_content = terminal.update(cx, |term, _| term.get_content());
        let first_max_blank = max_consecutive_blank_after_first_prompt(&first_content);
        assert!(
            first_max_blank <= 1,
            "found {first_max_blank} consecutive blank lines on the FIRST connection (fresh HOLDER spawn) — \
             test setup problem, not the reattach-specific bug this test targets. Screen:\n{first_content:?}"
        );

        // Close this connection exactly the way closing a Som tab does —
        // the NUL-byte tab-close signal, mirroring `TerminalView::
        // on_removed`'s own follow-up-byte workaround — then drop it,
        // WITHOUT ever sending `RelayInput::Close`, so the remote HOLDER
        // stays alive (a real Som restart, as opposed to an explicit tab
        // close, never signals the HOLDER to exit either — that's the
        // whole point of a HOLDER surviving across Som restarts).
        drop(terminal);
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Real report was after Som had been closed/reopened SEVERAL times
        // over the course of a session (confirmed via the HOLDER's own
        // log: 3 separate `relay connected` events across ~14 minutes) —
        // repeat the reattach cycle a few times rather than just once, in
        // case the bug only shows up on the Nth reattach, not the 2nd.
        for attempt in 2..=4 {
            let (terminal_n, _completion_rx_n) = build_test_terminal_with_arguments(cx, "ssh".to_string(), args()).await;
            wait_for_prompt(&terminal_n, cx).await;
            for _ in 0..10 {
                terminal_n.update(cx, |terminal, _cx| terminal.input(vec![b'\r']));
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            cx.run_until_parked();
            let content = terminal_n.update(cx, |term, _| term.get_content());
            let max_blank = max_consecutive_blank_after_first_prompt(&content);
            assert!(
                max_blank <= 1,
                "found {max_blank} consecutive blank lines after 10 Enter presses on reattach #{attempt} (same \
                 pane_id as an already-live HOLDER) — this is the reattach-specific variant of the doubled-CRLF-\
                 between-prompts bug: clean on first connect, broken after Som restarts and reattaches. \
                 Screen:\n{content:?}"
            );
            drop(terminal_n);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    mod perf {
        use super::super::*;
        use gpui::{
            Entity, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualContext,
            VisualTestContext, point,
        };
        use util::default;
        use util_macros::perf;

        async fn init_scroll_perf_test(
            cx: &mut TestAppContext,
        ) -> (Entity<Terminal>, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings_store = settings::SettingsStore::test(cx);
                cx.set_global(settings_store);
            });

            cx.executor().allow_parking();

            let window = cx.add_empty_window();
            let builder = window
                .update(|window, cx| {
                    let settings = TerminalSettings::get_global(cx);
                    let test_path_hyperlink_timeout_ms = 100;
                    TerminalBuilder::new(
                        None,
                        None,
                        task::Shell::System,
                        HashMap::default(),
                        CursorShape::default(),
                        AlternateScroll::On,
                        None,
                        settings.path_hyperlink_regexes.clone(),
                        test_path_hyperlink_timeout_ms,
                        false,
                        window.window_handle().window_id().as_u64(),
                        None,
                        cx,
                        vec![],
                        PathStyle::local(),
                    )
                })
                .await
                .unwrap();
            let terminal = window.new(|cx| builder.subscribe(cx));

            terminal.update(window, |term, cx| {
                term.write_output("long line ".repeat(1000).as_bytes(), cx);
            });

            (terminal, window)
        }

        #[perf]
        #[gpui::test]
        async fn scroll_long_line_benchmark(cx: &mut TestAppContext) {
            let (terminal, window) = init_scroll_perf_test(cx).await;
            let wobble = point(FIND_HYPERLINK_THROTTLE_PX, px(0.0));
            let mut scroll_by = |lines: i32| {
                window.update_window_entity(&terminal, |terminal, window, cx| {
                    let bounds = terminal.last_content.terminal_bounds.bounds;
                    let center = bounds.origin + bounds.center();
                    let position = center + wobble * lines as f32;

                    terminal.mouse_move(
                        &MouseMoveEvent {
                            position,
                            ..default()
                        },
                        cx,
                    );

                    terminal.scroll_wheel(
                        &ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Lines(Point::new(0.0, lines as f32)),
                            ..default()
                        },
                        1.0,
                    );

                    assert!(
                        terminal
                            .events
                            .iter()
                            .any(|event| matches!(event, InternalEvent::Scroll(_))),
                        "Should have Scroll event when scrolling within terminal bounds"
                    );
                    terminal.sync(window, cx);
                });
            };

            for _ in 0..20000 {
                scroll_by(1);
                scroll_by(-1);
            }
        }

        #[test]
        fn test_num_lines_float_precision() {
            let line_heights = [
                20.1f32, 16.7, 18.3, 22.9, 14.1, 15.6, 17.8, 19.4, 21.3, 23.7,
            ];
            for &line_height in &line_heights {
                for n in 1..=100 {
                    let height = n as f32 * line_height;
                    let bounds = TerminalBounds::new(
                        px(line_height),
                        px(8.0),
                        Bounds {
                            origin: Point::default(),
                            size: Size {
                                width: px(800.0),
                                height: px(height),
                            },
                        },
                    );
                    assert_eq!(
                        bounds.num_lines(),
                        n,
                        "num_lines() should be {n} for height={height}, line_height={line_height}"
                    );
                }
            }
        }

        #[test]
        fn test_num_columns_float_precision() {
            let cell_widths = [8.1f32, 7.3, 9.7, 6.9, 10.1];
            for &cell_width in &cell_widths {
                for n in 1..=200 {
                    let width = n as f32 * cell_width;
                    let bounds = TerminalBounds::new(
                        px(20.0),
                        px(cell_width),
                        Bounds {
                            origin: Point::default(),
                            size: Size {
                                width: px(width),
                                height: px(400.0),
                            },
                        },
                    );
                    assert_eq!(
                        bounds.num_columns(),
                        n,
                        "num_columns() should be {n} for width={width}, cell_width={cell_width}"
                    );
                }
            }
        }
    }
}
