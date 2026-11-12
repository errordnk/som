//! Som's side of plugins (`PLUGINS_DESIGN.md`): loading a plugin from
//! `~/.config/som/plugins`, starting it in the daemon of the active tab's
//! machine (through the tab's link for an ssh/wsl tab), and the panel that
//! shows it.
//!
//! Opened with `plugin_view::OpenPlugin { name }`, bound in settings.json's
//! `keys` as `"Plugin:<name>"`.

mod client;
mod loader;
mod view;

use std::collections::HashMap;

use gpui::{Action, App, AppContext as _, Global, Window};
use schemars::JsonSchema;
use serde::Deserialize;
use terminal_view::TerminalView;
use workspace::Workspace;

pub use view::PluginView;

/// Opens the plugin `name` in a new tab, on the machine of the active tab.
#[derive(PartialEq, Clone, Deserialize, JsonSchema, Action)]
#[action(namespace = plugin_view)]
#[serde(deny_unknown_fields)]
pub struct OpenPlugin {
    pub name: String,
}

/// settings.json's `plugins` section: each plugin's own settings, passed to
/// it as `ctx.config`. Som does not read them.
#[derive(Default)]
pub struct PluginConfigs(pub HashMap<String, serde_json::Value>);

impl Global for PluginConfigs {}

pub fn set_configs(configs: HashMap<String, serde_json::Value>, cx: &mut App) {
    cx.set_global(PluginConfigs(configs));
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(open_plugin);
    })
    .detach();
}

fn open_plugin(workspace: &mut Workspace, action: &OpenPlugin, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let plugin = match loader::load(&action.name) {
        Ok(plugin) => plugin,
        Err(err) => {
            log::warn!("opening plugin {}: {err:#}", action.name);
            workspace.show_error(&err, cx);
            return;
        }
    };
    // The active tab decides the machine: its link reaches that machine's
    // daemon; a local tab has none and uses this machine's.
    let (link, cwd) = workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<TerminalView>())
        .map(|view| {
            let terminal = view.read(cx).terminal().read(cx);
            (terminal.srv_link(), terminal.working_directory())
        })
        .unwrap_or((None, None));
    let cwd = cwd.map(|cwd| cwd.to_string_lossy().into_owned()).unwrap_or_default();
    let config = cx
        .try_global::<PluginConfigs>()
        .and_then(|configs| configs.0.get(&action.name).cloned())
        .unwrap_or_else(|| serde_json::json!({}));
    let view = cx.new(|cx| PluginView::new(plugin, link, cwd, config, window, cx));
    workspace.add_item_to_main_pane(Box::new(view), None, window, cx);
}
