# Som

A minimal terminal emulator with tabs, forked from [Zed](https://github.com/zed-industries/zed).

## What it is

Som is Zed stripped down to just the terminal. No editor, no LSP, no AI, no collaboration. Just a fast GPU-accelerated Windows/MacOS/Linux terminal with tabs, ligatures powered by GPUI.

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

| Crate                                   | Purpose                                         |
| --------------------------------------- | ----------------------------------------------- |
| `gpui`                                  | GPU-accelerated UI framework                    |
| `terminal` + `terminal_view`            | PTY terminal emulator                           |
| `workspace`                             | Tabs and panes                                  |
| `ui`                                    | UI components                                   |
| `settings` + `theme` + `theme_settings` | Settings and theming                            |
| `title_bar` + `platform_title_bar`      | Window chrome                                   |
| `fs`                                    | Filesystem abstraction                          |
| `db` + `sqlez`                          | SQLite persistence (workspace layout, settings) |
| `project` + `worktree`                  | File tree (used by terminal for cwd context)    |

## Building

```
cargo build -p som --release
```

Release builds use LTO=thin.
