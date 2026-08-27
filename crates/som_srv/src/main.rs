// `bounds`/`session`/`server`/`relay` implement som-srv's HOLDER/RELAY
// architecture — a headless `alacritty_terminal::Term` on the HOLDER side
// sends its full serialized state (`Term::snapshot()`) to a (re)connecting
// RELAY, which restores it onto a throwaway local `Term` and does ONE full
// ANSI repaint via `redraw.rs` (which only ever runs that one-shot, not
// continuously — see that module's own doc comment) before switching to
// forwarding raw PTY bytes unmodified. Cross-platform:
// `alacritty_terminal::tty::new()` already works on both Windows and Unix,
// so this single implementation covers the Windows-local profile AND every
// Unix (WSL/SSH) profile.
mod bounds;
mod redraw;
mod relay;
mod server;
mod session;
mod srv_cache;
mod term_size;

/// Parsed command line — two shapes depending on whether this process is
/// starting as a RELAY (Som's direct PTY child, the normal case) or the
/// shared DAEMON (spawned by a RELAY itself, detached, see
/// `relay::spawn_detached_daemon`; never invoked directly by Som). The
/// daemon is host-scoped and generic — unlike the old per-pane HOLDER, it
/// takes no profile/pane-id/program/etc at startup at all; a RELAY tells
/// it which session it wants via `RelayInput::Register` once connected.
struct Args {
    daemon: bool,
    profile: String,
    pane_id: String,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    /// The connecting client machine's IP, as `relay::spawn_detached_daemon`
    /// read it off ITS OWN `$SSH_CLIENT` (set by sshd on the RELAY, which —
    /// for an SSH `tmux: true` profile — really does run on this same
    /// remote host). Travels in this RELAY's own `RelayInput::Register`
    /// message rather than daemon startup argv now — the daemon is shared
    /// across every session on the machine, so it has no single "this
    /// process's client" to record on its own command line the way the
    /// old per-pane HOLDER did. `None` for a local/WSL RELAY (no sshd
    /// involved) or one left over from before this field existed.
    client_id: Option<String>,
    /// Mirrors Som's own `TerminalSettings::cursor_shape` — passed through
    /// explicitly rather than the daemon guessing/defaulting, since it's
    /// the daemon's `Session`, not Som's `TerminalElement`, that actually
    /// owns the `alacritty_terminal::Term` whose `Config` this configures
    /// now (see `crate::session::Session::spawn`). Parsed here as a plain
    /// string (not `terminal_settings::CursorShape` — this crate
    /// deliberately doesn't depend on that crate) and turned into the real
    /// `CursorShape` enum in `session.rs`.
    cursor_shape: Option<String>,
    /// Mirrors Som's own `TerminalSettings::max_scroll_history_lines` — same
    /// reasoning as `cursor_shape` above.
    scrollback: Option<usize>,
    /// Som's real font cell size in pixels (`width;height`) at the moment
    /// it spawned this RELAY — see `relay::run`'s own doc comment for why
    /// this is a one-time command-line value (Som's `TerminalBounds::
    /// cell_width()`/`line_height()`, the same figures GPUI actually
    /// renders text with) rather than something updated live for the rest
    /// of the session: a live channel (an escape-sequence marker injected
    /// into the same PTY byte stream real keystrokes/VT responses flow
    /// through) was tried and confirmed to introduce multi-second delays
    /// in unrelated Kitty graphics image data sharing that same pipe —
    /// this flag exists specifically to avoid that shared-channel
    /// contention, at the cost of never updating if the font/DPI changes
    /// mid-session (rare enough to accept).
    cell_pixel_size: Option<(u16, u16)>,
}

fn parse_args() -> Args {
    let mut daemon = false;
    let mut profile = None;
    let mut pane_id = None;
    let mut program = None;
    let mut cwd = None;
    let mut client_id = None;
    let mut cursor_shape = None;
    let mut scrollback = None;
    let mut cell_pixel_size = None;
    let mut extra_args = Vec::new();

    let mut iter = std::env::args().skip(1).peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Deliberately checked before any other parsing/validation —
            // this is the ONLY thing a deploy check (see `project_som_tmux`
            // memory, "Обновление 19"/23 — remote binary version/platform
            // comparison before deciding whether to scp a newer build over)
            // needs from a remote binary it hasn't run as a real daemon/
            // RELAY yet. Prints ONLY the raw handshake JSON (no log
            // preamble) so the caller (a plain `ssh host ... --version`)
            // can parse stdout directly without scraping a log file.
            "--version" => {
                println!("{}", serde_json::to_string(&som_srv::protocol::HandshakeInfo::current()).unwrap());
                std::process::exit(0);
            }
            // Self-contained admin subcommands — connect to the daemon
            // ALREADY running on this same machine (see `som_srv::admin`),
            // print a JSON result, exit. These are what `terminal_panel.
            // rs`'s `kill_orphaned_holders` replacement runs over SSH
            // (`ssh host som-srv --list-sessions ...`), mirroring the exact
            // `--version` probe pattern above rather than opening a raw
            // pipe connection to the remote daemon from Som's own side.
            "--list-sessions" => {
                let client_id = iter.next().filter(|s| !s.is_empty());
                match som_srv::admin::list_sessions(client_id) {
                    Ok(sessions) => {
                        println!("{}", serde_json::to_string(&sessions).unwrap());
                        std::process::exit(0);
                    }
                    Err(err) => {
                        eprintln!("failed to list sessions: {err:#}");
                        std::process::exit(1);
                    }
                }
            }
            "--kill-session" => {
                let client_id = iter.next().filter(|s| !s.is_empty());
                let Some(pane_id) = iter.next() else {
                    eprintln!("usage: som-srv --kill-session <client-id-or-empty-string> <pane-id>");
                    std::process::exit(2);
                };
                match som_srv::admin::kill_session(client_id, pane_id) {
                    Ok(()) => std::process::exit(0),
                    Err(err) => {
                        eprintln!("failed to kill session: {err:#}");
                        std::process::exit(1);
                    }
                }
            }
            "--daemon" => daemon = true,
            "--profile" => profile = iter.next(),
            "--pane-id" => pane_id = iter.next(),
            "--program" => program = iter.next(),
            "--cwd" => cwd = iter.next(),
            "--client-id" => client_id = iter.next(),
            "--cursor-shape" => cursor_shape = iter.next(),
            "--scrollback" => scrollback = iter.next().and_then(|s| s.parse().ok()),
            "--cell-pixel-size" => {
                cell_pixel_size = iter.next().and_then(|s| {
                    let (w, h) = s.split_once(';')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                });
            }
            "--" => {
                extra_args.extend(iter.by_ref());
            }
            other => {
                // The RELAY's own invocation (what Som actually runs as
                // the "shell") is `som-srv [--client-id ...]
                // [--cursor-shape ...] [--scrollback ...] <profile>
                // <pane-id> <program> [args...]` — positional past the
                // flags above, since Som substitutes this in as a plain
                // shell command (see `project_som_tmux` memory,
                // "Обновление 17") rather than constructing named flags
                // itself for the parts that aren't settings. Only
                // `--daemon` mode (spawned by
                // `relay::spawn_detached_daemon`, never by Som directly)
                // ignores these positionals entirely — it takes no
                // profile/pane_id/program of its own.
                if profile.is_none() {
                    profile = Some(other.to_string());
                } else if pane_id.is_none() {
                    pane_id = Some(other.to_string());
                } else if program.is_none() {
                    program = Some(other.to_string());
                } else {
                    extra_args.push(other.to_string());
                }
            }
        }
    }

    let usage = || -> ! {
        eprintln!(
            "usage:\n  som-srv [--client-id <id>] [--cursor-shape <shape>] [--scrollback <n>] <profile> <pane-id> <program> [args...]   (relay mode, what Som invokes)\n  som-srv --daemon   (shared daemon mode, spawned automatically — never invoked directly)"
        );
        std::process::exit(2);
    };

    if daemon {
        return Args {
            daemon,
            profile: String::new(),
            pane_id: String::new(),
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            client_id: None,
            cursor_shape: None,
            scrollback: None,
            cell_pixel_size: None,
        };
    }

    let Some(profile) = profile.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(pane_id) = pane_id.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(program) = program.filter(|s| !s.trim().is_empty()) else { usage() };

    Args { daemon, profile, pane_id, program, args: extra_args, cwd, client_id, cursor_shape, scrollback, cell_pixel_size }
}

fn main() {
    let args = parse_args();
    init_logging(&args.profile, &args.pane_id, args.daemon);

    // Cross-platform — same daemon/RELAY split, same dispatch, on both
    // Windows and Unix.
    let result = if args.daemon {
        server::run()
    } else {
        // A RELAY's own invocation is positional (`<profile> <pane-id>
        // <program>` — see `Args`'s doc comment), so `--client-id` is
        // never actually passed to it by `wrap_remote_command_args` on
        // the Som side; `args.client_id` is only ever populated for a
        // future/manual named-flag invocation. The REAL source, for an
        // SSH `tmux: true` profile, is this RELAY process's OWN
        // `$SSH_CLIENT` (set by sshd, since the RELAY itself is what
        // `ssh host ~/.local/bin/som-srv ...` spawned) — read here,
        // mirroring what the old per-pane HOLDER spawn used to do via
        // `ssh_client_id()` before this process ever becomes a RELAY.
        // `None` for a local/WSL RELAY (no sshd involved, so no
        // `$SSH_CLIENT` to read) — `ssh_client_id()` itself already
        // returns `None` in that case.
        let client_id = args.client_id.or_else(som_srv::protocol::ssh_client_id);
        relay::run(
            &args.profile,
            &args.pane_id,
            client_id,
            args.program,
            args.args,
            args.cwd,
            args.cursor_shape,
            args.scrollback,
            args.cell_pixel_size,
        )
    };

    if let Err(err) = result {
        log::error!("som-srv exiting: {err:#}");
        std::process::exit(1);
    }
}

/// The daemon is spawned detached and outlives Som's own window (that's
/// the entire point — see `project_som_tmux` memory), so there's no
/// console to print to by the time anything goes wrong; logging to a file
/// is the only way to debug it after the fact. The RELAY is a normal PTY
/// child of Som with a short lifetime, but gets the same file-based
/// logging for consistency (and because its own stdout is reserved
/// entirely for the ANSI bytes Som's terminal parser reads — writing logs
/// there would corrupt the stream). The daemon has no single profile/pane
/// of its own (it's shared across every session on the machine), so it
/// gets one fixed log file name instead of a per-pane one.
fn init_logging(profile_name: &str, pane_id: &str, is_daemon: bool) {
    zlog::init();
    let log_path: &'static std::path::PathBuf = Box::leak(Box::new(if is_daemon {
        paths::logs_dir().join("som-srv-daemon.log")
    } else {
        paths::logs_dir().join(format!("som-srv-{profile_name}-{pane_id}-relay.log"))
    }));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    zlog::init_output_file(log_path, None).ok();
}
