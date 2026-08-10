// `bounds`/`session`/`server`/`relay` implement som-tmux's HOLDER/RELAY
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
mod kitty_replay;
mod redraw;
mod relay;
mod server;
mod session;
mod term_size;

/// Parsed command line — two shapes depending on whether this process is
/// starting as a RELAY (Som's direct PTY child, the normal case) or a
/// HOLDER (only ever spawned by a RELAY itself, detached, see
/// `relay::spawn_detached_holder`; never invoked directly by Som).
struct Args {
    holder: bool,
    profile: String,
    pane_id: String,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    /// The connecting client machine's IP, as `relay::spawn_detached_holder`
    /// read it off ITS OWN `$SSH_CLIENT` (set by sshd on the RELAY, which —
    /// for an SSH `tmux: true` profile — really does run on this same
    /// remote host) before spawning this `--holder` process. Not read or
    /// used anywhere in `--holder` mode itself: only parsed here so it
    /// stays visible in this HOLDER's OWN `ps` command line, which is the
    /// entire point — `kill_orphaned_holders` (`terminal_panel.rs`, on the
    /// Som side) greps a live `ps` listing for exactly this flag to decide
    /// which HOLDERs belong to which client machine before ever touching
    /// one. See `som_tmux::protocol::ssh_client_ip`'s doc comment for the
    /// full reasoning. `None` for a local/WSL HOLDER (no sshd involved) or
    /// one left over from before this field existed.
    #[allow(dead_code)]
    client_id: Option<String>,
    /// Mirrors Som's own `TerminalSettings::cursor_shape` — passed through
    /// explicitly rather than the HOLDER guessing/defaulting, since it's
    /// the HOLDER's `Session`, not Som's `TerminalElement`, that actually
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
    let mut holder = false;
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
            // needs from a remote binary it hasn't run as a real HOLDER/
            // RELAY yet. Prints ONLY the raw handshake JSON (no log
            // preamble) so the caller (a plain `ssh host ... --version`)
            // can parse stdout directly without scraping a log file.
            "--version" => {
                println!("{}", serde_json::to_string(&som_tmux::protocol::HandshakeInfo::current()).unwrap());
                std::process::exit(0);
            }
            "--holder" => holder = true,
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
                // the "shell") is `som-tmux [--cursor-shape ...]
                // [--scrollback ...] <profile> <pane-id> <program>
                // [args...]` — positional past the flags above, since
                // Som substitutes this in as a plain shell command (see
                // `project_som_tmux` memory, "Обновление 17") rather than
                // constructing named flags itself for the parts that
                // aren't settings. Only `--holder` mode (spawned by
                // `relay::spawn_detached_holder`, never by Som directly)
                // uses the fully named-flag form above for everything.
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
            "usage:\n  som-tmux [--client-id <id>] [--cursor-shape <shape>] [--scrollback <n>] <profile> <pane-id> <program> [args...]   (relay mode, what Som invokes)\n  som-tmux --holder --profile <p> --pane-id <id> --program <prog> [--cwd <dir>] [--client-id <id>] [--cursor-shape <shape>] [--scrollback <n>] [-- args...]"
        );
        std::process::exit(2);
    };

    let Some(profile) = profile.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(pane_id) = pane_id.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(program) = program.filter(|s| !s.trim().is_empty()) else { usage() };

    Args { holder, profile, pane_id, program, args: extra_args, cwd, client_id, cursor_shape, scrollback, cell_pixel_size }
}

fn main() {
    let args = parse_args();
    init_logging(&args.profile, &args.pane_id, args.holder);

    // Cross-platform — same HOLDER/RELAY split, same dispatch, on both
    // Windows and Unix.
    let result = if args.holder {
        server::run(&args.profile, &args.pane_id, args.program, args.args, args.cwd, args.cursor_shape, args.scrollback)
    } else {
        relay::run(
            &args.profile,
            &args.pane_id,
            args.program,
            args.args,
            args.cwd,
            args.cursor_shape,
            args.scrollback,
            args.cell_pixel_size,
        )
    };

    if let Err(err) = result {
        log::error!("som-tmux exiting: {err:#}");
        std::process::exit(1);
    }
}

/// The HOLDER is spawned detached and outlives Som's own window (that's the
/// entire point — see `project_som_tmux` memory), so there's no console to
/// print to by the time anything goes wrong; logging to a per-pane file is
/// the only way to debug it after the fact. The RELAY is a normal PTY
/// child of Som with a short lifetime, but gets the same file-based
/// logging for consistency (and because its own stdout is reserved
/// entirely for the ANSI bytes Som's terminal parser reads — writing logs
/// there would corrupt the stream).
fn init_logging(profile_name: &str, pane_id: &str, is_holder: bool) {
    zlog::init();
    let role = if is_holder { "holder" } else { "relay" };
    let log_path: &'static std::path::PathBuf = Box::leak(Box::new(
        paths::logs_dir().join(format!("som-tmux-{profile_name}-{pane_id}-{role}.log")),
    ));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    zlog::init_output_file(log_path, None).ok();
}
