use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Global};

use super::node::{DynamicNode, NodeId};

/// Owns live nodes by [`NodeId`]. Reconciliation is set-diff: pass the alive
/// id set; everything else is dropped (cancelling its task).
///
/// In Phase 1 this is exercised programmatically. The future node editor
/// will compute the alive set from its graph and call [`reconcile`].
#[derive(Default)]
pub struct DynamicRegistry {
    nodes: HashMap<NodeId, Arc<dyn DynamicNode>>,
}

impl Global for DynamicRegistry {}

impl DynamicRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init(cx: &mut App) {
        cx.set_global(Self::new());
    }

    pub fn get(&self, id: NodeId) -> Option<Arc<dyn DynamicNode>> {
        self.nodes.get(&id).cloned()
    }

    pub fn insert(&mut self, node: Arc<dyn DynamicNode>) {
        self.nodes.insert(node.id(), node);
    }

    pub fn get_or_build(
        &mut self,
        id: NodeId,
        build: impl FnOnce() -> Arc<dyn DynamicNode>,
    ) -> Arc<dyn DynamicNode> {
        if let Some(existing) = self.nodes.get(&id) {
            return existing.clone();
        }
        let node = build();
        debug_assert_eq!(node.id(), id, "build closure produced a node with a different id");
        self.nodes.insert(id, node.clone());
        node
    }

    /// Drop every node whose id is not in `alive`. Cancels their tasks via
    /// `JoinHandleDropGuard` (assuming no external strong refs are held).
    pub fn reconcile(&mut self, alive: &HashSet<NodeId>) {
        self.nodes.retain(|id, _| alive.contains(id));
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}
