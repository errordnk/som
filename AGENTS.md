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

### Goal: `assets/` contains exactly 4 files, single `settings.json` for everything

**Final `assets/` layout:**
```
assets/
  fonts/
    FiraCodeNerdFont-Regular.ttf   ← единственный embedded шрифт
  settings/
    default-windows.json           ← дефолты для Windows
    default-macos.json             ← дефолты для macOS
    default-linux.json             ← дефолты для Linux
```

**Settings philosophy:**
- Один файл `~/.config/som/settings.json` (или платформенный эквивалент) — всё в нём:
  шрифты, тема, кеймапы, иконки табов.
- Иконки табов — unicode codepoints из FiraCode Nerd Font (например `` = ).
- Нет отдельного keymap-файла. Нет SVG иконок.

---

### Phase 1 — Единственный шрифт: FiraCode Nerd Font

**Текущее состояние:**
- `assets/fonts/ibm-plex-sans/` — UI шрифт, alias `.ZedSans`
- `assets/fonts/lilex/` — моно шрифт, alias `.ZedMono`
- Loaded via `Assets::load_fonts()` → `main.rs`, embedded через `RustEmbed` в `crates/assets`
- Aliases: `crates/gpui/src/text_system.rs` строки 1185–1203

**План:**
1. Скачать `FiraCodeNerdFont-Regular.ttf` (один файл, ~3.5MB, содержит и моно и Nerd Font иконки)
   и положить в `assets/fonts/FiraCodeNerdFont-Regular.ttf`
2. Удалить `assets/fonts/ibm-plex-sans/` и `assets/fonts/lilex/`
3. В `text_system.rs` изменить alias-разрешение:
   - `.ZedMono` / `Zed Plex Mono` → `"FiraCode Nerd Font"`
   - `.ZedSans` / `Zed Plex Sans` → системный UI шрифт по платформе (не embedded):
     - Windows: `"Segoe UI"`
     - macOS: `"SF Pro"`  (или `".AppleSystemUIFont"`)
     - Linux: `"sans-serif"`
4. `Assets::load_fonts()` теперь грузит только один `.ttf`
5. `buffer_font_family` в default.json → `"FiraCode Nerd Font"`
6. `ui_font_family` в default.json → платформо-зависимый системный шрифт (убрать из embedded defaults, задать через platform-specific default.json)

---

### Phase 2 — Три платформенных default.json вместо одного

**Текущее состояние:**
- `assets/settings/default.json` — один файл для всех платформ, загружается через `SettingsAssets`
- `crates/settings/src/settings.rs`: `pub fn default_settings()` → `asset_str("settings/default.json")`

**План:**
1. Разбить `default.json` на три файла: `default-windows.json`, `default-macos.json`, `default-linux.json`
2. В `crates/settings/src/settings.rs` добавить `#[cfg(target_os = ...)]` аналогично `DEFAULT_KEYMAP_PATH`:
   ```rust
   #[cfg(target_os = "windows")]
   pub fn default_settings() -> Cow<'static, str> {
       asset_str::<SettingsAssets>("settings/default-windows.json")
   }
   ```
3. Удалить `assets/settings/default.json`
4. Каждый платформенный файл задаёт:
   - `ui_font_family` — системный для платформы
   - `buffer_font_family` — `"FiraCode Nerd Font"`
   - `terminal.font_family` — `"FiraCode Nerd Font"`
   - `theme` — тема по умолчанию
   - `base_keymap` — `"None"` (кеймапы встроены inline, см. Phase 3)

---

### Phase 3 — Убрать keymap-файлы, встроить keybindings в settings.json

**Текущее состояние:**
- `assets/keymaps/default-{platform}.json` — базовые кеймапы, загружаются через `SettingsAssets`
- `assets/keymaps/vim.json`, `assets/keymaps/storybook.json` и пр.
- `base_keymap` поле выбирает пресет (VSCode/JetBrains/etc.)

**План:**
1. Содержимое `assets/keymaps/default-{platform}.json` перенести в `const &str` прямо в
   `crates/settings/src/settings.rs` (три константы, `#[cfg]` по платформе)
2. Добавить поле `keybindings` в `settings.json` и `SettingsContent`:
   ```json
   {
     "keybindings": [
       { "context": "Terminal", "bindings": { "ctrl-shift-c": "terminal::Copy" } }
     ]
   }
   ```
3. В `crates/zed/src/zed.rs`: переключить watch с отдельного keymap-файла на settings-файл
4. Убрать пункты меню `Open Keymap` / `Open Keymap File` (всё в settings)
5. Удалить `assets/keymaps/`
6. Удалить `OpenKeymap`, `OpenKeymapFile` action handlers из `zed.rs`

---

### Phase 4 — Tab icons через Nerd Font unicode

**Концепция:**
- Nerd Font иконки — unicode символы в Private Use Area (PUA), например:
  - `` ()  — иконка терминала
  - `` ()  — папка
  - `` ()  — файл
- Никакого SVG, никаких asset-файлов, просто unicode string в настройках

**Поле в settings.json:**
```json
{
  "tabs": {
    "terminal_icon": ""
  }
}
```

**План:**
1. Добавить поле `terminal_icon: Option<String>` в `WorkspaceSettingsContent` (или `ItemSettingsContent`)
2. Дефолт из platform default.json: `"terminal_icon": ""`
3. В `terminal_view` при создании таба передавать этот символ как label/icon
4. Нет SVG pipeline вообще — просто рендерится как текст через FiraCode Nerd Font

---

### Phase 5 — Удалить всё остальное из assets/

После фаз 1–4 удалить:
- `assets/icons/` (296 SVG)
- `assets/images/` (app icons — остаются в платформенных ресурсах вне assets/)
- `assets/sounds/` (bell.ogg — заменить системным bell или убрать)
- `assets/prompts/` (AI prompts)
- `assets/themes/` — **пока оставить** или решить отдельно
- `assets/settings/initial_*.json`, `assets/settings/default_semantic_token_rules.json` — удалить или inline

**После фазы 5 в assets/ останется:**
```
assets/fonts/FiraCodeNerdFont-Regular.ttf
assets/settings/default-windows.json
assets/settings/default-macos.json
assets/settings/default-linux.json
```
`RustEmbed` в `crates/assets` и `crates/settings` будет включать только эти 4 файла.

---

### Размер бинаря

Цель: ~20MB release. Текущий dev: ~38MB.
Основная экономия:
- Удаление 296 SVG иконок (~1MB embedded)
- Замена двух TTF шрифтов (IBM Plex Sans + Lilex ~2MB) одним FiraCode Nerd Font (~3.5MB)
  — чистая потеря ~1.5MB, но выигрыш в функциональности (Nerd Font иконки)
- Удаление неиспользуемых крейтов из дерева зависимостей

---

## Development rules

- **No stubs** — delete code entirely, never replace with empty impls
- **gpui и terminal — трогать можно** (подтверждено пользователем)
- **Не реструктурировать** `terminal_view`, `ui`, `settings`, `theme` — только мёртвый код
- **После каждого блока изменений:** `cargo check -p som`
- **Release builds:** запускает разработчик вручную, никогда не автоматизировать
