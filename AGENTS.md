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

## What to work on next — main roadmap

### Goal: eliminate `assets/` directory, single `settings.json` for everything

**Settings philosophy:** one file `~/.config/som/settings.json` (or platform equivalent) controls
everything — fonts, theme, keybindings, tab icons. No separate keymap file. No embedded assets.

---

### Phase 1 — Kill embedded fonts (assets/fonts/)

**Current state:**
- `assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf` — UI font alias `.ZedSans`
- `assets/fonts/lilex/Lilex-Regular.ttf` — mono font alias `.ZedMono`
- Loaded via `Assets::load_fonts()` in `main.rs`, embedded via `RustEmbed` in `crates/assets`
- Aliases resolved in `crates/gpui/src/text_system.rs` lines 1185–1203

**Plan:**
1. Remove `assets/fonts/` directory entirely
2. In `text_system.rs` change alias resolution: `.ZedMono` → system mono per platform, `.ZedSans` → system sans per platform
3. Platform defaults (hardcoded fallback when no user setting):
   - Windows:  UI = `Segoe UI`,  mono = `Cascadia Code` (fallback: `Consolas`)
   - macOS:    UI = `SF Pro`,    mono = `SF Mono` (fallback: `Menlo`)
   - Linux:    UI = `sans-serif`, mono = `monospace`
4. User overrides via `settings.json`:
   ```json
   { "ui_font_family": "My Font", "buffer_font_family": "My Mono" }
   ```
5. Remove `Assets::load_fonts()` call from `main.rs`
6. Remove font paths from `RustEmbed` in `crates/assets/src/assets.rs`

---

### Phase 2 — Merge keymap into settings.json

**Current state:**
- Platform keymap loaded from `assets/keymaps/default-{platform}.json` via `SettingsAssets` (RustEmbed)
- User can have separate `keymap.json` file
- `base_keymap` field in `SettingsContent` selects a base preset (VSCode/JetBrains/etc.)

**Plan:**
1. Move platform keymap JSON content from files into `const &str` per platform in `crates/settings/src/settings.rs`
   (inline the file content, delete `assets/keymaps/`)
2. Add `keybindings` array field to `settings.json`:
   ```json
   {
     "keybindings": [
       { "context": "Terminal", "bindings": { "ctrl-shift-c": "terminal::Copy" } }
     ]
   }
   ```
3. Wire `keybindings` from `SettingsContent` into the keymap reload path in `zed.rs`
   (currently watches separate keymap file — redirect to watch settings file instead)
4. Keep `base_keymap` field for preset selection
5. Remove `OpenKeymapFile` / `OpenKeymap` menu items (already in settings)
6. Delete `assets/keymaps/` directory

---

### Phase 3 — Tab icons without assets/icons/

**Current state:**
- `assets/icons/` has 295 SVG files, almost all AI/editor-specific, none terminal-relevant
- Icon theme loaded via Assets/RustEmbed
- Tabs in `workspace` use icon theme paths

**Plan:**
1. Remove `assets/icons/` entirely (delete `icon_theme` from settings too, or make it no-op)
2. Tab icon configured in `settings.json` as unicode char or external SVG path:
   ```json
   {
     "tabs": {
       "terminal_icon": "▶",
       "terminal_icon_svg": "/path/to/icon.svg"
     }
   }
   ```
3. Default: unicode `▶` (no asset dependency)
4. External SVG: loaded from filesystem path at runtime (not embedded)
5. Add `terminal_icon` / `terminal_icon_svg` fields to `SettingsContent` → `WorkspaceSettingsContent`

---

### Phase 4 — Kill remaining assets/ subdirectories

After phases 1–3:
- `assets/images/` — delete (app icons handled by platform-specific resources)
- `assets/sounds/` — delete (bell sound; terminal bell can use system bell)
- `assets/prompts/` — delete (AI prompts)
- `assets/settings/initial_*.json` — delete (only `default.json` and `default_semantic_token_rules.json` remain)
- `assets/themes/` — **keep** (themes still loaded via RustEmbed, user can switch via settings)

After phase 4, `RustEmbed` in `crates/assets` covers only `themes/**` and `settings/default*.json`.

---

### Phase 5 — Slim `assets/settings/default.json`

Remove all fields that reference deleted functionality:
- `icon_theme`, `auto_update`, `agent_*`, `git_*`, `collaboration_panel`, etc.
- Keep: `theme`, `buffer_font_*`, `ui_font_*`, `terminal`, `tabs`, `base_keymap`, `keybindings`

---

### Size goal

~20MB release binary (currently ~38MB dev build, release with LTO will be smaller).
Main wins: removing embedded fonts (~2MB), removing 295 SVG icons (~1MB embedded strings).

---

## Development rules

- **No stubs** — delete code entirely, never replace with empty impls
- **gpui and terminal are now touchable** (confirmed by user) for bug fixes and asset removal
- **Do not restructure** `terminal_view`, `ui`, `settings`, `theme` — only remove dead code
- **After every change block:** `cargo check -p som`
- **Release builds:** run manually by the developer, never automated
