# Som Manual

This document describes every user-facing feature of Som and every option in `~/.config/som/settings.json`. It reflects the actual behavior of the code as of this writing, including a few gaps between what's shipped and what's documented — those are called out explicitly rather than glossed over.

## Table of contents

1. [Where things live](#where-things-live)
2. [settings.json reference](#settingsjson-reference)
3. [Default keybindings](#default-keybindings)
4. [Tabs](#tabs)
5. [Splits](#splits)
6. [Session persistence (db.json)](#session-persistence-dbjson)
7. [Restore on launch](#restore-on-launch)
8. [som-tmux: keeping remote sessions alive](#som-tmux-keeping-remote-sessions-alive)
9. [Theming](#theming)
10. [Known gaps between docs and behavior](#known-gaps-between-docs-and-behavior)

---

## Where things live

| Path | Purpose |
|---|---|
| `~/.config/som/settings.json` | Your configuration. Created from a platform-specific template on first launch. Watched and live-reloaded — edits apply immediately, with an in-app error banner if the JSON is invalid. |
| `~/.config/som/themes/nord.json` | The bundled "Nord" theme, written out on first launch. |
| `~/.config/som/db.json` | Tab/split/session layout, rewritten on every tab/split change and on quit. Not something you should hand-edit; described below for troubleshooting. |
| `~/.config/som/tmux/{windows,macos,linux-amd}/som-tmux` | Pre-built `som-tmux` binaries used to deploy to remote hosts for SSH sessions with `"tmux": true`. |

---

## settings.json reference

All fields are optional; anything you omit falls back to the shipped default for your platform. Unknown keys are silently ignored (no error), so a typo in a key name will not warn you — double-check spelling against this table.

Two behaviors are fixed and not configurable: the terminal bell is always silent (no setting to turn on a system alert sound), and copy-on-select (auto-copying your selection to the clipboard) is always on.

### `window`

| Key | Type | Effect |
|---|---|---|
| `window.theme` | string | Active theme name (e.g. `"Nord Dark"`). Applied correctly. |
| `window.mode` | `"windowed"` (default) / `"maximized"` / `"minimized"` / `"fullscreen"` | Initial window placement, applied on every launch (not just first run). `"windowed"` remembers its position/size in `db.json` (see [Session persistence](#session-persistence-dbjson)) — moving or resizing the window updates that automatically. If no geometry has been remembered yet, it defaults to the display size minus 100px in each dimension, positioned 50px from the top-left. |
| `window.padding.{top,bottom,left,right}` | number, pixels | Inset between the OS window edge and Som's content (title bar included), on that side. `0` (default) means no inset. |
| `window.selection` | hex string, e.g. `"#88c0d0"` | Terminal selection-highlight color (the background behind text you've selected with the mouse). Also recolors a few other accent-colored UI bits as a side effect (e.g. search-match highlighting), since it shares the theme's single `text.accent` color rather than being terminal-specific. |

### `log`

| Key | Type | Default | Effect |
|---|---|---|---|
| `log.level` | string | `"all"` (`"trace"` on the shipped Windows template) | Log filter level, read once at startup. |
| `log.days` | number | `7` | Windows only: how many days of rotated log files to keep. |

### `font`

| Key | Type | Effect |
|---|---|---|
| `font.face` | string | Terminal AND UI buffer font family. |
| `font.size` | number | Terminal font size (px). |
| `font.weight` | string | **Currently not applied** — declared and shipped but has no effect. |
| `font.lineHeight` | number | Terminal line height multiplier. |
| `font.features` | object (OpenType feature tag → `true`/`false` or a non-negative integer) | OpenType font features — see [Ligatures and font features](#ligatures-and-font-features) below. |

#### Ligatures and font features

`font.features` maps a 4-character OpenType feature tag to either a boolean (`true`/`false`, enable/disable) or an integer (for features with multiple variants, like stylistic sets). This mirrors Zed's own `terminal.font_features`/`buffer_font_features` format exactly:

```json
"font": {
  "face": "FiraCode Nerd Font",
  "features": {
    "calt": true,
    "cv02": 1,
    "cv01": 7
  }
}
```

`calt` is the standard OpenType tag for contextual alternates, which is what most fonts (including FiraCode, Cascadia Code, JetBrains Mono) use to draw ligatures like `->`, `=>`, `!=`, `>=`, `&&` as single joined glyphs. **Ligatures are off by default** — set `"calt": true` to turn them on (the font itself must support ligatures; Som doesn't synthesize them). Invalid tags (not exactly 4 alphanumeric characters) or invalid values (anything other than a boolean or non-negative integer) are logged and skipped rather than breaking the rest of your settings.

### `cursor`

| Key | Type | Effect |
|---|---|---|
| `cursor.shape` | `"block"` / `"bar"` / `"underline"` / `"hollow"` | Cursor shape. |
| `cursor.color` | hex string, e.g. `"#90ee90"` | Cursor color. |
| `cursor.blinking` | `"on"` / `"off"` / anything else | `"on"`/`"off"` map literally; any other value means "terminal controlled" (blink behavior follows the shell app's own escape sequences). |

### `scroll`

| Key | Type | Default | Effect |
|---|---|---|---|
| `scroll.scrollMultiplier` | number | `3.0` | Mouse-wheel scroll speed multiplier. |
| `scroll.maxScrollHistory` | number | `10000` | Scrollback line cap (0 disables scrolling; internally capped at 100,000). |
| `scroll.alternateScroll` | `"on"` / `"off"` | `"on"` | Whether mouse wheel sends arrow keys inside alt-screen apps (vim, less, htop). |

### `tabs`

An ordered array of tab profiles. The first entry (`tabs[0]`) is what `Ctrl+N`/`Cmd+N` opens; entries 2-9 are reachable via `Ctrl+Shift+2`..`Ctrl+Shift+9` on the Windows template (see [Default keybindings](#default-keybindings) — macOS/Linux templates currently only wire up profile 1).

```json
"tabs": [
  { "name": "shell", "icon": "", "shell": "$SHELL", "home": "~" },
  { "name": "server", "shell": "ssh myhost", "home": "~", "tmux": true }
]
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `name` | string | `""` | Tab title, and the label used in the profile picker dropdown. |
| `icon` | string | none | Glyph shown on the tab (works best with a Nerd Font — see `font.face`). |
| `shell` | string | system default shell | Command to run. Supports plain shells, `ssh <host>`, and `wsl` invocations. |
| `home` | string | none | Working directory; `~` is expanded. Must resolve to an existing directory or is ignored. |
| `tmux` | bool | `false` | Opt-in to the `som-tmux` backend for this profile — keeps the session alive across Som restarts. See [som-tmux](#som-tmux-keeping-remote-sessions-alive). Splits are not yet supported on `tmux: true` tabs. |

### `keys`

A flat map of keystroke → action name. User entries are merged on top of the platform defaults key-by-key (you only need to list the keys you want to change or add; everything else keeps its default).

```json
"keys": {
  "ctrl-shift-t": "New",
  "ctrl-alt-left": "PrevTab"
}
```

Recognized action names: `Copy`, `Paste`, `CloseTab`, `SplitTab`, `UnSplitTab`, `ClosePane`, `NextPane`, `PrevPane`, `NextTab`, `PrevTab`, `Quit`, `FontIncrease`, `FontDecrease`, `FontReset`, `New`, `New1`..`New9`. Any other string is silently ignored — no binding is created and no error is shown, so check spelling carefully against this list.

Keystroke syntax follows GPUI conventions: lowercase, hyphen-separated modifiers, e.g. `ctrl-shift-c`, `cmd-v`, `alt-f4`.

---

## Default keybindings

### Windows

| Key | Action |
|---|---|
| `Ctrl+Insert` / `Ctrl+Shift+C` | Copy |
| `Shift+Insert` / `Ctrl+V` | Paste |
| `Ctrl+N` | New tab (profile 1) |
| `Ctrl+Shift+1`..`Ctrl+Shift+9` | New tab from profile 1-9 |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Increase / decrease / reset font size (session only, not saved) |
| `Ctrl+F4` | Close active tab |
| `Ctrl+\` | Split active pane |
| `Ctrl+Shift+\` | Close active split pane |
| `Ctrl+Left` / `Ctrl+Right` | Focus previous / next split pane |
| `Ctrl+Shift+Left` / `Ctrl+Shift+Right` | Activate previous / next tab |
| `Alt+F4` | Quit |

### macOS

| Key | Action |
|---|---|
| `Cmd+C` | Copy |
| `Cmd+V` | Paste |
| `Cmd+N` / `Cmd+Shift+1` | New tab (profile 1 — both open the same profile; there is no profile-2 binding shipped) |
| `Cmd+Q` | Quit |

### Linux

| Key | Action |
|---|---|
| `Ctrl+Insert` / `Ctrl+Shift+C` | Copy |
| `Shift+Insert` / `Ctrl+V` | Paste |
| `Ctrl+N` / `Ctrl+Shift+1` | New tab (profile 1 — same caveat as macOS) |
| `Alt+F4` | Quit |

### Always active, not configurable via settings.json

| Key | Context | Action |
|---|---|---|
| `Up` / `Down` / `Tab` | any open menu/dropdown | Move selection |
| `Enter` | any open menu/dropdown | Confirm selection |
| `Escape` | any open menu/dropdown | Close menu |
| `Ctrl+=` / `Ctrl+-` | global | Font size nudges (session only) |

---

## Tabs

- All tabs live in a single row in the **title bar** — there's no separate tab-bar row below it.
- Click `+` to open the default profile (`tabs[0]`); if you've configured more than one profile, a small dropdown next to `+` lets you pick which one to open.
- The tab strip scrolls horizontally with the mouse wheel if you have more tabs than fit.
- Every "new tab" action — the `+` button, the profile dropdown, and any keybinding — goes through the same code path, so behavior is consistent regardless of how you opened it.

## Splits

- Each tab supports up to **3 levels of split panes** (main pane + 3 splits).
- `Ctrl+\` splits the currently focused pane; direction cycles right → down → right as you keep splitting.
- `Ctrl+Shift+\` closes the most recently created split and refocuses a neighboring pane.
- `Ctrl+Left`/`Ctrl+Right` cycle focus between the main pane and any live splits.
- Switching away from a tab "parks" its splits (removes them from the visible layout but remembers their sizes); switching back restores them exactly as you left them — this works per-tab, so each tab keeps its own independent split layout.
- **Splits are not yet supported on `tmux: true` tabs** — a tmux-backed tab only ever has one pane.

## Session persistence (db.json)

Som remembers your open tabs, their split layout, (for `tmux: true` tabs) their remote session IDs, and — when `window.mode` is `"windowed"` — the window's last position and size, all in `~/.config/som/db.json`. Relaunching Som puts you back where you left off, including reattaching to still-running remote shells rather than starting fresh ones.

This file is rewritten automatically whenever you open/close a tab or split, switch tabs, resize/move the window, or quit — you shouldn't need to touch it directly. If it's ever missing or corrupted, Som just falls back to a single empty tab (and default window placement) rather than failing to start.

## Restore on launch

When Som starts, it doesn't wait for slow connections before showing you the window:

- Every saved tab appears immediately as a placeholder (name and icon only), in the exact order you last had them, with the previously-active tab already selected.
- Each tab's real terminal/shell connects in the background, replacing its placeholder as soon as it's ready — a slow SSH login on one tab doesn't hold up a fast local shell on another.
- The title bar shows a small spinner in the drag zone (the empty space next to the tab strip) for as long as any tab is still restoring, connecting, or being checked for tmux redeployment — once everything settles, it disappears.
- Split panes are recreated after all tabs exist, and your previously-focused tab and pane are refocused last.

## som-tmux: keeping remote sessions alive

Setting `"tmux": true` on a tab profile routes that tab's terminal through a small companion process (`som-tmux`) instead of a plain PTY:

- **Local shells:** a detached process holds the real shell and PTY; it keeps running even if Som is closed, so reopening Som reconnects to the exact same session (scrollback, running programs, and all) instead of starting over.
- **Remote shells (`ssh`/`wsl`):** the same mechanism runs on the far end — Som deploys a small pre-built `som-tmux` binary to `~/.local/bin/` on the remote host (only re-copying it when the version is out of date) and the remote shell runs inside it. This means a flaky connection or closing Som doesn't kill your remote session; reconnecting picks the same shell back up.
- Each tmux-backed pane gets its own persistent session, identified internally by a UUID — this is transparent to you, but it's what's saved in `db.json` to make reattachment possible.
- Som periodically cleans up abandoned remote sessions (ones that no longer correspond to anything in your current tab list) on the same machine, so stale processes don't accumulate on hosts you connect to often. This is best-effort: if it fails for some reason, it's logged and ignored rather than blocking your tab from opening.
- Splits are not currently supported on tmux-backed tabs (see [Splits](#splits)).

This feature has no separate settings.json toggle beyond the per-profile `"tmux": true` — there's no global on/off switch.

## Theming

Som ships a single bundled theme, "Nord Dark" (`window.theme` / `general` default), written to `~/.config/som/themes/nord.json` on first run. The full underlying theme engine (inherited from Zed) supports arbitrary theme files with about 150 individually addressable colors, but Som doesn't currently expose a way to install additional themes other than manually placing a compatible theme JSON file in the themes directory and setting `window.theme` to its name.

---

## Known gaps between docs and behavior

These are documented here deliberately rather than silently ignored, so you don't spend time debugging a setting that simply doesn't do anything yet:

- **`font.weight` is parsed but never applied.** Present in the shipped default files but currently does nothing.
- **Only the Windows default template wires up `Ctrl+Shift+2`..`9` for additional tab profiles.** The macOS/Linux templates only bind profile 1 (bound twice, redundantly) — if you add more profiles on those platforms, you'll need to add your own `keys` entries (e.g. `"cmd-shift-2": "New2"`) to reach them.
