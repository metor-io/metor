//! The outline's row model: the namespace tree flattened to the rows the
//! user has disclosed.
//!
//! Disclosure follows [`JsonTree`](crate::views::JsonTree): a branch is open
//! by default down to [`DEFAULT_EXPAND_DEPTH`], and one set holds every path
//! whose state differs from that default — so a fresh outline persists as an
//! empty set, and toggling is symmetric in both directions. A filter query
//! overrides all of it: the tree is pruned to matches and shown fully open,
//! since a search that hid its hits behind folded branches would be useless.
//!
//! A branch can also be *pivoted*: its branch children become instances
//! (rows) and the union of their leaf paths becomes fields (cells), which is
//! how four reaction wheels read as a grid instead of four folded subtrees.
//! Alike siblings are detected structurally, so nothing in the FSW has to
//! opt in; siblings missing a field simply get an empty cell.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use gpui::SharedString;
use metor_proto::types::ComponentId;

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
    pub kind: RowKind,
}

/// What a row stands for beyond its node.
#[derive(Clone)]
pub(crate) enum RowKind {
    /// A plain tree node, `node` being the one it shows.
    Node,
    /// A pivoted branch; `node` is the branch.
    PivotBranch(Arc<Pivot>),
    /// The field labels above a pivot's instances; `node` is the branch.
    PivotHeader(Arc<Pivot>),
    /// One instance of a pivot; `node` is the instance's own subtree root.
    PivotInstance { pivot: Arc<Pivot>, ix: usize },
}

impl OutlineRow {
    pub fn is_branch(&self) -> bool {
        !self.node.children.is_empty()
    }

    pub fn is_pivoted(&self) -> bool {
        matches!(self.kind, RowKind::PivotBranch(_))
    }
}

/// A branch rotated into instances × fields.
pub(crate) struct Pivot {
    /// Leaf paths relative to an instance, in name order.
    pub fields: Vec<SharedString>,
    /// Widest strip each field needs, in element cells.
    pub cells: Vec<usize>,
    pub instances: Vec<PivotInstance>,
}

pub(crate) struct PivotInstance {
    pub node: Arc<ComponentNode>,
    /// One slot per field; `None` where this instance lacks it.
    pub ids: Vec<Option<ComponentId>>,
}

/// Rotate `branch`: every child that is itself a branch becomes an instance.
/// `None` when nothing qualifies, so a pivot on a flat branch is a no-op
/// rather than an empty grid.
pub(crate) fn build_pivot(
    branch: &ComponentNode,
    cell_count: &dyn Fn(ComponentId) -> usize,
) -> Option<Pivot> {
    let candidates: Vec<&Arc<ComponentNode>> = branch
        .children
        .values()
        .filter(|c| !c.children.is_empty())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let per_instance: Vec<BTreeMap<SharedString, ComponentId>> = candidates
        .iter()
        .map(|inst| {
            let mut leaves = BTreeMap::new();
            collect_leaves(inst, inst.full_name.len() + 1, &mut leaves);
            leaves
        })
        .collect();
    let fields: Vec<SharedString> = per_instance
        .iter()
        .flat_map(|m| m.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let instances: Vec<PivotInstance> = candidates
        .iter()
        .zip(&per_instance)
        .map(|(node, leaves)| PivotInstance {
            node: (*node).clone(),
            ids: fields.iter().map(|f| leaves.get(f).copied()).collect(),
        })
        .collect();
    let cells = (0..fields.len())
        .map(|f| {
            instances
                .iter()
                .filter_map(|i| i.ids[f])
                .map(cell_count)
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    Some(Pivot {
        fields,
        cells,
        instances,
    })
}

/// Every component under `node` keyed by its name past `prefix_len`.
fn collect_leaves(
    node: &Arc<ComponentNode>,
    prefix_len: usize,
    out: &mut BTreeMap<SharedString, ComponentId>,
) {
    if let Some(id) = node.component_id
        && node.full_name.len() > prefix_len
    {
        out.insert(
            SharedString::from(node.full_name[prefix_len..].to_string()),
            id,
        );
    }
    for child in node.children.values() {
        collect_leaves(child, prefix_len, out);
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

/// Everything that shapes the flattened rows besides the tree itself.
pub(crate) struct Layout<'a> {
    pub disclosure: &'a Disclosure,
    pub pivoted: &'a HashSet<SharedString>,
    pub query: &'a Query,
    /// Strip cells a component renders; sizes pivot columns.
    pub cell_count: &'a dyn Fn(ComponentId) -> usize,
}

/// The rows currently on screen, in display order.
///
/// `root` is the synthetic tree root, which never renders itself. With a
/// non-empty query the tree is pruned to matching names and every surviving
/// branch is open regardless of disclosure.
pub(crate) fn flatten(root: &Arc<ComponentNode>, layout: &Layout) -> Vec<OutlineRow> {
    let mut rows = Vec::new();
    let filtering = !layout.query.is_empty();
    for child in root.children.values() {
        let child = if filtering {
            match prune_to_matches(child, layout.query) {
                Some(pruned) => pruned,
                None => continue,
            }
        } else {
            child.clone()
        };
        push_rows(&child, 0, layout, filtering, &mut rows);
    }
    rows
}

fn push_rows(
    node: &Arc<ComponentNode>,
    depth: usize,
    layout: &Layout,
    force_open: bool,
    rows: &mut Vec<OutlineRow>,
) {
    let is_branch = !node.children.is_empty();
    let expanded =
        is_branch && (force_open || layout.disclosure.is_expanded(&node.full_name, depth));
    let pivot = if expanded && layout.pivoted.contains(&node.full_name) {
        build_pivot(node, layout.cell_count).map(Arc::new)
    } else {
        None
    };
    rows.push(OutlineRow {
        node: node.clone(),
        depth,
        expanded,
        component_count: component_count(node),
        kind: match &pivot {
            Some(p) => RowKind::PivotBranch(p.clone()),
            None => RowKind::Node,
        },
    });
    if !expanded {
        return;
    }
    let Some(pivot) = pivot else {
        for child in node.children.values() {
            push_rows(child, depth + 1, layout, force_open, rows);
        }
        return;
    };
    // Leaf children aren't instances; they keep their ordinary rows above
    // the grid.
    for child in node.children.values().filter(|c| c.children.is_empty()) {
        push_rows(child, depth + 1, layout, force_open, rows);
    }
    rows.push(OutlineRow {
        node: node.clone(),
        depth: depth + 1,
        expanded: false,
        component_count: 0,
        kind: RowKind::PivotHeader(pivot.clone()),
    });
    for (ix, instance) in pivot.instances.iter().enumerate() {
        rows.push(OutlineRow {
            node: instance.node.clone(),
            depth: depth + 1,
            expanded: false,
            component_count: component_count(&instance.node),
            kind: RowKind::PivotInstance {
                pivot: pivot.clone(),
                ix,
            },
        });
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

    fn rows(
        tree: &Arc<ComponentNode>,
        disclosure: &Disclosure,
        pivoted: &HashSet<SharedString>,
        query: &Query,
    ) -> Vec<OutlineRow> {
        flatten(
            tree,
            &Layout {
                disclosure,
                pivoted,
                query,
                cell_count: &|_| 1,
            },
        )
    }

    fn plain(tree: &Arc<ComponentNode>, disclosure: &Disclosure, query: &Query) -> Vec<OutlineRow> {
        rows(tree, disclosure, &HashSet::new(), query)
    }

    #[test]
    fn default_opens_only_the_top_level() {
        let rows = plain(&tree(), &Disclosure::default(), &Query::default());
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
        let rows = plain(&tree(), &disclosure, &Query::default());
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
        let rows = plain(&tree(), &disclosure, &Query::default());
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
        let rows = plain(&tree(), &disclosure, &Query::parse("gyro"));
        assert_eq!(names(&rows), ["sat", "sat.imu", "sat.imu.gyro"]);
        assert!(rows.iter().filter(|r| r.is_branch()).all(|r| r.expanded));
    }

    /// `wheels` → {`0`, `1`} × {`speed`, `motor.temp`}, plus a `count` leaf;
    /// wheel 1 lacks the temperature.
    fn wheels() -> Arc<ComponentNode> {
        branch(
            "",
            vec![branch(
                "wheels",
                vec![
                    branch(
                        "wheels.0",
                        vec![
                            leaf("wheels.0.speed"),
                            branch("wheels.0.motor", vec![leaf("wheels.0.motor.temp")]),
                        ],
                    ),
                    branch("wheels.1", vec![leaf("wheels.1.speed")]),
                    leaf("wheels.count"),
                ],
            )],
        )
    }

    #[test]
    fn pivot_unions_fields_and_leaves_gaps() {
        let tree = wheels();
        let pivot = build_pivot(tree.children.get("wheels").unwrap(), &|_| 3).unwrap();
        assert_eq!(pivot.fields, ["motor.temp", "speed"]);
        assert_eq!(pivot.cells, [3, 3]);
        assert_eq!(pivot.instances.len(), 2);
        assert!(pivot.instances[0].ids.iter().all(|id| id.is_some()));
        assert_eq!(pivot.instances[1].ids[0], None);
        assert!(pivot.instances[1].ids[1].is_some());
    }

    #[test]
    fn pivot_needs_a_branch_child() {
        let tree = tree();
        let imu = tree
            .children
            .get("sat")
            .unwrap()
            .children
            .get("imu")
            .unwrap();
        assert!(build_pivot(imu, &|_| 1).is_none());
    }

    #[test]
    fn a_pivoted_branch_lists_leaves_then_header_then_instances() {
        let pivoted: HashSet<SharedString> = ["wheels".into()].into_iter().collect();
        let rows = rows(
            &wheels(),
            &Disclosure::default(),
            &pivoted,
            &Query::default(),
        );
        assert_eq!(
            names(&rows),
            ["wheels", "wheels.count", "wheels", "wheels.0", "wheels.1"]
        );
        assert!(rows[0].is_pivoted());
        assert!(matches!(rows[2].kind, RowKind::PivotHeader(_)));
        assert!(matches!(rows[3].kind, RowKind::PivotInstance { ix: 0, .. }));
        assert!(matches!(rows[4].kind, RowKind::PivotInstance { ix: 1, .. }));
    }

    #[test]
    fn a_folded_pivot_hides_its_grid() {
        let pivoted: HashSet<SharedString> = ["wheels".into()].into_iter().collect();
        let mut disclosure = Disclosure::default();
        disclosure.set_expanded(&"wheels".into(), 0, false);
        let rows = rows(&wheels(), &disclosure, &pivoted, &Query::default());
        assert_eq!(names(&rows), ["wheels"]);
        assert!(!rows[0].is_pivoted());
    }
}
