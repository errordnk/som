# Som Multiplexer — Plan

## Goal

Replace Zed's pane management with a tmux-style multiplexer built into Som.
Each tab is a window with up to 4 terminal panes. Tabs and their pane layouts
persist across restarts.

---

## Core Concepts

| tmux term | Som term | Description |
|-----------|----------|-------------|
| session   | —        | always one (the app itself) |
| window    | SomTab   | a named tab, has its own pane layout |
| pane      | SomPane  | one terminal process inside a tab |

**Rules:**
- All tabs share one tab bar (in title bar)
- Switching tabs switches the entire pane layout at once
- Closing a tab closes all its panes
- Each pane is an independent pty process (same shell profile as the tab)
- Up to 4 panes per tab (2×2 grid or 1+1 splits)
- State (cwd, layout, shell) saved on close, restored on next launch

---

## Data Structures

```rust
// crates/som_mux/src/lib.rs

pub struct SomMux {
    pub tabs: Vec<SomTab>,
    pub active_tab: usize,
    pub next_tab_id: u64,
}

pub struct SomTab {
    pub id: u64,
    pub name: String,
    pub profile: TabProfile,      // shell binary + working_dir from settings
    pub layout: SomLayout,
    pub panes: Vec<SomPane>,      // 1..=4 elements
    pub active_pane: usize,
}

pub enum SomLayout {
    Single,                       // [0]
    SplitH,                       // [0|1]  left/right
    SplitV,                       // [0/1]  top/bottom
    SplitH3,                      // [0|1|2]
    Quad,                         // [0|1 / 2|3]
}

pub struct SomPane {
    pub id: u64,
    pub terminal: Entity<TerminalView>,
    pub saved_cwd: Option<PathBuf>,   // updated periodically for restore
}
```

---

## Rendering

Replace Workspace center content with `SomMuxView`:

```
┌─────────────────────────────────────────────┐
│ [+][v]  Shell ×  WSL ×  Python ×   [─][□][✕]│  ← title bar (existing)
├─────────────────────────────────────────────┤
│                    │                         │
│   pane 0           │   pane 1                │  ← SomMuxView renders this
│                    │                         │     based on SomTab.layout
│                    │                         │
└─────────────────────────────────────────────┘
```

- `SomMuxView` is a GPUI `View` that renders a grid of `TerminalView` entities
- Pane borders: 1px separator, no title bar on panes
- Active pane: subtle border highlight (configurable color)
- Focus follows last click

---

## Layout Engine

```
SomLayout::Single   →  one full-size terminal

SomLayout::SplitH   →  h_flex: [pane0 | pane1]  (50/50)

SomLayout::SplitV   →  v_flex: [pane0 / pane1]  (50/50)

SomLayout::SplitH3  →  h_flex: [pane0 | pane1 | pane2]  (33/33/33)

SomLayout::Quad     →  v_flex:
                          h_flex: [pane0 | pane1]
                          h_flex: [pane2 | pane3]
```

Split direction cycle on double-click / ctrl-\:
- 1 pane  → SplitH   (add pane 1, split right)
- SplitH  → Quad     (add panes 2+3, split each row vertically) — or SplitH3?
- At 4 panes: do nothing

> **Decision needed:** after SplitH do we go SplitH3 or Quad?

---

## Keybindings

| Key | Action | Notes |
|-----|--------|-------|
| ctrl-\ | SplitPane | split active pane |
| ctrl-shift-\ | UnsplitPane | close active pane (not the tab) |
| ctrl-f4 | CloseTab | close tab + all panes |
| ctrl-n | NewTab | new tab with default profile |
| ctrl-shift-1..9 | NewTab(profile N) | new tab with specific profile |
| ctrl-tab | NextTab | cycle tabs |
| ctrl-shift-tab | PrevTab | cycle tabs |
| alt-arrow | FocusPane(dir) | move focus between panes |

---

## Integration Points

### Remove from Zed
- `Workspace::som_split_panes: Vec<WeakEntity<Pane>>` — replaced by SomMux
- `SomSplitPane` / `SomUnsplitPane` actions — replaced by SomMux actions
- Double-click on tab to split — moved to SomMuxView

### Keep from Zed
- `TerminalView` — still used per-pane
- `TerminalPanel::add_center_terminal_named` — reuse for creating terminals
- `Workspace` — kept as shell, center content replaced with SomMuxView
- Title bar — updated to read from SomMux instead of TabProfiles

### New crate: `crates/som_mux/`
- `lib.rs` — SomMux, SomTab, SomPane, SomLayout structs
- `view.rs` — SomMuxView (GPUI View, renders the grid)
- `persist.rs` — save/load state to JSON
- `actions.rs` — SplitPane, UnsplitPane, FocusPane, CloseTab, NewTab

---

## Persistence (Phase 2)

On app close, save to `~/.config/som/session.json`:

```json
{
  "active_tab": 1,
  "tabs": [
    {
      "id": 1,
      "name": "Shell",
      "profile": { "shell": "pwsh.exe", "working_dir": "~" },
      "layout": "SplitH",
      "panes": [
        { "id": 1, "cwd": "C:/Users/dnk/projects/som" },
        { "id": 2, "cwd": "C:/Users/dnk" }
      ],
      "active_pane": 0
    }
  ]
}
```

On launch: restore tabs + pane layouts, reopen terminals with saved cwd.

---

## Implementation Phases

### Phase 1 — Foundation
1. Create `crates/som_mux/` with data structures
2. `SomMuxView` renders a single pane (replaces center content in workspace)
3. Tab bar in title bar reads from `SomMux` global
4. `NewTab` action creates a tab and opens a terminal

### Phase 2 — Split
5. `SplitPane` action: add pane to active tab, update layout, render grid
6. `UnsplitPane`: remove active pane
7. `CloseTab`: close all panes of tab, remove tab
8. Pane focus with alt-arrow

### Phase 3 — Polish
9. Active pane border highlight
10. Tab close button (×) closes tab + panes
11. No pane title bars

### Phase 4 — Persistence
12. Save session on quit
13. Restore session on launch
14. Per-pane cwd tracking (poll `proc_info` or shell hook)

---

## Open Questions

1. **Split sequence:** SplitH → SplitH3 → (max) or SplitH → Quad → (max)?
2. **Pane resize:** fixed 50/50 splits, or draggable dividers? (Phase 3+)
3. **Tab reorder:** drag tabs in title bar? (Phase 3+)
4. **Shell hook for cwd:** inject `$PROMPT_COMMAND` / `precmd` hook, or poll `/proc/PID/cwd`?
5. **Workspace:** keep MultiWorkspace/Workspace as container, or bypass completely?
