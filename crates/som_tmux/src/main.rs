// `bounds`/`session`/`server`/`relay` are the v3 architecture (see
// `SOM_MUX_PLAN.md`'s "som-tmux v3" section) — a headless
// `alacritty_terminal::Term` on the HOLDER side sends its full serialized
// state (`Term::snapshot()`) to a (re)connecting RELAY, which restores it
// onto a throwaway local `Term` and does ONE full ANSI repaint via
// `redraw.rs` (which now only ever runs that one-shot, not continuously —
// see that module's own doc comment) before switching to forwarding raw
// PTY bytes unmodified. Cross-platform: `alacritty_terminal::tty::new()`
// already works on both Windows and Unix, so this single implementation
// covers the Windows-local profile AND every Unix (WSL/SSH) profile —
// unlike v2 (`tmux_backend`, below), which needed Windows kept on a
// permanently separate path since real tmux has no Windows PTY port.
mod bounds;
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
}

fn parse_args() -> Args {
    let mut holder = false;
    let mut profile = None;
    let mut pane_id = None;
    let mut program = None;
    let mut cwd = None;
    let mut cursor_shape = None;
    let mut scrollback = None;
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
            "--cursor-shape" => cursor_shape = iter.next(),
            "--scrollback" => scrollback = iter.next().and_then(|s| s.parse().ok()),
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
            "usage:\n  som-tmux [--cursor-shape <shape>] [--scrollback <n>] <profile> <pane-id> <program> [args...]   (relay mode, what Som invokes)\n  som-tmux --holder --profile <p> --pane-id <id> --program <prog> [--cwd <dir>] [--cursor-shape <shape>] [--scrollback <n>] [-- args...]"
        );
        std::process::exit(2);
    };

    let Some(profile) = profile.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(pane_id) = pane_id.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(program) = program.filter(|s| !s.trim().is_empty()) else { usage() };

    Args { holder, profile, pane_id, program, args: extra_args, cwd, cursor_shape, scrollback }
}

fn main() {
    let args = parse_args();
    init_logging(&args.profile, &args.pane_id, args.holder);

    // v3 architecture (see `SOM_MUX_PLAN.md`) is cross-platform — same
    // HOLDER/RELAY split, same dispatch, on both Windows and Unix now.
    let result = if args.holder {
        server::run(&args.profile, &args.pane_id, args.program, args.args, args.cwd, args.cursor_shape, args.scrollback)
    } else {
        relay::run(&args.profile, &args.pane_id, args.program, args.args, args.cwd, args.cursor_shape, args.scrollback)
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
