//! The view model of an instance as `somsrv` keeps it: every `ViewOp` the
//! plugin makes is applied here first, so a Som that attaches later gets the
//! whole view in one `Snapshot`.

use srp_client::plugin::{Block, BlockId, ViewModel, ViewOp};

#[derive(Debug, Default)]
pub struct Model {
    pub view: ViewModel,
    pub(super) next_id: BlockId,
}

impl Model {
    pub fn next_block_id(&mut self) -> BlockId {
        self.next_id += 1;
        self.next_id
    }

    /// Applies `op`. Fails (and changes nothing) for a block id that is not
    /// in the view, so a plugin's mistake reaches it as a Lua error.
    pub fn apply(&mut self, op: &ViewOp) -> anyhow::Result<()> {
        match op {
            ViewOp::Snapshot(view) => self.view = view.clone(),
            ViewOp::Append { parent, block } => match parent {
                None => self.view.blocks.push(block.clone()),
                Some(parent) => find_mut(&mut self.view.blocks, *parent)
                    .ok_or_else(|| anyhow::anyhow!("no block {parent}"))?
                    .children
                    .push(block.clone()),
            },
            ViewOp::Update { id, props } => {
                let block = find_mut(&mut self.view.blocks, *id).ok_or_else(|| anyhow::anyhow!("no block {id}"))?;
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
                if !remove(&mut self.view.blocks, *id) {
                    anyhow::bail!("no block {id}");
                }
            }
            ViewOp::Clear => self.view.blocks.clear(),
            ViewOp::Header(header) => self.view.header = header.clone(),
            ViewOp::Input(input) => self.view.input = input.clone(),
            ViewOp::ScrollTo { id } => {
                if find_mut(&mut self.view.blocks, *id).is_none() {
                    anyhow::bail!("no block {id}");
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

    pub fn set_collapsed(&mut self, id: BlockId, collapsed: bool) {
        if let Some(block) = find_mut(&mut self.view.blocks, id) {
            block.collapsed = collapsed;
        }
    }
}

fn find_mut(blocks: &mut [Block], id: BlockId) -> Option<&mut Block> {
    for block in blocks {
        if block.id == id {
            return Some(block);
        }
        if let Some(found) = find_mut(&mut block.children, id) {
            return Some(found);
        }
    }
    None
}

fn remove(blocks: &mut Vec<Block>, id: BlockId) -> bool {
    if let Some(index) = blocks.iter().position(|block| block.id == id) {
        blocks.remove(index);
        return true;
    }
    blocks.iter_mut().any(|block| remove(&mut block.children, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(model: &mut Model, kind: &str, props: serde_json::Value) -> Block {
        Block { id: model.next_block_id(), kind: kind.into(), props, collapsed: false, children: Vec::new() }
    }

    #[test]
    fn append_update_remove_nested_blocks() {
        let mut model = Model::default();
        let group = block(&mut model, "group", json!({ "title": "Bash" }));
        let group_id = group.id;
        model.apply(&ViewOp::Append { parent: None, block: group }).unwrap();
        let code = block(&mut model, "code", json!({ "text": "ls" }));
        let code_id = code.id;
        model.apply(&ViewOp::Append { parent: Some(group_id), block: code }).unwrap();

        let mut props = serde_json::Map::new();
        props.insert("text".into(), json!("ls -la"));
        model.apply(&ViewOp::Update { id: code_id, props }).unwrap();
        assert_eq!(model.view.blocks[0].children[0].props, json!({ "text": "ls -la" }));

        model.apply(&ViewOp::Remove { id: code_id }).unwrap();
        assert!(model.view.blocks[0].children.is_empty());
    }

    #[test]
    fn an_unknown_block_is_an_error_and_changes_nothing() {
        let mut model = Model::default();
        assert!(model.apply(&ViewOp::Remove { id: 7 }).is_err());
        assert!(model.apply(&ViewOp::Update { id: 7, props: serde_json::Map::new() }).is_err());
        let orphan = block(&mut model, "text", json!({}));
        assert!(model.apply(&ViewOp::Append { parent: Some(7), block: orphan }).is_err());
        assert!(model.view.blocks.is_empty());
    }
}
