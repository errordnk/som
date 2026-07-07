mod bounds;
mod redraw;
mod server;
mod session;

fn main() {
    let profile_name = match std::env::args().nth(1) {
        Some(name) if !name.trim().is_empty() => name,
        _ => {
            eprintln!("usage: som-tmux-server <profile-name>");
            std::process::exit(2);
        }
    };

    init_logging(&profile_name);

    if let Err(err) = server::run(&profile_name) {
        log::error!("som-tmux-server exiting: {err:#}");
        std::process::exit(1);
    }
}

/// This process is spawned detached and outlives Som's own window (that's
/// the entire point — see `project_som_tmux` memory), so there's no console
/// to print to by the time anything goes wrong. Logging to a per-profile
/// file is the only way to debug it after the fact.
fn init_logging(profile_name: &str) {
    zlog::init();
    let log_path: &'static std::path::PathBuf = Box::leak(Box::new(
        paths::logs_dir().join(format!("som-tmux-server-{profile_name}.log")),
    ));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    zlog::init_output_file(log_path, None).ok();
}
