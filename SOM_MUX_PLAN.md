# Som Multiplexer — Plan

## Status (2026-07-04)

The original version of this plan proposed a separate `crates/som_mux/`
crate with its own `SomLayout` enum (`Single/SplitH/SplitV/SplitH3/Quad`).
That was never built. What actually shipped instead — directly inside
`crates/workspace/src/workspace.rs` and `crates/terminal_view/src/terminal_panel.rs`
— is simpler and already working:

- Tabs are plain `Workspace` main-pane items (no separate tab data structure).
- Each tab can have up to **3 extra split panes** (4 panes total: main + 3),
  tracked in `Workspace::som_split_panes: Vec<WeakEntity<Pane>>` and
  `som_level_locked: [bool; 3]`. Split directions are fixed:
  `[Right, Down, Right]` (level 0 splits right, level 1 splits that pane down,
  level 2 splits that pane right again) — not a freeform grid.
- Switching tabs parks the current tab's splits (`som_parked_splits`) and
  unparks the target tab's, rather than destroying/recreating panes.
- Persistence is a flat `~/.config/som/db.json`, not a `session.json` with
  the shape this doc originally proposed. See "Persistence" below for the
  real schema.
- `TabProfile` (`crates/workspace/src/workspace.rs:443`) has 5 fields:
  `name`, `shell: Option<String>`, `keystroke`, `icon`, `working_dir`.

This doc is kept as a reference for what's implemented and what's still
open — not as a build plan for a crate that doesn't exist.

---

## Core Concepts

| tmux term | Som term | Description |
|-----------|----------|-------------|
| session   | —        | always one (the app itself) |
| window    | tab      | a main-pane item; has 0-3 split panes |
| pane      | pane     | one terminal process inside a tab |

**Rules (as implemented):**
- All tabs share one tab bar (in title bar)
- Switching tabs switches the entire pane layout at once (park/unpark)
- Closing a tab closes all its panes
- Each pane is an independent PTY process (same shell profile as the tab)
- Up to 4 panes per tab (main + 3 splits, fixed Right/Down/Right layout)
- State (tab order, profile, split count, active tab/pane) saved on every
  change and restored on next launch — split *sizes* (flex) are NOT
  persisted across restarts by design (always start equal); they ARE
  remembered across in-session tab switches (`som_tab_flexes`)

---

## Persistence — `~/.config/som/db.json`

```json
{
  "tabs": ["0.0", "6.3", "3.1"],
  "active": "1.2"
}
```

- Each tab string is `"<profile_index>.<extra_splits>"` — `profile_index`
  indexes into `settings.json`'s `tabs[]` array, `extra_splits` is 0-3.
- Array position = tab order in the tab bar.
- `active` is `"<tab_index>.<pane_index>"` — `pane_index` 0 = main pane,
  1-3 = a split level.
- Missing/corrupt file → default `{"tabs": ["0.0"], "active": "0.0"}`.
- Implemented in `crates/workspace/src/som_db.rs`
  (`load_som_db`/`save_som_db`/`SomDbState`).
- Restore logic: `TerminalPanel::restore_som_tabs`
  (`crates/terminal_view/src/terminal_panel.rs`), invoked via the
  `SomTabsRestorer` global hook so `workspace` doesn't need a dependency on
  `terminal_view`.
  - Tabs' terminals are created **concurrently** (a slow ssh login doesn't
    block a fast local shell from appearing), then explicitly reordered to
    match `db.json`'s order via `Pane::reorder_item_to` (connection speed,
    not array order, determines completion order) — see the "fixed bugs"
    note below.
  - Splits are created **sequentially**, one tab at a time — this is
    deliberate; concurrent split creation was the original source of the
    ssh-MOTD duplication bug (see below).
  - The saved active split pane is restored via
    `Workspace::som_focus_pane_by_index`.

### Bugs fixed during db.json rollout (2026-07-02), kept here for context
- **Tab order / wrong split content after restore**: fixed by matching
  tabs by `EntityId` instead of assuming pane-array position matches
  creation order, then resyncing `som_active_tab_index`
  (`Workspace::som_resync_active_tab_index`) since reordering moves the
  active item without emitting `ActivateItem`.
- **SSH MOTD duplication**: root cause was `PaneGroup::split` resizing
  *all* sibling panes in a tab (not just the new one) whenever a split is
  added — including panes still mid-login, which then got a real SIGWINCH
  and the remote shell repainted its banner. Fixed with a short
  (`RESIZE_GRACE_PERIOD`, 1.5s) post-creation window during which a
  terminal's PTY-side resize forwarding is suppressed
  (`crates/terminal/src/terminal.rs`, `Terminal::created_at`).

---

## Keybindings (as implemented — `crates/zed/src/som_config.rs` +
`assets/windows.json`)

| Key | Action | Notes |
|-----|--------|-------|
| ctrl-\\ | SomSplitPane | split active pane (up to 3 splits) |
| ctrl-shift-\\ | SomClosePane | close active split pane, refocus |
| ctrl-f4 | SomCloseTab | close tab + all its panes |
| ctrl-n | NewTerminal | new tab, default profile |
| ctrl-shift-1..9 | NewTerminal(profile N) | new tab, specific profile |
| ctrl-shift-right | SomActivateNextTab | next tab |
| ctrl-shift-left | SomActivatePrevTab | previous tab |
| ctrl-right | SomActivateNextPane | next split pane |
| ctrl-left | SomActivatePrevPane | previous split pane |

`SomUnsplitPane` exists as an action (used from a UI click handler) but has
no default keybinding.

---

## SSH Reconnect After Disconnect (new — not yet started)

### Current state: greenfield, nothing to build on

Investigated 2026-07-04. Som has **no SSH-awareness at all** today:

- A tab's `shell` field (`TabProfile::shell`) is an opaque string, e.g.
  `"ssh user@host"`. It gets naively split into program+args
  (`crates/project/src/terminals.rs`) and handed to the PTY as a plain
  child process — same as running any other command. There is no
  `RemoteClient`/`SshSession`/connection object, and no `crates/remote` or
  `crates/remote_server` in this fork.
- The only SSH-adjacent flag is `Terminal::is_remote_terminal`
  (`crates/terminal/src/terminal.rs`) — purely cosmetic, it just disables
  cwd tracking/persistence ("can't introspect a remote shell's cwd"). No
  connection state.
- Process exit IS already detected (`completion_tx`/`completion_rx`,
  `child_exited`, `register_task_finished` in `terminal.rs`) — but for any
  interactive shell (SSH included), exit unconditionally emits
  `Event::CloseTerminal` and the pane just closes. Nothing distinguishes
  "user typed `exit`" from "network dropped the connection."
- No restart/respawn/reconnect action exists anywhere in the codebase.
- The closest reusable building block: `Terminal::clone_builder`
  (`terminal.rs`) already knows how to rebuild a `TerminalBuilder` from a
  terminal's own shell/env/cwd template — this is what split-pane cloning
  uses today, and reconnect would reuse the same idea (rebuild with the
  same shell command against the same pane, instead of a new pane).

### Design sketch

1. **Mark a pane as "reconnectable."** Need a way to know a given
   `Terminal`/`TerminalView` is an SSH session worth reconnecting, as
   opposed to a normal shell that exited on purpose. Simplest approach:
   treat any tab/pane whose `shell` command matches `ssh ...` (or a new
   explicit `is_ssh: bool` on `TabProfile`, safer than string-sniffing) as
   reconnectable, rather than trying to detect this after the fact.

2. **Distinguish disconnect from intentional exit.** `register_task_finished`
   currently only looks at exit code / whether the user typed input. For
   SSH specifically, a non-zero exit shortly after having been connected
   (or an `ExitStatus` matching typical ssh connection-drop codes) should
   be treated as "disconnected," not "closed." This needs its own branch
   in the exit-handling path, gated on the pane being marked SSH per (1).

3. **Don't close the pane on disconnect — show a reconnect state.** Instead
   of emitting `Event::CloseTerminal`, keep the pane alive and render an
   inline "Connection lost — reconnecting…" / "Connection lost — press R
   to retry" state in place of the dead terminal content. This is new UI,
   not present anywhere in `TerminalView` today.

4. **Reconnect action.** Rebuild the terminal in place using the same
   approach as `clone_builder`/`clone_terminal` (same shell command, same
   working directory if known), replacing the dead terminal's content
   without tearing down the pane itself (so split layout, focus, and tab
   position are undisturbed). Exposed as:
   - Automatic retry with backoff (e.g. 1s, 2s, 5s, then give up and show
     a manual-retry prompt), AND
   - A manual action/keybinding (e.g. `ctrl-shift-r` while a dead pane is
     focused) to force an immediate retry.

5. **Don't reconnect-loop forever.** Cap automatic retries (e.g. 5
   attempts) before falling back to a manual-only "press R to retry"
   state, so a genuinely-down host doesn't spin forever or spam
   connection attempts.

6. **Session survival across app restart.** Since `db.json` already saves
   which tabs/splits had an SSH profile, a pane that was mid-disconnect at
   quit time should just attempt a fresh connection on next launch (same
   as any other restored tab) rather than trying to persist "was
   disconnected" state — simpler, and consistent with how restore already
   works for everything else.

### Open questions
1. Should SSH-awareness be an explicit `TabProfile` field (`is_ssh: bool`)
   set from settings.json, or inferred from the `shell` string starting
   with `ssh`/`ssh.exe`? Explicit is more robust (works with `autossh`,
   wrapper scripts, `Plink`, etc. too) but requires a settings schema
   change; inferred is zero-config but fragile.
2. What counts as "disconnected" vs "exited on purpose"? Exit code alone
   is unreliable (a remote `exit` command and a dropped connection can
   both produce non-zero codes depending on the shell/ssh client). May
   need to watch for specific stderr patterns (`ssh` prints
   "Connection to X closed" / "Connection reset by peer" / "Broken pipe"
   on unexpected drops) rather than relying on exit code alone.
3. Does reconnecting need to restore scrollback/session state, or is a
   fresh login (new MOTD, new shell) acceptable? (Given the MOTD-duplicate
   bug just fixed, a fresh reconnect banner is expected — just needs to
   render once, not duplicated.)
4. Backoff/retry-limit values — needs real-world tuning, not a guess.

---

## Open Questions (carried over, still unresolved)

1. **Pane resize:** fixed splits, or draggable dividers? Currently fixed
   equal-flex on restart; in-session resize is remembered per tab
   (`som_tab_flexes`) but there's no drag-to-resize UI yet.
2. **Tab reorder:** drag tabs in title bar? Not implemented.
3. **Shell hook for cwd:** inject `$PROMPT_COMMAND`/`precmd` hook, or poll
   the process's cwd, for terminals where `is_remote_terminal` currently
   just gives up on cwd tracking?
