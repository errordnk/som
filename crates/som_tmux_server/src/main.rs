mod bounds;
mod redraw;
mod relay;
mod server;
mod session;

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
}

fn parse_args() -> Args {
    let mut holder = false;
    let mut profile = None;
    let mut pane_id = None;
    let mut program = None;
    let mut cwd = None;
    let mut extra_args = Vec::new();

    let mut iter = std::env::args().skip(1).peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--holder" => holder = true,
            "--profile" => profile = iter.next(),
            "--pane-id" => pane_id = iter.next(),
            "--program" => program = iter.next(),
            "--cwd" => cwd = iter.next(),
            "--" => {
                extra_args.extend(iter.by_ref());
            }
            other => {
                // The RELAY's own invocation (what Som actually runs as
                // the "shell") is `som-tmux-server <profile> <pane-id>
                // <program> [args...]` — positional, not flagged, since
                // Som substitutes this in as a plain shell command (see
                // `project_som_tmux` memory, "Обновление 17") rather than
                // constructing named flags itself. Only `--holder` mode
                // (spawned by `relay::spawn_detached_holder`, never by
                // Som directly) uses the named-flag form above.
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
            "usage:\n  som-tmux-server <profile> <pane-id> <program> [args...]   (relay mode, what Som invokes)\n  som-tmux-server --holder --profile <p> --pane-id <id> --program <prog> [--cwd <dir>] [-- args...]"
        );
        std::process::exit(2);
    };

    let Some(profile) = profile.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(pane_id) = pane_id.filter(|s| !s.trim().is_empty()) else { usage() };
    let Some(program) = program.filter(|s| !s.trim().is_empty()) else { usage() };

    Args { holder, profile, pane_id, program, args: extra_args, cwd }
}

fn main() {
    let args = parse_args();
    init_logging(&args.profile, &args.pane_id, args.holder);

    let result = if args.holder {
        server::run(&args.profile, &args.pane_id, args.program, args.args, args.cwd)
    } else {
        relay::run(&args.profile, &args.pane_id, args.program, args.args, args.cwd)
    };

    if let Err(err) = result {
        log::error!("som-tmux-server exiting: {err:#}");
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
        paths::logs_dir().join(format!("som-tmux-server-{profile_name}-{pane_id}-{role}.log")),
    ));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    zlog::init_output_file(log_path, None).ok();
}
