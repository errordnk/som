//! Manual smoke test for `som_tmux_client::connect_or_spawn` — not part of
//! the app, just a standalone way to exercise connect-or-spawn with a real
//! `current_exe()` sitting next to the actual `som-tmux-server.exe` binary
//! (unlike a unit test running from `target/debug/deps/`, which wouldn't
//! have the server binary alongside it). Run with:
//! `cargo run -p terminal_view --example som_tmux_client_test -- <profile>`

fn main() {
    let profile = std::env::args().nth(1).unwrap_or_else(|| "dnk".to_string());
    println!("connecting to (or spawning) som-tmux-server for profile {profile:?}...");
    match terminal_view::som_tmux_client::connect_or_spawn(&profile) {
        Ok(_connection) => println!("connected successfully"),
        Err(err) => {
            eprintln!("failed: {err:#}");
            std::process::exit(1);
        }
    }
}
