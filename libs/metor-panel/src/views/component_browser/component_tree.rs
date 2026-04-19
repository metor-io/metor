use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::SharedString;
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

/// A node in the dot-delimited component namespace tree.
///
/// Each component name like `cube_sat.fsw.imu.accel` contributes a chain of
/// nodes: `cube_sat`, `fsw`, `imu`, `accel`. A node is a *leaf* when it has a
/// real component (`component_id.is_some()`) and no further children. A node
/// can be both a component *and* a branch when a component name is a prefix
/// of another component's name.
pub struct ComponentNode {
    /// Last path segment (e.g. `"imu"`).
    pub segment: SharedString,
    /// Full dot-delimited path from the root (e.g. `"cube_sat.fsw.imu"`).
    pub full_name: SharedString,
    /// `Some` when this node corresponds to a real component.
    pub component_id: Option<ComponentId>,
    /// Direct children keyed by their segment, ordered for stable rendering.
    pub children: BTreeMap<SharedString, Arc<ComponentNode>>,
}

/// Build a root node whose children are the top-level namespace segments.
///
/// The returned root is synthetic (empty segment / full name, no `component_id`).
pub fn build_tree(db: &DB) -> Arc<ComponentNode> {
    let mut root = Builder::new(SharedString::new_static(""), String::new());
    db.with_state(|state| {
        for (id, meta) in state.component_metadata_iter() {
            insert(&mut root, &meta.name, *id);
        }
    });
    Arc::new(root.freeze())
}

/// Walk the tree one segment at a time, returning the chain of resolved nodes.
/// Truncates at the first missing segment.
pub fn resolve_path(
    root: &Arc<ComponentNode>,
    path: &[SharedString],
) -> SmallVec<[Arc<ComponentNode>; 8]> {
    let mut out = SmallVec::new();
    let mut current: Arc<ComponentNode> = root.clone();
    for seg in path {
        let Some(next) = current.children.get(seg).cloned() else {
            break;
        };
        out.push(next.clone());
        current = next;
    }
    out
}

struct Builder {
    segment: SharedString,
    full_name: String,
    component_id: Option<ComponentId>,
    children: BTreeMap<SharedString, Builder>,
}

impl Builder {
    fn new(segment: SharedString, full_name: String) -> Self {
        Self {
            segment,
            full_name,
            component_id: None,
            children: BTreeMap::new(),
        }
    }

    fn freeze(self) -> ComponentNode {
        ComponentNode {
            segment: self.segment,
            full_name: SharedString::from(self.full_name),
            component_id: self.component_id,
            children: self
                .children
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v.freeze())))
                .collect(),
        }
    }
}

fn insert(root: &mut Builder, full_name: &str, id: ComponentId) {
    let mut current = root;
    let mut accumulated = String::new();
    let segments: Vec<&str> = full_name.split('.').collect();
    if segments.is_empty() {
        return;
    }
    let last_ix = segments.len() - 1;
    for (ix, seg) in segments.iter().enumerate() {
        if ix > 0 {
            accumulated.push('.');
        }
        accumulated.push_str(seg);
        let seg_ss = SharedString::from(seg.to_string());
        let acc = accumulated.clone();
        current = current
            .children
            .entry(seg_ss.clone())
            .or_insert_with(|| Builder::new(seg_ss, acc));
        if ix == last_ix {
            current.component_id = Some(id);
        }
    }
}
