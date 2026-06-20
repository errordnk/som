use anyhow::{Context as _, Result, anyhow};
use fs::Fs;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::mpsc;
use gpui::{App, AsyncApp, Global, WindowHandle};
use std::path::Path;
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::thread;
use util::ResultExt;
use util::paths::PathWithPosition;
use settings::Settings as _;
use workspace::{AppState, MultiWorkspace, OpenResult};

#[derive(Default, Debug)]
pub struct OpenRequest {
    pub kind: Option<OpenRequestKind>,
    pub open_paths: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub dev_container: bool,
}

#[derive(Debug)]
pub enum OpenRequestKind {
    FocusApp,
    DockMenuAction {
        index: usize,
    },
    Setting {
        setting_path: Option<String>,
    },
}

impl OpenRequest {
    pub fn is_focus_app_only(&self) -> bool {
        matches!(self.kind, Some(OpenRequestKind::FocusApp))
            && self.open_paths.is_empty()
            && self.diff_paths.is_empty()
    }

    pub fn parse(request: RawOpenRequest, _cx: &App) -> Result<Self> {
        let mut this = Self::default();

        this.diff_paths = request.diff_paths;
        this.diff_all = request.diff_all;
        this.dev_container = request.dev_container;

        for url in request.urls {
            if let Some(action_index) = url.strip_prefix("zed-dock-action://") {
                this.kind = Some(OpenRequestKind::DockMenuAction {
                    index: action_index.parse()?,
                });
            } else if let Some(file) = url.strip_prefix("file://") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://file") {
                this.parse_file_path(file)
            } else if url == "zed://" || url == "zed://open" || url == "zed://open/" {
                this.kind = Some(OpenRequestKind::FocusApp);
            } else if url == "zed://settings" || url == "zed://settings/" {
                this.kind = Some(OpenRequestKind::Setting { setting_path: None });
            } else if let Some(setting_path) = url.strip_prefix("zed://settings/") {
                this.kind = Some(OpenRequestKind::Setting {
                    setting_path: Some(setting_path.to_string()),
                });
            } else {
                log::error!("unhandled url: {}", url);
            }
        }

        Ok(this)
    }

    fn parse_file_path(&mut self, file: &str) {
        if let Some(decoded) = urlencoding::decode(file).log_err() {
            self.open_paths.push(decoded.into_owned())
        }
    }
}


#[derive(Clone)]
pub struct OpenListener(UnboundedSender<RawOpenRequest>);

#[derive(Default)]
pub struct RawOpenRequest {
    pub urls: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub dev_container: bool,
}

impl Global for OpenListener {}

impl OpenListener {
    pub fn new() -> (Self, UnboundedReceiver<RawOpenRequest>) {
        let (tx, rx) = mpsc::unbounded();
        (OpenListener(tx), rx)
    }

    pub fn open(&self, request: RawOpenRequest) {
        self.0
            .unbounded_send(request)
            .context("no listener for open requests")
            .log_err();
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn listen_for_cli_connections(opener: OpenListener) -> Result<()> {
    use release_channel::RELEASE_CHANNEL_NAME;
    use std::os::unix::net::UnixDatagram;

    let sock_path = paths::data_dir().join(format!("zed-{}.sock", *RELEASE_CHANNEL_NAME));
    if let Err(e) = UnixDatagram::unbound()?.connect(&sock_path)
        && e.kind() == std::io::ErrorKind::ConnectionRefused
    {
        std::fs::remove_file(&sock_path)?;
    }
    let listener = UnixDatagram::bind(&sock_path)?;
    thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(len) = listener.recv(&mut buf) {
            opener.open(RawOpenRequest {
                urls: vec![String::from_utf8_lossy(&buf[..len]).to_string()],
                ..Default::default()
            });
        }
    });
    Ok(())
}

pub async fn open_paths_with_positions(
    path_positions: &[PathWithPosition],
    _diff_paths: &[[String; 2]],
    _diff_all: bool,
    app_state: Arc<AppState>,
    open_options: workspace::OpenOptions,
    cx: &mut AsyncApp,
) -> Result<(
    WindowHandle<MultiWorkspace>,
    Vec<Option<Result<Box<dyn workspace::item::ItemHandle>>>>,
)> {
    let paths = path_positions
        .iter()
        .map(|path_with_position| path_with_position.path.clone())
        .collect::<Vec<_>>();

    let OpenResult {
        window: multi_workspace,
        opened_items: mut items,
        ..
    } = cx
        .update(|cx| workspace::open_paths(&paths, app_state.clone(), open_options, cx))
        .await?;

    for (item, path) in items.iter_mut().zip(&paths) {
        if let Some(Err(error)) = item {
            *error = anyhow!("error opening {path:?}: {error:#}");
        }
    }

    let items_for_navigation = items
        .iter()
        .map(|item| item.as_ref().and_then(|r| r.as_ref().ok()).cloned())
        .collect::<Vec<_>>();
    navigate_to_positions(&multi_workspace, items_for_navigation, path_positions, cx);

    Ok((multi_workspace, items))
}

pub fn open_options_for_request(
    location: &workspace::SerializedWorkspaceLocation,
    cx: &App,
) -> workspace::OpenOptions {
    let add_dirs_to_sidebar = workspace::WorkspaceSettings::get_global(cx).cli_default_open_behavior
        == settings::CliDefaultOpenBehavior::ExistingWindow;
    workspace::OpenOptions {
        workspace_matching: workspace::WorkspaceMatching::MatchExact,
        add_dirs_to_sidebar,
        requesting_window: workspace::workspace_windows_for_location(location, cx)
            .into_iter()
            .next(),
        ..Default::default()
    }
}

pub async fn derive_paths_with_position(
    fs: &dyn Fs,
    path_strings: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<PathWithPosition> {
    let path_strings: Vec<_> = path_strings.into_iter().collect();
    let mut result = Vec::with_capacity(path_strings.len());
    for path_str in path_strings {
        let original_path = Path::new(path_str.as_ref());
        let mut parsed = PathWithPosition::parse_str(path_str.as_ref());

        if !cfg!(windows)
            && parsed.row.is_some()
            && parsed.path != original_path
            && fs.is_file(original_path).await
        {
            parsed = PathWithPosition::from_path(original_path.to_path_buf());
        }

        if let Ok(canonicalized) = fs.canonicalize(&parsed.path).await {
            parsed.path = canonicalized;
        }

        result.push(parsed);
    }
    result
}

fn navigate_to_positions(
    _window: &WindowHandle<MultiWorkspace>,
    _items: impl IntoIterator<Item = Option<Box<dyn workspace::item::ItemHandle>>>,
    _positions: &[PathWithPosition],
    _cx: &mut AsyncApp,
) {
}
