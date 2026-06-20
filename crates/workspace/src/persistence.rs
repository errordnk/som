pub mod model;

use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, NaiveDateTime, Utc};
use fs::Fs;

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use db::{
    kvp::KeyValueStore,
    query,
    sqlez::{connection::Connection, domain::Domain},
    sqlez_macros::sql,
};
use gpui::{Axis, Bounds, Task, WindowBounds, WindowId, point, size};
use project::{
    ProjectGroupKey,
    trusted_worktrees::{DbTrustedPaths, RemoteHostLocation},
};

use serde::{Deserialize, Serialize};
use sqlez::{
    bindable::{Bind, Column, StaticColumnCount},
    statement::Statement,
    thread_safe_connection::ThreadSafeConnection,
};

use ui::{App, SharedString, px};
use util::{ResultExt, maybe};
use uuid::Uuid;

use crate::{
    WorkspaceId,
    path_list::{PathList, SerializedPathList},
};

use model::{
    GroupId, ItemId, PaneId, SerializedItem, SerializedPane,
    SerializedPaneGroup, SerializedWorkspace,
};

use self::model::{DockStructure, SerializedWorkspaceLocation, SessionWorkspace};

// https://www.sqlite.org/limits.html
// > <..> the maximum value of a host parameter number is SQLITE_MAX_VARIABLE_NUMBER,
// > which defaults to <..> 32766 for SQLite versions after 3.32.0.
const MAX_QUERY_PLACEHOLDERS: usize = 32000;

#[derive(Debug, Clone, PartialEq)]
pub enum BreakpointState {
    Enabled,
    Disabled,
}

impl BreakpointState {
    fn to_int(&self) -> i64 {
        match self {
            BreakpointState::Enabled => 0,
            BreakpointState::Disabled => 1,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SourceBreakpoint {
    pub row: u32,
    pub path: Arc<Path>,
    pub message: Option<Arc<str>>,
    pub condition: Option<Arc<str>>,
    pub hit_condition: Option<Arc<str>>,
    pub state: BreakpointState,
}

fn parse_timestamp(text: &str) -> DateTime<Utc> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .map(|naive| naive.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

fn contains_wsl_path(paths: &PathList) -> bool {
    cfg!(windows)
        && paths
            .paths()
            .iter()
            .any(|path| util::paths::WslPath::from_path(path).is_some())
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct SerializedAxis(pub(crate) gpui::Axis);
impl sqlez::bindable::StaticColumnCount for SerializedAxis {}
impl sqlez::bindable::Bind for SerializedAxis {
    fn bind(
        &self,
        statement: &sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<i32> {
        match self.0 {
            gpui::Axis::Horizontal => "Horizontal",
            gpui::Axis::Vertical => "Vertical",
        }
        .bind(statement, start_index)
    }
}

impl sqlez::bindable::Column for SerializedAxis {
    fn column(
        statement: &mut sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<(Self, i32)> {
        String::column(statement, start_index).and_then(|(axis_text, next_index)| {
            Ok((
                match axis_text.as_str() {
                    "Horizontal" => Self(Axis::Horizontal),
                    "Vertical" => Self(Axis::Vertical),
                    _ => anyhow::bail!("Stored serialized item kind is incorrect"),
                },
                next_index,
            ))
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub(crate) struct SerializedWindowBounds(pub(crate) WindowBounds);

impl StaticColumnCount for SerializedWindowBounds {
    fn column_count() -> usize {
        5
    }
}

impl Bind for SerializedWindowBounds {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        match self.0 {
            WindowBounds::Windowed(bounds) => {
                let next_index = statement.bind(&"Windowed", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
            WindowBounds::Maximized(bounds) => {
                let next_index = statement.bind(&"Maximized", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
            WindowBounds::Fullscreen(bounds) => {
                let next_index = statement.bind(&"FullScreen", start_index)?;
                statement.bind(
                    &(
                        SerializedPixels(bounds.origin.x),
                        SerializedPixels(bounds.origin.y),
                        SerializedPixels(bounds.size.width),
                        SerializedPixels(bounds.size.height),
                    ),
                    next_index,
                )
            }
        }
    }
}

impl Column for SerializedWindowBounds {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (window_state, next_index) = String::column(statement, start_index)?;
        let ((x, y, width, height), _): ((i32, i32, i32, i32), _) =
            Column::column(statement, next_index)?;
        let bounds = Bounds {
            origin: point(px(x as f32), px(y as f32)),
            size: size(px(width as f32), px(height as f32)),
        };

        let status = match window_state.as_str() {
            "Windowed" | "Fixed" => SerializedWindowBounds(WindowBounds::Windowed(bounds)),
            "Maximized" => SerializedWindowBounds(WindowBounds::Maximized(bounds)),
            "FullScreen" => SerializedWindowBounds(WindowBounds::Fullscreen(bounds)),
            _ => bail!("Window State did not have a valid string"),
        };

        Ok((status, next_index + 4))
    }
}

const DEFAULT_WINDOW_BOUNDS_KEY: &str = "default_window_bounds";

pub fn read_default_window_bounds(kvp: &KeyValueStore) -> Option<(Uuid, WindowBounds)> {
    let json_str = kvp
        .read_kvp(DEFAULT_WINDOW_BOUNDS_KEY)
        .log_err()
        .flatten()?;

    let (display_uuid, persisted) =
        serde_json::from_str::<(Uuid, WindowBoundsJson)>(&json_str).ok()?;
    Some((display_uuid, persisted.into()))
}

pub async fn write_default_window_bounds(
    kvp: &KeyValueStore,
    bounds: WindowBounds,
    display_uuid: Uuid,
) -> anyhow::Result<()> {
    let persisted = WindowBoundsJson::from(bounds);
    let json_str = serde_json::to_string(&(display_uuid, persisted))?;
    kvp.write_kvp(DEFAULT_WINDOW_BOUNDS_KEY.to_string(), json_str)
        .await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub enum WindowBoundsJson {
    Windowed {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    Maximized {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    Fullscreen {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

impl From<WindowBounds> for WindowBoundsJson {
    fn from(b: WindowBounds) -> Self {
        match b {
            WindowBounds::Windowed(bounds) => {
                let origin = bounds.origin;
                let size = bounds.size;
                WindowBoundsJson::Windowed {
                    x: f32::from(origin.x).round() as i32,
                    y: f32::from(origin.y).round() as i32,
                    width: f32::from(size.width).round() as i32,
                    height: f32::from(size.height).round() as i32,
                }
            }
            WindowBounds::Maximized(bounds) => {
                let origin = bounds.origin;
                let size = bounds.size;
                WindowBoundsJson::Maximized {
                    x: f32::from(origin.x).round() as i32,
                    y: f32::from(origin.y).round() as i32,
                    width: f32::from(size.width).round() as i32,
                    height: f32::from(size.height).round() as i32,
                }
            }
            WindowBounds::Fullscreen(bounds) => {
                let origin = bounds.origin;
                let size = bounds.size;
                WindowBoundsJson::Fullscreen {
                    x: f32::from(origin.x).round() as i32,
                    y: f32::from(origin.y).round() as i32,
                    width: f32::from(size.width).round() as i32,
                    height: f32::from(size.height).round() as i32,
                }
            }
        }
    }
}

impl From<WindowBoundsJson> for WindowBounds {
    fn from(n: WindowBoundsJson) -> Self {
        match n {
            WindowBoundsJson::Windowed {
                x,
                y,
                width,
                height,
            } => WindowBounds::Windowed(Bounds {
                origin: point(px(x as f32), px(y as f32)),
                size: size(px(width as f32), px(height as f32)),
            }),
            WindowBoundsJson::Maximized {
                x,
                y,
                width,
                height,
            } => WindowBounds::Maximized(Bounds {
                origin: point(px(x as f32), px(y as f32)),
                size: size(px(width as f32), px(height as f32)),
            }),
            WindowBoundsJson::Fullscreen {
                x,
                y,
                width,
                height,
            } => WindowBounds::Fullscreen(Bounds {
                origin: point(px(x as f32), px(y as f32)),
                size: size(px(width as f32), px(height as f32)),
            }),
        }
    }
}

fn read_multi_workspace_state(window_id: WindowId, cx: &App) -> model::MultiWorkspaceState {
    let kvp = KeyValueStore::global(cx);
    kvp.scoped("multi_workspace_state")
        .read(&window_id.as_u64().to_string())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub async fn write_multi_workspace_state(
    kvp: &KeyValueStore,
    window_id: WindowId,
    state: model::MultiWorkspaceState,
) {
    if let Ok(json_str) = serde_json::to_string(&state) {
        kvp.scoped("multi_workspace_state")
            .write(window_id.as_u64().to_string(), json_str)
            .await
            .log_err();
    }
}

pub fn read_serialized_multi_workspaces(
    session_workspaces: Vec<model::SessionWorkspace>,
    cx: &App,
) -> Vec<model::SerializedMultiWorkspace> {
    let mut window_groups: Vec<Vec<model::SessionWorkspace>> = Vec::new();
    let mut window_id_to_group: HashMap<WindowId, usize> = HashMap::default();

    for session_workspace in session_workspaces {
        match session_workspace.window_id {
            Some(window_id) => {
                let group_index = *window_id_to_group.entry(window_id).or_insert_with(|| {
                    window_groups.push(Vec::new());
                    window_groups.len() - 1
                });
                window_groups[group_index].push(session_workspace);
            }
            None => {
                window_groups.push(vec![session_workspace]);
            }
        }
    }

    window_groups
        .into_iter()
        .filter_map(|group| {
            let window_id = group.first().and_then(|sw| sw.window_id);
            let state = window_id
                .map(|wid| read_multi_workspace_state(wid, cx))
                .unwrap_or_default();
            let active_workspace = state
                .active_workspace_id
                .and_then(|id| group.iter().position(|ws| ws.workspace_id == id))
                .or(Some(0))
                .and_then(|index| group.into_iter().nth(index))?;
            Some(model::SerializedMultiWorkspace {
                active_workspace,
                state,
            })
        })
        .collect()
}

const DEFAULT_DOCK_STATE_KEY: &str = "default_dock_state";

pub fn read_default_dock_state(kvp: &KeyValueStore) -> Option<DockStructure> {
    let json_str = kvp.read_kvp(DEFAULT_DOCK_STATE_KEY).log_err().flatten()?;

    serde_json::from_str::<DockStructure>(&json_str).ok()
}

pub async fn write_default_dock_state(
    kvp: &KeyValueStore,
    docks: DockStructure,
) -> anyhow::Result<()> {
    let json_str = serde_json::to_string(&docks)?;
    kvp.write_kvp(DEFAULT_DOCK_STATE_KEY.to_string(), json_str)
        .await?;
    Ok(())
}

#[derive(Debug)]
pub struct Breakpoint {
    pub position: u32,
    pub message: Option<Arc<str>>,
    pub condition: Option<Arc<str>>,
    pub hit_condition: Option<Arc<str>>,
    pub state: BreakpointState,
}

/// Wrapper for DB type of a breakpoint
struct BreakpointStateWrapper<'a>(Cow<'a, BreakpointState>);

impl From<BreakpointState> for BreakpointStateWrapper<'static> {
    fn from(kind: BreakpointState) -> Self {
        BreakpointStateWrapper(Cow::Owned(kind))
    }
}

impl StaticColumnCount for BreakpointStateWrapper<'_> {
    fn column_count() -> usize {
        1
    }
}

impl Bind for BreakpointStateWrapper<'_> {
    fn bind(&self, statement: &Statement, start_index: i32) -> anyhow::Result<i32> {
        statement.bind(&self.0.to_int(), start_index)
    }
}

impl Column for BreakpointStateWrapper<'_> {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let state = statement.column_int(start_index)?;

        match state {
            0 => Ok((BreakpointState::Enabled.into(), start_index + 1)),
            1 => Ok((BreakpointState::Disabled.into(), start_index + 1)),
            _ => anyhow::bail!("Invalid BreakpointState discriminant {state}"),
        }
    }
}

impl sqlez::bindable::StaticColumnCount for Breakpoint {
    fn column_count() -> usize {
        // Position, log message, condition message, and hit condition message
        4 + BreakpointStateWrapper::column_count()
    }
}

impl sqlez::bindable::Bind for Breakpoint {
    fn bind(
        &self,
        statement: &sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<i32> {
        let next_index = statement.bind(&self.position, start_index)?;
        let next_index = statement.bind(&self.message, next_index)?;
        let next_index = statement.bind(&self.condition, next_index)?;
        let next_index = statement.bind(&self.hit_condition, next_index)?;
        statement.bind(
            &BreakpointStateWrapper(Cow::Borrowed(&self.state)),
            next_index,
        )
    }
}

impl Column for Breakpoint {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let position = statement
            .column_int(start_index)
            .with_context(|| format!("Failed to read BreakPoint at index {start_index}"))?
            as u32;
        let (message, next_index) = Option::<String>::column(statement, start_index + 1)?;
        let (condition, next_index) = Option::<String>::column(statement, next_index)?;
        let (hit_condition, next_index) = Option::<String>::column(statement, next_index)?;
        let (state, next_index) = BreakpointStateWrapper::column(statement, next_index)?;

        Ok((
            Breakpoint {
                position,
                message: message.map(Arc::from),
                condition: condition.map(Arc::from),
                hit_condition: hit_condition.map(Arc::from),
                state: state.0.into_owned(),
            },
            next_index,
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SerializedPixels(gpui::Pixels);
impl sqlez::bindable::StaticColumnCount for SerializedPixels {}

impl sqlez::bindable::Bind for SerializedPixels {
    fn bind(
        &self,
        statement: &sqlez::statement::Statement,
        start_index: i32,
    ) -> anyhow::Result<i32> {
        let this: i32 = u32::from(self.0) as _;
        this.bind(statement, start_index)
    }
}

pub struct WorkspaceDb(ThreadSafeConnection);

impl Domain for WorkspaceDb {
    const NAME: &str = stringify!(WorkspaceDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE workspaces(
                workspace_id INTEGER PRIMARY KEY,
                workspace_location BLOB UNIQUE,
                dock_visible INTEGER, // Deprecated. Preserving so users can downgrade Zed.
                dock_anchor TEXT, // Deprecated. Preserving so users can downgrade Zed.
                dock_pane INTEGER, // Deprecated.  Preserving so users can downgrade Zed.
                left_sidebar_open INTEGER, // Boolean
                timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
                FOREIGN KEY(dock_pane) REFERENCES panes(pane_id)
            ) STRICT;

            CREATE TABLE pane_groups(
                group_id INTEGER PRIMARY KEY,
                workspace_id INTEGER NOT NULL,
                parent_group_id INTEGER, // NULL indicates that this is a root node
                position INTEGER, // NULL indicates that this is a root node
                axis TEXT NOT NULL, // Enum: 'Vertical' / 'Horizontal'
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
                ON UPDATE CASCADE,
                FOREIGN KEY(parent_group_id) REFERENCES pane_groups(group_id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE panes(
                pane_id INTEGER PRIMARY KEY,
                workspace_id INTEGER NOT NULL,
                active INTEGER NOT NULL, // Boolean
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
                ON UPDATE CASCADE
            ) STRICT;

            CREATE TABLE center_panes(
                pane_id INTEGER PRIMARY KEY,
                parent_group_id INTEGER, // NULL means that this is a root pane
                position INTEGER, // NULL means that this is a root pane
                FOREIGN KEY(pane_id) REFERENCES panes(pane_id)
                ON DELETE CASCADE,
                FOREIGN KEY(parent_group_id) REFERENCES pane_groups(group_id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE items(
                item_id INTEGER NOT NULL, // This is the item's view id, so this is not unique
                workspace_id INTEGER NOT NULL,
                pane_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                position INTEGER NOT NULL,
                active INTEGER NOT NULL,
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
                ON UPDATE CASCADE,
                FOREIGN KEY(pane_id) REFERENCES panes(pane_id)
                ON DELETE CASCADE,
                PRIMARY KEY(item_id, workspace_id)
            ) STRICT;
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN window_state TEXT;
            ALTER TABLE workspaces ADD COLUMN window_x REAL;
            ALTER TABLE workspaces ADD COLUMN window_y REAL;
            ALTER TABLE workspaces ADD COLUMN window_width REAL;
            ALTER TABLE workspaces ADD COLUMN window_height REAL;
            ALTER TABLE workspaces ADD COLUMN display BLOB;
        ),
        // Drop foreign key constraint from workspaces.dock_pane to panes table.
        sql!(
            CREATE TABLE workspaces_2(
                workspace_id INTEGER PRIMARY KEY,
                workspace_location BLOB UNIQUE,
                dock_visible INTEGER, // Deprecated. Preserving so users can downgrade Zed.
                dock_anchor TEXT, // Deprecated. Preserving so users can downgrade Zed.
                dock_pane INTEGER, // Deprecated.  Preserving so users can downgrade Zed.
                left_sidebar_open INTEGER, // Boolean
                timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
                window_state TEXT,
                window_x REAL,
                window_y REAL,
                window_width REAL,
                window_height REAL,
                display BLOB
            ) STRICT;
            INSERT INTO workspaces_2 SELECT * FROM workspaces;
            DROP TABLE workspaces;
            ALTER TABLE workspaces_2 RENAME TO workspaces;
        ),
        // Add panels related information
        sql!(
            ALTER TABLE workspaces ADD COLUMN left_dock_visible INTEGER; //bool
            ALTER TABLE workspaces ADD COLUMN left_dock_active_panel TEXT;
            ALTER TABLE workspaces ADD COLUMN right_dock_visible INTEGER; //bool
            ALTER TABLE workspaces ADD COLUMN right_dock_active_panel TEXT;
            ALTER TABLE workspaces ADD COLUMN bottom_dock_visible INTEGER; //bool
            ALTER TABLE workspaces ADD COLUMN bottom_dock_active_panel TEXT;
        ),
        // Add panel zoom persistence
        sql!(
            ALTER TABLE workspaces ADD COLUMN left_dock_zoom INTEGER; //bool
            ALTER TABLE workspaces ADD COLUMN right_dock_zoom INTEGER; //bool
            ALTER TABLE workspaces ADD COLUMN bottom_dock_zoom INTEGER; //bool
        ),
        // Add pane group flex data
        sql!(
            ALTER TABLE pane_groups ADD COLUMN flexes TEXT;
        ),
        // Add fullscreen field to workspace
        // Deprecated, `WindowBounds` holds the fullscreen state now.
        // Preserving so users can downgrade Zed.
        sql!(
            ALTER TABLE workspaces ADD COLUMN fullscreen INTEGER; //bool
        ),
        // Add preview field to items
        sql!(
            ALTER TABLE items ADD COLUMN preview INTEGER; //bool
        ),
        // Add centered_layout field to workspace
        sql!(
            ALTER TABLE workspaces ADD COLUMN centered_layout INTEGER; //bool
        ),
        sql!(
            CREATE TABLE remote_projects (
                remote_project_id INTEGER NOT NULL UNIQUE,
                path TEXT,
                dev_server_name TEXT
            );
            ALTER TABLE workspaces ADD COLUMN remote_project_id INTEGER;
            ALTER TABLE workspaces RENAME COLUMN workspace_location TO local_paths;
        ),
        sql!(
            DROP TABLE remote_projects;
            CREATE TABLE dev_server_projects (
                id INTEGER NOT NULL UNIQUE,
                path TEXT,
                dev_server_name TEXT
            );
            ALTER TABLE workspaces DROP COLUMN remote_project_id;
            ALTER TABLE workspaces ADD COLUMN dev_server_project_id INTEGER;
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN local_paths_order BLOB;
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN session_id TEXT DEFAULT NULL;
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN window_id INTEGER DEFAULT NULL;
        ),
        sql!(
            ALTER TABLE panes ADD COLUMN pinned_count INTEGER DEFAULT 0;
        ),
        sql!(
            CREATE TABLE ssh_projects (
                id INTEGER PRIMARY KEY,
                host TEXT NOT NULL,
                port INTEGER,
                path TEXT NOT NULL,
                user TEXT
            );
            ALTER TABLE workspaces ADD COLUMN ssh_project_id INTEGER REFERENCES ssh_projects(id) ON DELETE CASCADE;
        ),
        sql!(
            ALTER TABLE ssh_projects RENAME COLUMN path TO paths;
        ),
        sql!(
            CREATE TABLE toolchains (
                workspace_id INTEGER,
                worktree_id INTEGER,
                language_name TEXT NOT NULL,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                PRIMARY KEY (workspace_id, worktree_id, language_name)
            );
        ),
        sql!(
            ALTER TABLE toolchains ADD COLUMN raw_json TEXT DEFAULT "{}";
        ),
        sql!(
            CREATE TABLE breakpoints (
                workspace_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                breakpoint_location INTEGER NOT NULL,
                kind INTEGER NOT NULL,
                log_message TEXT,
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
                ON UPDATE CASCADE
            );
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN local_paths_array TEXT;
            CREATE UNIQUE INDEX local_paths_array_uq ON workspaces(local_paths_array);
            ALTER TABLE workspaces ADD COLUMN local_paths_order_array TEXT;
        ),
        sql!(
            ALTER TABLE breakpoints ADD COLUMN state INTEGER DEFAULT(0) NOT NULL
        ),
        sql!(
            ALTER TABLE breakpoints DROP COLUMN kind
        ),
        sql!(ALTER TABLE toolchains ADD COLUMN relative_worktree_path TEXT DEFAULT "" NOT NULL),
        sql!(
            ALTER TABLE breakpoints ADD COLUMN condition TEXT;
            ALTER TABLE breakpoints ADD COLUMN hit_condition TEXT;
        ),
        sql!(CREATE TABLE toolchains2 (
            workspace_id INTEGER,
            worktree_id INTEGER,
            language_name TEXT NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            raw_json TEXT NOT NULL,
            relative_worktree_path TEXT NOT NULL,
            PRIMARY KEY (workspace_id, worktree_id, language_name, relative_worktree_path)) STRICT;
            INSERT INTO toolchains2
                SELECT * FROM toolchains;
            DROP TABLE toolchains;
            ALTER TABLE toolchains2 RENAME TO toolchains;
        ),
        sql!(
            CREATE TABLE ssh_connections (
                id INTEGER PRIMARY KEY,
                host TEXT NOT NULL,
                port INTEGER,
                user TEXT
            );

            INSERT INTO ssh_connections (host, port, user)
            SELECT DISTINCT host, port, user
            FROM ssh_projects;

            CREATE TABLE workspaces_2(
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT,
                ssh_connection_id INTEGER REFERENCES ssh_connections(id),
                timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
                window_state TEXT,
                window_x REAL,
                window_y REAL,
                window_width REAL,
                window_height REAL,
                display BLOB,
                left_dock_visible INTEGER,
                left_dock_active_panel TEXT,
                right_dock_visible INTEGER,
                right_dock_active_panel TEXT,
                bottom_dock_visible INTEGER,
                bottom_dock_active_panel TEXT,
                left_dock_zoom INTEGER,
                right_dock_zoom INTEGER,
                bottom_dock_zoom INTEGER,
                fullscreen INTEGER,
                centered_layout INTEGER,
                session_id TEXT,
                window_id INTEGER
            ) STRICT;

            INSERT
            INTO workspaces_2
            SELECT
                workspaces.workspace_id,
                CASE
                    WHEN ssh_projects.id IS NOT NULL THEN ssh_projects.paths
                    ELSE
                        CASE
                            WHEN workspaces.local_paths_array IS NULL OR workspaces.local_paths_array = "" THEN
                                NULL
                            ELSE
                                replace(workspaces.local_paths_array, ',', CHAR(10))
                        END
                END as paths,

                CASE
                    WHEN ssh_projects.id IS NOT NULL THEN ""
                    ELSE workspaces.local_paths_order_array
                END as paths_order,

                CASE
                    WHEN ssh_projects.id IS NOT NULL THEN (
                        SELECT ssh_connections.id
                        FROM ssh_connections
                        WHERE
                            ssh_connections.host IS ssh_projects.host AND
                            ssh_connections.port IS ssh_projects.port AND
                            ssh_connections.user IS ssh_projects.user
                    )
                    ELSE NULL
                END as ssh_connection_id,

                workspaces.timestamp,
                workspaces.window_state,
                workspaces.window_x,
                workspaces.window_y,
                workspaces.window_width,
                workspaces.window_height,
                workspaces.display,
                workspaces.left_dock_visible,
                workspaces.left_dock_active_panel,
                workspaces.right_dock_visible,
                workspaces.right_dock_active_panel,
                workspaces.bottom_dock_visible,
                workspaces.bottom_dock_active_panel,
                workspaces.left_dock_zoom,
                workspaces.right_dock_zoom,
                workspaces.bottom_dock_zoom,
                workspaces.fullscreen,
                workspaces.centered_layout,
                workspaces.session_id,
                workspaces.window_id
            FROM
                workspaces LEFT JOIN
                ssh_projects ON
                workspaces.ssh_project_id = ssh_projects.id;

            DELETE FROM workspaces_2
            WHERE workspace_id NOT IN (
                SELECT MAX(workspace_id)
                FROM workspaces_2
                GROUP BY ssh_connection_id, paths
            );

            DROP TABLE ssh_projects;
            DROP TABLE workspaces;
            ALTER TABLE workspaces_2 RENAME TO workspaces;

            CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(ssh_connection_id, paths);
        ),
        // Fix any data from when workspaces.paths were briefly encoded as JSON arrays
        sql!(
            UPDATE workspaces
            SET paths = CASE
                WHEN substr(paths, 1, 2) = '[' || '"' AND substr(paths, -2, 2) = '"' || ']' THEN
                    replace(
                        substr(paths, 3, length(paths) - 4),
                        '"' || ',' || '"',
                        CHAR(10)
                    )
                ELSE
                    replace(paths, ',', CHAR(10))
            END
            WHERE paths IS NOT NULL
        ),
        sql!(
            CREATE TABLE remote_connections(
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                host TEXT,
                port INTEGER,
                user TEXT,
                distro TEXT
            );

            CREATE TABLE workspaces_2(
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT,
                remote_connection_id INTEGER REFERENCES remote_connections(id),
                timestamp TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL,
                window_state TEXT,
                window_x REAL,
                window_y REAL,
                window_width REAL,
                window_height REAL,
                display BLOB,
                left_dock_visible INTEGER,
                left_dock_active_panel TEXT,
                right_dock_visible INTEGER,
                right_dock_active_panel TEXT,
                bottom_dock_visible INTEGER,
                bottom_dock_active_panel TEXT,
                left_dock_zoom INTEGER,
                right_dock_zoom INTEGER,
                bottom_dock_zoom INTEGER,
                fullscreen INTEGER,
                centered_layout INTEGER,
                session_id TEXT,
                window_id INTEGER
            ) STRICT;

            INSERT INTO remote_connections
            SELECT
                id,
                "ssh" as kind,
                host,
                port,
                user,
                NULL as distro
            FROM ssh_connections;

            INSERT
            INTO workspaces_2
            SELECT
                workspace_id,
                paths,
                paths_order,
                ssh_connection_id as remote_connection_id,
                timestamp,
                window_state,
                window_x,
                window_y,
                window_width,
                window_height,
                display,
                left_dock_visible,
                left_dock_active_panel,
                right_dock_visible,
                right_dock_active_panel,
                bottom_dock_visible,
                bottom_dock_active_panel,
                left_dock_zoom,
                right_dock_zoom,
                bottom_dock_zoom,
                fullscreen,
                centered_layout,
                session_id,
                window_id
            FROM
                workspaces;

            DROP TABLE workspaces;
            ALTER TABLE workspaces_2 RENAME TO workspaces;

            CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(remote_connection_id, paths);
        ),
        sql!(CREATE TABLE user_toolchains (
            remote_connection_id INTEGER,
            workspace_id INTEGER NOT NULL,
            worktree_id INTEGER NOT NULL,
            relative_worktree_path TEXT NOT NULL,
            language_name TEXT NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            raw_json TEXT NOT NULL,

            PRIMARY KEY (workspace_id, worktree_id, relative_worktree_path, language_name, name, path, raw_json)
        ) STRICT;),
        sql!(
            DROP TABLE ssh_connections;
        ),
        sql!(
            ALTER TABLE remote_connections ADD COLUMN name TEXT;
            ALTER TABLE remote_connections ADD COLUMN container_id TEXT;
        ),
        sql!(
            CREATE TABLE IF NOT EXISTS trusted_worktrees (
                trust_id INTEGER PRIMARY KEY AUTOINCREMENT,
                absolute_path TEXT,
                user_name TEXT,
                host_name TEXT
            ) STRICT;
        ),
        sql!(CREATE TABLE toolchains2 (
            workspace_id INTEGER,
            worktree_root_path TEXT NOT NULL,
            language_name TEXT NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            raw_json TEXT NOT NULL,
            relative_worktree_path TEXT NOT NULL,
            PRIMARY KEY (workspace_id, worktree_root_path, language_name, relative_worktree_path)) STRICT;
            INSERT OR REPLACE INTO toolchains2
                // The `instr(paths, '\n') = 0` part allows us to find all
                // workspaces that have a single worktree, as `\n` is used as a
                // separator when serializing the workspace paths, so if no `\n` is
                // found, we know we have a single worktree.
                SELECT toolchains.workspace_id, paths, language_name, name, path, raw_json, relative_worktree_path FROM toolchains INNER JOIN workspaces ON toolchains.workspace_id = workspaces.workspace_id AND instr(paths, '\n') = 0;
            DROP TABLE toolchains;
            ALTER TABLE toolchains2 RENAME TO toolchains;
        ),
        sql!(CREATE TABLE user_toolchains2 (
            remote_connection_id INTEGER,
            workspace_id INTEGER NOT NULL,
            worktree_root_path TEXT NOT NULL,
            relative_worktree_path TEXT NOT NULL,
            language_name TEXT NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            raw_json TEXT NOT NULL,

            PRIMARY KEY (workspace_id, worktree_root_path, relative_worktree_path, language_name, name, path, raw_json)) STRICT;
            INSERT OR REPLACE INTO user_toolchains2
                // The `instr(paths, '\n') = 0` part allows us to find all
                // workspaces that have a single worktree, as `\n` is used as a
                // separator when serializing the workspace paths, so if no `\n` is
                // found, we know we have a single worktree.
                SELECT user_toolchains.remote_connection_id, user_toolchains.workspace_id, paths, relative_worktree_path, language_name, name, path, raw_json  FROM user_toolchains INNER JOIN workspaces ON user_toolchains.workspace_id = workspaces.workspace_id AND instr(paths, '\n') = 0;
            DROP TABLE user_toolchains;
            ALTER TABLE user_toolchains2 RENAME TO user_toolchains;
        ),
        sql!(
            ALTER TABLE remote_connections ADD COLUMN use_podman BOOLEAN;
        ),
        sql!(
            ALTER TABLE remote_connections ADD COLUMN remote_env TEXT;
        ),
        sql!(
            CREATE TABLE bookmarks (
                workspace_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                row INTEGER NOT NULL,
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
                ON UPDATE CASCADE
            );
        ),
        sql!(
            ALTER TABLE workspaces ADD COLUMN identity_paths TEXT;
            ALTER TABLE workspaces ADD COLUMN identity_paths_order TEXT;
        ),
    ];

    // Allow recovering from bad migration that was initially shipped to nightly
    // when introducing the ssh_connections table.
    fn should_allow_migration_change(_index: usize, old: &str, new: &str) -> bool {
        old.starts_with("CREATE TABLE ssh_connections")
            && new.starts_with("CREATE TABLE ssh_connections")
    }
}

db::static_connection!(WorkspaceDb, []);

impl WorkspaceDb {
    /// Returns a serialized workspace for the given worktree_roots. If the passed array
    /// is empty, the most recent workspace is returned instead. If no workspace for the
    /// passed roots is stored, returns none.
    pub(crate) fn workspace_for_roots<P: AsRef<Path>>(
        &self,
        worktree_roots: &[P],
    ) -> Option<SerializedWorkspace> {
        self.workspace_for_roots_internal(worktree_roots)
    }

    pub(crate) fn workspace_for_roots_internal<P: AsRef<Path>>(
        &self,
        worktree_roots: &[P],
    ) -> Option<SerializedWorkspace> {
        let root_paths = PathList::new(worktree_roots);

        if root_paths.is_empty() {
            return None;
        }

        let (
            workspace_id,
            paths,
            paths_order,
            identity_paths,
            identity_paths_order,
            window_bounds,
            display,
            centered_layout,
            docks,
            window_id,
        ): (
            WorkspaceId,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<SerializedWindowBounds>,
            Option<Uuid>,
            Option<bool>,
            DockStructure,
            Option<u64>,
        ) = self
            .select_row_bound(sql! {
                SELECT
                    workspace_id,
                    paths,
                    paths_order,
                    identity_paths,
                    identity_paths_order,
                    window_state,
                    window_x,
                    window_y,
                    window_width,
                    window_height,
                    display,
                    centered_layout,
                    left_dock_visible,
                    left_dock_active_panel,
                    left_dock_zoom,
                    right_dock_visible,
                    right_dock_active_panel,
                    right_dock_zoom,
                    bottom_dock_visible,
                    bottom_dock_active_panel,
                    bottom_dock_zoom,
                    window_id
                FROM workspaces
                WHERE
                    paths IS ? AND
                    remote_connection_id IS NULL
                LIMIT 1
            })
            .and_then(|mut prepared_statement| {
                (prepared_statement)(root_paths.serialize().paths)
            })
            .context("No workspaces found")
            .warn_on_err()
            .flatten()?;

        let paths = PathList::deserialize(&SerializedPathList {
            paths,
            order: paths_order,
        });
        let identity_paths = identity_paths.map(|paths| {
            PathList::deserialize(&SerializedPathList {
                paths,
                order: identity_paths_order.unwrap_or_default(),
            })
        });

        Some(SerializedWorkspace {
            id: workspace_id,
            location: SerializedWorkspaceLocation::Local,
            paths,
            identity_paths,
            center_group: self
                .get_center_pane_group(workspace_id)
                .context("Getting center group")
                .log_err()?,
            window_bounds,
            centered_layout: centered_layout.unwrap_or(false),
            display,
            docks,
            session_id: None,
            window_id,
        })
    }

    /// Returns the workspace with the given ID, loading all associated data.
    pub(crate) fn workspace_for_id(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<SerializedWorkspace> {
        let (
            paths,
            paths_order,
            identity_paths,
            identity_paths_order,
            window_bounds,
            display,
            centered_layout,
            docks,
            window_id,
        ): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<SerializedWindowBounds>,
            Option<Uuid>,
            Option<bool>,
            DockStructure,
            Option<u64>,
        ) = self
            .select_row_bound(sql! {
                SELECT
                    paths,
                    paths_order,
                    identity_paths,
                    identity_paths_order,
                    window_state,
                    window_x,
                    window_y,
                    window_width,
                    window_height,
                    display,
                    centered_layout,
                    left_dock_visible,
                    left_dock_active_panel,
                    left_dock_zoom,
                    right_dock_visible,
                    right_dock_active_panel,
                    right_dock_zoom,
                    bottom_dock_visible,
                    bottom_dock_active_panel,
                    bottom_dock_zoom,
                    window_id
                FROM workspaces
                WHERE workspace_id = ?
            })
            .and_then(|mut prepared_statement| (prepared_statement)(workspace_id))
            .context("No workspace found for id")
            .warn_on_err()
            .flatten()?;

        let paths = PathList::deserialize(&SerializedPathList {
            paths,
            order: paths_order,
        });
        let identity_paths = identity_paths.map(|paths| {
            PathList::deserialize(&SerializedPathList {
                paths,
                order: identity_paths_order.unwrap_or_default(),
            })
        });

        Some(SerializedWorkspace {
            id: workspace_id,
            location: SerializedWorkspaceLocation::Local,
            paths,
            identity_paths,
            center_group: self
                .get_center_pane_group(workspace_id)
                .context("Getting center group")
                .log_err()?,
            window_bounds,
            centered_layout: centered_layout.unwrap_or(false),
            display,
            docks,
            session_id: None,
            window_id,
        })
    }

    #[allow(dead_code)]
    fn breakpoints(&self, workspace_id: WorkspaceId) -> BTreeMap<Arc<Path>, Vec<SourceBreakpoint>> {
        let breakpoints: Result<Vec<(PathBuf, Breakpoint)>> = self
            .select_bound(sql! {
                SELECT path, breakpoint_location, log_message, condition, hit_condition, state
                FROM breakpoints
                WHERE workspace_id = ?
            })
            .and_then(|mut prepared_statement| (prepared_statement)(workspace_id));

        match breakpoints {
            Ok(bp) => {
                if bp.is_empty() {
                    log::debug!("Breakpoints are empty after querying database for them");
                }

                let mut map: BTreeMap<Arc<Path>, Vec<SourceBreakpoint>> = Default::default();

                for (path, breakpoint) in bp {
                    let path: Arc<Path> = path.into();
                    map.entry(path.clone()).or_default().push(SourceBreakpoint {
                        row: breakpoint.position,
                        path,
                        message: breakpoint.message,
                        condition: breakpoint.condition,
                        hit_condition: breakpoint.hit_condition,
                        state: breakpoint.state,
                    });
                }

                for (path, bps) in map.iter() {
                    log::info!(
                        "Got {} breakpoints from database at path: {}",
                        bps.len(),
                        path.to_string_lossy()
                    );
                }

                map
            }
            Err(msg) => {
                log::error!("Breakpoints query failed with msg: {msg}");
                Default::default()
            }
        }
    }


    pub(crate) async fn save_workspace(&self, workspace: SerializedWorkspace) {
        let paths = workspace.paths.serialize();
        let identity_paths = workspace.identity_paths.map(|paths| paths.serialize());
        log::debug!("Saving workspace at location: {:?}", workspace.location);
        self.write(move |conn| {
            conn.with_savepoint("update_worktrees", || {
                // Clear out panes and pane_groups
                conn.exec_bound(sql!(
                    DELETE FROM pane_groups WHERE workspace_id = ?1;
                    DELETE FROM panes WHERE workspace_id = ?1;))?(workspace.id)
                    .context("Clearing old panes")?;

                // Clear out old workspaces with the same paths.
                // Skip this for empty workspaces - they are identified by workspace_id, not paths.
                // Multiple empty workspaces with different content should coexist.
                if !paths.paths.is_empty() {
                    conn.exec_bound(sql!(
                        DELETE
                        FROM workspaces
                        WHERE
                            workspace_id != ?1 AND
                            paths IS ?2 AND
                            remote_connection_id IS NULL
                    ))?((
                        workspace.id,
                        paths.paths.clone(),
                    ))
                    .context("clearing out old locations")?;
                }

                // Upsert
                let query = sql!(
                    INSERT INTO workspaces(
                        workspace_id,
                        paths,
                        paths_order,
                        identity_paths,
                        identity_paths_order,
                        left_dock_visible,
                        left_dock_active_panel,
                        left_dock_zoom,
                        right_dock_visible,
                        right_dock_active_panel,
                        right_dock_zoom,
                        bottom_dock_visible,
                        bottom_dock_active_panel,
                        bottom_dock_zoom,
                        session_id,
                        window_id,
                        timestamp
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, CURRENT_TIMESTAMP)
                    ON CONFLICT DO
                    UPDATE SET
                        paths = ?2,
                        paths_order = ?3,
                        identity_paths = ?4,
                        identity_paths_order = ?5,
                        left_dock_visible = ?6,
                        left_dock_active_panel = ?7,
                        left_dock_zoom = ?8,
                        right_dock_visible = ?9,
                        right_dock_active_panel = ?10,
                        right_dock_zoom = ?11,
                        bottom_dock_visible = ?12,
                        bottom_dock_active_panel = ?13,
                        bottom_dock_zoom = ?14,
                        session_id = ?15,
                        window_id = ?16,
                        timestamp = CURRENT_TIMESTAMP
                );
                let mut prepared_query = conn.exec_bound(query)?;
                let args = (
                    workspace.id,
                    paths.paths.clone(),
                    paths.order.clone(),
                    identity_paths.as_ref().map(|paths| paths.paths.clone()),
                    identity_paths.as_ref().map(|paths| paths.order.clone()),
                    workspace.docks,
                    workspace.session_id,
                    workspace.window_id,
                );

                prepared_query(args).context("Updating workspace")?;

                // Save center pane group
                Self::save_pane_group(conn, workspace.id, &workspace.center_group, None)
                    .context("save pane group in save workspace")?;

                Ok(())
            })
            .log_err();
        })
        .await;
    }

    query! {
        pub async fn next_id() -> Result<WorkspaceId> {
            INSERT INTO workspaces DEFAULT VALUES RETURNING workspace_id
        }
    }

    fn recent_workspaces(
        &self,
    ) -> Result<
        Vec<(
            WorkspaceId,
            PathList,
            Option<PathList>,
            Option<String>,
            DateTime<Utc>,
        )>,
    > {
        Ok(self
            .recent_workspaces_query()?
            .into_iter()
            .map(
                |(
                    id,
                    paths,
                    order,
                    identity_paths,
                    identity_paths_order,
                    session_id,
                    timestamp,
                )| {
                    (
                        id,
                        PathList::deserialize(&SerializedPathList { paths, order }),
                        identity_paths.map(|paths| {
                            PathList::deserialize(&SerializedPathList {
                                paths,
                                order: identity_paths_order.unwrap_or_default(),
                            })
                        }),
                        session_id,
                        parse_timestamp(&timestamp),
                    )
                },
            )
            .collect())
    }

    query! {
        fn recent_workspaces_query() -> Result<Vec<(WorkspaceId, String, String, Option<String>, Option<String>, Option<String>, String)>> {
            SELECT workspace_id, paths, paths_order, identity_paths, identity_paths_order, session_id, timestamp
            FROM workspaces
            WHERE
                paths IS NOT NULL AND
                remote_connection_id IS NULL
            ORDER BY timestamp DESC
        }
    }

    fn session_workspaces(
        &self,
        session_id: String,
    ) -> Result<Vec<(WorkspaceId, PathList, Option<u64>)>>
    {
        Ok(self
            .session_workspaces_query(session_id)?
            .into_iter()
            .map(
                |(workspace_id, paths, order, window_id)| {
                    (
                        WorkspaceId(workspace_id),
                        PathList::deserialize(&SerializedPathList { paths, order }),
                        window_id,
                    )
                },
            )
            .collect())
    }

    query! {
        fn session_workspaces_query(session_id: String) -> Result<Vec<(i64, String, String, Option<u64>)>> {
            SELECT workspace_id, paths, paths_order, window_id
            FROM workspaces
            WHERE session_id = ?1 AND remote_connection_id IS NULL
            ORDER BY timestamp DESC
        }
    }

    query! {
        pub fn breakpoints_for_file(workspace_id: WorkspaceId, file_path: &Path) -> Result<Vec<Breakpoint>> {
            SELECT breakpoint_location
            FROM breakpoints
            WHERE  workspace_id= ?1 AND path = ?2
        }
    }

    query! {
        pub fn clear_breakpoints(file_path: &Path) -> Result<()> {
            DELETE FROM breakpoints
            WHERE file_path = ?2
        }
    }

    query! {
        pub async fn delete_workspace_by_id(id: WorkspaceId) -> Result<()> {
            DELETE FROM workspaces
            WHERE workspace_id IS ?
        }
    }

    async fn all_paths_exist_with_a_directory(paths: &[PathBuf], fs: &dyn Fs) -> bool {
        let mut any_dir = false;
        for path in paths {
            match fs.metadata(path).await.ok().flatten() {
                None => return false,
                Some(meta) => {
                    if meta.is_dir {
                        any_dir = true;
                    }
                }
            }
        }
        any_dir
    }

    // Returns the raw recent workspace history. Scratch workspaces (no paths) are filtered
    // out because they are restored separately by `last_session_workspace_locations`.
    pub async fn recent_project_workspaces_ungrouped(
        &self,
        fs: &dyn Fs,
    ) -> Result<Vec<RecentWorkspace>> {
        let mut result = Vec::new();
        for (id, paths, identity_paths_hint, _session_id, timestamp) in
            self.recent_workspaces()?
        {
            if paths.paths().is_empty() || contains_wsl_path(&paths) {
                continue;
            }

            if Self::all_paths_exist_with_a_directory(paths.paths(), fs).await {
                let identity_paths = resolve_local_workspace_identity(fs, &paths)
                    .await
                    .or(identity_paths_hint)
                    .unwrap_or_else(|| paths.clone());
                result.push(RecentWorkspace {
                    workspace_id: id,
                    location: SerializedWorkspaceLocation::Local,
                    paths,
                    identity_paths,
                    timestamp,
                });
            }
        }

        Ok(result)
    }

    // Returns the recent project workspaces suitable for recent-project UIs.
    // Entries are deduplicated by git worktree identity, but preserve the original
    // serialized paths for reopening.
    pub async fn recent_project_workspaces(&self, fs: &dyn Fs) -> Result<Vec<RecentWorkspace>> {
        Ok(dedupe_recent_workspaces(
            self.recent_project_workspaces_ungrouped(fs).await?,
        ))
    }

    pub async fn delete_recent_workspace_group(
        &self,
        target: &RecentWorkspace,
    ) -> Result<Vec<WorkspaceId>> {
        let target_paths = &target.identity_paths;

        let mut workspace_ids = Vec::new();
        for (workspace_id, paths, identity_paths, _, _) in
            self.recent_workspaces()?
        {
            if &identity_paths.unwrap_or(paths) == target_paths {
                workspace_ids.push(workspace_id);
            }
        }

        futures::future::join_all(
            workspace_ids
                .iter()
                .copied()
                .map(|workspace_id| self.delete_workspace_by_id(workspace_id)),
        )
        .await;

        Ok(workspace_ids)
    }

    pub async fn garbage_collect_workspaces(
        &self,
        fs: &dyn Fs,
        current_session_id: &str,
        last_session_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        let mut workspaces_to_delete = Vec::new();
        for (id, paths, _identity_paths_hint, session_id, timestamp) in
            self.recent_workspaces()?
        {
            if let Some(session_id) = session_id.as_deref() {
                if session_id == current_session_id || Some(session_id) == last_session_id {
                    continue;
                }
            }

            if contains_wsl_path(&paths) {
                workspaces_to_delete.push(id);
                continue;
            }

            if !Self::all_paths_exist_with_a_directory(paths.paths(), fs).await
                && now - timestamp >= chrono::Duration::days(7)
            {
                workspaces_to_delete.push(id);
            }
        }

        futures::future::join_all(
            workspaces_to_delete
                .into_iter()
                .map(|id| self.delete_workspace_by_id(id)),
        )
        .await;
        Ok(())
    }

    pub async fn last_workspace(&self, fs: &dyn Fs) -> Result<Option<RecentWorkspace>> {
        Ok(self.recent_project_workspaces(fs).await?.into_iter().next())
    }

    // Returns the locations of the workspaces that were still opened when the last
    // session was closed (i.e. when Zed was quit).
    // If `last_session_window_order` is provided, the returned locations are ordered
    // according to that.
    pub async fn last_session_workspace_locations(
        &self,
        last_session_id: &str,
        last_session_window_stack: Option<Vec<WindowId>>,
        fs: &dyn Fs,
    ) -> Result<Vec<SessionWorkspace>> {
        let mut workspaces = Vec::new();

        for (workspace_id, paths, window_id) in
            self.session_workspaces(last_session_id.to_owned())?
        {
            let window_id = window_id.map(WindowId::from);

            if paths.is_empty() || Self::all_paths_exist_with_a_directory(paths.paths(), fs).await {
                workspaces.push(SessionWorkspace {
                    workspace_id,
                    location: SerializedWorkspaceLocation::Local,
                    paths,
                    window_id,
                });
            }
        }

        if let Some(stack) = last_session_window_stack {
            workspaces.sort_by_key(|workspace| {
                workspace
                    .window_id
                    .and_then(|id| stack.iter().position(|&order_id| order_id == id))
                    .unwrap_or(usize::MAX)
            });
        }

        Ok(workspaces)
    }

    fn get_center_pane_group(&self, workspace_id: WorkspaceId) -> Result<SerializedPaneGroup> {
        Ok(self
            .get_pane_group(workspace_id, None)?
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                SerializedPaneGroup::Pane(SerializedPane {
                    active: true,
                    children: vec![],
                    pinned_count: 0,
                })
            }))
    }

    fn get_pane_group(
        &self,
        workspace_id: WorkspaceId,
        group_id: Option<GroupId>,
    ) -> Result<Vec<SerializedPaneGroup>> {
        type GroupKey = (Option<GroupId>, WorkspaceId);
        type GroupOrPane = (
            Option<GroupId>,
            Option<SerializedAxis>,
            Option<PaneId>,
            Option<bool>,
            Option<usize>,
            Option<String>,
        );
        self.select_bound::<GroupKey, GroupOrPane>(sql!(
            SELECT group_id, axis, pane_id, active, pinned_count, flexes
                FROM (SELECT
                        group_id,
                        axis,
                        NULL as pane_id,
                        NULL as active,
                        NULL as pinned_count,
                        position,
                        parent_group_id,
                        workspace_id,
                        flexes
                      FROM pane_groups
                    UNION
                      SELECT
                        NULL,
                        NULL,
                        center_panes.pane_id,
                        panes.active as active,
                        pinned_count,
                        position,
                        parent_group_id,
                        panes.workspace_id as workspace_id,
                        NULL
                      FROM center_panes
                      JOIN panes ON center_panes.pane_id = panes.pane_id)
                WHERE parent_group_id IS ? AND workspace_id = ?
                ORDER BY position
        ))?((group_id, workspace_id))?
        .into_iter()
        .map(|(group_id, axis, pane_id, active, pinned_count, flexes)| {
            let maybe_pane = maybe!({ Some((pane_id?, active?, pinned_count?)) });
            if let Some((group_id, axis)) = group_id.zip(axis) {
                let flexes = flexes
                    .map(|flexes: String| serde_json::from_str::<Vec<f32>>(&flexes))
                    .transpose()?;

                Ok(SerializedPaneGroup::Group {
                    axis,
                    children: self.get_pane_group(workspace_id, Some(group_id))?,
                    flexes,
                })
            } else if let Some((pane_id, active, pinned_count)) = maybe_pane {
                Ok(SerializedPaneGroup::Pane(SerializedPane::new(
                    self.get_items(pane_id)?,
                    active,
                    pinned_count,
                )))
            } else {
                bail!("Pane Group Child was neither a pane group or a pane");
            }
        })
        // Filter out panes and pane groups which don't have any children or items
        .filter(|pane_group| match pane_group {
            Ok(SerializedPaneGroup::Group { children, .. }) => !children.is_empty(),
            Ok(SerializedPaneGroup::Pane(pane)) => !pane.children.is_empty(),
            _ => true,
        })
        .collect::<Result<_>>()
    }

    fn save_pane_group(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane_group: &SerializedPaneGroup,
        parent: Option<(GroupId, usize)>,
    ) -> Result<()> {
        if parent.is_none() {
            log::debug!("Saving a pane group for workspace {workspace_id:?}");
        }
        match pane_group {
            SerializedPaneGroup::Group {
                axis,
                children,
                flexes,
            } => {
                let (parent_id, position) = parent.unzip();

                let flex_string = flexes
                    .as_ref()
                    .map(|flexes| serde_json::json!(flexes).to_string());

                let group_id = conn.select_row_bound::<_, i64>(sql!(
                    INSERT INTO pane_groups(
                        workspace_id,
                        parent_group_id,
                        position,
                        axis,
                        flexes
                    )
                    VALUES (?, ?, ?, ?, ?)
                    RETURNING group_id
                ))?((
                    workspace_id,
                    parent_id,
                    position,
                    *axis,
                    flex_string,
                ))?
                .context("Couldn't retrieve group_id from inserted pane_group")?;

                for (position, group) in children.iter().enumerate() {
                    Self::save_pane_group(conn, workspace_id, group, Some((group_id, position)))?
                }

                Ok(())
            }
            SerializedPaneGroup::Pane(pane) => {
                Self::save_pane(conn, workspace_id, pane, parent)?;
                Ok(())
            }
        }
    }

    fn save_pane(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane: &SerializedPane,
        parent: Option<(GroupId, usize)>,
    ) -> Result<PaneId> {
        let pane_id = conn.select_row_bound::<_, i64>(sql!(
            INSERT INTO panes(workspace_id, active, pinned_count)
            VALUES (?, ?, ?)
            RETURNING pane_id
        ))?((workspace_id, pane.active, pane.pinned_count))?
        .context("Could not retrieve inserted pane_id")?;

        let (parent_id, order) = parent.unzip();
        conn.exec_bound(sql!(
            INSERT INTO center_panes(pane_id, parent_group_id, position)
            VALUES (?, ?, ?)
        ))?((pane_id, parent_id, order))?;

        Self::save_items(conn, workspace_id, pane_id, &pane.children).context("Saving items")?;

        Ok(pane_id)
    }

    fn get_items(&self, pane_id: PaneId) -> Result<Vec<SerializedItem>> {
        self.select_bound(sql!(
            SELECT kind, item_id, active, preview FROM items
            WHERE pane_id = ?
                ORDER BY position
        ))?(pane_id)
    }

    fn save_items(
        conn: &Connection,
        workspace_id: WorkspaceId,
        pane_id: PaneId,
        items: &[SerializedItem],
    ) -> Result<()> {
        let mut insert = conn.exec_bound(sql!(
            INSERT INTO items(workspace_id, pane_id, position, kind, item_id, active, preview) VALUES (?, ?, ?, ?, ?, ?, ?)
        )).context("Preparing insertion")?;
        for (position, item) in items.iter().enumerate() {
            insert((workspace_id, pane_id, position, item))?;
        }

        Ok(())
    }

    query! {
        pub async fn update_timestamp(workspace_id: WorkspaceId) -> Result<()> {
            UPDATE workspaces
            SET timestamp = CURRENT_TIMESTAMP
            WHERE workspace_id = ?
        }
    }

    #[cfg(test)]
    query! {
        pub(crate) async fn set_timestamp_for_tests(workspace_id: WorkspaceId, timestamp: String) -> Result<()> {
            UPDATE workspaces
            SET timestamp = ?2
            WHERE workspace_id = ?1
        }
    }

    query! {
        pub(crate) async fn set_window_open_status(workspace_id: WorkspaceId, bounds: SerializedWindowBounds, display: Uuid) -> Result<()> {
            UPDATE workspaces
            SET window_state = ?2,
                window_x = ?3,
                window_y = ?4,
                window_width = ?5,
                window_height = ?6,
                display = ?7
            WHERE workspace_id = ?1
        }
    }

    query! {
        pub(crate) async fn set_centered_layout(workspace_id: WorkspaceId, centered_layout: bool) -> Result<()> {
            UPDATE workspaces
            SET centered_layout = ?2
            WHERE workspace_id = ?1
        }
    }

    query! {
        pub(crate) async fn set_session_id(workspace_id: WorkspaceId, session_id: Option<String>) -> Result<()> {
            UPDATE workspaces
            SET session_id = ?2
            WHERE workspace_id = ?1
        }
    }

    query! {
        pub(crate) async fn set_session_binding(workspace_id: WorkspaceId, session_id: Option<String>, window_id: Option<u64>) -> Result<()> {
            UPDATE workspaces
            SET session_id = ?2, window_id = ?3
            WHERE workspace_id = ?1
        }
    }


    pub(crate) async fn save_trusted_worktrees(
        &self,
        trusted_worktrees: HashMap<Option<RemoteHostLocation>, HashSet<PathBuf>>,
    ) -> anyhow::Result<()> {
        use anyhow::Context as _;
        use db::sqlez::statement::Statement;
        use itertools::Itertools as _;

        self.clear_trusted_worktrees()
            .await
            .context("clearing previous trust state")?;

        let trusted_worktrees = trusted_worktrees
            .into_iter()
            .flat_map(|(host, abs_paths)| {
                abs_paths
                    .into_iter()
                    .map(move |abs_path| (Some(abs_path), host.clone()))
            })
            .collect::<Vec<_>>();
        let mut first_worktree;
        let mut last_worktree = 0_usize;
        for (count, placeholders) in std::iter::once("(?, ?, ?)")
            .cycle()
            .take(trusted_worktrees.len())
            .chunks(MAX_QUERY_PLACEHOLDERS / 3)
            .into_iter()
            .map(|chunk| {
                let mut count = 0;
                let placeholders = chunk
                    .inspect(|_| {
                        count += 1;
                    })
                    .join(", ");
                (count, placeholders)
            })
            .collect::<Vec<_>>()
        {
            first_worktree = last_worktree;
            last_worktree = last_worktree + count;
            let query = format!(
                r#"INSERT INTO trusted_worktrees(absolute_path, user_name, host_name)
VALUES {placeholders};"#
            );

            let trusted_worktrees = trusted_worktrees[first_worktree..last_worktree].to_vec();
            self.write(move |conn| {
                let mut statement = Statement::prepare(conn, query)?;
                let mut next_index = 1;
                for (abs_path, host) in trusted_worktrees {
                    let abs_path = abs_path.as_ref().map(|abs_path| abs_path.to_string_lossy());
                    next_index = statement.bind(
                        &abs_path.as_ref().map(|abs_path| abs_path.as_ref()),
                        next_index,
                    )?;
                    next_index = statement.bind(
                        &host
                            .as_ref()
                            .and_then(|host| Some(host.user_name.as_ref()?.as_str())),
                        next_index,
                    )?;
                    next_index = statement.bind(
                        &host.as_ref().map(|host| host.host_identifier.as_str()),
                        next_index,
                    )?;
                }
                statement.exec()
            })
            .await
            .context("inserting new trusted state")?;
        }
        Ok(())
    }

    pub fn fetch_trusted_worktrees(&self) -> Result<DbTrustedPaths> {
        let trusted_worktrees = self.trusted_worktrees()?;
        Ok(trusted_worktrees
            .into_iter()
            .filter_map(|(abs_path, user_name, host_name)| {
                let db_host = match (user_name, host_name) {
                    (None, Some(host_name)) => Some(RemoteHostLocation {
                        user_name: None,
                        host_identifier: SharedString::new(host_name),
                    }),
                    (Some(user_name), Some(host_name)) => Some(RemoteHostLocation {
                        user_name: Some(SharedString::new(user_name)),
                        host_identifier: SharedString::new(host_name),
                    }),
                    _ => None,
                };
                Some((db_host, abs_path?))
            })
            .fold(HashMap::default(), |mut acc, (remote_host, abs_path)| {
                acc.entry(remote_host)
                    .or_insert_with(HashSet::default)
                    .insert(abs_path);
                acc
            }))
    }

    query! {
        fn trusted_worktrees() -> Result<Vec<(Option<PathBuf>, Option<String>, Option<String>)>> {
            SELECT absolute_path, user_name, host_name
            FROM trusted_worktrees
        }
    }

    query! {
        pub async fn clear_trusted_worktrees() -> Result<()> {
            DELETE FROM trusted_worktrees
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecentWorkspace {
    pub workspace_id: WorkspaceId,
    pub location: SerializedWorkspaceLocation,
    pub paths: PathList,
    pub identity_paths: PathList,
    pub timestamp: DateTime<Utc>,
}

impl RecentWorkspace {
    pub fn project_group_key(&self) -> ProjectGroupKey {
        ProjectGroupKey::new(None, self.identity_paths.clone())
    }
}

async fn resolve_local_workspace_identity(_fs: &dyn Fs, _paths: &PathList) -> Option<PathList> {
    None
}

fn dedupe_recent_workspaces(
    workspaces: impl IntoIterator<Item = RecentWorkspace>,
) -> Vec<RecentWorkspace> {
    let mut indices_by_key: HashMap<Vec<PathBuf>, usize> = HashMap::default();
    let mut result: Vec<RecentWorkspace> = Vec::new();
    for workspace in workspaces {
        let key = workspace.identity_paths.paths().to_vec();
        if let Some(&existing_index) = indices_by_key.get(&key) {
            if workspace.timestamp > result[existing_index].timestamp {
                result[existing_index] = workspace;
            }
        } else {
            indices_by_key.insert(key, result.len());
            result.push(workspace);
        }
    }

    result
}

pub fn delete_unloaded_items(
    alive_items: Vec<ItemId>,
    workspace_id: WorkspaceId,
    table: &'static str,
    db: &ThreadSafeConnection,
    cx: &mut App,
) -> Task<Result<()>> {
    let db = db.clone();
    cx.spawn(async move |_| {
        let placeholders = alive_items
            .iter()
            .map(|_| "?")
            .collect::<Vec<&str>>()
            .join(", ");

        let query = format!(
            "DELETE FROM {table} WHERE workspace_id = ? AND item_id NOT IN ({placeholders})"
        );

        db.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&workspace_id, 1)?;
            for id in alive_items {
                next_index = statement.bind(&id, next_index)?;
            }
            statement.exec()
        })
        .await
    })
}

