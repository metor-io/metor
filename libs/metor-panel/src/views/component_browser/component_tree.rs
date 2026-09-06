use std::cmp::Ordering;
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
    pub children: Children,
}

/// A node's children in natural segment order, so `slot2` lists before
/// `slot10` everywhere the tree is drawn. Frozen once built: the tree is a
/// snapshot, and every derived tree (pruned, compressed, synthetic) is
/// collected fresh.
#[derive(Clone, Default)]
pub struct Children(Vec<Arc<ComponentNode>>);

impl Children {
    pub fn get(&self, segment: &str) -> Option<&Arc<ComponentNode>> {
        self.0
            .binary_search_by(|c| natural_cmp(&c.segment, segment))
            .ok()
            .map(|ix| &self.0[ix])
    }

    pub fn values(&self) -> std::slice::Iter<'_, Arc<ComponentNode>> {
        self.0.iter()
    }

    pub fn first(&self) -> Option<&Arc<ComponentNode>> {
        self.0.first()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Arc<ComponentNode>> for Children {
    fn from_iter<I: IntoIterator<Item = Arc<ComponentNode>>>(iter: I) -> Self {
        let mut nodes: Vec<Arc<ComponentNode>> = iter.into_iter().collect();
        nodes.sort_by(|a, b| natural_cmp(&a.segment, &b.segment));
        Self(nodes)
    }
}

/// Natural order: runs of digits compare by value, everything else
/// bytewise, so `slot1 < slot2 < slot10`. Strings whose runs all tie
/// (`wheel_2` and `wheel_02`) fall back to bytewise order, keeping the
/// order total.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        let (Some(&cx), Some(&cy)) = (x.first(), y.first()) else {
            return x.len().cmp(&y.len()).then_with(|| a.cmp(b));
        };
        let ord = if cx.is_ascii_digit() && cy.is_ascii_digit() {
            let (dx, rx) = split_run(x, |c| c.is_ascii_digit());
            let (dy, ry) = split_run(y, |c| c.is_ascii_digit());
            let (dx, dy) = (trim_zeros(dx), trim_zeros(dy));
            (x, y) = (rx, ry);
            dx.len().cmp(&dy.len()).then_with(|| dx.cmp(dy))
        } else {
            let (tx, rx) = split_run(x, |c| !c.is_ascii_digit());
            let (ty, ry) = split_run(y, |c| !c.is_ascii_digit());
            (x, y) = (rx, ry);
            tx.cmp(ty)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

fn split_run(s: &[u8], pred: impl Fn(u8) -> bool) -> (&[u8], &[u8]) {
    let end = s.iter().position(|&c| !pred(c)).unwrap_or(s.len());
    s.split_at(end)
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits
        .iter()
        .position(|&c| c != b'0')
        .unwrap_or(digits.len());
    &digits[start..]
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
    Arc::new(ComponentNode {
        segment: raw.segment,
        full_name: raw.full_name,
        component_id: raw.component_id,
        children: raw
            .children
            .values()
            .map(|c| compress_subtree(c.clone()))
            .collect(),
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
        children: compressed.into_iter().collect(),
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

/// The nodes from `root` down to the one named `name`, matched by full
/// path so a compressed chain resolves the same as an expanded one. The
/// chain ends on that node when it exists; otherwise it stops at the last
/// prefix found, so a caller checks the tail's name to know.
pub fn chain_to(root: &Arc<ComponentNode>, name: &str) -> SmallVec<[Arc<ComponentNode>; 8]> {
    let mut out = SmallVec::new();
    let mut current = root.clone();
    while current.full_name.as_ref() != name {
        let next = current
            .children
            .values()
            .find(|c| under(&c.full_name, name))
            .cloned();
        let Some(next) = next else {
            break;
        };
        out.push(next.clone());
        current = next;
    }
    out
}

/// Whether `name` is `prefix` itself or a path beneath it; everything is
/// under the empty prefix.
pub fn under(prefix: &str, name: &str) -> bool {
    prefix.is_empty()
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
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
                .into_values()
                .map(|v| Arc::new(v.freeze()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted<'a>(names: &[&'a str]) -> Vec<&'a str> {
        let mut v = names.to_vec();
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn digit_runs_compare_by_value() {
        assert_eq!(
            sorted(&["slot10", "slot2", "slot1", "slot11"]),
            ["slot1", "slot2", "slot10", "slot11"]
        );
    }

    #[test]
    fn leading_zeros_tie_then_fall_back_to_bytes() {
        assert_eq!(natural_cmp("wheel_2", "wheel_02"), Ordering::Greater);
        assert_eq!(natural_cmp("wheel_02", "wheel_2"), Ordering::Less);
        assert_eq!(natural_cmp("a", "a"), Ordering::Equal);
    }

    #[test]
    fn text_runs_stay_bytewise() {
        assert_eq!(sorted(&["b", "B", "a1", "a"]), ["B", "a", "a1", "b"]);
        assert_eq!(sorted(&["x9y", "x10", "x9"]), ["x9", "x9y", "x10"]);
    }

    #[test]
    fn chain_to_walks_by_full_path_through_compressed_segments() {
        let node = |seg: &str, name: &str, children: Children| {
            Arc::new(ComponentNode {
                segment: SharedString::from(seg.to_string()),
                full_name: SharedString::from(name.to_string()),
                component_id: None,
                children,
            })
        };
        let tree = node(
            "",
            "",
            [node(
                "sat.wheels",
                "sat.wheels",
                [node("0", "sat.wheels.0", Children::default())]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
        );
        let names = |chain: SmallVec<[Arc<ComponentNode>; 8]>| -> Vec<String> {
            chain.iter().map(|n| n.full_name.to_string()).collect()
        };
        assert_eq!(
            names(chain_to(&tree, "sat.wheels.0")),
            ["sat.wheels", "sat.wheels.0"]
        );
        assert_eq!(names(chain_to(&tree, "sat.wheels")), ["sat.wheels"]);
        assert_eq!(names(chain_to(&tree, "sat.wheels.9")), ["sat.wheels"]);
        assert!(chain_to(&tree, "other").is_empty());
        assert!(under("sat", "sat.wheels"));
        assert!(under("sat", "sat"));
        assert!(!under("sat", "satellite"));
        assert!(under("", "sat"));
    }

    #[test]
    fn children_look_up_by_segment_in_natural_order() {
        let leaf = |seg: &str| {
            Arc::new(ComponentNode {
                segment: SharedString::from(seg.to_string()),
                full_name: SharedString::from(seg.to_string()),
                component_id: None,
                children: Children::default(),
            })
        };
        let children: Children = ["w10", "w2", "w1"].into_iter().map(leaf).collect();
        let order: Vec<&str> = children.values().map(|c| c.segment.as_ref()).collect();
        assert_eq!(order, ["w1", "w2", "w10"]);
        assert_eq!(children.get("w10").map(|c| c.segment.as_ref()), Some("w10"));
        assert!(children.get("w3").is_none());
        assert_eq!(children.first().map(|c| c.segment.as_ref()), Some("w1"));
    }
}
