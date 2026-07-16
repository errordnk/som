//! Plain (non-SQLite) serialized-state types that used to live in
//! `persistence/model.rs` alongside Zed's `WorkspaceDb` SQL column
//! bind/read code. These types are still genuinely used — dock
//! visibility/active-panel/zoom state (`DockStructure`/`DockData`) is
//! written to/read from `~/.config/som/default_dock_state.json`, and
//! `MultiWorkspaceState`/`SerializedProjectGroup` are written to
//! `~/.config/som/multi_workspace_state.json` — but neither needs a SQL
//! `Bind`/`Column` impl any more since both are `serde_json`-only now.

use crate::{WorkspaceId, multi_workspace::SerializedProjectGroupState};
use project::ProjectGroupKey;
use util::path_list::{PathList, SerializedPathList};

pub type ItemId = u64;

#[derive(Debug, PartialEq, Clone, serde::Serialize, serde::Deserialize)]
pub enum SerializedWorkspaceLocation {
    Local,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SerializedProjectGroup {
    pub path_list: SerializedPathList,
    pub(crate) location: SerializedWorkspaceLocation,
    #[serde(default = "default_expanded")]
    pub expanded: bool,
}

fn default_expanded() -> bool {
    true
}

impl SerializedProjectGroup {
    pub fn from_group(key: &ProjectGroupKey, expanded: bool) -> Self {
        Self {
            path_list: key.path_list().serialize(),
            location: SerializedWorkspaceLocation::Local,
            expanded,
        }
    }

    pub fn into_restored_state(self) -> SerializedProjectGroupState {
        let path_list = PathList::deserialize(&self.path_list);
        SerializedProjectGroupState {
            key: ProjectGroupKey::new(None, path_list),
            expanded: self.expanded,
        }
    }
}

impl From<SerializedProjectGroup> for ProjectGroupKey {
    fn from(value: SerializedProjectGroup) -> Self {
        value.into_restored_state().key
    }
}

/// Per-window state for a MultiWorkspace, persisted to
/// `~/.config/som/multi_workspace_state.json`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MultiWorkspaceState {
    pub active_workspace_id: Option<WorkspaceId>,
    pub sidebar_open: bool,
    #[serde(alias = "project_group_keys")]
    pub project_groups: Vec<SerializedProjectGroup>,
    #[serde(default)]
    pub sidebar_state: Option<String>,
}

#[derive(Debug, PartialEq, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DockStructure {
    pub left: DockData,
    pub right: DockData,
    pub bottom: DockData,
}

#[derive(Debug, PartialEq, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DockData {
    pub visible: bool,
    pub active_panel: Option<String>,
    pub zoom: bool,
}
