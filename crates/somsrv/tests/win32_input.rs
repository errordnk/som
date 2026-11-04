//! How keys reach a console program in a Som tab on Windows: Som's ConPTY
//! asks the terminal for win32-input-mode (`CSI ? 9001 h`), Som then sends
//! every key as `CSI Vk;Sc;Uc;Kd;Cs;Rc _`, and ConPTY hands the program the
//! exact INPUT_RECORD. In a `win` tab there are two ConPTYs - Som's, around
//! the relay, and the session's, around the shell - and the relay, which
//! reads its keys as VT, asks its own console for the mode too, so the keys
//! it passes on are still the win32-input ones.
//!
//! Far Manager switches its panel views with Ctrl+1..4 and opens its sort
//! menu with Ctrl+F12; in VT Ctrl+1 was CSI u (typed into Far as `[49;5u`)
//! and Ctrl+2 a NUL, which ended the whole session (2026-11-04).
//!
//! The "programs" are PowerShell scripts that print what they read; nothing
//! is simulated, every ConPTY is the real one Som runs.

#![cfg(all(windows, target_arch = "x86_64"))]

use alacritty_terminal::event::{VoidListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, EventedReadWrite};
use alacritty_terminal::vte::ansi::Processor;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const PWSH: &str = r"C:\Program Files\PowerShell\7\pwsh.exe";
const COLS: u16 = 160;
const LINES: u16 = 40;

/// Prints `K vk=<VirtualKeyCode> ch=<char> cs=<ControlKeyState>` for every
/// key pressed - what Far Manager reads.
const READ_KEYS: &str = r#"Write-Host READY
while ($true) {
    $k = $Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown')
    Write-Host ('K vk=' + $k.VirtualKeyCode + ' ch=' + [int]$k.Character + ' cs=' + ([int]$k.ControlKeyState -band 0x1f))
}
"#;

/// Reads its console as VT text, the way the relay does (no line input, no
/// echo, no processed input, VT input), after asking for win32-input-mode
/// the way the relay does, and prints what it got with control bytes as
/// `<hex>`.
const READ_VT: &str = r#"Add-Type -Namespace P -Name K -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern IntPtr GetStdHandle(int n);
[DllImport("kernel32.dll")] public static extern bool GetConsoleMode(IntPtr h, out uint m);
[DllImport("kernel32.dll")] public static extern bool SetConsoleMode(IntPtr h, uint m);
[DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern bool ReadConsoleW(IntPtr h, char[] b, uint n, out uint r, IntPtr c);
'@
$h = [P.K]::GetStdHandle(-10); $m = 0; [void][P.K]::GetConsoleMode($h, [ref]$m)
[void][P.K]::SetConsoleMode($h, ($m -band (-bnot 0x7)) -bor 0x200)
[Console]::Out.Write("`e[?9001h"); [Console]::Out.Flush()
Write-Host READY
$b = New-Object char[] 256
while ($true) {
    $r = 0; [void][P.K]::ReadConsoleW($h, $b, 256, [ref]$r, [IntPtr]::Zero)
    $s = ''; for ($i = 0; $i -lt $r; $i++) { $c = [int]$b[$i]; if ($c -lt 32 -or $c -gt 126) { $s += ('<{0:x2}>' -f $c) } else { $s += [char]$c } }
    Write-Host ('V ' + $s)
}
"#;

/// Ctrl+1, pressed and released, as Som sends it in win32-input-mode.
const CTRL_1: &[u8] = b"\x1b[49;2;0;1;8;1_\x1b[49;2;0;0;8;1_";
const CTRL_2: &[u8] = b"\x1b[50;3;0;1;8;1_\x1b[50;3;0;0;8;1_";
const CTRL_F12: &[u8] = b"\x1b[123;88;0;1;8;1_\x1b[123;88;0;0;8;1_";

fn script(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("som-win32-input-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A program in Som's own ConPTY, its output read the way Som reads it.
struct Tab {
    pty: tty::Pty,
    raw: Vec<u8>,
    term: Term<VoidListener>,
    parser: Processor,
}

impl Tab {
    fn start(program: &str, args: &[&str]) -> Tab {
        somsrv::conpty::wire();
        let options = tty::Options {
            shell: Some(tty::Shell::new(program.into(), args.iter().map(|arg| arg.to_string()).collect())),
            working_directory: None,
            drain_on_exit: true,
            env: Default::default(),
            escape_args: true,
        };
        let size = WindowSize { num_lines: LINES, num_cols: COLS, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0).expect("a program starts in a ConPTY");
        let term = Term::new(Config::default(), &TermSize, VoidListener);
        Tab { pty, raw: Vec::new(), term, parser: Processor::new() }
    }

    fn pump(&mut self) {
        let mut buf = [0u8; 8192];
        loop {
            match self.pty.reader().read(&mut buf) {
                Ok(read) if read > 0 => {
                    self.raw.extend_from_slice(&buf[..read]);
                    self.parser.advance(&mut self.term, &buf[..read]);
                },
                _ => return,
            }
        }
    }

    fn screen(&self) -> String {
        let grid = self.term.grid();
        let mut text = String::new();
        for line in 0..grid.screen_lines() as i32 {
            for column in 0..grid.columns() {
                text.push(grid[Line(line)][Column(column)].c);
            }
            text.push('\n');
        }
        text
    }

    fn wait_for(&mut self, what: &str, patience: Duration) {
        let deadline = Instant::now() + patience;
        loop {
            self.pump();
            if self.screen().contains(what) {
                return;
            }
            assert!(Instant::now() < deadline, "gave up waiting for {what:?}; the screen:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn press(&mut self, bytes: &[u8]) {
        self.pty.writer().write_all(bytes).unwrap();
        self.pty.writer().flush().unwrap();
    }
}

struct TermSize;

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        LINES as usize
    }

    fn screen_lines(&self) -> usize {
        LINES as usize
    }

    fn columns(&self) -> usize {
        COLS as usize
    }
}

#[test]
fn conpty_asks_for_win32_input_mode_and_a_console_program_gets_the_exact_keys() {
    let path = script("read-keys.ps1", READ_KEYS);
    let mut tab = Tab::start(PWSH, &["-NoProfile", "-NoLogo", "-File", path.to_str().unwrap()]);
    tab.wait_for("READY", Duration::from_secs(20));
    assert!(
        tab.raw.windows(8).any(|window| window == b"\x1b[?9001h"),
        "ConPTY no longer asks for win32-input-mode: {:?}",
        String::from_utf8_lossy(&tab.raw)
    );

    tab.press(CTRL_1);
    tab.wait_for("K vk=49 ch=0 cs=8", Duration::from_secs(10));
    tab.press(CTRL_F12);
    tab.wait_for("K vk=123 ch=0 cs=8", Duration::from_secs(10));
}

#[test]
fn a_vt_reader_in_win32_input_mode_gets_the_keys_as_win32_input_and_text_as_text() {
    let path = script("read-vt.ps1", READ_VT);
    let mut tab = Tab::start(PWSH, &["-NoProfile", "-NoLogo", "-File", path.to_str().unwrap()]);
    tab.wait_for("READY", Duration::from_secs(30));

    tab.press(CTRL_1);
    tab.wait_for("V <1b>[49;2;0;1;8;1_", Duration::from_secs(10));
    // typed text stays text, so the relay passes it on as it came
    tab.press(b"dir");
    tab.wait_for("V dir", Duration::from_secs(10));
}

/// Whether the test daemon has a session for the pane; a failed listing is
/// asked again, it says nothing either way.
fn has_session(pane_id: &str) -> bool {
    let mut tries = 0;
    loop {
        match somsrv::admin::list_sessions(None) {
            Ok(sessions) => return sessions.iter().any(|session| session.pane_id == pane_id),
            Err(err) if tries < 50 => {
                tries += 1;
                eprintln!("list_sessions failed ({err:#}), asking again");
                std::thread::sleep(Duration::from_millis(100));
            },
            Err(err) => panic!("the daemon does not list its sessions: {err:#}"),
        }
    }
}

/// The whole way of a `win` tab: Som's ConPTY, the relay in it, the
/// session's ConPTY in the daemon, the program in that.
#[test]
fn a_win_tab_hands_far_manager_its_view_keys_and_ctrl_2_does_not_end_the_session() {
    somsrv::protocol::use_test_daemon();
    let path = script("read-keys-relay.ps1", READ_KEYS);
    let pane_id = format!("win32-input-{}", std::process::id());
    let mut tab = Tab::start(
        env!("CARGO_BIN_EXE_somsrv"),
        &["win32-input-test", &pane_id, PWSH, "-NoProfile", "-NoLogo", "-File", path.to_str().unwrap(), "--cursor-shape", "block"],
    );
    tab.wait_for("READY", Duration::from_secs(40));

    tab.press(CTRL_1);
    tab.wait_for("K vk=49 ch=0 cs=8", Duration::from_secs(10));
    tab.press(CTRL_2);
    tab.wait_for("K vk=50 ch=0 cs=8", Duration::from_secs(10));
    tab.press(CTRL_F12);
    tab.wait_for("K vk=123 ch=0 cs=8", Duration::from_secs(10));
    // a NUL typed in VT (Ctrl+2, Ctrl+Space before win32-input-mode) is a key too
    tab.press(b"\x00");
    tab.wait_for("K vk=50 ch=0 cs=24", Duration::from_secs(10));
    assert!(has_session(&pane_id), "the session ended");

    // Closing a local tab is asking the daemon, as `TerminalView::on_removed`
    // does (the link's frames go to remote tabs only).
    somsrv::admin::kill_session(None, pane_id.clone()).expect("the daemon ends the session");
    let deadline = Instant::now() + Duration::from_secs(20);
    while has_session(&pane_id) {
        assert!(Instant::now() < deadline, "closing the tab did not end its session");
        std::thread::sleep(Duration::from_millis(100));
    }
}
