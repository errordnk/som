# Som

**Sessions that never end and live content in the terminal — on all three platforms.**

Normally you have to choose: either a GPU terminal with graphics, or Windows, or tmux — which you still have to install and configure. Som gives you all of it at once, out of the box: SSH sessions that survive a dropped connection, images, video, audio and markdown through its own Som Rich Protocol, GPU rendering, tabs in the title bar and splits — as a single binary on Windows, macOS and Linux, with no installation and no dependencies.

See [MANUAL.md](MANUAL.md) for the full reference and every `settings.json` option.

## Why it stands out

Living sessions, in-terminal graphics, GPU rendering, a native Windows build, zero-install operation — individually, all of this exists somewhere. The problem is that each item usually comes from a *different* tool: a terminal with graphics support doesn't build on Windows, tmux has to be installed and configured on every server, a portable terminal does neither. Som covers the whole list with one binary on all three platforms.

**0 — installs and setup.** Download one file and you already have both session persistence and live content in the terminal. No `apt install tmux` on every server, no `.tmux.conf`, no external daemons for file previews: `somsrv` deploys itself to the remote host, and graphics are on by default.

**3 / 3 — Windows, macOS and Linux, as equals.** One engine, one codebase, the same feature set on all three platforms. A living `somsrv` session works in any client → host combination just the same: Windows to a Linux server, Linux to macOS, no difference.

**100% — Rust, no compromises.** Not one line of C/C++ in the terminal logic — the entire stack, from the GPU renderer to the PTY plumbing to the graphics protocol parser, is written in a language chosen for industrial software that must not crash or leak memory for years.

## Features

At the core of Som is [GPUI](https://gpui.rs/): GPU rendering and a theming system without compromises, the same engine that carries the Zed editor. Som builds a terminal on it from scratch, with no editor, LSP, AI or collaboration — arriving at speed and stability you don't just assemble from bare Rust + GPU.

- **GPU rendering** — smooth scrolling and text painting without stutter, even on large scrollback buffers.
- **Tabs in the title bar** — no separate tab strip; tabs live directly in the title bar. Profiles with icons, fast switching, a drag zone with a restore spinner.
- **Splits up to 3 levels** — `Ctrl+Shift+\` opens a new split, the direction alternating right → down. Each tab keeps its own layout independently, parked on switch.
- **Full session restore** — tabs, splits, window position and size all live in `db.json`. A restart puts you back exactly where you stopped, without waiting on connections.
- **Themes out of the box** — the theme format matches Zed's, so Catppuccin, Tokyo Night, Dracula, One Dark Pro and any other Zed theme work without conversion.
- **Nerd Font and ligatures** — full control over OpenType features: ligatures, stylistic sets, font weight 100–900 — everything you'd expect from a real editor engine.
- **One portable binary, ~40 MB** — no installation, no setup: download `som`, run it from any folder, it works immediately. It downloads nothing on first launch, asks for no runtimes and leaves no traces in the system.
- **Everything it needs is embedded** — the binary carries per-OS settings, the theme, fonts (including a proportional one, for markdown), icons, `somsrv` for all three platforms, a patched ConPTY and the FFmpeg video decoder. All zstd-compressed and unpacked automatically on first launch. FFmpeg was separately trimmed to just the codecs in use: together with compression that took the five DLLs from ~18 MB raw to ~2 MB embedded and cut ~15 MB — about 29% — off the binary.
- **Flexible window modes** — windowed / maximized / fullscreen / minimized, precise terminal padding from the window edge, fixed startup position and size.
- **Configurable keybindings** — every action, from a new tab to split focus, is remappable in JSON. The defaults are thought through separately for Windows, macOS and Linux.
- **Content right in the terminal** — Som Rich Protocol: the bundled `somcat` renders images, video, audio and markdown straight into the terminal stream (more below).
- **An open protocol, not a feature locked inside Som** — any program living inside a PTY can be taught to speak SRP, the same way it might already speak the Kitty graphics protocol or Sixel. The [yazi](https://github.com/errordnk/yazi) file manager already does — image, video and audio previews right in the file list, over the same protocol Som's terminal speaks.
- **somkey: keyboard diagnostics** — a separate bundled tool that draws a physical keyboard and shows exactly what the engine receives from each key, making it immediately obvious whether a KVM, an OS remapper or your own config is the one lying.
- **A broken config doesn't break your work** — invalid JSON neither crashes the terminal nor resets your settings: Som shows an error banner and keeps running on the last valid values until you fix the file.

## somsrv: sessions that don't die

A minimal tmux-alike built directly into Som. Open an SSH tab with `"tmux": true` and from then on the connection to the host can drop as many times as it likes.

- Close Som or lose the connection — the process on the remote machine keeps running.
- On reconnect you see the same scrollback and the same running programs, not a new session.
- Works locally too: the shell survives a restart of Som on the same machine.
- Automatically cleans up orphaned processes on remote hosts.
- `somsrv` itself is three-platform too: host and client can each be Windows, macOS or Linux in any combination — a Windows machine holds a local session at home and a living session on a remote Linux server just the same.
- Binaries for all three platforms are embedded in Som and deploy themselves to the server — nothing to copy by hand.
- A full headless terminal lives on the host: reconnecting hands over a snapshot of its state rather than a replay of the last bytes, so the screen comes back correct — borders, colors and cursor position included.
- One daemon per host serves all your sessions, instead of a process per tab.

```
Som (client)                    SSH tab with tmux: true
      │
      ▼
HOLDER on the remote host       holds the real shell and PTY, lives independently of Som
      │
      ▼
Connection drops / Som closes   the process on the server carries on as if nothing happened
      │
      ▼
Tab reopened                    reconnect to the same HOLDER — same scrollback, same htop
```

## Som Rich Protocol: images, video, audio and markdown right in the text

Som implements its own binary protocol for streaming rich content, so `somcat` (shipped with Som) emits into the terminal stream not a mosaic of colored characters, but real images, playing video, an audio track with progress, and rendered markdown. No external daemons, no overlay windows: everything is drawn by the same GPU pipeline as the text and behaves like part of the terminal's content. The protocol is open, and Som isn't the only thing speaking it today: the yazi file manager shows live previews through it right in the file list.

- **Images and GIF** — PNG and JPEG render as full-color previews, animated GIFs play frame by frame right in the terminal grid. Aspect ratio is preserved: the image fits the area it's given instead of stretching to the character cells.
- **Video with controls** — a video file opens directly in the terminal stream, with pause, seeking and a progress bar. Data is pulled as you watch rather than loaded whole, so a 16 GB file starts playing as quickly as a short clip. *Windows only for now.*
- **Audio** — the audio track is decoded and played by Som itself, with transfer and playback progress — handy for quickly listening to a file on a remote host without downloading it.
- **Markdown actually renders** — a README, a note, a changelog opens as a real document right in the terminal stream: proper heading levels, lists, blockquotes, rules, bold and italic, a dedicated monospace font for code. Not syntax highlighting bolted onto text — an honest layout: what's `# Heading` in the source looks like a heading on screen, not a line with a hash in front of it. This is already the foundation for something bigger — Som is meant to grow into a markdown browser inside the terminal, not stay a shell emulator with extras.
- **Content is real grid text** — every transferred object becomes a block of Unicode placeholder cells right in the terminal's scrollback, so the terminal's ordinary scroll, clear and history handling positions and hides it correctly — with no special-casing.
- **Survives an SSH drop** — through `somsrv` content is restored along with the rest of the session: reconnect to the remote host and the preview is still there, no manual redraw needed.
- **Files travel around the PTY** — image and video bytes move over `somsrv`'s separate binary channel instead of being encoded into the terminal's text stream, so a multi-megabyte preview never clogs the same pipe your keystrokes go through.

## Platforms

Windows, macOS, Linux — one codebase, native speed. Each build is a self-contained binary for its platform, with no installer and no external dependencies. The feature set is identical everywhere: splits, themes, living sessions and in-terminal content don't depend on where you work from.

- **Windows** — amd64 · an embedded patched ConPTY from the Windows Terminal project, without the standard system-console bugs.
- **macOS** — Apple Silicon (arm64) · native integration with the system title bar and trackpad gestures.
- **Linux** — amd64 · X11 and Wayland through GPUI; `somsrv` deploys to remote Linux servers too.

## Configuration

One JSON file, live reload. `~/.config/som/settings.json` — edit it, save, and the changes apply instantly. Invalid JSON doesn't break Som: it shows an error banner and keeps working on the previous settings.

```json
{
  "window": {
    "theme": "Nord Dark",
    "mode": "maximized",
    "selection": "#88c0d0"
  },
  "font": {
    "face": "FiraCode Nerd Font",
    "size": 14,
    "features": { "calt": true }
  },
  "tabs": [
    { "name": "local", "shell": "/bin/zsh", "default": true },
    { "name": "prod", "shell": "ssh prod-1", "tmux": true }
  ]
}
```

**What's configurable:** theme, font and ligatures, cursor shape and color, scroll speed, tab profiles with any command (a plain shell, `ssh`, `wsl`), a hotkey for every action, terminal padding, startup window position and size.

The default profile is marked with `"default": true` — that's the one `Ctrl+Shift+=` and the "+" button open.

## Themes

The theme format matches Zed's, so any Zed theme works without conversion. Som ships with a single built-in theme, Nord Dark, but the theme engine supports arbitrary JSON files with ~150 colors — the same files people install into Zed.

| Theme | Palette |
| --- | --- |
| [Catppuccin](https://github.com/catppuccin/zed/blob/main/themes/catppuccin-mauve.json) | Mauve — pastel palette, soft contrast |
| [Tokyo Night](https://github.com/ssaunderss/zed-tokyo-night/blob/main/themes/tokyo-night.json) | Deep blue background, neon accents |
| [One Dark Pro](https://github.com/MordFustang21/zed-one-dark-pro/blob/main/themes/one-dark.json) | The editor-theme classic, familiar to many |
| [Dracula](https://github.com/dracula/zed/blob/main/themes/dracula.json) | High contrast, purple-pink accents |

Installing: download the theme's JSON file into `~/.config/som/themes/`, then set `window.theme` in `settings.json` to the value of the `"name"` field *from inside the file itself* — it doesn't always match the file name, and some files describe several variants at once.

## Keybindings

Sensible defaults, fully remappable. Every Som-specific action sits on `Ctrl+Shift+*` so it doesn't collide with standard system shortcuts.

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+=` | New tab (default profile) |
| `Ctrl+Shift+1`…`0` | Tab from profile 1–10 |
| `Ctrl+Shift+-` | Close the active tab |
| `Ctrl+Shift+\` | New split |
| `Ctrl+Shift+Backspace` | Close split |
| `Ctrl+Shift+←/→` | Switch tab |
| `Ctrl+Shift+↑/↓` | Focus between splits |
| `Ctrl+=/-/0` | Font scale |
| `Ctrl+Scroll` | Font scale with the wheel |
| `Ctrl+Shift+C` / `Ctrl+V` | Copy / paste |

## Building

```
cargo build -p som --release
```

---

Som is built on [GPUI](https://gpui.rs/) — the same engine that carries the [Zed](https://github.com/zed-industries/zed) editor, whose source is much of how GPUI got learned in the first place. GPL-3.0-or-later.
