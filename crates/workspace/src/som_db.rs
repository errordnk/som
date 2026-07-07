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
//! `som-tmux-server` pane ids as a third, colon-separated segment:
//! `"x.y:uuid1,uuid2,uuid3"` — one pane id per pane (main first, then splits
//! in creation order), only ever written for tabs that are actually tmux. A
//! plain (non-tmux) tab never has this segment at all — it's not "no
//! sessions", it's "not applicable". Each pane id is a UUID string used as
//! that pane's `som-tmux-server` pipe name (see `project_som_tmux` memory,
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
    /// One `som-tmux-server` pane id per live pane (main first, then splits
    /// in order), only ever present for tabs whose profile has `tmux:
    /// true`. `None` for a plain (non-tmux) tab — this is NOT "no sessions
    /// to restore", it's "this tab was never tmux in the first place".
    /// Length must match `1 + extra_splits` when present; restore treats a
    /// mismatched length as corrupt and falls back to creating fresh
    /// sessions for the whole tab rather than trying to partially trust it.
    pub tmux_sessions: Option<Vec<String>>,
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
                tmux_sessions: None,
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
