# Agent memo — Som

Read this first. After reading you will have full context on the project and can work immediately.

## What this project is

**Som** (previously called Sarnet) — a minimal GPU-accelerated terminal emulator with tabs for Windows, macOS, and Linux.
It is a fork of [Zed](https://github.com/zed-industries/zed), with everything except the terminal stripped out.
No editor. No LSP. No AI. No collaboration. No git integration. Just tabs + PTY terminal.

**Binary:** `target/release/som` (`.exe` on Windows)
**App name constant:** `crates/paths/src/paths.rs` — `APP_NAME = "Som"` (drives all data/config paths)
**Cargo package name:** `som` (was `sarnet`, was `zed`)

## What was surgically removed from Zed

These crates are **deleted from disk** — do not reference them, they do not exist:
- `cli`, `askpass` — IPC/CLI handshake (replaced with raw Windows named pipe in `open_listener.rs`)
- `language`, `language_core` — language support
- `ztracing_macro` — proc-macro for `#[instrument]` (all usages removed from call sites)
- `lsp`, `lsp_types` — language server protocol
- `git`, `libgit2` — git integration
- `rpc`, `proto`, `client` — collaboration/network
- `remote`, `context_server` — remote projects
- `cloud_llm_client`, `language_model_core`, `cloud_api_*` — AI
- `telemetry`, `telemetry_events` — telemetry
- `channel`, `notifications` — collab UI
- `node_runtime` — Node.js / Prettier
- `buffer_diff`, `feature_flags`, `zeta_prompt` — editor features
- `dap` — debugger
- `fuzzy` / `CharBag` — removed from worktree

These crates are **still in workspace but removed from som's production dep tree:**
- `util_macros` — was used for `#[perf]` in gpui tests only (now dev-dep only)
- `ztracing` — still exists but has no `ztracing_macro` dep anymore; only `log`-based tracing remains

## Crates that remain (the core)

```
gpui            — GPU-accelerated UI framework (DO NOT TOUCH)
gpui_*          — platform backends (windows, tokio, wgpu, macros, ...)
terminal        — PTY emulator (DO NOT TOUCH)
terminal_view   — UI for terminal (only remove dead code, do not restructure)
workspace       — tabs, panes, dock (DO NOT TOUCH structure)
ui / ui_macros  — UI components (DO NOT TOUCH)
settings        — settings system (DO NOT TOUCH)
theme / theme_settings — theming (DO NOT TOUCH)
title_bar / platform_title_bar — window chrome (simplified, auto_update removed)
fs              — filesystem (git dep is optional/test-support only)
db / sqlez / sqlez_macros — SQLite persistence for workspace layout
project / worktree — file tree, cwd context for terminal
session         — session restore
release_channel — app version/channel enum
assets          — bundled fonts, icons
```

## Non-obvious code changes from upstream Zed

### `crates/zed/src/zed/open_listener.rs`
- `RawOpenRequest` — field `open_behavior` removed (was `cli::OpenBehavior`)
- `OpenRequestKind::CliConnection` variant — removed entirely
- `open_options_for_request(location, cx)` — now public, signature changed (no `open_behavior` param)
- Uses `settings::Settings as _` for trait method access

### `crates/zed/src/zed/windows_only_instance.rs`
- Single-instance IPC rebuilt without `cli` crate
- `send_args_to_instance` writes URL string directly to named pipe (no IPC handshake)

### `crates/zed/src/main.rs`
- `AppContext as _` import needed for `.new()` method on `App`
- No `FORCE_CLI_MODE`, no `askpass`, no `cli`
- `stdout_is_a_pty()` simplified to `io::stdout().is_terminal()`
- `handle_open_request` — no `CliConnection` arm

### `crates/zed/src/zed.rs`
- `pub(crate) mod open_listener` with explicit re-exports (not `pub use open_listener::*`)
- `app_id: Some("dev.som.Som".to_owned())` hardcoded in WindowOptions
- `ReleaseChannel::global(cx).display_name()` → `"Som".into()`

### `crates/worktree/src/worktree.rs`
- Constants `DOT_GIT`, `GITIGNORE`, `GitSummary`, `TrackedSummary`, `DiskState`, `ByteContent`,
  `FILE_ANALYSIS_BYTES`, `analyze_byte_content` — copied locally (no longer from `language` crate)
- `impl language::File` and `impl language::LocalFile` removed

### `crates/terminal_view/src/...`
- `language::CursorShape` → `terminal::terminal_settings::CursorShape`

## Rules — always follow these

1. **No stubs.** When removing functionality, delete it completely. Never replace with `todo!()`, `unimplemented!()`, or empty impls.
2. **Do not touch** `terminal`, `terminal_view` (except dead code removal), `gpui`, `ui`, `settings`, `theme`.
3. **After every block of changes:** `cargo check -p som` via Bash.
4. **Release builds** are run by the developer manually. Never automate them.
5. **No agents/subagents** — work in the main conversation only.
6. **Speak Russian** with the user.
7. **Never use** `unwrap()` — use `?` or `.log_err()`.
8. **Never silently discard errors** with `let _ =`.

## Cargo check command

```bash
cargo check -p som 2>&1 | tail -20
```

## Typical investigation pattern

To check if a crate is in som's production dependency tree:
```bash
cargo tree -p som --invert <crate-name> -e no-dev 2>&1
```
If it returns nothing, the crate is not in the production tree.

## What to work on next (open tasks)

- Dead code audit in `workspace` and `project` (may still have editor references)
- `migrator` — simplify: remove migrations for editor/LSP/AI keymaps, keep only relevant ones
- Size goal: ~20MB release binary (currently ~38MB dev, smaller with release LTO)

## Development rules

- **No stubs** — delete code entirely, never replace with empty impls
- **Do not touch:** `terminal`, `terminal_view` (except dead code removal), `gpui`, `ui`, `settings`, `theme`
- **After every change block:** `cargo check -p som`
- **Release builds:** run manually by the developer, never automated
