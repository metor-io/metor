use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Global};

use super::node::{DynamicNode, NodeId};

/// Owns live nodes by [`NodeId`]. Reconciliation is set-diff: pass the alive
/// id set; everything else is dropped (cancelling its task).
///
/// The node editor computes each pane's alive set from its graph; the
/// `GraphCoordinator` unions those sets and calls [`reconcile`](Self::reconcile).
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

    /// Drop every node whose id is not in `alive`. Returns the removed
    /// `Arc`s so the caller can choose where to actually drop them — for
    /// instance, sending them to the stellarator worker thread to avoid a
    /// cross-thread spinlock deadlock when a `Sleep`-based task (e.g.
    /// `fixed_rate`) is dropped on a thread that doesn't own the timer.
    ///
    /// Most callers can ignore the return value; the legacy behavior (drop
    /// in place on the calling thread) is just `let _ = registry.reconcile(&alive);`.
    pub fn reconcile(&mut self, alive: &HashSet<NodeId>) -> Vec<Arc<dyn DynamicNode>> {
        let mut removed: Vec<Arc<dyn DynamicNode>> = Vec::new();
        self.nodes.retain(|id, node| {
            if alive.contains(id) {
                true
            } else {
                removed.push(node.clone());
                false
            }
        });
        removed
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}
