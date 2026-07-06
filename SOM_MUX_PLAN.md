# Som Multiplexer — Plan

## Status (2026-07-06)

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

The old "SSH Reconnect After Disconnect" design sketch that used to live in
this doc has been **superseded entirely** by `som-tmux` (below) — a real
detached PTY server that survives Som closing, rather than a reconnect-retry
layer bolted onto a plain PTY. That old section is gone; this doc now covers
what's actually being built.

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
- **Will be extended** for `som-tmux` tabs with a per-panel session-id field
  (see below) — not yet implemented.

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

## som-tmux — detached PTY server (in progress)

### Why

Even with tabs/splits/db.json restore working, a process in a pane still
dies when Som closes: restore re-spawns the *same command* on next launch
(new PID, no continuity), it doesn't reattach to a still-running one. E.g. a
`ping google.com` running in a pane does not survive closing Som via the X
button or Alt+F4 — restore just runs `ping google.com` again from scratch.

`som-tmux` fixes this properly: for tabs whose profile opts in, the actual
PTY/shell process lives in a **separate, detached server process** that
outlives Som's own window. Closing/reopening a tab reattaches to it instead
of respawning. This is deliberately *not* full tmux (no tmux panes, splits,
or keybindings are reused — Som has its own for all of that) — only the
server/session-survival part is our own, built from scratch (evaluated
reusing an existing Windows tmux port, `psmux` — not viable: no separable
server crate, no documented/stable wire protocol, would mean vendoring an
actively-changing external codebase).

### Activation

A tab's profile in `settings.json` opts in with `"tmux": true` (default
`false` — must be explicit). Applies to **all** panes in that tab (main +
up to 3 splits).

**Guiding goal for the whole feature**: maximum context restoration across
all panes on Som restart, at maximum zero-config. Both matter equally —
where they conflict (e.g. fully recovering a session after the remote
server machine itself rebooted would require root access there to
auto-restart the server), zero-config wins; that specific case falls back
to plain new-session creation instead (see health-check, below).

The `tmux` field stays in the config schema permanently — what changes over
time is its **default**, not its existence. Right now it's an explicit
opt-in (`false` by default, must write `"tmux": true`). The long-term goal
is for the default to flip to `true` — tmux behavior on for every profile
with zero config needed — while the field remains available as an explicit
**opt-out** (`"tmux": false`) for anyone who wants the plain direct-PTY path
for a specific profile. The only *automatic* (non-user-driven) fallback to
the plain path is on setups where `som-tmux-server` genuinely can't be run
at all (silent, no user action needed). So: the requirement to *write* the
setting to get tmux behavior disappears; the setting itself does not.

### Where the server actually runs — real tmux semantics, not "Windows-only helper"

This is the single most important design point, revisited and corrected
mid-implementation: **`som-tmux-server` runs on whichever machine actually
executes the shell**, not always next to Som:

- Local Windows profile (e.g. `dnk`, running `pwsh.exe`) → server runs on
  this same Windows machine. Transport: a Windows named pipe.
- WSL profile (`wsl --cd ~`) → server is a **separate Linux binary**,
  running *inside* WSL. Transport to it from Windows-side Som: **not yet
  decided** (candidates: `wsl.exe` as a transport wrapper, a TCP port
  forwarded from WSL2 to Windows localhost).
- SSH profiles (`ssh host`) → server is a binary built for the **remote**
  machine's platform, running *there*, launched over the same SSH
  connection the shell itself would have used (same idea as real tmux's
  `ssh host tmux attach`) — not a separate direct network channel by IP.
  - Being tested via a `loc` profile (`"shell": "ssh localhost"`) added
    specifically to exercise the SSH transport without needing to
    cross-compile for another OS or touch a real remote box — `ssh
    localhost` on this same Windows machine, once its OpenSSH server
    (`sshd` Windows service) is running.
  - Leading transport design (not fully locked in yet): Som spawns
    `ssh <host> som-tmux-server <profile>` as a child process and talks to
    the server directly over that child's stdin/stdout — no named pipe on
    the far side in this mode. Requires abstracting the pipe-connection
    type behind a small trait/enum (`WindowsNamedPipe` vs
    `ChildProcessStdio`, same read/write interface) and a `--stdio` launch
    mode for the server binary.
- Protocol itself (`ClientMessage`/`ServerMessage`, length-prefixed JSON)
  is transport-agnostic — none of this changes `protocol.rs`, only how its
  bytes physically move.

**Deliberate divergence from real tmux**, called out explicitly because
it's easy to "fix" this the wrong way later: real tmux runs *one server per
user per host*. Som instead runs **one server per profile**, always — even
if several profiles point at the exact same host (three profiles all SSHing
to the same Mac = three independent `som-tmux-server` processes there, not
one shared one). This avoids needing to resolve "are `mac` and
`192.168.50.6` the same host" (hostname/IP/alias equivalence is exactly the
kind of fragile check this design sidesteps) at the cost of some duplicate
server processes on hosts with multiple profiles. Chosen intentionally for
simplicity, not an oversight.

**Practical rollout order**: get the Windows-local case (profile `dnk`,
named-pipe transport) fully working end-to-end first — it already is, see
below — before tackling WSL/SSH transports, which are meaningfully
different problems (cross-compilation, remote spawn-and-connect).

### Server core — implemented and tested (`crates/som_tmux_server/`)

- Separate crate, split into a `[lib]` (transport: `pipe.rs`, `protocol.rs`
  — the only parts Som's client side needs) and a `[[bin]]` `som-tmux-server`
  (PTY/session internals: `bounds.rs`, `session.rs`, `server.rs` — private
  to the server process).
- **Grouping**: one server process per profile name. It multiplexes many
  sessions (= panes, across any number of tabs of that profile) over a
  single pipe, keyed by `session_id: Uuid` in the protocol — not one pipe
  per session.
- **Protocol** (`protocol.rs`): `ClientMessage::{NewSession, Attach, Write,
  Resize, CloseSession}`, `ServerMessage::{SessionCreated, GridUpdate,
  AttachFailed, SessionClosed, Error}`. `GridUpdate` carries a full
  plain-text grid snapshot (`grid_text: String`) sent on `Attach` and again
  on every change — not raw PTY bytes (alacritty's IO thread parses bytes
  into its own `Term` and never exposes them raw, only a "something
  changed" signal) and not a diff (first-iteration simplicity; no
  colors/attributes/cursor position carried yet — known, deliberate
  limitation).
- **Session lifecycle** (`session.rs`): each session owns a real PTY
  (`alacritty_terminal::tty`) and its own `Term`, plus a permanent internal
  "pump" thread that answers terminal escape-sequence queries
  (`PtyWrite` events, e.g. a Device Attributes request PowerShell sends on
  startup) immediately — without this, the shell sits waiting for a reply
  that never comes and produces no further output (found the hard way: an
  empty 1920-blank-cell grid).
- **Detach vs. kill semantics**:
  - Detach = the connection drops but the session keeps running. Happens
    passively whenever Som just exits/crashes (no command sent, nothing to
    do differently).
  - Closing a tab/pane through the UI sends an explicit
    `CloseSession(session_id)` — this really kills the PTY process, it's
    not a "leave it running" detach.
  - When a server's last session is closed this way, the server exits
    itself (`std::process::exit(0)`).
  - On next launch, Som attempts `Attach` for any `tmux:true` tab's saved
    session id; if the server isn't there/doesn't know it (didn't survive,
    e.g. a reboot), falls back to creating a fresh session.
- **IPC transport (Windows-local case)**: `\\.\pipe\som-tmux-<profile>`,
  using **overlapped I/O** (`FILE_FLAG_OVERLAPPED`), not synchronous pipe
  calls. This was a real, subtle bug: a synchronous (non-overlapped) named
  pipe handle serializes *all* I/O through the OS — a blocking `ReadFile`
  pending on one thread will block a concurrent `WriteFile` from another
  thread on the *same handle* until the read completes. Since the server
  needs one thread reading incoming client messages while another streams
  `GridUpdate`s out on the same connection, this deadlocked reliably after
  the first update. Confirmed as documented Windows behavior (Microsoft
  Learn: "Synchronous and Overlapped Pipe I/O"), fixed by giving every
  read/write its own `OVERLAPPED` + manual-reset event.
- **Verified end-to-end** via a manual PowerShell test client
  (`test_client.ps1`, kept in the repo root for further manual testing):
  spawn a session, attach, type an interactive command, see live streamed
  grid updates land, see the echoed output actually appear, explicitly
  close the session, confirm the server process exits on its own.

### Not yet done

1. **UI rendering component.** Decided: a **new, separate view component**
   for `tmux:true` panes rather than patching the existing `TerminalView` —
   `TerminalView` is tightly coupled to a live local `alacritty_terminal::Term`
   in nearly every method (render, scrolling, hover, resize all read/write
   `self.terminal`), and tmux panes have fundamentally different data (a
   plain-text grid snapshot arriving over IPC, no colors/attributes yet).
   Patching would mean threading an `if tmux {...} else {...}` branch
   through ~90 methods; a new component keeps the existing, already-solid
   `tmux:false` path completely untouched while this is being built out.
   Since the long-term direction may make `tmux:true` the *only* mode
   someday (except where the server can't run), the new component should be
   designed as a real, full replacement candidate (not a throwaway shim) —
   just only wired up for `tmux:true` panes for now.
2. `som_config::TabProfile` / `workspace::TabProfile` — add `tmux: bool`
   (default false).
3. `db.json` schema — add a per-tab session-id list for `tmux:true` tabs.
4. Client-side connect-or-spawn logic (detached process creation on
   Windows, `PipeConnection::connect` retry loop) — not yet written.
5. Wire the new view component into tab creation / close / restore paths.
6. WSL and SSH transports (see above) — separate follow-up work, not
   started; `sshd` (Windows OpenSSH Server) needs to be running locally to
   test the `loc` (`ssh localhost`) profile once SSH transport exists.
7. **Per-pane health-check.** Each `tmux:true` pane needs to detect a
   dropped connection — deliberately kept simple, on the user's own
   correction mid-design: don't try to distinguish *why* the channel died
   (client-side network blip vs. the server's own machine rebooting) and
   don't try to build a special "resurrect the exact same session after the
   server host reboots" mechanism. Instead: on a detected drop, retry
   `Attach` to the same `session_id` (cheap, handles the common
   "network blipped, server's still there" case transparently); if that
   doesn't succeed after some retries, just fall back to creating a **brand
   new session**, via the exact same attach-or-create path restore already
   uses when a server doesn't respond on launch. No new third state, no
   separate server-side recovery mechanism — health-check is this same
   fallback path, just triggered live during a session instead of only at
   Som startup.
   **Why no server-side auto-recovery**: making `som-tmux-server`
   auto-start itself after the remote machine reboots (e.g. a systemd unit
   on Linux/a router, launchd on Mac) and re-register old sessions would
   need root/admin access on that remote machine to set up the OS-level
   autostart. That's an unacceptable requirement on the user's environment
   (ARM routers often don't grant root at all; personal servers — the user
   may not want to hand an app that kind of access). This isn't a "simplify
   now, revisit later" shortcut — it's a firm constraint: som-tmux must
   never require root on the remote side, so it will never try to
   self-restart/recover itself at the OS level. Falling back to a plain new
   session is the only approach compatible with that.
   Not designed yet — open questions: detection mechanism
   (heartbeat/ping-pong in the protocol vs. a timeout on missing
   `GridUpdate`s), retry count/backoff before falling back to a new session,
   what the new view component (item 1) shows while a reconnect attempt is
   in flight. This subsumes what used to be a separate "SSH Reconnect After
   Disconnect" idea in an earlier version of this doc — folded into
   som-tmux's health-check rather than a standalone reconnect layer bolted
   onto a plain PTY.

Full blow-by-blow implementation log, exact file states, and the discovery
process for each bug above lives in this session's `project_som_tmux`
memory note (not duplicated here) — this doc is the durable summary, that
memory is the working log.

---

## Open Questions (carried over, still unresolved)

1. **Pane resize:** fixed splits, or draggable dividers? Currently fixed
   equal-flex on restart; in-session resize is remembered per tab
   (`som_tab_flexes`) but there's no drag-to-resize UI yet.
2. **Tab reorder:** drag tabs in title bar? Not implemented.
3. **Shell hook for cwd:** inject `$PROMPT_COMMAND`/`precmd` hook, or poll
   the process's cwd, for terminals where `is_remote_terminal` currently
   just gives up on cwd tracking?
