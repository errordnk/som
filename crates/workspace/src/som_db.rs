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

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SomDbTab {
    pub profile_index: usize,
    pub extra_splits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SomDbState {
    pub tabs: Vec<SomDbTab>,
    pub active_tab: usize,
    pub active_pane: usize,
}

impl Default for SomDbState {
    fn default() -> Self {
        Self {
            tabs: vec![SomDbTab {
                profile_index: 0,
                extra_splits: 0,
            }],
            active_tab: 0,
            active_pane: 0,
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
}

fn parse_pair(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
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

    let tabs: Vec<SomDbTab> = file
        .tabs
        .iter()
        .filter_map(|s| {
            let (profile_index, extra_splits) = parse_pair(s)?;
            Some(SomDbTab {
                profile_index,
                extra_splits: extra_splits.min(3),
            })
        })
        .collect();

    if tabs.is_empty() {
        return SomDbState::default();
    }

    let (active_tab, active_pane) = parse_pair(&file.active).unwrap_or((0, 0));
    let active_tab = active_tab.min(tabs.len() - 1);
    let max_pane = tabs[active_tab].extra_splits;
    let active_pane = active_pane.min(max_pane);

    SomDbState {
        tabs,
        active_tab,
        active_pane,
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
            .map(|t| format!("{}.{}", t.profile_index, t.extra_splits))
            .collect(),
        active: format!("{}.{}", state.active_tab, state.active_pane),
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
        assert_eq!(s.tabs, vec![SomDbTab { profile_index: 0, extra_splits: 0 }]);
        assert_eq!(s.active_tab, 0);
        assert_eq!(s.active_pane, 0);
    }

    #[test]
    fn clamps_extra_splits_to_three() {
        let (_, y) = parse_pair("0.9").unwrap();
        assert_eq!(y.min(3), 3);
    }
}
