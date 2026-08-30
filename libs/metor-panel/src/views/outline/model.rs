//! The outline's row model: the namespace tree flattened to the rows the
//! user has disclosed.
//!
//! Disclosure follows [`JsonTree`](crate::views::JsonTree): a branch is open
//! by default down to [`DEFAULT_EXPAND_DEPTH`], and one set holds every path
//! whose state differs from that default — so a fresh outline persists as an
//! empty set, and toggling is symmetric in both directions. A filter query
//! overrides all of it: the tree is pruned to matches and shown fully open,
//! since a search that hid its hits behind folded branches would be useless.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::SharedString;

use crate::query::Query;
use crate::views::component_browser::component_tree::ComponentNode;
use crate::views::component_browser::prune_to_matches;

/// Depth below which branches start open: the top-level namespaces are
/// disclosed and everything beneath them is folded.
pub(crate) const DEFAULT_EXPAND_DEPTH: usize = 1;

/// One visible row of the outline.
#[derive(Clone)]
pub(crate) struct OutlineRow {
    pub node: Arc<ComponentNode>,
    pub depth: usize,
    /// Open branch; `false` for a folded branch or a leaf.
    pub expanded: bool,
    /// Components at or below this node, the node itself included.
    pub component_count: usize,
}

impl OutlineRow {
    pub fn is_branch(&self) -> bool {
        !self.node.children.is_empty()
    }
}

/// Which branches are open, as the set of paths flipped away from the depth
/// default.
#[derive(Default, Clone)]
pub(crate) struct Disclosure {
    toggled: HashSet<SharedString>,
}

impl Disclosure {
    pub fn from_paths<I: IntoIterator<Item = String>>(paths: I) -> Self {
        Self {
            toggled: paths.into_iter().map(SharedString::from).collect(),
        }
    }

    /// The flipped paths, sorted so the persisted form is stable.
    pub fn toggled_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self.toggled.iter().map(|p| p.to_string()).collect();
        out.sort();
        out
    }

    pub fn is_expanded(&self, path: &str, depth: usize) -> bool {
        (depth < DEFAULT_EXPAND_DEPTH) ^ self.toggled.contains(path)
    }

    pub fn toggle(&mut self, path: &SharedString) {
        if !self.toggled.remove(path) {
            self.toggled.insert(path.clone());
        }
    }

    pub fn set_expanded(&mut self, path: &SharedString, depth: usize, expanded: bool) {
        if self.is_expanded(path, depth) != expanded {
            self.toggle(path);
        }
    }

    /// Open or fold every branch at and below `node`.
    pub fn set_subtree(&mut self, node: &Arc<ComponentNode>, depth: usize, expanded: bool) {
        if node.children.is_empty() {
            return;
        }
        self.set_expanded(&node.full_name, depth, expanded);
        for child in node.children.values() {
            self.set_subtree(child, depth + 1, expanded);
        }
    }
}

/// Components at or below `node`, the node itself included.
pub(crate) fn component_count(node: &ComponentNode) -> usize {
    usize::from(node.component_id.is_some())
        + node
            .children
            .values()
            .map(|c| component_count(c))
            .sum::<usize>()
}

/// The rows currently on screen, in display order.
///
/// `root` is the synthetic tree root, which never renders itself. With a
/// non-empty `query` the tree is pruned to matching names and every surviving
/// branch is open regardless of `disclosure`.
pub(crate) fn flatten(
    root: &Arc<ComponentNode>,
    disclosure: &Disclosure,
    query: &Query,
) -> Vec<OutlineRow> {
    let mut rows = Vec::new();
    let filtering = !query.is_empty();
    for child in root.children.values() {
        let child = if filtering {
            match prune_to_matches(child, query) {
                Some(pruned) => pruned,
                None => continue,
            }
        } else {
            child.clone()
        };
        push_rows(&child, 0, disclosure, filtering, &mut rows);
    }
    rows
}

fn push_rows(
    node: &Arc<ComponentNode>,
    depth: usize,
    disclosure: &Disclosure,
    force_open: bool,
    rows: &mut Vec<OutlineRow>,
) {
    let is_branch = !node.children.is_empty();
    let expanded = is_branch && (force_open || disclosure.is_expanded(&node.full_name, depth));
    rows.push(OutlineRow {
        node: node.clone(),
        depth,
        expanded,
        component_count: component_count(node),
    });
    if !expanded {
        return;
    }
    for child in node.children.values() {
        push_rows(child, depth + 1, disclosure, force_open, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn leaf(name: &str) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: Some(metor_proto::types::ComponentId::new(name)),
            children: BTreeMap::new(),
        })
    }

    fn branch(name: &str, children: Vec<Arc<ComponentNode>>) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: None,
            children: children
                .into_iter()
                .map(|c| (c.segment.clone(), c))
                .collect(),
        })
    }

    /// `sat` → `imu` → {`temp`, `gyro`}; `sat` → `mode`.
    fn tree() -> Arc<ComponentNode> {
        branch(
            "",
            vec![branch(
                "sat",
                vec![
                    branch("sat.imu", vec![leaf("sat.imu.temp"), leaf("sat.imu.gyro")]),
                    leaf("sat.mode"),
                ],
            )],
        )
    }

    fn names(rows: &[OutlineRow]) -> Vec<&str> {
        rows.iter().map(|r| r.node.full_name.as_ref()).collect()
    }

    #[test]
    fn default_opens_only_the_top_level() {
        let rows = flatten(&tree(), &Disclosure::default(), &Query::default());
        assert_eq!(names(&rows), ["sat", "sat.imu", "sat.mode"]);
        assert!(rows[0].expanded);
        assert!(!rows[1].expanded);
        assert_eq!(rows[0].component_count, 3);
        assert_eq!(rows[1].component_count, 2);
    }

    #[test]
    fn toggling_a_branch_discloses_its_children() {
        let mut disclosure = Disclosure::default();
        disclosure.toggle(&"sat.imu".into());
        let rows = flatten(&tree(), &disclosure, &Query::default());
        assert_eq!(
            names(&rows),
            ["sat", "sat.imu", "sat.imu.gyro", "sat.imu.temp", "sat.mode"]
        );
        assert_eq!(disclosure.toggled_paths(), ["sat.imu"]);
    }

    #[test]
    fn folding_the_root_hides_everything_below() {
        let mut disclosure = Disclosure::default();
        disclosure.set_expanded(&"sat".into(), 0, false);
        let rows = flatten(&tree(), &disclosure, &Query::default());
        assert_eq!(names(&rows), ["sat"]);
    }

    #[test]
    fn subtree_open_and_close_round_trip_to_an_empty_set() {
        let tree = tree();
        let sat = tree.children.get("sat").unwrap();
        let mut disclosure = Disclosure::default();
        disclosure.set_subtree(sat, 0, true);
        assert_eq!(disclosure.toggled_paths(), ["sat.imu"]);
        disclosure.set_subtree(sat, 0, false);
        assert_eq!(disclosure.toggled_paths(), ["sat"]);
        disclosure.set_subtree(sat, 0, true);
        assert_eq!(disclosure.toggled_paths(), ["sat.imu"]);
    }

    #[test]
    fn a_query_prunes_and_opens_everything() {
        let mut disclosure = Disclosure::default();
        disclosure.set_expanded(&"sat".into(), 0, false);
        let rows = flatten(&tree(), &disclosure, &Query::parse("gyro"));
        assert_eq!(names(&rows), ["sat", "sat.imu", "sat.imu.gyro"]);
        assert!(rows.iter().filter(|r| r.is_branch()).all(|r| r.expanded));
    }
}
