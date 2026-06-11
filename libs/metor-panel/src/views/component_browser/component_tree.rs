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
/// Single-child chains are collapsed via [`compress_subtree`] so a long
/// non-branching prefix (`cube_sat.sim.reaction_wheels.…`) renders as
/// one row instead of a column-per-segment drill-down.
pub fn build_tree(db: &DB) -> Arc<ComponentNode> {
    let mut root = Builder::new(SharedString::new_static(""), String::new());
    db.with_state(|state| {
        for (id, meta) in state.component_metadata_iter() {
            // DB-internal components (derived LoD series) stay queryable
            // but never list in the browser.
            if meta.is_hidden() {
                continue;
            }
            insert(&mut root, &meta.name, *id);
        }
    });
    let raw = root.freeze();
    let compressed_children: BTreeMap<SharedString, Arc<ComponentNode>> = raw
        .children
        .values()
        .map(|c| compress_subtree(c.clone()))
        .map(|c| (c.segment.clone(), c))
        .collect();
    Arc::new(ComponentNode {
        segment: raw.segment,
        full_name: raw.full_name,
        component_id: raw.component_id,
        children: compressed_children,
    })
}

/// Post-order fuse a non-component branch with its only surviving child.
///
/// A node collapses into its child only when it has no `component_id`
/// of its own (a real component registered on a non-leaf name must
/// stay clickable) and exactly one child after the child's own
/// compression. The fused node adopts the child's `full_name`,
/// `component_id`, and `children` and uses `"parent.child"` as its
/// segment so the column browser displays the full collapsed prefix.
pub(crate) fn compress_subtree(node: Arc<ComponentNode>) -> Arc<ComponentNode> {
    let compressed: Vec<Arc<ComponentNode>> = node
        .children
        .values()
        .map(|c| compress_subtree(c.clone()))
        .collect();

    if node.component_id.is_none() && compressed.len() == 1 {
        let child = &compressed[0];
        return Arc::new(ComponentNode {
            segment: SharedString::from(format!("{}.{}", node.segment, child.segment)),
            full_name: child.full_name.clone(),
            component_id: child.component_id,
            children: child.children.clone(),
        });
    }

    Arc::new(ComponentNode {
        segment: node.segment.clone(),
        full_name: node.full_name.clone(),
        component_id: node.component_id,
        children: compressed
            .into_iter()
            .map(|c| (c.segment.clone(), c))
            .collect(),
    })
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
