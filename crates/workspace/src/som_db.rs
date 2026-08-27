//! Flat-file JSON persistence for Som's tab/pane layout, replacing the
//! SQLite-based workspace/terminal persistence Som inherited from Zed.
//!
//! Stored at `~/.config/som/db.json` as:
//! ```json
//! {"tabs": ["3.0", "1.2", "4.1"], "active": "1.1"}
//! ```
//! Each tab entry is `"x.y"`: `x` is the index into settings.json's `tabs[]`
//! profile list, `y` is how many extra split panes that tab has (0-3). The
//! array's own position is the tab's left-to-right order in the tab bar.
//! `active` is `"i.j"`: `i` is the index into the `tabs` array (which open tab
//! is active), `j` is which pane within that tab is active (0 = main,
//! 1-3 = split panes). A missing/unreadable file falls back to a single tab
//! using profile 0, no splits, active — `{"tabs": ["0.0"], "active": "0.0"}`.
//!
//! Tabs whose profile has `tmux: true` additionally carry their
//! `som-srv` pane ids as a third, colon-separated segment:
//! `"x.y:uuid1,uuid2,uuid3"` — one pane id per pane (main first, then splits
//! in creation order), only ever written for tabs that are actually tmux. A
//! plain (non-tmux) tab never has this segment at all — it's not "no
//! sessions", it's "not applicable". Each pane id is a UUID string used as
//! that pane's `som-srv` pipe name (see `project_som_tmux` memory,
//! "Обновление 17"/19) — restoring a tab reuses the same id so it
//! reconnects to the same still-running HOLDER process if one survived,
//! rather than starting a fresh shell. (Formerly a protocol session id to
//! `Attach` with — that concept is gone along with the old JSON IPC
//! protocol; the on-disk format/validation is unchanged, just the meaning
//! of what's stored.)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SomDbTab {
    pub profile_index: usize,
    pub extra_splits: usize,
    /// One `som-srv` pane id per live pane (main first, then splits
    /// in order), only ever present for tabs whose profile has `tmux:
    /// true`. `None` for a plain (non-tmux) tab — this is NOT "no sessions
    /// to restore", it's "this tab was never tmux in the first place".
    /// Length must match `1 + extra_splits` when present; restore treats a
    /// mismatched length as corrupt and falls back to creating fresh
    /// sessions for the whole tab rather than trying to partially trust it.
    pub tmux_sessions: Option<Vec<String>>,
}

/// Windowed-mode OS window position/size, in physical pixels. Only ever
/// populated/consulted when `window.mode` in settings.json is `"windowed"`
/// (see `som_config::WindowMode`) — maximized/minimized/fullscreen don't
/// have a meaningful restore rect of their own here, they just use
/// whatever the display reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SomWindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SomDbState {
    pub tabs: Vec<SomDbTab>,
    pub active_tab: usize,
    pub active_pane: usize,
    pub window_bounds: Option<SomWindowBounds>,
}

impl Default for SomDbState {
    fn default() -> Self {
        Self {
            tabs: vec![SomDbTab {
                profile_index: 0,
                extra_splits: 0,
                tmux_sessions: None,
            }],
            active_tab: 0,
            active_pane: 0,
            window_bounds: None,
        }
    }
}

/// Raw on-disk shape, kept separate from `SomDbState` so a malformed "x.y"
/// entry degrades gracefully (that one tab is dropped/clamped) instead of
/// failing to parse the whole file.
#[derive(Debug, Serialize, Deserialize)]
struct SomDbFile {
    tabs: Vec<String>,
    active: String,
    /// `"x,y,width,height"`, physical pixels. Absent (old file, or windowed
    /// mode never used) means "no remembered windowed-mode geometry yet".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_bounds: Option<String>,
}

fn format_window_bounds(b: &SomWindowBounds) -> String {
    format!("{},{},{},{}", b.x, b.y, b.width, b.height)
}

fn parse_window_bounds(s: &str) -> Option<SomWindowBounds> {
    let mut parts = s.split(',');
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let width = parts.next()?.parse().ok()?;
    let height = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SomWindowBounds { x, y, width, height })
}

fn parse_pair(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Parses one `tabs[]` entry: `"x.y"` or `"x.y:uuid1,uuid2,..."`. The tmux
/// session-id segment is validated against `extra_splits` (must have exactly
/// `1 + extra_splits` ids) — a mismatch is treated as corrupt data for that
/// segment only, so the tab still restores as a plain (non-tmux) tab rather
/// than failing entirely.
fn parse_tab_entry(s: &str) -> Option<SomDbTab> {
    let (pair, sessions_part) = match s.split_once(':') {
        Some((pair, rest)) => (pair, Some(rest)),
        None => (s, None),
    };
    let (profile_index, extra_splits) = parse_pair(pair)?;
    let extra_splits = extra_splits.min(3);

    let tmux_sessions = sessions_part.and_then(|rest| {
        // Validated as UUIDs (rejects corrupt/garbage data), but stored as
        // plain strings — a pane id is just a pipe-name component now, not
        // a typed protocol identifier (see module doc comment).
        let ids: Option<Vec<String>> =
            rest.split(',').map(|part| Uuid::parse_str(part).ok().map(|id| id.to_string())).collect();
        let ids = ids?;
        (ids.len() == 1 + extra_splits).then_some(ids)
    });

    Some(SomDbTab {
        profile_index,
        extra_splits,
        tmux_sessions,
    })
}

pub fn som_db_path() -> PathBuf {
    paths::config_dir().join("db.json")
}

/// Loads and parses `db.json`, falling back to the single-default-tab state
/// on any I/O error, JSON error, or if the file is entirely empty/unparsable.
/// Individual malformed tab entries are skipped rather than failing the load.
pub fn load_som_db() -> SomDbState {
    let path = som_db_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return SomDbState::default();
    };
    let Ok(file) = serde_json::from_str::<SomDbFile>(&contents) else {
        return SomDbState::default();
    };

    let tabs: Vec<SomDbTab> = file.tabs.iter().filter_map(|s| parse_tab_entry(s)).collect();

    if tabs.is_empty() {
        return SomDbState::default();
    }

    let (active_tab, active_pane) = parse_pair(&file.active).unwrap_or((0, 0));
    let active_tab = active_tab.min(tabs.len() - 1);
    let max_pane = tabs[active_tab].extra_splits;
    let active_pane = active_pane.min(max_pane);

    let window_bounds = file.window_bounds.as_deref().and_then(parse_window_bounds);

    SomDbState {
        tabs,
        active_tab,
        active_pane,
        window_bounds,
    }
}

/// Serializes and writes `db.json`. Synchronous — the file is tiny and this
/// is called infrequently (tab/split creation, close, switch), so there's no
/// need for the debounced-write machinery Zed's heavier SQLite persistence
/// uses.
pub fn save_som_db(state: &SomDbState) {
    let file = SomDbFile {
        tabs: state
            .tabs
            .iter()
            .map(|t| {
                let pair = format!("{}.{}", t.profile_index, t.extra_splits);
                match &t.tmux_sessions {
                    Some(ids) => {
                        let ids = ids.join(",");
                        format!("{pair}:{ids}")
                    }
                    None => pair,
                }
            })
            .collect(),
        active: format!("{}.{}", state.active_tab, state.active_pane),
        window_bounds: state.window_bounds.as_ref().map(format_window_bounds),
    };
    let Ok(json) = serde_json::to_string_pretty(&file) else {
        return;
    };
    let path = som_db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

/// Read-modify-write just the window bounds, leaving tabs/active untouched.
/// Window resize/move events are independent of tab state changes, so this
/// avoids clobbering concurrent tab/split writes (or vice versa) the way
/// blindly calling `save_som_db` with a stale in-memory `SomDbState` would.
pub fn save_window_bounds_to_som_db(bounds: SomWindowBounds) {
    let mut state = load_som_db();
    state.window_bounds = Some(bounds);
    save_som_db(&state);
}

/// Dock panel size persistence — kept in its own small file
/// (`~/.config/som/dock_panel_sizes.json`), separate from the main
/// `db.json`, since it's unrelated to tabs/window geometry and (as of this
/// writing) no panel is ever actually docked in Som — there's no
/// `add_panel` call site since the docked Terminal Panel was removed. This
/// exists purely so `Workspace::persist_panel_size_state`'s generic dock
/// code has somewhere to write without depending on SQLite; in practice
/// the file is never created because nothing ever calls
/// `save_dock_panel_size` in Som today.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DockPanelSizesFile {
    #[serde(default)]
    panels: std::collections::HashMap<String, DockPanelSizeEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct DockPanelSizeEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flex: Option<f32>,
}

fn dock_panel_sizes_path() -> PathBuf {
    paths::config_dir().join("dock_panel_sizes.json")
}

fn load_dock_panel_sizes_file() -> DockPanelSizesFile {
    let Ok(contents) = std::fs::read_to_string(dock_panel_sizes_path()) else {
        return DockPanelSizesFile::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn load_dock_panel_size(panel_key: &str) -> Option<crate::dock::PanelSizeState> {
    let file = load_dock_panel_sizes_file();
    let entry = file.panels.get(panel_key)?;
    Some(crate::dock::PanelSizeState {
        size: entry.size.map(gpui::px),
        flex: entry.flex,
    })
}

pub fn save_dock_panel_size(panel_key: String, size: Option<f32>, flex: Option<f32>) {
    let mut file = load_dock_panel_sizes_file();
    file.panels.insert(panel_key, DockPanelSizeEntry { size, flex });
    let Ok(json) = serde_json::to_string_pretty(&file) else {
        return;
    };
    let path = dock_panel_sizes_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

/// Default dock layout (left/right/bottom open/closed + size) for an empty
/// workspace, and the live `MultiWorkspace` state (active workspace id,
/// project groups, sidebar) — both were SQLite/KVP-backed in Zed. Neither
/// is meaningfully exercised in Som today: no panel is ever docked (so
/// dock state is always the same empty structure), and Som doesn't restore
/// project groups/sidebar state on launch (that's all driven by
/// `db.json`/`SomTabsRestorer` instead) — but the write/read call sites are
/// still live code, so kept working via small standalone JSON files rather
/// than deleted outright.
fn default_dock_state_path() -> PathBuf {
    paths::config_dir().join("default_dock_state.json")
}

pub fn load_default_dock_state<T: serde::de::DeserializeOwned>() -> Option<T> {
    let contents = std::fs::read_to_string(default_dock_state_path()).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn save_default_dock_state<T: Serialize>(docks: &T) {
    let Ok(json) = serde_json::to_string_pretty(docks) else {
        return;
    };
    let path = default_dock_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

fn multi_workspace_state_path() -> PathBuf {
    paths::config_dir().join("multi_workspace_state.json")
}

pub fn save_multi_workspace_state<T: Serialize>(state: &T) {
    let Ok(json) = serde_json::to_string_pretty(state) else {
        return;
    };
    let path = multi_workspace_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

/// Trusted-worktree paths (which directories the user has explicitly
/// approved for language-server/extension execution when opened over
/// SSH/WSL/locally) — security-relevant state, previously a proper
/// relational SQLite table (`trusted_worktrees`, with `absolute_path`/
/// `user_name`/`host_name` columns). Preserved as a flat JSON file with the
/// exact same semantics (a set of trusted absolute paths per remote host,
/// or per "local machine" when there's no host) rather than simplified or
/// dropped, since this gates a real security prompt.
///
/// `RemoteHostLocation` (from the `project` crate) doesn't derive
/// `Serialize`/`Deserialize`, so hosts are represented here as their two
/// plain fields instead of reusing that type directly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SomTrustedHost {
    pub user_name: Option<String>,
    pub host_identifier: String,
}

impl SomTrustedHost {
    pub fn from_remote_host_location(host: &project::trusted_worktrees::RemoteHostLocation) -> Self {
        Self {
            user_name: host.user_name.as_ref().map(|s| s.to_string()),
            host_identifier: host.host_identifier.to_string(),
        }
    }

    pub fn into_remote_host_location(self) -> project::trusted_worktrees::RemoteHostLocation {
        project::trusted_worktrees::RemoteHostLocation {
            user_name: self.user_name.map(gpui::SharedString::new),
            host_identifier: gpui::SharedString::new(self.host_identifier),
        }
    }
}

/// Converts trusted-worktree data keyed by [`project::trusted_worktrees::RemoteHostLocation`]
/// (used in-memory by `TrustedWorktrees`) to/from the JSON-serializable
/// [`SomTrustedHost`] key used on disk.
pub fn save_trusted_worktrees_by_host(
    trusted: collections::HashMap<
        Option<project::trusted_worktrees::RemoteHostLocation>,
        collections::HashSet<PathBuf>,
    >,
) {
    let converted = trusted
        .into_iter()
        .map(|(host, paths)| (host.map(|h| SomTrustedHost::from_remote_host_location(&h)), paths))
        .collect();
    save_trusted_worktrees(converted);
}

pub fn load_trusted_worktrees_by_host()
-> collections::HashMap<Option<project::trusted_worktrees::RemoteHostLocation>, collections::HashSet<PathBuf>> {
    load_trusted_worktrees()
        .into_iter()
        .map(|(host, paths)| (host.map(SomTrustedHost::into_remote_host_location), paths))
        .collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustedWorktreesFile {
    /// `None` host = paths trusted on the local machine (not over SSH/WSL).
    #[serde(default)]
    entries: Vec<TrustedWorktreesEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TrustedWorktreesEntry {
    #[serde(default)]
    host: Option<SomTrustedHost>,
    paths: Vec<PathBuf>,
}

fn trusted_worktrees_path() -> PathBuf {
    paths::config_dir().join("trusted_worktrees.json")
}

pub fn load_trusted_worktrees()
-> collections::HashMap<Option<SomTrustedHost>, collections::HashSet<PathBuf>> {
    let Ok(contents) = std::fs::read_to_string(trusted_worktrees_path()) else {
        return Default::default();
    };
    let file: TrustedWorktreesFile = serde_json::from_str(&contents).unwrap_or_default();
    file.entries
        .into_iter()
        .map(|entry| (entry.host, entry.paths.into_iter().collect()))
        .collect()
}

pub fn save_trusted_worktrees(
    trusted: collections::HashMap<Option<SomTrustedHost>, collections::HashSet<PathBuf>>,
) {
    let file = TrustedWorktreesFile {
        entries: trusted
            .into_iter()
            .map(|(host, paths)| TrustedWorktreesEntry {
                host,
                paths: paths.into_iter().collect(),
            })
            .collect(),
    };
    let Ok(json) = serde_json::to_string_pretty(&file) else {
        return;
    };
    let path = trusted_worktrees_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

pub fn clear_trusted_worktrees() {
    let _ = std::fs::remove_file(trusted_worktrees_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pair() {
        assert_eq!(parse_pair("3.0"), Some((3, 0)));
        assert_eq!(parse_pair("1.2"), Some((1, 2)));
        assert_eq!(parse_pair("bad"), None);
        assert_eq!(parse_pair("1.x"), None);
    }

    #[test]
    fn default_state_is_single_tab() {
        let s = SomDbState::default();
        assert_eq!(
            s.tabs,
            vec![SomDbTab { profile_index: 0, extra_splits: 0, tmux_sessions: None }]
        );
        assert_eq!(s.active_tab, 0);
        assert_eq!(s.active_pane, 0);
    }

    #[test]
    fn clamps_extra_splits_to_three() {
        let (_, y) = parse_pair("0.9").unwrap();
        assert_eq!(y.min(3), 3);
    }

    #[test]
    fn parses_plain_tab_entry_without_tmux_sessions() {
        let tab = parse_tab_entry("3.2").unwrap();
        assert_eq!(tab.profile_index, 3);
        assert_eq!(tab.extra_splits, 2);
        assert_eq!(tab.tmux_sessions, None);
    }

    #[test]
    fn parses_tab_entry_with_tmux_sessions() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let entry = format!("0.1:{id1},{id2}");
        let tab = parse_tab_entry(&entry).unwrap();
        assert_eq!(tab.profile_index, 0);
        assert_eq!(tab.extra_splits, 1);
        assert_eq!(tab.tmux_sessions, Some(vec![id1.to_string(), id2.to_string()]));
    }

    #[test]
    fn tmux_sessions_length_mismatch_falls_back_to_none() {
        // extra_splits=0 means 1 pane expected, but two ids given.
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let entry = format!("0.0:{id1},{id2}");
        let tab = parse_tab_entry(&entry).unwrap();
        assert_eq!(tab.tmux_sessions, None);
    }

    #[test]
    fn tab_entry_formats_and_parses_symmetrically() {
        let id = Uuid::new_v4();
        let tab = SomDbTab { profile_index: 1, extra_splits: 0, tmux_sessions: Some(vec![id.to_string()]) };
        let formatted = format!("{}.{}:{}", tab.profile_index, tab.extra_splits, id);
        let parsed = parse_tab_entry(&formatted).unwrap();
        assert_eq!(parsed, tab);
    }
}
