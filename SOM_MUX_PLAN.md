# Som Multiplexer — Plan

## Status (2026-07-09)

Tabs/splits/db.json restore (the "Core Concepts" / "Persistence" /
"Keybindings" sections below) are unchanged and already shipped — see those
sections for the real, working design.

**`som-tmux` is on its THIRD architecture.** Two previous designs were
built, tested against real terminal apps, and abandoned for concrete,
confirmed reasons:

1. **v1 — from-scratch ANSI-diff proxy** (HOLDER owns a real
   `alacritty_terminal::Term`, `redraw.rs` diffs it against a
   last-painted-cell cache and replays minimal ANSI to the RELAY,
   reimplementing tmux's own `tty.c` model from scratch). Abandoned:
   diffing visible grid content structurally cannot carry anything that
   ISN'T content — terminal modes (DECCKM), Device Attributes queries,
   etc. — so real full-screen apps kept surfacing new categories of bugs.
2. **v2 — thin wrapper around a real, installed `tmux` binary** (RELAY
   execs `tmux attach-session`, HOLDER just ensures a detached tmux
   session exists). Abandoned: confirmed via direct testing that this
   architecture has TWO terminal emulators in the round-trip (tmux's own,
   plus Som's own `alacritty_terminal::Term` upstream of it) — both
   answering mode/identification queries independently — which corrupted
   plain typed text with garbled escape-sequence fragments, not just
   htop/F2. This is a KNOWN, already-documented failure mode: see
   <https://github.com/zed-industries/zed/discussions/50584>, where a
   Zed contributor (`@dsturnbull`) explicitly rejected embedding tmux for
   exactly this reason ("double terminal emulation ... incompatibilities
   with less common escape sequences").
3. **v3 (current) — headless `alacritty_terminal::Term` + serde
   snapshot/restore**, see below. This is what that same `dsturnbull`
   discussion/prototype landed on instead of tmux, and it maps closely
   onto our OWN v1 design's actual shape (a HOLDER-side `Term` tracking
   real state) — just fixing v1's actual flaw (replaying only visible
   content) by sending the terminal's full, already-parsed STATE instead
   of diffing bytes. Sources:
   - Discussion/RFC: <https://github.com/zed-industries/zed/discussions/50584>
   - Prototype daemon (Apache-2.0): <https://github.com/dsturnbull/pty-host>
   - `alacritty_terminal` fork adding `Term::snapshot()`/`Term::restore()`:
     <https://github.com/dsturnbull/alacritty> (the specific, isolated
     commit this borrows from is `36fd512f` — see "Where snapshot/restore
     comes from" below; the rest of that fork's history, an unrelated
     zstd scrollback-compression feature, is NOT used here).

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
- Per-pane tmux session tracking: `Workspace::som_tab_tmux_sessions:
  HashMap<EntityId, Vec<String>>`, one `pane_id` (a UUID Som itself
  generates, used as the real tmux session name — see below) per pane in
  that tab. Populated via `set_tmux_sessions_for_item`, persisted through
  `som_persist_tab_state`/`som_persist_db_json`.

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

## som-tmux v3 — headless `alacritty_terminal::Term` + serde snapshot/restore (in progress, 2026-07-09)

### Where snapshot/restore comes from

Borrowed from `dsturnbull/alacritty`'s isolated commit `36fd512f`
(<https://github.com/dsturnbull/alacritty/commit/36fd512f>), NOT the rest
of that fork's history — the fork also contains ~9 unrelated commits
adding zstd-based scrollback compression (a real feature, but a separate
concern this project doesn't need). `36fd512f` alone is 420 lines: a new
`alacritty_terminal/src/term/serialize.rs` (345 lines, self-contained) plus
small additions to `term/mod.rs` (+35: `Term::snapshot()`/`Term::restore()`)
and `grid/mod.rs` (+4/-2: makes `Cursor<T>` serde-serializable, which was
previously explicitly skipped).

Confirmed compatible with our fork before committing to this: our
`alacritty_terminal` pin (`zed-industries/alacritty` at `fcf32feacb367b`)
is 37 commits AHEAD of `dsturnbull`'s fork's base (their fork branched
earlier), but `grid/mod.rs`'s existing `#[cfg(feature = "serde")]`
infrastructure is IDENTICAL in both — same lines, same `serde(skip)`
pattern the patch modifies — so this is expected to cherry-pick cleanly
rather than needing a from-scratch reimplementation.

`TermState` (the snapshot struct) captures: both grid buffers (active +
inactive, i.e. covers alt-screen), cursor position/template, and —
critically, this is the part that actually fixes v1's flaw — **the full
`TermMode` bitflags** (`mode_bits: u32`, not a hand-picked subset like
v1's `ALT_SCREEN`-only-then-`APP_CURSOR`-added-later approach). Restoring
`mode_bits` directly onto the client's own `Term` means DECCKM, Device
Attributes negotiation state, and anything else `TermMode` tracks are ALL
correct after every reconnect, by construction — not "correct for the
specific modes someone remembered to add tracking for."

### Architecture

- **HOLDER** (long-lived, detached, survives Som restart — same role as
  v1/v2's HOLDER):
  1. Owns a REAL PTY + the real shell process (same
     `alacritty_terminal::tty` usage v1's `session.rs` already had — this
     part of v1 was never the problem, only `redraw.rs`'s diffing was).
  2. ALSO owns a headless `Term<VoidListener>` (mirrors v1's `Session`
     having a `Term`, and mirrors `pty-host`'s `headless.rs`) that
     consumes every byte read from the real PTY through the normal
     `ansi::Processor::advance` path — so the HOLDER's own `Term` is
     always a fully accurate, live mirror of the real shell's terminal
     state, same as v1.
  3. On a RELAY (re)connecting: calls `term.snapshot()`, serializes with
     bincode, sends it as ONE message — not a stream of ANSI bytes trying
     to reproduce the screen, and not tmux's own control-mode protocol.
  4. After the initial snapshot, forwards live PTY output to the RELAY
     as an "apply these bytes to your own `Term`" message — same shape
     as v1's incremental updates, but the RELAY applies them by feeding
     its OWN local `Term` (see below), not by interpreting an already-
     diffed byte stream `redraw.rs` produced.
- **RELAY** (short-lived, Som's own PTY child, same role as v1/v2)
  — **CORRECTED mid-implementation, see below**: RELAY is a SEPARATE OS
  process from Som, communicating only through its own stdout (the ConPTY
  Som created for it) — unlike `pty-host`'s own client, which IS the
  editor's terminal in-process and can call `term.restore()` directly on
  the exact `Term` the UI renders. RELAY has no such access; the only way
  it can affect what Som's own `Terminal`/`TerminalElement` shows is by
  writing bytes to its stdout for Som's own `ansi::Processor` to parse,
  same as v1/v2 always required.
  1. Connects to (or spawns) the HOLDER, same as v1.
  2. On the initial snapshot message: `bincode::deserialize::<TermState>`,
     `term.restore(state)` onto a LOCAL, throwaway `Term` (used only to
     hold the deserialized state long enough to serialize it back out —
     see item 3), then serializes that `Term`'s content (including mode-
     switching escapes for anything `TermMode` tracks, e.g. DECCKM) into
     a plain ANSI byte stream and writes THAT to its own stdout. This is
     a full, not incremental, repaint — no previous-state diffing needed
     (unlike v1's `Redrawer`, which HAD to diff since it ran continuously)
     since this only happens once per (re)connect. Still meaningfully
     simpler than v1's `Redrawer`: no per-cell last-painted-cache, no
     incremental-diff logic — literally "walk the restored grid, emit
     what's there," plus (the part v1 got wrong) the mode escapes.
  3. From then on, forwards subsequent HOLDER->RELAY raw bytes STRAIGHT to
     its own stdout, unmodified — Som's own unmodified `Terminal`/
     `ansi::Processor` (the SAME one that already parses a plain, non-tmux
     shell's output) does the actual parsing/rendering, exactly as it
     always has. RELAY itself needs no persistent `Term` of its own for
     this ongoing stream — only transiently, in step 2, to go from
     `TermState` back to bytes.
  4. Keystrokes/resize continue to flow RELAY -> HOLDER -> real PTY,
     same direction as v1/v2.
- **Protocol**: two message shapes cover everything — `Snapshot(Vec<u8>)`
  (bincode-encoded `TermState`, sent once per connect/reconnect) and
  `Bytes(Vec<u8>)` (raw PTY output, sent continuously afterward). No
  `RelayInput::Resize`/`HolderOutput::ShellExited`-style enumeration
  needed beyond what v1 already had for the non-content-diffing parts
  (resize, close) — only the CONTENT-carrying message shape changes.
- **What v1 code survives essentially unchanged**: `session.rs`'s real
  PTY/shell ownership, the pipe transport (`pipe.rs`), the NUL-byte
  tab-close convention, resize forwarding. **What goes away**: `redraw.rs`
  entirely (grid-diffing, `Redrawer`, per-cell last-painted-state
  tracking) — replaced by `term.snapshot()`/`term.restore()`, which
  `alacritty_terminal` itself now does the work for.
- **What v2 code goes away entirely**: `tmux_backend.rs`, the
  `<minimal.conf>` tmux config, `nix::pty::openpty`-based
  attach-session-over-a-real-pty plumbing, the `tmux`-installed-on-remote-
  host requirement and its settings-validation/fallback design (all of
  v2's open questions 1-7 in the prior version of this doc are now moot —
  not carried forward).
- **Windows-local case**: unlike v2 (which needed a permanent separate
  path for Windows since real tmux has no Windows PTY port), v3's
  `alacritty_terminal::Term`-based approach is PLATFORM-AGNOSTIC — the
  same HOLDER/RELAY snapshot/restore design can apply to the Windows-local
  profile too, once implemented. Whether to actually unify the local-
  Windows path onto v3 (retiring the CURRENT Windows-only HOLDER/RELAY
  code, which already works and was never buggy) or leave it alone since
  it isn't broken is an open question, not decided yet — no urgency
  either way since it isn't blocking anything.

### Implementation checklist

- [x] Renamed the crate/binary from `som_tmux_server`/`som-tmux-server` to
      `som_tmux`/`som-tmux` (dropping "server" — the crate now covers both
      RELAY and HOLDER roles, "server" was never an accurate name for the
      RELAY half). `crates/som_tmux_server/` -> `crates/som_tmux/` via
      `git mv`; every `use som_tmux_server::...`, `~/.local/bin/som-tmux-
      server` deploy path, log-file naming
      (`som-tmux-server-<profile>-<pane>-<role>.log` ->
      `som-tmux-<profile>-<pane>-<role>.log`), and doc-comment reference
      updated across `crates/terminal_view/src/terminal_panel.rs`,
      `crates/terminal/src/terminal.rs`, `crates/workspace/`,
      `crates/zed/src/som_config.rs`. Confirmed building clean
      (`cargo build -p som_tmux`, `cargo build -p som`) and existing tests
      still passing. Deployed binaries on WSL/Mac/deb also renamed on disk
      (`mv ~/.local/bin/som-tmux-server ~/.local/bin/som-tmux`) so existing
      `tmux: true` profiles keep finding their binary — done for all
      three hosts.
- [x] Forked `zed-industries/alacritty` -> `errordnk/alacritty`; created
      branch `som-snapshot-restore` off our exact pin
      (`fcf32feacb367b75ec84dd40f041e4fd411d3cc1`); cherry-picked
      `36fd512f` from `dsturnbull/alacritty` — applied clean except a
      `Cargo.lock` conflict (resolved by keeping our side, harmless: we
      don't depend on that repo's own lockfile). Confirmed
      `cargo test -p alacritty_terminal --features serde` passes (11/11
      new snapshot/restore tests including `snapshot_restore_preserves_
      modes` — the one that actually covers full `TermMode`/DECCKM — plus
      all 45 pre-existing tests, 0 regressions). Pushed to
      `errordnk/alacritty@c61de4be`.
- [x] **Discovered mid-implementation and fixed with a second, small patch
      on the same fork**: `alacritty_terminal::EventLoop` never exposes
      the raw PTY bytes it reads — only a `Wakeup`/"something changed"
      event — which blocks v3's HOLDER from forwarding a live byte stream
      to RELAYs (only sending a full ~27KB bincode snapshot on EVERY
      single change would be wasteful; measured directly via a throwaway
      test). Rather than reimplementing the entire PTY read loop from
      scratch (`pty-host`'s own approach, ~980 lines, and Windows-only
      incompatible besides — its own `Pty` has no `.file()`/raw-fd
      equivalent, only a ConPTY-specific `child_watcher()`), added
      `EventLoop::with_raw_byte_sink(Box<dyn Write + Send>)` — a ~15-line
      patch reusing the event loop's EXISTING (but previously
      file-hardcoded, `ref_test`-only) "copy raw bytes somewhere before
      parsing" mechanism, now exposed as a public, generic sink. Does not
      change `EventLoop::new`'s signature; existing callers (Som's own
      `terminal` crate) need no changes. Confirmed 56/56 tests still pass
      after this second patch too. Pushed to
      `errordnk/alacritty@9e31affd`; `Cargo.toml` updated to this rev.
- [x] Updated `Cargo.toml`'s `alacritty_terminal` git dependency to
      `errordnk/alacritty` at `c61de4be` (then `9e31affd`, see above).
      Confirmed `cargo build -p
      som_tmux` and `cargo build -p som` (the whole editor) build clean.
      `cargo test -p terminal` shows 28 pre-existing failures in
      `terminal_hyperlinks` (fragile hardcoded Windows test paths like
      `test\cool.rs`) — confirmed via `git stash` that these ALSO fail on
      the OLD fork pin, so NOT a regression from this change. Full
      regression pass DONE and clean: `cargo test -p som_tmux -p
      terminal_view -p terminal --lib -- --skip terminal_hyperlinks` ->
      27+30 passed, 0 failed; `cargo test -p som_tmux` (own suite,
      including all the `redraw.rs`/v1 tests that still exist pending
      deletion below) -> 28/28 passed. No regressions anywhere from the
      alacritty_terminal fork swap.
- [ ] Delete `crates/som_tmux/src/tmux_backend.rs` and the
      `<minimal.conf>` writing logic — the v2 architecture in full. NOT
      done yet — file still exists, unused (nothing calls into it after
      `main.rs`'s dispatch is updated — see below).
- [x] **IMPORTANT CORRECTION found mid-implementation** (see
      "Architecture" above, RELAY section — updated in place): RELAY is a
      SEPARATE OS PROCESS from Som, not part of the editor like
      `pty-host`'s own client is. It cannot call `Term::restore()` on
      whatever `Term` Som's `TerminalElement` actually renders — the only
      channel it has is writing bytes to its own stdout for Som's
      existing `ansi::Processor` to parse, same as v1/v2 always required.
      So RELAY DOES still need SOME "turn a Term's state into ANSI bytes"
      step after `restore()`ing a snapshot onto a local, throwaway `Term`
      — just a ONE-TIME full repaint per (re)connect, not v1's
      continuously-running incremental diffing.
      **Resolved by reusing `redraw.rs`, not deleting it**: a fresh
      `Redrawer::new()`'s very first `redraw()` call already IS a full,
      from-scratch repaint (documented behavior, unchanged from v1) — so
      RELAY can restore a snapshot onto a throwaway `Term`, run ONE
      `Redrawer::new()` + `redraw()` pass over it, write those bytes to
      its own stdout once, and then switch to forwarding subsequent raw
      HOLDER bytes UNMODIFIED afterward (no further redraw calls, no
      diff-state to maintain — this is the part that's simpler than v1).
      `redraw.rs` survives, but its ROLE shrinks to "one-shot state-to-
      ANSI serializer for a fresh (re)connect," never running continuously
      again. Still needs: extending `Redrawer`/its full-redraw path to
      emit ALL of `TermMode`'s relevant bits from the restored `Term`
      (currently only `ALT_SCREEN`-triggers-repaint and an explicit
      `APP_CURSOR`/DECCKM re-emit exist, added in v1's own bugfixing —
      NOT a generic "emit every mode bit `TermState.mode()` carries"
      pass yet, which is what actually delivers v3's core promise).
- [x] Added `Snapshot(Vec<u8>)` to `protocol.rs`'s `HolderOutput` (bincode-
      encoded `TermState`), replacing the old "Bytes-as-full-redraw-on-
      connect" role; `Bytes(Vec<u8>)` kept for the ongoing raw-byte stream
      after the initial snapshot (now literally raw PTY output, not
      already-diffed ANSI).
- [x] `Session::snapshot()` (`session.rs`) added — locks the HOLDER's own
      `Term`, calls `term.snapshot()`, bincode-encodes it
      (`bincode::serde::encode_to_vec`, bincode v2 API).
- [x] HOLDER (`server.rs`) rewritten: `handle_relay` now sends
      `HolderOutput::Snapshot(session.snapshot())` right after the
      handshake+resize (replacing the old `send_redraw`/`Redrawer::new()`
      call), subscribing to raw bytes (`Session::subscribe_raw_bytes`,
      see below) BEFORE sending the snapshot so nothing written by the
      real shell in the gap between "snapshot captured" and "subscribed"
      is lost. `spawn_forwarder` now runs TWO independent threads per
      connected RELAY: one forwards live raw bytes as `HolderOutput::
      Bytes`, the other still watches `Session::subscribe()`'s `Exit`
      event for `HolderOutput::ShellExited` (its `Wakeup` case is now a
      no-op — the raw-bytes thread already covers what changed).
- [x] `Session` (`session.rs`) extended: `RawByteBroadcaster` (a `Write`
      impl that fans out every write to every subscriber) is registered
      via the NEW `EventLoop::with_raw_byte_sink` (from the alacritty
      fork patch above) at session-spawn time; `subscribe_raw_bytes()`
      gives each connecting RELAY its own independent channel — same
      one-channel-per-subscriber pattern `subscribe()` (for `AlacTermEvent`)
      already used, for the same competing-consumers reason.
- [ ] RELAY (`relay.rs`) — **NOT YET REWRITTEN, next step**. Currently
      still has the OLD `HolderOutput::Bytes`-means-"write straight to
      stdout" loop, which will misinterpret v3's raw PTY bytes as
      already-final ANSI (mostly harmless since v3's raw bytes basically
      ARE what should reach stdout for the ONGOING stream — but the new
      `HolderOutput::Snapshot` variant has NO handler yet, which is the
      actual compile error blocking everything right now). Needs: on
      `Snapshot`, decode + `Term::restore()` onto a throwaway local `Term`
      + one `Redrawer::new()` full-redraw pass + write those bytes to
      stdout; on subsequent `Bytes`, write straight through unmodified
      (this part needs NO change from what's already there).
- [ ] `main.rs` dispatch: decide whether Unix builds should still route
      through `tmux_backend::run` (v2, currently what Unix uses) or the
      rewritten `relay.rs`/`server.rs` (v3) — NOT switched over yet, v3
      code isn't wired into `main()` at all currently. This is likely the
      very next thing to do once `relay.rs` compiles.
- [ ] Re-run (and expect to newly PASS, this time for real) the F2/htop
      regression test — `test_ssh_tmux_backend_htop_f2_opens_setup_screen`
      style, renamed for v3 — against a real remote host.
- [ ] Re-run the plain-text-through-the-pipeline sanity check
      (`test_ssh_tmux_backend_basic_io_and_close`-style) to confirm the v2
      double-emulation corruption (garbled `1c0`/`☁`-style fragments) is
      actually gone, not just no-longer-triggered-by-this-specific-test.
- [ ] Tab-close (NUL byte) re-confirmed working against the NEW protocol
      shape (kill the real shell process directly again, same as v1 — no
      tmux session to `kill-session` anymore).
- [ ] Resize re-confirmed: HOLDER's headless `Term::resize()` +
      `Session`'s real PTY resize, same as v1 (not `TIOCSWINSZ` on a
      tmux-attach child's pty — that mechanism is gone along with v2).
- [ ] Deployed and verified end-to-end on WSL, Mac, and deb.
- [ ] `SOM_MUX_PLAN.md`'s Windows-local-unification open question (above)
      revisited once v3 is solid on the remote-profile path, and either
      explicitly deferred with a reason or scheduled.

Full blow-by-blow implementation log for v1 and v2 (exact file states,
the discovery process for each abandoned design's bugs) lives in this
session's `project_som_tmux` memory note (not duplicated here) — this doc
is the durable summary, that memory is the working log.

---

## Open Questions (carried over, still unresolved)

1. **Pane resize:** fixed splits, or draggable dividers? Currently fixed
   equal-flex on restart; in-session resize is remembered per tab
   (`som_tab_flexes`) but there's no drag-to-resize UI yet.
2. **Tab reorder:** drag tabs in title bar? Not implemented.
3. **Shell hook for cwd:** inject `$PROMPT_COMMAND`/`precmd` hook, or poll
   the process's cwd, for terminals where `is_remote_terminal` currently
   just gives up on cwd tracking?
