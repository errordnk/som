# Som

**Sessions that never end, and images in the terminal — on all three platforms.**

Usually you have to pick: a GPU-accelerated terminal with inline graphics, *or* Windows support, *or* tmux (which you still have to install and configure yourself). Som gives you all of it out of the box: SSH sessions that survive disconnects, images rendered via Som's own rich-content protocol, GPU rendering, tabs in the title bar, and split panes — one binary on Windows, macOS, and Linux, no install, no dependencies.

Forked from [Zed](https://github.com/zed-industries/zed), stripped down to just the terminal.

See [MANUAL.md](MANUAL.md) for the full feature list and every `settings.json` option.

## Why it's not "just a Zed fork"

Living sessions, in-terminal graphics, GPU rendering, a native Windows build, zero-install operation — each one exists somewhere else individually. The catch is that each usually comes from a *different* tool: a terminal with image support that doesn't build on Windows, tmux that needs installing and configuring on every server by hand, a portable terminal that does neither. Som closes the whole list with one binary on all three platforms.

- **Zero installs or setup.** Download one file and you already have session persistence and in-terminal graphics. No `apt install tmux` on every server, no `.tmux.conf`, no external daemon for image previews — `som-srv` deploys itself to the remote host, graphics work by default.
- **3/3 platforms, no cut corners.** Terminals in this class are usually Linux/macOS tools first, with Windows either missing or an afterthought. Som was built for three platforms on one engine from the start — the Windows build is native and gets exactly the same feature set, in-terminal graphics and living SSH sessions included.
- **100% Rust.** Not a line of C/C++ in the terminal logic — the whole stack, from the GPU renderer to the PTY plumbing to the graphics-protocol parser, is in the language chosen for software that isn't allowed to crash or leak memory for years.

## Features

Built on GPUI — a GPU-accelerated UI engine without compromises. Som keeps only what a terminal needs from it and drops the editor, LSP, AI, and collaboration layers, landing on speed and stability that's hard to get from bare Rust + GPU otherwise.

- **GPU rendering** — smooth scrolling and text painting even on large scrollback buffers.
- **Tabs in the title bar** — no separate tab-bar row; profiles with icons, fast switching, a drag zone with a restore spinner.
- **Splits up to 3 levels** — `Ctrl+Shift+\` opens a new split, direction alternates right → down. Each tab keeps its own layout independently, with parking on tab switch.
- **Full session restore** — tabs, splits, window position and size, all in `db.json`. Restarting puts you back exactly where you left off, without waiting on reconnects.
- **Zed-format themes out of the box** — Catppuccin, Tokyo Night, Dracula, One Dark Pro, and any other Zed theme work with no conversion.
- **Nerd Font and ligatures** — full OpenType feature control: ligatures, stylistic sets, font weight 100–900.
- **One portable binary, ~32MB** — no installer. `som` runs from any directory — config, theme, `som-srv`, and a patched ConPTY are embedded and extract themselves on first launch. A fraction of the size of the full GPUI stack it's cut from.
- **Flexible window modes** — windowed / maximized / fullscreen / minimized, precise terminal padding, fixed startup position and size.
- **Configurable keybindings** — every action, from new tab to split focus, is remappable in JSON. Defaults are tuned separately for Windows, macOS, and Linux.
- **Images in the terminal** — Som's own rich-content protocol streams full-color image previews (see below).

## som-srv: sessions that don't die

A minimal tmux-equivalent built directly into Som. Open an SSH tab with `"tmux": true`, and the connection to the host can drop as many times as it wants afterward.

- Close Som or lose the connection — the process on the remote machine keeps running.
- Reconnecting shows you the same scrollback and the same running programs, not a fresh session.
- Works locally too — the shell survives a restart of Som on the same machine.
- Automatically cleans up orphaned processes on remote hosts.
- Windows, macOS, and Linux binaries are embedded in Som and deploy to the server themselves — nothing to copy by hand.

## Som Rich Protocol: real images in the terminal

Som implements its own binary protocol for streaming rich content through the PTY, so `somcat` (bundled alongside Som) and other clients that speak it show actual images instead of a mosaic of colored characters. No external daemon, no overlay window — the image is painted by the same GPU pipeline as the text, and behaves like part of the terminal's content.

- **Images are real grid text** — each streamed image becomes a block of Unicode placeholder cells in the terminal's own scrollback, so the terminal's normal scroll/clear/history handling positions and hides it correctly, no special-casing needed.
- **Aspect ratio is preserved** — the image fits into its allotted area instead of stretching to the character grid, unlike a naive implementation.
- **Survives an SSH disconnect** — through `som-srv`, images come back along with the rest of the session content: reconnect to the remote host and previews are right where they were, no manual redraw needed.

## Platforms

Windows, macOS, Linux — one codebase, native speed. Each build is a self-contained binary for its platform, no installer, no external dependencies.

- **Windows** — amd64. Embedded, patched ConPTY from the Windows Terminal project, without the standard system-console bugs.
- **macOS** — Apple Silicon (arm64). Native integration with the system title bar and trackpad gestures.
- **Linux** — amd64. X11 and Wayland via GPUI; `som-srv` deploys to remote Linux servers too.

## Configuration

One JSON file, live reload. On first launch Som writes a default `~/.config/som/settings.json` (a platform-specific template embedded in the binary). Edit it, save, and changes apply immediately — invalid JSON doesn't break Som, it shows an error banner and keeps running on the previous settings.

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

Configurable: theme, font and ligatures, cursor shape and color, scroll speed, tab profiles with arbitrary commands (a plain shell, `ssh`, `wsl`), a keybinding per action, terminal padding, and the window's starting position and size. The default profile is marked with `"default": true` — it's what `Ctrl+Shift+=`/`Cmd+Shift+=` and the `+` button open.

Full reference, including every `settings.json` key: [MANUAL.md](MANUAL.md).

## Theming

Same theme format as Zed — any Zed theme works with no conversion. Som ships with one bundled theme, Nord Dark, but the theme engine supports arbitrary JSON files with ~150 colors — the same files Zed itself installs.

- [Catppuccin](https://github.com/catppuccin/zed/blob/main/themes/catppuccin-mauve.json)
- [Tokyo Night](https://github.com/ssaunderss/zed-tokyo-night/blob/main/themes/tokyo-night.json)
- [One Dark Pro](https://github.com/MordFustang21/zed-one-dark-pro/blob/main/themes/one-dark.json)
- [Dracula](https://github.com/dracula/zed/blob/main/themes/dracula.json)

To install one: download the theme's JSON file into `~/.config/som/themes/`, then set `window.theme` in `settings.json` to the `"name"` field *from inside that file* — it doesn't always match the filename, and some files describe several variants at once.

## What was removed from Zed

Everything that isn't the terminal:

- Editor, language support, LSP, tree-sitter
- Git integration (libgit2)
- Collaboration/RPC/proto (client, rpc, remote, channel)
- AI (language_model_core, cloud_llm_client, cloud_api_*)
- Telemetry
- CLI/askpass IPC
- Extension system (WASM)
- Node.js runtime / Prettier
- Task runner
- Project search
- Debugger (DAP)

## What remains

| Crate                                   | Purpose                                                              |
| ---------------------------------------- | ---------------------------------------------------------------------|
| `gpui`                                  | GPU-accelerated UI framework                                          |
| `terminal` + `terminal_view`            | PTY terminal emulator, tabs, splits, Som Rich Protocol, `som-srv` integration |
| `som_srv`                               | HOLDER/RELAY multiplexer that keeps a shell alive after Som exits/restarts (see MANUAL.md) |
| `workspace`                             | Tabs and panes, `~/.config/som/db.json` session persistence            |
| `ui`                                    | UI components                                                          |
| `settings` + `theme` + `theme_settings` | Settings and theming                                                   |
| `title_bar` + `platform_title_bar`      | Window chrome, tab strip                                                |
| `fs`                                    | Filesystem abstraction                                                  |
| `project` + `worktree`                  | File tree (used by terminal for cwd context)                            |

## Building

```
cargo build -p som --release
```
