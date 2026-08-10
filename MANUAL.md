# Som Manual

This document describes every user-facing feature of Som and every option in `~/.config/som/settings.json`. It reflects the actual behavior of the code as of this writing.

## Table of contents

1. [Where things live](#where-things-live)
2. [settings.json reference](#settingsjson-reference)
3. [Default keybindings](#default-keybindings)
4. [Tabs](#tabs)
5. [Splits](#splits)
6. [Session persistence (db.json)](#session-persistence-dbjson)
7. [Restore on launch](#restore-on-launch)
8. [som-tmux: keeping remote sessions alive](#som-tmux-keeping-remote-sessions-alive)
9. [Kitty Graphics Protocol: images in the terminal](#kitty-graphics-protocol-images-in-the-terminal)
10. [Theming](#theming)

---

## Where things live

| Path | Purpose |
|---|---|
| `~/.config/som/settings.json` | Your configuration. Created from a platform-specific template on first launch. Watched and live-reloaded — edits apply immediately, with an in-app error banner if the JSON is invalid. |
| `~/.config/som/themes/nord.json` | The bundled "Nord" theme, written out on first launch. |
| `~/.config/som/db.json` | Tab/split/session layout, rewritten on every tab/split change and on quit. Not something you should hand-edit; described below for troubleshooting. |
| `~/.config/som/tmux/{windows-amd,macos-arm,linux-amd}/som-tmux` | Pre-built `som-tmux` binaries used to deploy to remote hosts for SSH sessions with `"tmux": true` — auto-extracted from Som's own bundled copies on first use, not something you need to provide. |

---

## settings.json reference

All fields are optional; anything you omit falls back to the shipped default for your platform. Unknown keys are silently ignored (no error), so a typo in a key name will not warn you — double-check spelling against this table.

Two behaviors are fixed and not configurable: the terminal bell is always silent (no setting to turn on a system alert sound), and copy-on-select (auto-copying your selection to the clipboard) is always on.

### `window`

| Key | Type | Effect |
|---|---|---|
| `window.theme` | string | Active theme name (e.g. `"Nord Dark"`). Applied correctly. |
| `window.mode` | `"windowed"` (default) / `"maximized"` / `"minimized"` / `"fullscreen"` | Initial window placement, applied on every launch (not just first run). `"windowed"` remembers its position/size in `db.json` (see [Session persistence](#session-persistence-dbjson)) — moving or resizing the window updates that automatically. If no geometry has been remembered yet, it defaults to the display size minus 100px in each dimension, positioned 50px from the top-left. `window.position`/`window.size` (below) can override this — see their own row. |
| `window.position.{top,left}` / `window.size.{width,height}` | number, physical pixels | Explicit startup position/size, used only when `window.mode` is `"windowed"`. Takes effect **only if both `position` and `size` are set and every one of their four fields is non-zero** — if even one is missing or `0`, this is ignored entirely and `db.json`'s remembered geometry is used instead (exactly as if neither were set). Not remembered/updated afterward — moving or resizing the window still only updates `db.json`, not these settings.json values, so they'll re-apply the same fixed geometry on every subsequent launch as long as they stay set. |
| `window.padding.{top,bottom,left,right}` | number, pixels | Inset between the OS window edge and the terminal/split area, on that side. The title bar and tab strip always stay flush against the window edge regardless of this setting. `0` (default) means no inset. |
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
| `font.weight` | integer, 100-900 | Terminal AND UI buffer font weight (400 = normal, 700 = bold). Out-of-range values are clamped. |
| `font.lineHeight` | number | Terminal line height multiplier. |
| `font.features` | object (OpenType feature tag → `true`/`false` or a non-negative integer) | OpenType font features — see [Ligatures and font features](#ligatures-and-font-features) below. |

#### Ligatures and font features

`font.features` maps a 4-character OpenType feature tag to either a boolean (`true`/`false`, enable/disable) or an integer (for features with multiple variants, like stylistic sets):

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

An ordered array of tab profiles. The bare `NewTab` action (`Ctrl+Shift+=`/`Cmd+Shift+=` in the shipped defaults) opens the *default* profile: the one entry with `"default": true`, or `tabs[0]` if none is marked. Entries 2-9 are reachable via `Ctrl+Shift+2`..`Ctrl+Shift+9` on the Windows template regardless of which one is default (see [Default keybindings](#default-keybindings) — macOS/Linux templates currently only wire up profile 1).

```json
"tabs": [
  { "name": "shell", "icon": "", "shell": "$SHELL", "home": "~" },
  { "name": "server", "shell": "ssh myhost", "home": "~", "tmux": true, "default": true }
]
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `name` | string | `""` | Tab title, and the label used in the profile picker dropdown. |
| `icon` | string | none | Glyph shown on the tab (works best with a Nerd Font — see `font.face`). |
| `shell` | string | system default shell | Command to run. Supports plain shells, `ssh <host>`, and `wsl` invocations. |
| `home` | string | none | Working directory; `~` is expanded. Must resolve to an existing directory or is ignored. |
| `tmux` | bool | `false` | Opt-in to the `som-tmux` backend for this profile — keeps the session alive across Som restarts. See [som-tmux](#som-tmux-keeping-remote-sessions-alive). |
| `default` | bool | `false` | Marks this profile as the one the `+` button/bare `NewTab` action opens, instead of `tabs[0]`. At most one profile may set this — `settings.json` fails to parse (falls back to built-in defaults, with an "Invalid settings.json" banner) if more than one does. |

### `keys`

A flat map of keystroke → action name. User entries are merged on top of the platform defaults key-by-key (you only need to list the keys you want to change or add; everything else keeps its default).

```json
"keys": {
  "ctrl-shift-=": "NewTab",
  "ctrl-shift--": "CloseTab",
  "ctrl-shift-left": "PrevTab"
}
```

Recognized action names: `Copy`, `Paste`, `CloseTab`, `NewPane`, `ClosePane`, `NextPane`, `PrevPane`, `NextTab`, `PrevTab`, `Quit`, `IncreaseFont`, `DecreaseFont`, `ResetFont`, `NewTab`, `NewTab1`..`NewTab10`. Any other string is silently ignored — no binding is created and no error is shown, so check spelling carefully against this list.

`NewTab1`..`NewTab10` open `tabs[0]`..`tabs[9]` by position — if settings.json has fewer profiles than that (e.g. `NewTab9` with only 5 `tabs[]` entries), pressing that key shows a "No profile #9…" notification instead of silently opening a different profile. Bare `NewTab` can't hit this: it always resolves to the default profile (see `tabs`' `default` field above), which is guaranteed to exist.

Naming convention: Som-specific tab/pane actions (`NewTab*`, `CloseTab`, `NewPane`, `ClosePane`, `PrevPane`/`NextPane`, `PrevTab`/`NextTab`) are meant to live on `Ctrl+Shift+*` combinations in the shipped defaults, keeping them clear of `Copy`/`Paste`/`Quit`/font-size shortcuts, which follow standard, non-Shift system conventions instead.

Keystroke syntax follows GPUI conventions: lowercase, hyphen-separated modifiers, e.g. `ctrl-shift-c`, `cmd-v`, `alt-f4`.

---

## Default keybindings

### Windows

| Key | Action |
|---|---|
| `Ctrl+Insert` / `Ctrl+Shift+C` | Copy |
| `Shift+Insert` / `Ctrl+V` | Paste |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Increase / decrease / reset font size (session only, not saved) |
| `Ctrl+Scroll` | Zoom font size in/out (session only, not saved) |
| `Ctrl+Shift+=` | New tab from the default profile (`default: true`, or profile 1 if none is marked) |
| `Ctrl+Shift+1`..`Ctrl+Shift+9` / `Ctrl+Shift+0` | New tab from profile 1-9 / profile 10 |
| `Ctrl+Shift+-` | Close active tab |
| `Ctrl+Shift+\` | Split active pane (new pane) |
| `Ctrl+Shift+Backspace` | Close active split pane |
| `Ctrl+Shift+Up` / `Ctrl+Shift+Down` | Focus previous / next split pane |
| `Ctrl+Shift+Left` / `Ctrl+Shift+Right` | Activate previous / next tab |
| `Alt+F4` | Quit |

### macOS

| Key | Action |
|---|---|
| `Cmd+C` | Copy |
| `Cmd+V` | Paste |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Increase / decrease / reset font size (session only, not saved) |
| `Cmd+Scroll` | Zoom font size in/out (session only, not saved) |
| `Cmd+Shift+=` | New tab from the default profile (`default: true`, or profile 1 if none is marked) |
| `Cmd+Shift+1`..`Cmd+Shift+9` / `Cmd+Shift+0` | New tab from profile 1-9 / profile 10 |
| `Cmd+Shift+-` | Close active tab |
| `Cmd+Shift+\` | Split active pane (new pane) |
| `Cmd+Shift+Backspace` | Close active split pane |
| `Cmd+Shift+Up` / `Cmd+Shift+Down` | Focus previous / next split pane |
| `Cmd+Shift+Left` / `Cmd+Shift+Right` | Activate previous / next tab |
| `Cmd+Q` | Quit |

### Linux

| Key | Action |
|---|---|
| `Ctrl+Insert` / `Ctrl+Shift+C` | Copy |
| `Shift+Insert` / `Ctrl+V` | Paste |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Increase / decrease / reset font size (session only, not saved) |
| `Ctrl+Scroll` | Zoom font size in/out (session only, not saved) |
| `Ctrl+Shift+=` | New tab from the default profile (`default: true`, or profile 1 if none is marked) |
| `Ctrl+Shift+1`..`Ctrl+Shift+9` / `Ctrl+Shift+0` | New tab from profile 1-9 / profile 10 |
| `Ctrl+Shift+-` | Close active tab |
| `Ctrl+Shift+\` | Split active pane (new pane) |
| `Ctrl+Shift+Backspace` | Close active split pane |
| `Ctrl+Shift+Up` / `Ctrl+Shift+Down` | Focus previous / next split pane |
| `Ctrl+Shift+Left` / `Ctrl+Shift+Right` | Activate previous / next tab |
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
- `Ctrl+Shift+\` splits the currently focused pane; direction cycles right → down → right as you keep splitting.
- `Ctrl+Shift+Backspace` closes the most recently created split and refocuses a neighboring pane.
- `Ctrl+Shift+Up`/`Ctrl+Shift+Down` cycle focus between the main pane and any live splits.
- Switching away from a tab "parks" its splits (removes them from the visible layout but remembers their sizes); switching back restores them exactly as you left them — this works per-tab, so each tab keeps its own independent split layout.
- Works on `tmux: true` tabs too — each split pane gets its own independent persistent session.

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
- **Remote shells (`ssh`/`wsl`):** the same mechanism runs on the far end — Som deploys a small pre-built `som-tmux` binary to `~/.local/bin/` on the remote host (only re-copying it when the version is out of date) and the remote shell runs inside it. This means a flaky connection or closing Som doesn't kill your remote session; reconnecting picks the same shell back up. The pre-built binary itself comes bundled inside Som (Windows/macOS/Linux amd64 — Linux arm64 isn't built yet, and a `tmux: true` profile pointed at one falls back to a plain, non-persistent connection with a notification rather than failing to open).
- Each tmux-backed pane gets its own persistent session, identified internally by a UUID — this is transparent to you, but it's what's saved in `db.json` to make reattachment possible.
- Som periodically cleans up abandoned remote sessions (ones that no longer correspond to anything in your current tab list) on the same machine, so stale processes don't accumulate on hosts you connect to often. This is best-effort: if it fails for some reason, it's logged and ignored rather than blocking your tab from opening.
- Splitting a tmux-backed tab works the same as any other tab — each new split pane gets its own independent persistent session (its own pane_id), not a shared one.

This feature has no separate settings.json toggle beyond the per-profile `"tmux": true` — there's no global on/off switch.

## Kitty Graphics Protocol: images in the terminal

Som implements the [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), so terminal apps that support it — `yazi` being the main one in practice — render actual images inline instead of falling back to block-character/Sixel approximations. There's no settings.json toggle for this: it's always on, and Som advertises support the way clients already expect (including setting `KITTY_WINDOW_ID` in the shell environment so clients that detect terminals by environment variable, like yazi, pick the Kitty driver instead of probing).

- **Both transmission formats are supported**: PNG, and raw RGB/RGBA pixel streams (the format yazi actually sends), including chunked transfers of multi-megabyte images spread across many PTY reads.
- **Both placement modes are supported**: the classic cursor-relative placement, and the newer Unicode-placeholder mode (`U=1`) that yazi uses, where the image is anchored to placeholder glyphs in the grid rather than moved with `a=p` commands.
- **Images keep their aspect ratio.** The protocol only gives Som a cell-grid bounding box (rounded up from the sender's pixel size divided by cell size, so it rarely matches the image's exact proportions) — Som fits the image inside that box rather than stretching it to fill it.
- **Images survive SSH reconnects on `tmux: true` tabs.** Reattaching to a `som-tmux` session that had images on screen redraws them along with the rest of the scrollback, without a manual refresh.
- **Rendering goes through the same GPU pipeline as text** — no overlay window, no external helper process. Images are decoded once and painted like any other GPUI content.

Known open issue: navigating file lists with arrow keys in `yazi` can leave a preview un-rendered until the next keypress (clicking with the mouse doesn't have this problem). Being tracked, not yet fixed.

**Windows setup note:** `yazi` needs the `file` command to detect a file's MIME type before it'll even attempt to preview it as an image — without it you'll see "Cannot find `file` to detect the file's MIME type" instead of a preview. Windows has no built-in `file`, and `yazi` doesn't search `PATH` for it there; install [Git for Windows](https://git-scm.com/download/win) (which bundles `file.exe`) and set a `YAZI_FILE_ONE` environment variable pointing at it, e.g. `C:\Program Files\Git\usr\bin\file.exe`. Restart Som (or any terminal that reads user environment variables at launch) after setting it.

## Theming

Som ships a single bundled theme, "Nord Dark" (`window.theme` / `general` default), written to `~/.config/som/themes/nord.json` on first run. The underlying theme engine supports arbitrary theme files with about 150 individually addressable colors, but Som doesn't currently expose a way to install additional themes other than manually placing a compatible theme JSON file in the themes directory and setting `window.theme` to its name.

Since Som's theme format is the same one Zed itself uses, any Zed theme JSON file works as-is. A few popular ones:

- [Catppuccin](https://github.com/catppuccin/zed/blob/main/themes/catppuccin-mauve.json)
- [Tokyo Night](https://github.com/ssaunderss/zed-tokyo-night/blob/main/themes/tokyo-night.json)
- [One Dark Pro](https://github.com/MordFustang21/zed-one-dark-pro/blob/main/themes/one-dark.json)
- [Dracula](https://github.com/dracula/zed/blob/main/themes/dracula.json)

To install one: download the raw JSON file into `~/.config/som/themes/`, then set `window.theme` in settings.json to the theme's `"name"` field from inside that file (open the file and check — it isn't always the same as the filename, and some files define multiple named variants).
