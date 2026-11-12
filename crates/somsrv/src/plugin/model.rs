//! The view model of an instance as `somsrv` keeps it: every `ViewOp` the
//! plugin makes is applied here first, so a Som that attaches later gets the
//! whole view in one `Snapshot`.

use srp_client::plugin::{BlockId, ViewModel, ViewOp};

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

    /// Applies `op` (`ViewModel::apply`); a plugin's mistake reaches it as a Lua error.
    pub fn apply(&mut self, op: &ViewOp) -> anyhow::Result<()> {
        self.view.apply(op).map_err(|err| anyhow::anyhow!(err))
    }

    pub fn set_collapsed(&mut self, id: BlockId, collapsed: bool) {
        if let Some(block) = self.view.block_mut(id) {
            block.collapsed = collapsed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use srp_client::plugin::Block;

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
