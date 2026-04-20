use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::SharedString;
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

/// Node in the dot-delimited component namespace tree.
///
/// A node can be both a component and a parent: the name `foo.bar` makes
/// `foo` a branch even when `foo` itself is registered as a component,
/// which is why `component_id` and `children` can coexist.
pub struct ComponentNode {
    pub segment: SharedString,
    pub full_name: SharedString,
    pub component_id: Option<ComponentId>,
    /// Children keyed by segment; `BTreeMap` keeps iteration order stable
    /// so columns don't shuffle between renders.
    pub children: BTreeMap<SharedString, Arc<ComponentNode>>,
}

/// Snapshot every component in `db` into a tree rooted at a synthetic node.
///
/// The synthetic root has empty `segment` and `full_name` and no
/// `component_id`; its children are the first-level namespace segments.
pub fn build_tree(db: &DB) -> Arc<ComponentNode> {
    let mut root = Builder::new(SharedString::new_static(""), String::new());
    db.with_state(|state| {
        for (id, meta) in state.component_metadata_iter() {
            insert(&mut root, &meta.name, *id);
        }
    });
    Arc::new(root.freeze())
}

/// Resolve `path` to the chain of nodes reached, stopping at the first
/// missing segment. The returned chain has at most `path.len()` entries.
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
