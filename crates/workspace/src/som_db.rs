//! Flat-file JSON persistence for Som's tab/pane layout, replacing the
//! SQLite-based workspace/terminal persistence Som inherited from Zed.
//!
//! Stored at `~/.config/som/db.json` as:
//! ```json
//! {
//!   "tabs": [
//!     {"profile": 0, "panes": ["<uuid>", "<uuid>"]},
//!     {"profile": 2, "panes": ["<uuid>"]}
//!   ],
//!   "active_pane": "<uuid>",
//!   "window_bounds": "x,y,width,height"
//! }
//! ```
//! **Panes are identified by UUID, never by position.** Each tab names the
//! settings.json profile it opens (`profile`, an index into `tabs[]` there)
//! and its panes: the main pane first, then its splits (at most three). A
//! pane's UUID is the pane's own identity — for a `tmux: true` profile it is
//! also the `somsrv` session id (`pane_id`) the pane's shell runs in, so a
//! restored pane reattaches to its session by that id. `active_pane` names
//! the focused pane by UUID; the tab holding it is the active tab. The array
//! order is the tab bar's left-to-right order.
//!
//! A missing or unreadable file (including any older format) falls back to
//! a single tab of profile 0 with one fresh pane.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// At most three splits beside a tab's main pane.
pub const MAX_PANES_PER_TAB: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SomDbTab {
    /// Index into settings.json's `tabs[]` profile list.
    #[serde(rename = "profile")]
    pub profile_index: usize,
    /// The tab's panes by UUID: the main pane first, then its splits.
    /// Never empty, at most `MAX_PANES_PER_TAB`.
    pub panes: Vec<String>,
}

impl SomDbTab {
    /// A tab of `profile_index` with one fresh pane.
    pub fn fresh(profile_index: usize) -> Self {
        Self { profile_index, panes: vec![Uuid::new_v4().to_string()] }
    }

    /// The panes after the main one.
    pub fn split_panes(&self) -> &[String] {
        self.panes.get(1..).unwrap_or_default()
    }
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
    /// The focused pane, by UUID. `None` (or a UUID no tab has) means the
    /// first tab's main pane.
    pub active_pane: Option<String>,
    pub window_bounds: Option<SomWindowBounds>,
}

impl Default for SomDbState {
    fn default() -> Self {
        Self { tabs: vec![SomDbTab::fresh(0)], active_pane: None, window_bounds: None }
    }
}

impl SomDbState {
    /// The tab holding `active_pane`, or the first tab.
    pub fn active_tab(&self) -> usize {
        self.active_pane
            .as_deref()
            .and_then(|active| self.tabs.iter().position(|tab| tab.panes.iter().any(|pane| pane == active)))
            .unwrap_or(0)
    }

    /// Every pane UUID across every tab.
    pub fn pane_ids(&self) -> impl Iterator<Item = &str> {
        self.tabs.iter().flat_map(|tab| tab.panes.iter().map(String::as_str))
    }
}

/// On-disk shape. Tabs are validated one by one after parsing, so a single
/// malformed tab is dropped rather than failing the whole file.
#[derive(Debug, Serialize, Deserialize)]
struct SomDbFile {
    tabs: Vec<SomDbTab>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_pane: Option<String>,
    /// `"x,y,width,height"`, physical pixels. Absent means "no remembered
    /// windowed-mode geometry yet".
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

/// A tab is kept if it has 1..=`MAX_PANES_PER_TAB` panes, every one a UUID
/// and none repeated anywhere in the file (`seen`) — two panes sharing an id
/// would share one `somsrv` session.
fn valid_tab(tab: &SomDbTab, seen: &mut std::collections::HashSet<String>) -> bool {
    (1..=MAX_PANES_PER_TAB).contains(&tab.panes.len())
        && tab.panes.iter().all(|pane| Uuid::parse_str(pane).is_ok())
        && tab.panes.iter().all(|pane| seen.insert(pane.clone()))
}

pub fn som_db_path() -> PathBuf {
    paths::config_dir().join("db.json")
}

/// Parses `db.json`'s contents. Anything that is not this format (an older
/// one included) gives the default single-tab state; individual invalid tabs
/// are dropped.
fn parse_som_db(contents: &str) -> SomDbState {
    let Ok(file) = serde_json::from_str::<SomDbFile>(contents) else {
        return SomDbState::default();
    };
    let mut seen = std::collections::HashSet::new();
    let tabs: Vec<SomDbTab> = file.tabs.into_iter().filter(|tab| valid_tab(tab, &mut seen)).collect();
    if tabs.is_empty() {
        return SomDbState::default();
    }
    SomDbState {
        tabs,
        active_pane: file.active_pane,
        window_bounds: file.window_bounds.as_deref().and_then(parse_window_bounds),
    }
}

/// Loads `db.json`, falling back to the single-default-tab state on any I/O
/// or format error.
pub fn load_som_db() -> SomDbState {
    match std::fs::read_to_string(som_db_path()) {
        Ok(contents) => parse_som_db(&contents),
        Err(_) => SomDbState::default(),
    }
}

fn format_som_db(state: &SomDbState) -> anyhow::Result<String> {
    let file = SomDbFile {
        tabs: state.tabs.clone(),
        active_pane: state.active_pane.clone(),
        window_bounds: state.window_bounds.as_ref().map(format_window_bounds),
    };
    Ok(serde_json::to_string_pretty(&file)?)
}

/// Serializes and writes `db.json`. Synchronous — the file is tiny and this
/// is called infrequently (tab/split creation, close, switch). Written to a
/// temporary file and renamed over the old one, so a crash mid-write never
/// leaves a truncated file behind.
pub fn save_som_db(state: &SomDbState) {
    if let Err(err) = write_som_db(state) {
        log::error!("failed to write db.json: {err:#}");
    }
}

fn write_som_db(state: &SomDbState) -> anyhow::Result<()> {
    let json = format_som_db(state)?;
    let path = som_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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

    fn uuid() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn default_state_is_one_tab_with_one_fresh_pane() {
        let s = SomDbState::default();
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].profile_index, 0);
        assert_eq!(s.tabs[0].panes.len(), 1);
        assert!(Uuid::parse_str(&s.tabs[0].panes[0]).is_ok());
        assert_eq!(s.active_tab(), 0);
    }

    #[test]
    fn formats_and_parses_symmetrically() {
        let (a, b, c) = (uuid(), uuid(), uuid());
        let state = SomDbState {
            tabs: vec![
                SomDbTab { profile_index: 1, panes: vec![a.clone(), b.clone()] },
                SomDbTab { profile_index: 0, panes: vec![c.clone()] },
            ],
            active_pane: Some(b.clone()),
            window_bounds: Some(SomWindowBounds { x: 1, y: 2, width: 3, height: 4 }),
        };
        let parsed = parse_som_db(&format_som_db(&state).expect("formats"));
        assert_eq!(parsed, state);
        assert_eq!(parsed.active_tab(), 0, "the tab holding the active pane");
        assert_eq!(parsed.tabs[0].split_panes(), [b]);
    }

    #[test]
    fn an_older_format_gives_the_default() {
        let old = "e3fb3908-8b82-4f7e-b65d-11f4bff2b878";
        let parsed = parse_som_db(&format!(r#"{{"tabs": ["0.0:{old}"], "active": "0.0"}}"#));
        assert_eq!(parsed.tabs.len(), 1);
        assert_ne!(parsed.tabs[0].panes[0], old);
    }

    #[test]
    fn invalid_tabs_are_dropped_and_the_rest_kept() {
        let (a, b) = (uuid(), uuid());
        let five: Vec<String> = (0..5).map(|_| uuid()).collect();
        let json = serde_json::json!({
            "tabs": [
                {"profile": 0, "panes": [a]},
                {"profile": 1, "panes": []},
                {"profile": 2, "panes": ["not-a-uuid"]},
                {"profile": 3, "panes": five},
                {"profile": 4, "panes": [a]},
                {"profile": 5, "panes": [b]},
            ],
        });
        let parsed = parse_som_db(&json.to_string());
        let profiles: Vec<usize> = parsed.tabs.iter().map(|tab| tab.profile_index).collect();
        assert_eq!(profiles, [0, 5], "empty, non-UUID, too many and repeated panes are dropped");
    }

    #[test]
    fn an_unknown_active_pane_means_the_first_tab() {
        let state = SomDbState {
            tabs: vec![SomDbTab::fresh(0), SomDbTab::fresh(1)],
            active_pane: Some(uuid()),
            window_bounds: None,
        };
        assert_eq!(state.active_tab(), 0);
    }
}
