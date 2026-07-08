//! Isolated probe: does a lone NUL byte written through `alacritty_
//! terminal`'s `EventLoop`/`Notifier` (the exact mechanism `Session::write`
//! uses) actually survive the trip through a real `ssh.exe` PTY child, all
//! the way to the remote side echoing it back via `cat -v`? Bypasses the
//! whole RELAY/HOLDER protocol AND `Session` entirely — just a raw PTY +
//! EventLoop, same shape `session.rs::Session::spawn` builds.
//!
//! Run with: `cargo run -p som_tmux --example nul_byte_ssh_probe -- <ssh-host>`

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;

#[derive(Clone)]
struct Listener;
impl EventListener for Listener {
    fn send_event(&self, _event: AlacTermEvent) {}
}

#[derive(Clone, Copy)]
struct Bounds {
    cols: u16,
    rows: u16,
}
impl Dimensions for Bounds {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

fn main() {
    let host = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: cargo run -p som_tmux --example nul_byte_ssh_probe -- <ssh-host>");
        std::process::exit(2);
    });

    let bounds = Bounds { cols: 80, rows: 24 };
    let pty_options = tty::Options {
        shell: Some(tty::Shell::new("ssh".to_string(), vec![host, "cat -v".to_string()])),
        working_directory: None,
        drain_on_exit: true,
        env: Default::default(),
        #[cfg(not(windows))]
        child_signal_mask: None,
        #[cfg(windows)]
        escape_args: true,
    };
    let window_size = WindowSize { num_lines: bounds.rows, num_cols: bounds.cols, cell_width: 1, cell_height: 1 };
    let pty = tty::new(&pty_options, window_size, 0).expect("failed to spawn pty");

    let term = Term::new(Config::default(), &bounds, Listener);
    let term = std::sync::Arc::new(alacritty_terminal::sync::FairMutex::new(term));

    let event_loop = EventLoop::new(term.clone(), Listener, pty, pty_options.drain_on_exit, false).expect("failed to create event loop");
    let notifier = Notifier(event_loop.channel());
    let _io_thread = event_loop.spawn();

    std::thread::sleep(std::time::Duration::from_secs(3));

    println!("writing plain text 'before' ...");
    notifier.notify(b"before\n".to_vec());
    std::thread::sleep(std::time::Duration::from_millis(500));

    println!("writing NUL+NUL in ONE SINGLE write() call ...");
    notifier.notify(vec![0u8, 0u8]);
    std::thread::sleep(std::time::Duration::from_secs(3));
    println!("writing plain 'x' now to see if the double-NUL landed invisibly ...");
    notifier.notify(b"x\n".to_vec());
    std::thread::sleep(std::time::Duration::from_secs(2));

    let grid_text: String = {
        let term = term.lock();
        term.renderable_content().display_iter.map(|indexed| indexed.cell.c).collect()
    };
    println!("grid text:\n{grid_text}");
}
