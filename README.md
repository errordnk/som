# Som

A minimal terminal emulator with tabs, splits, and SSH session persistence, forked from [Zed](https://github.com/zed-industries/zed).

## What it is

Som is Zed stripped down to just the terminal. No editor, no LSP, no AI, no collaboration. Just a fast GPU-accelerated Windows/macOS/Linux terminal powered by GPUI, with tabs living in the title bar, split panes, and an optional `som-tmux` backend that keeps remote (SSH) sessions alive across restarts.

See [MANUAL.md](MANUAL.md) for the full feature list and every `settings.json` option.

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
| `terminal` + `terminal_view`            | PTY terminal emulator, tabs, splits, `som-tmux` integration            |
| `som_tmux`                               | HOLDER/RELAY multiplexer that keeps a shell alive after Som exits/restarts (see MANUAL.md) |
| `workspace`                             | Tabs and panes, `~/.config/som/db.json` session persistence            |
| `ui`                                    | UI components                                                          |
| `settings` + `theme` + `theme_settings` | Settings and theming                                                   |
| `title_bar` + `platform_title_bar`      | Window chrome, tab strip                                                |
| `fs`                                    | Filesystem abstraction                                                  |
| `db` + `sqlez`                          | SQLite (legacy; tabs/splits now use `db.json`, not SQLite)              |
| `project` + `worktree`                  | File tree (used by terminal for cwd context)                            |

## Configuration

On first launch Som writes a default `~/.config/som/settings.json` (platform-specific template embedded in the binary). Edit that file to change fonts, colors, keybindings, tab profiles, and SSH/tmux behavior — it's watched and reloaded live. Full reference: [MANUAL.md](MANUAL.md).

## Building

```
cargo build -p som --release
```
