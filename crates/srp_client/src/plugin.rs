//! The plugin part of the `somsrv` protocol (`PLUGINS_DESIGN.md`).
//!
//! A plugin instance runs inside `somsrv` on the tab's host; Som starts it,
//! attaches to it, and draws its view. These are the types both sides share:
//! the view model the plugin builds (`ViewModel`, changed by `ViewOp`s), what
//! the user does in the panel (`ViewEvent`), and what the plugin asks of Som
//! (`SomCall`, answered with `SomReply`).

use serde::{Deserialize, Serialize};

/// The version of this protocol. A plugin's manifest names the lowest one it
/// needs (`"api"`); `somsrv` refuses to start a plugin that needs a newer one.
pub const PLUGIN_API: u32 = 1;

/// The id of a block in a view. Given by `somsrv`, unique within an instance.
pub type BlockId = u64;

/// What a plugin's panel shows: a header, a list of blocks, an input line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewModel {
    pub header: Header,
    pub blocks: Vec<Block>,
    pub input: Input,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// `idle`, `busy`, `waiting`, `error`; also the tab's badge.
    pub status: Option<String>,
    /// The last error of the plugin itself (a failed handler), set by `somsrv`.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub enabled: bool,
    pub placeholder: Option<String>,
    pub multiline: bool,
}

/// One block of a view. `kind` decides how Som draws `props` (see the table
/// in `PLUGINS_DESIGN.md` section 4); a kind Som does not know is drawn as text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub kind: String,
    pub props: serde_json::Value,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub children: Vec<Block>,
}

impl ViewModel {
    /// Applies `op`; both sides use this, `somsrv` on its model and Som on
    /// what it draws. Fails for a block id that is not in the view (a single
    /// op then changes nothing).
    pub fn apply(&mut self, op: &ViewOp) -> Result<(), String> {
        match op {
            ViewOp::Snapshot(view) => *self = view.clone(),
            ViewOp::Append { parent, block } => match parent {
                None => self.blocks.push(block.clone()),
                Some(parent) => self.block_mut(*parent).ok_or_else(|| format!("no block {parent}"))?.children.push(block.clone()),
            },
            ViewOp::Update { id, props } => {
                let block = self.block_mut(*id).ok_or_else(|| format!("no block {id}"))?;
                if !block.props.is_object() {
                    block.props = serde_json::Value::Object(serde_json::Map::new());
                }
                if let Some(target) = block.props.as_object_mut() {
                    for (key, value) in props {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            ViewOp::Remove { id } => {
                if !remove_block(&mut self.blocks, *id) {
                    return Err(format!("no block {id}"));
                }
            }
            ViewOp::Clear => self.blocks.clear(),
            ViewOp::Header(header) => self.header = header.clone(),
            ViewOp::Input(input) => self.input = input.clone(),
            ViewOp::ScrollTo { id } => {
                if self.block_mut(*id).is_none() {
                    return Err(format!("no block {id}"));
                }
            }
            ViewOp::Batch(ops) => {
                for op in ops {
                    self.apply(op)?;
                }
            }
        }
        Ok(())
    }

    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        find_block(&mut self.blocks, id)
    }
}

fn find_block(blocks: &mut [Block], id: BlockId) -> Option<&mut Block> {
    for block in blocks {
        if block.id == id {
            return Some(block);
        }
        if let Some(found) = find_block(&mut block.children, id) {
            return Some(found);
        }
    }
    None
}

fn remove_block(blocks: &mut Vec<Block>, id: BlockId) -> bool {
    if let Some(index) = blocks.iter().position(|block| block.id == id) {
        blocks.remove(index);
        return true;
    }
    blocks.iter_mut().any(|block| remove_block(&mut block.children, id))
}

/// A change to a view, `somsrv` -> Som.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewOp {
    /// The whole view: the first thing an attached Som gets.
    Snapshot(ViewModel),
    /// A new block at the end of `parent`'s children (the top level when `None`).
    Append { parent: Option<BlockId>, block: Block },
    /// Sets the given keys of a block's props (the others are kept).
    Update { id: BlockId, props: serde_json::Map<String, serde_json::Value> },
    Remove { id: BlockId },
    Clear,
    Header(Header),
    Input(Input),
    ScrollTo { id: BlockId },
    /// Several changes Som applies in one frame (`ui.batch`).
    Batch(Vec<ViewOp>),
}

/// What the user did in the panel, Som -> `somsrv`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewEvent {
    Action { block_id: BlockId, button_id: String },
    Submit { text: String },
    Interrupt,
    /// A group was collapsed or expanded; kept in the model so a reattached
    /// Som shows it the same way. Not passed to the plugin.
    Toggle { id: BlockId, collapsed: bool },
    /// One of the manifest's `commands`.
    Command { name: String },
}

/// What a plugin asks Som to do (`som.*`), `somsrv` -> Som.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SomCall {
    /// A terminal tab on the same host.
    OpenTerminal { cmd: Vec<String>, cwd: Option<String> },
    Notify { title: String, body: Option<String> },
    /// Shows a file of the host through SRP.
    Preview { path: String },
    ClipboardWrite { text: String },
    /// A modal question; answered with the index of the chosen button.
    Confirm { text: String, buttons: Vec<String> },
    OpenUrl { url: String },
    OpenPlugin { name: String, config: serde_json::Value },
}

impl SomCall {
    /// The manifest permission the call needs, if any.
    pub fn permission(&self) -> Option<&'static str> {
        match self {
            SomCall::OpenTerminal { .. } => Some("terminal"),
            SomCall::Notify { .. } => Some("notify"),
            SomCall::Preview { .. } => Some("preview"),
            SomCall::ClipboardWrite { .. } => Some("clipboard"),
            SomCall::OpenUrl { .. } => Some("open_url"),
            SomCall::Confirm { .. } | SomCall::OpenPlugin { .. } => None,
        }
    }
}

/// Som's answer to a `SomCall`. `Err` carries `denied`, `unsupported`,
/// `detached` or a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SomReply {
    Ok(serde_json::Value),
    Err(String),
}

/// One running instance, as `ListPlugins` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub instance: String,
    pub plugin: String,
    pub version: String,
    pub cwd: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub attached: bool,
}
