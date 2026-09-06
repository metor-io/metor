//! The outline's row model: the namespace tree flattened to the rows the
//! user has disclosed.
//!
//! Disclosure follows [`JsonTree`](crate::views::JsonTree): a branch is open
//! by default down to [`DEFAULT_EXPAND_DEPTH`], and two sets hold the paths
//! flipped away from that default — so a fresh outline persists as nothing,
//! and a layout author names branches to open or fold without knowing their
//! depth. A filter query overrides all of it: the tree is pruned to matches
//! and shown fully open, since a search that hid its hits behind folded
//! branches would be useless.
//!
//! The outline can be *rooted* on a branch, which lists that branch's
//! children as the top level and nothing else; the root is a full path so
//! a saved layout survives the tree compressing differently. Siblings sort
//! naturally at every level, or in reverse when the user flips the name
//! column.
//!
//! A branch can also be *pivoted*: its branch children become instances
//! (rows) and the union of their leaf paths becomes fields (cells), which is
//! how four reaction wheels read as a grid instead of four folded subtrees.
//! Alike siblings are detected structurally, so nothing in the FSW has to
//! opt in; siblings missing a field simply get an empty cell. A
//! [`PivotLayout`] records how the user arranged a grid — field order,
//! hidden fields, instance order — apart from the grid itself, so it
//! survives fields and instances coming and going.
//!
//! The same grid works across the namespace: a [`FrameType`] is a shape —
//! the sorted leaf paths of a subtree — and its instances are every subtree
//! anywhere with that shape, so `DUT1.PSU` and `DUT2.PSU` land in one table
//! labelled by full path. Types show as synthetic branches above the tree,
//! and the outline can focus on one to show nothing else.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use gpui::SharedString;
use metor_proto::types::ComponentId;

use crate::query::Query;
use crate::views::component_browser::component_tree::{
    ComponentNode, chain_to, natural_cmp, under,
};
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
    /// A pivoted branch; `node` is the branch. Its row carries the grid's
    /// field labels, so the instances sit directly beneath it.
    PivotBranch(Arc<Pivot>),
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

/// A branch, or a frame type, rotated into instances × fields.
pub(crate) struct Pivot {
    /// What the grid belongs to — the branch path or the type key — so
    /// its rows can share per-grid state such as the scroll offset.
    pub key: SharedString,
    /// Leaf paths relative to an instance, in display order.
    pub fields: Vec<SharedString>,
    /// Widest strip each field needs, in element cells.
    pub cells: Vec<usize>,
    pub instances: Vec<PivotInstance>,
    /// Fields the layout hides, so the header menu can offer them back.
    pub hidden: Vec<SharedString>,
}

pub(crate) struct PivotInstance {
    pub node: Arc<ComponentNode>,
    /// Row label: the segment under a branch, the full path in a type.
    pub label: SharedString,
    /// One slot per field; `None` where this instance lacks it.
    pub ids: Vec<Option<ComponentId>>,
}

/// How the user arranged one pivot grid. Names that no longer exist are
/// kept, not pruned, so a field that comes back keeps its slot.
#[derive(Default, Clone, PartialEq, Eq)]
pub(crate) struct PivotLayout {
    /// Fields first, in this order; the rest follow in natural order.
    pub order: Vec<SharedString>,
    pub hidden: HashSet<SharedString>,
    /// Instance labels first, in this order; the rest keep their sort.
    pub rows: Vec<SharedString>,
}

impl PivotLayout {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.hidden.is_empty() && self.rows.is_empty()
    }
}

/// A shape shared by alike subtrees: their leaf paths, sorted.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FrameType {
    pub label: SharedString,
    pub fields: Vec<SharedString>,
}

/// The synthetic path a type's branch row toggles under. Namespaced so it
/// can't collide with a real component.
pub(crate) fn type_key(label: &str) -> SharedString {
    SharedString::from(format!("type:{label}"))
}

/// The name a set of alike frames share: the longest trailing run of
/// segments common to every path, so `dut1.psu` and `dut2.bay.psu` are
/// "psu" and `cube_sat.nav.health` and `cube_sat.ctrl.health` are
/// "health". Empty when nothing is shared.
pub(crate) fn common_suffix<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut names = names.into_iter();
    let Some(first) = names.next() else {
        return String::new();
    };
    let mut shared: Vec<&str> = first.split('.').collect();
    for name in names {
        let segments: Vec<&str> = name.split('.').collect();
        let common = shared
            .iter()
            .rev()
            .zip(segments.iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        shared.drain(..shared.len() - common);
    }
    shared.join(".")
}

/// The shape of `node`: its leaf paths relative to itself, sorted.
pub(crate) fn signature(node: &Arc<ComponentNode>) -> Vec<SharedString> {
    let mut leaves = BTreeMap::new();
    collect_leaves(node, node.full_name.len() + 1, &mut leaves);
    leaves.into_keys().collect()
}

/// Every subtree under `root` whose shape is exactly `fields`, in name
/// order. A match's own subtree is never searched: anything inside it has
/// fewer leaves, so it can't match too.
pub(crate) fn alike(root: &Arc<ComponentNode>, fields: &[SharedString]) -> Vec<Arc<ComponentNode>> {
    let mut out = Vec::new();
    for child in root.children.values() {
        collect_alike(child, fields, &mut out);
    }
    out
}

fn collect_alike(
    node: &Arc<ComponentNode>,
    fields: &[SharedString],
    out: &mut Vec<Arc<ComponentNode>>,
) {
    if node.children.is_empty() {
        return;
    }
    // Leaf count is a cheap pre-check before building the full signature.
    if component_count(node) - usize::from(node.component_id.is_some()) == fields.len()
        && signature(node) == fields
    {
        out.push(node.clone());
        return;
    }
    for child in node.children.values() {
        collect_alike(child, fields, out);
    }
}

/// Rotate `branch`: every child that is itself a branch becomes an instance.
/// `None` when nothing qualifies, so a pivot on a flat branch is a no-op
/// rather than an empty grid.
pub(crate) fn build_pivot(branch: &ComponentNode, layout: &Layout) -> Option<Pivot> {
    let candidates: Vec<(Arc<ComponentNode>, SharedString)> = ordered(branch, layout.descending)
        .filter(|c| !c.children.is_empty())
        .map(|c| (c.clone(), c.segment.clone()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    Some(pivot_from(branch.full_name.clone(), candidates, layout))
}

/// Lay `instances` out on the union of their fields, arranged by the
/// grid's [`PivotLayout`] when it has one.
fn pivot_from(
    key: SharedString,
    candidates: Vec<(Arc<ComponentNode>, SharedString)>,
    layout: &Layout,
) -> Pivot {
    let arrangement = layout.layouts.get(&key);
    let candidates = match arrangement {
        Some(a) => lead_by(candidates, &a.rows, |(_, label)| label),
        None => candidates,
    };
    let per_instance: Vec<BTreeMap<SharedString, ComponentId>> = candidates
        .iter()
        .map(|(inst, _)| {
            let mut leaves = BTreeMap::new();
            collect_leaves(inst, inst.full_name.len() + 1, &mut leaves);
            leaves
        })
        .collect();
    let mut fields: Vec<SharedString> = per_instance
        .iter()
        .flat_map(|m| m.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    fields.sort_by(|a, b| natural_cmp(a, b));
    let mut hidden = Vec::new();
    if let Some(a) = arrangement {
        fields = lead_by(fields, &a.order, |f| f);
        (hidden, fields) = fields.into_iter().partition(|f| a.hidden.contains(f));
    }
    let instances: Vec<PivotInstance> = candidates
        .into_iter()
        .zip(&per_instance)
        .map(|((node, label), leaves)| PivotInstance {
            node,
            label,
            ids: fields.iter().map(|f| leaves.get(f).copied()).collect(),
        })
        .collect();
    let cells = (0..fields.len())
        .map(|f| {
            instances
                .iter()
                .filter_map(|i| i.ids[f])
                .map(layout.cell_count)
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    Pivot {
        key,
        fields,
        cells,
        instances,
        hidden,
    }
}

/// `items` with those named in `lead` moved to the front in that order;
/// names in `lead` that match nothing are skipped.
fn lead_by<T>(
    mut items: Vec<T>,
    lead: &[SharedString],
    name: impl Fn(&T) -> &SharedString,
) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    for want in lead {
        if let Some(ix) = items.iter().position(|i| name(i) == want) {
            out.push(items.remove(ix));
        }
    }
    out.extend(items);
    out
}

/// `items` with `from` moved into the slot `to` occupies, the way a
/// dragged column takes the place of the one it was dropped on. Unknown
/// names leave the order unchanged.
pub(crate) fn take_slot(
    items: &[SharedString],
    from: &SharedString,
    to: &SharedString,
) -> Vec<SharedString> {
    let mut out = items.to_vec();
    let (Some(from_ix), Some(to_ix)) = (
        items.iter().position(|i| i == from),
        items.iter().position(|i| i == to),
    ) else {
        return out;
    };
    let item = out.remove(from_ix);
    out.insert(to_ix, item);
    out
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

/// Which branches are open, as the paths flipped away from the depth
/// default: `expanded` ones open below it, `collapsed` ones fold above it.
#[derive(Default, Clone)]
pub(crate) struct Disclosure {
    expanded: HashSet<SharedString>,
    collapsed: HashSet<SharedString>,
}

impl Disclosure {
    pub fn from_paths(
        expanded: impl IntoIterator<Item = String>,
        collapsed: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            expanded: expanded.into_iter().map(SharedString::from).collect(),
            collapsed: collapsed.into_iter().map(SharedString::from).collect(),
        }
    }

    /// The paths opened below the default, sorted so the persisted form
    /// is stable.
    pub fn expanded_paths(&self) -> Vec<String> {
        sorted_paths(&self.expanded)
    }

    /// The paths folded above the default, sorted.
    pub fn collapsed_paths(&self) -> Vec<String> {
        sorted_paths(&self.collapsed)
    }

    pub fn is_expanded(&self, path: &str, depth: usize) -> bool {
        if self.expanded.contains(path) {
            return true;
        }
        if self.collapsed.contains(path) {
            return false;
        }
        depth < DEFAULT_EXPAND_DEPTH
    }

    pub fn toggle(&mut self, path: &SharedString, depth: usize) {
        let open = !self.is_expanded(path, depth);
        self.set_expanded(path, depth, open);
    }

    /// Returning to the depth default clears the path from both sets.
    pub fn set_expanded(&mut self, path: &SharedString, depth: usize, expanded: bool) {
        self.expanded.remove(path);
        self.collapsed.remove(path);
        if expanded != (depth < DEFAULT_EXPAND_DEPTH) {
            let set = if expanded {
                &mut self.expanded
            } else {
                &mut self.collapsed
            };
            set.insert(path.clone());
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

fn sorted_paths(set: &HashSet<SharedString>) -> Vec<String> {
    let mut out: Vec<String> = set.iter().map(|p| p.to_string()).collect();
    out.sort();
    out
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
    /// Arrangements keyed by pivot key: a branch path or a type key.
    pub layouts: &'a HashMap<SharedString, PivotLayout>,
    /// Frame types shown as synthetic branches above the tree.
    pub types: &'a [FrameType],
    /// A type label to show alone, hiding the tree and the other types.
    pub focus: Option<&'a SharedString>,
    /// The branch whose children form the top level; empty for the whole
    /// tree.
    pub root: &'a str,
    /// Reverse the natural sibling order at every level.
    pub descending: bool,
    pub query: &'a Query,
    /// Strip cells a component renders; sizes pivot columns.
    pub cell_count: &'a dyn Fn(ComponentId) -> usize,
}

/// The node the outline is rooted on: the tree itself, or the named branch.
/// `None` while the name doesn't resolve — before the tree has populated,
/// or after the branch went away — which lists nothing rather than
/// falling back to everything.
pub(crate) fn root_node(tree: &Arc<ComponentNode>, root: &str) -> Option<Arc<ComponentNode>> {
    if root.is_empty() {
        return Some(tree.clone());
    }
    chain_to(tree, root)
        .last()
        .filter(|n| n.full_name.as_ref() == root)
        .cloned()
}

/// A node's children in display order.
fn ordered(
    node: &ComponentNode,
    descending: bool,
) -> Box<dyn Iterator<Item = &Arc<ComponentNode>> + '_> {
    let values = node.children.values();
    if descending {
        Box::new(values.rev())
    } else {
        Box::new(values)
    }
}

/// The rows currently on screen, in display order.
///
/// `tree` is the synthetic tree root, which never renders itself. With a
/// non-empty query the tree is pruned to matching names and every surviving
/// branch is open regardless of disclosure.
pub(crate) fn flatten(tree: &Arc<ComponentNode>, layout: &Layout) -> Vec<OutlineRow> {
    let mut rows = Vec::new();
    let filtering = !layout.query.is_empty();
    if let Some(label) = layout.focus {
        if let Some(t) = layout.types.iter().find(|t| &t.label == label) {
            push_type_rows(tree, t, layout, true, &mut rows);
        }
        return rows;
    }
    for t in layout.types {
        push_type_rows(tree, t, layout, false, &mut rows);
    }
    let Some(root) = root_node(tree, layout.root) else {
        return rows;
    };
    for child in ordered(&root, layout.descending) {
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
        build_pivot(node, layout).map(Arc::new)
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
        for child in ordered(node, layout.descending) {
            push_rows(child, depth + 1, layout, force_open, rows);
        }
        return;
    };
    push_grid_rows(depth + 1, &pivot, rows);
    // Leaf children aren't instances; they keep their ordinary rows, after
    // the grid so the instances stay under their field labels.
    for child in ordered(node, layout.descending).filter(|c| c.children.is_empty()) {
        push_rows(child, depth + 1, layout, force_open, rows);
    }
}

/// The instance rows of an open pivot.
fn push_grid_rows(depth: usize, pivot: &Arc<Pivot>, rows: &mut Vec<OutlineRow>) {
    for (ix, instance) in pivot.instances.iter().enumerate() {
        rows.push(OutlineRow {
            node: instance.node.clone(),
            depth,
            expanded: false,
            component_count: component_count(&instance.node),
            kind: RowKind::PivotInstance {
                pivot: pivot.clone(),
                ix,
            },
        });
    }
}

/// A type's synthetic branch row and, when open, its grid. Instances are
/// labelled by full path since they come from anywhere in the tree; a
/// query narrows them by that path, a root keeps only those beneath it,
/// and a type with no match under the query drops out. A focused type is
/// always open.
fn push_type_rows(
    tree: &Arc<ComponentNode>,
    t: &FrameType,
    layout: &Layout,
    focused: bool,
    rows: &mut Vec<OutlineRow>,
) {
    let filtering = !layout.query.is_empty();
    let mut instances: Vec<(Arc<ComponentNode>, SharedString)> = alike(tree, &t.fields)
        .into_iter()
        .filter(|n| !filtering || layout.query.matches_name(&n.full_name))
        .filter(|n| under(layout.root, &n.full_name))
        .map(|n| {
            let label = n.full_name.clone();
            (n, label)
        })
        .collect();
    if layout.descending {
        instances.reverse();
    }
    if filtering && instances.is_empty() {
        return;
    }
    let key = type_key(&t.label);
    let node = Arc::new(ComponentNode {
        segment: t.label.clone(),
        full_name: key.clone(),
        component_id: None,
        children: instances.iter().map(|(n, _)| n.clone()).collect(),
    });
    let expanded = focused || filtering || layout.disclosure.is_expanded(&key, 0);
    let pivot = Arc::new(pivot_from(key, instances, layout));
    rows.push(OutlineRow {
        node: node.clone(),
        depth: 0,
        expanded,
        component_count: pivot.instances.len(),
        kind: RowKind::PivotBranch(pivot.clone()),
    });
    if expanded {
        push_grid_rows(1, &pivot, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::component_browser::component_tree::Children;

    fn leaf(name: &str) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: Some(metor_proto::types::ComponentId::new(name)),
            children: Children::default(),
        })
    }

    fn branch(name: &str, children: Vec<Arc<ComponentNode>>) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: None,
            children: children.into_iter().collect(),
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

    /// Every knob of a [`Layout`] with a default, so a test names only
    /// what it exercises.
    #[derive(Default)]
    struct Knobs<'a> {
        disclosure: Disclosure,
        pivoted: HashSet<SharedString>,
        layouts: HashMap<SharedString, PivotLayout>,
        types: Vec<FrameType>,
        focus: Option<SharedString>,
        root: &'a str,
        descending: bool,
        query: Query,
    }

    impl Knobs<'_> {
        fn rows(&self, tree: &Arc<ComponentNode>) -> Vec<OutlineRow> {
            flatten(tree, &self.layout())
        }

        fn layout(&self) -> Layout<'_> {
            Layout {
                disclosure: &self.disclosure,
                pivoted: &self.pivoted,
                layouts: &self.layouts,
                types: &self.types,
                focus: self.focus.as_ref(),
                root: self.root,
                descending: self.descending,
                query: &self.query,
                cell_count: &|_| 1,
            }
        }
    }

    fn pivoted(path: &str) -> HashSet<SharedString> {
        [SharedString::from(path.to_string())].into_iter().collect()
    }

    #[test]
    fn default_opens_only_the_top_level() {
        let rows = Knobs::default().rows(&tree());
        assert_eq!(names(&rows), ["sat", "sat.imu", "sat.mode"]);
        assert!(rows[0].expanded);
        assert!(!rows[1].expanded);
        assert_eq!(rows[0].component_count, 3);
        assert_eq!(rows[1].component_count, 2);
    }

    #[test]
    fn toggling_a_branch_discloses_its_children() {
        let mut knobs = Knobs::default();
        knobs.disclosure.toggle(&"sat.imu".into(), 1);
        let rows = knobs.rows(&tree());
        assert_eq!(
            names(&rows),
            ["sat", "sat.imu", "sat.imu.gyro", "sat.imu.temp", "sat.mode"]
        );
        assert_eq!(knobs.disclosure.expanded_paths(), ["sat.imu"]);
        assert!(knobs.disclosure.collapsed_paths().is_empty());
    }

    #[test]
    fn folding_the_root_hides_everything_below() {
        let mut knobs = Knobs::default();
        knobs.disclosure.set_expanded(&"sat".into(), 0, false);
        let rows = knobs.rows(&tree());
        assert_eq!(names(&rows), ["sat"]);
        assert_eq!(knobs.disclosure.collapsed_paths(), ["sat"]);
    }

    #[test]
    fn subtree_open_and_close_round_trip_to_an_empty_set() {
        let tree = tree();
        let sat = tree.children.get("sat").unwrap();
        let mut disclosure = Disclosure::default();
        disclosure.set_subtree(sat, 0, true);
        assert_eq!(disclosure.expanded_paths(), ["sat.imu"]);
        assert!(disclosure.collapsed_paths().is_empty());
        disclosure.set_subtree(sat, 0, false);
        assert_eq!(disclosure.collapsed_paths(), ["sat"]);
        assert!(disclosure.expanded_paths().is_empty());
        disclosure.set_subtree(sat, 0, true);
        assert_eq!(disclosure.expanded_paths(), ["sat.imu"]);
        assert!(disclosure.collapsed_paths().is_empty());
    }

    #[test]
    fn a_query_prunes_and_opens_everything() {
        let mut knobs = Knobs::default();
        knobs.disclosure.set_expanded(&"sat".into(), 0, false);
        knobs.query = Query::parse("gyro");
        let rows = knobs.rows(&tree());
        assert_eq!(names(&rows), ["sat", "sat.imu", "sat.imu.gyro"]);
        assert!(rows.iter().filter(|r| r.is_branch()).all(|r| r.expanded));
    }

    #[test]
    fn descending_reverses_siblings_at_every_level_and_instances() {
        let mut knobs = Knobs {
            descending: true,
            ..Default::default()
        };
        knobs.disclosure.set_expanded(&"sat.imu".into(), 1, true);
        let rows = knobs.rows(&tree());
        assert_eq!(
            names(&rows),
            ["sat", "sat.mode", "sat.imu", "sat.imu.temp", "sat.imu.gyro"]
        );
        knobs.pivoted = pivoted("wheels");
        let rows = knobs.rows(&wheels());
        assert_eq!(
            names(&rows),
            ["wheels", "wheels.1", "wheels.0", "wheels.count"]
        );
    }

    #[test]
    fn a_root_lists_its_children_at_depth_zero() {
        let mut knobs = Knobs {
            root: "sat.imu",
            ..Default::default()
        };
        let rows = knobs.rows(&tree());
        assert_eq!(names(&rows), ["sat.imu.gyro", "sat.imu.temp"]);
        assert!(rows.iter().all(|r| r.depth == 0));
        knobs.root = "sat";
        let rows = knobs.rows(&tree());
        assert_eq!(
            names(&rows),
            ["sat.imu", "sat.imu.gyro", "sat.imu.temp", "sat.mode"]
        );
        assert!(rows[0].expanded);
    }

    #[test]
    fn an_unresolved_root_shows_nothing_until_it_appears() {
        let knobs = Knobs {
            root: "sat.gps",
            ..Default::default()
        };
        assert!(knobs.rows(&tree()).is_empty());
        assert!(root_node(&tree(), "sat.gps").is_none());
        assert!(root_node(&tree(), "").is_some());
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
        let knobs = Knobs::default();
        let mut layout = knobs.layout();
        layout.cell_count = &|_| 3;
        let pivot = build_pivot(tree.children.get("wheels").unwrap(), &layout).unwrap();
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
        assert!(build_pivot(imu, &Knobs::default().layout()).is_none());
    }

    #[test]
    fn a_pivoted_branch_lists_instances_then_leaves() {
        let knobs = Knobs {
            pivoted: pivoted("wheels"),
            ..Default::default()
        };
        let rows = knobs.rows(&wheels());
        assert_eq!(
            names(&rows),
            ["wheels", "wheels.0", "wheels.1", "wheels.count"]
        );
        assert!(rows[0].is_pivoted());
        assert!(matches!(rows[1].kind, RowKind::PivotInstance { ix: 0, .. }));
        assert!(matches!(rows[2].kind, RowKind::PivotInstance { ix: 1, .. }));
    }

    #[test]
    fn a_folded_pivot_hides_its_grid() {
        let mut knobs = Knobs {
            pivoted: pivoted("wheels"),
            ..Default::default()
        };
        knobs.disclosure.set_expanded(&"wheels".into(), 0, false);
        let rows = knobs.rows(&wheels());
        assert_eq!(names(&rows), ["wheels"]);
        assert!(!rows[0].is_pivoted());
    }

    fn wheel_pivot(knobs: &Knobs) -> Arc<Pivot> {
        let rows = knobs.rows(&wheels());
        let RowKind::PivotBranch(pivot) = &rows[0].kind else {
            panic!("expected the pivoted branch");
        };
        pivot.clone()
    }

    #[test]
    fn layout_order_leads_and_new_fields_follow_naturally() {
        let mut knobs = Knobs {
            pivoted: pivoted("wheels"),
            ..Default::default()
        };
        knobs.layouts.insert(
            "wheels".into(),
            PivotLayout {
                order: vec!["speed".into(), "gone".into()],
                ..Default::default()
            },
        );
        let pivot = wheel_pivot(&knobs);
        assert_eq!(pivot.fields, ["speed", "motor.temp"]);
        assert!(pivot.instances[1].ids[0].is_some());
        assert_eq!(pivot.instances[1].ids[1], None);
    }

    #[test]
    fn hidden_fields_leave_the_grid() {
        let mut knobs = Knobs {
            pivoted: pivoted("wheels"),
            ..Default::default()
        };
        knobs.layouts.insert(
            "wheels".into(),
            PivotLayout {
                hidden: ["motor.temp".into()].into_iter().collect(),
                ..Default::default()
            },
        );
        let pivot = wheel_pivot(&knobs);
        assert_eq!(pivot.fields, ["speed"]);
        assert_eq!(pivot.hidden, ["motor.temp"]);
        assert_eq!(pivot.cells.len(), 1);
    }

    #[test]
    fn row_order_leads_and_the_rest_stay_sorted() {
        let mut knobs = Knobs {
            pivoted: pivoted("wheels"),
            ..Default::default()
        };
        knobs.layouts.insert(
            "wheels".into(),
            PivotLayout {
                rows: vec!["1".into()],
                ..Default::default()
            },
        );
        let rows = knobs.rows(&wheels());
        assert_eq!(
            names(&rows),
            ["wheels", "wheels.1", "wheels.0", "wheels.count"]
        );
    }

    /// Two PSUs under different DUTs, plus a decoy with one field fewer.
    fn duts() -> Arc<ComponentNode> {
        let psu = |prefix: &str| {
            branch(
                &format!("{prefix}.psu"),
                vec![
                    leaf(&format!("{prefix}.psu.current")),
                    leaf(&format!("{prefix}.psu.voltage")),
                ],
            )
        };
        branch(
            "",
            vec![
                branch("dut1", vec![psu("dut1"), leaf("dut1.serial")]),
                branch("dut2", vec![branch("dut2.bay", vec![psu("dut2.bay")])]),
                branch(
                    "dut3",
                    vec![branch("dut3.psu", vec![leaf("dut3.psu.current")])],
                ),
            ],
        )
    }

    fn psu_type(tree: &Arc<ComponentNode>) -> FrameType {
        let exemplar = tree
            .children
            .get("dut1")
            .unwrap()
            .children
            .get("psu")
            .unwrap();
        FrameType {
            label: "psu".into(),
            fields: signature(exemplar),
        }
    }

    #[test]
    fn alike_finds_the_shape_at_any_depth_and_nothing_else() {
        let tree = duts();
        let t = psu_type(&tree);
        assert_eq!(t.fields, ["current", "voltage"]);
        let found: Vec<String> = alike(&tree, &t.fields)
            .iter()
            .map(|n| n.full_name.to_string())
            .collect();
        assert_eq!(found, ["dut1.psu", "dut2.bay.psu"]);
    }

    #[test]
    fn types_lead_the_outline_and_label_instances_by_path() {
        let tree = duts();
        let knobs = Knobs {
            types: vec![psu_type(&tree)],
            ..Default::default()
        };
        let rows = knobs.rows(&tree);
        assert_eq!(names(&rows)[..3], ["type:psu", "dut1.psu", "dut2.bay.psu"]);
        assert_eq!(names(&rows)[3], "dut1");
        let RowKind::PivotInstance { pivot, ix } = &rows[2].kind else {
            panic!("expected an instance row");
        };
        assert_eq!(pivot.instances[*ix].label, "dut2.bay.psu");
        assert_eq!(pivot.key, "type:psu");
    }

    #[test]
    fn focus_shows_only_that_type_open() {
        let tree = duts();
        let mut knobs = Knobs {
            types: vec![psu_type(&tree)],
            ..Default::default()
        };
        knobs.disclosure.set_expanded(&type_key("psu"), 0, false);
        knobs.focus = Some("psu".into());
        let rows = knobs.rows(&tree);
        assert_eq!(names(&rows), ["type:psu", "dut1.psu", "dut2.bay.psu"]);
    }

    #[test]
    fn a_query_narrows_type_instances_by_path() {
        let tree = duts();
        let knobs = Knobs {
            types: vec![psu_type(&tree)],
            query: Query::parse("bay"),
            ..Default::default()
        };
        let rows = knobs.rows(&tree);
        assert_eq!(names(&rows)[..2], ["type:psu", "dut2.bay.psu"]);
    }

    #[test]
    fn types_under_a_root_keep_only_instances_beneath_it() {
        let tree = duts();
        let knobs = Knobs {
            types: vec![psu_type(&tree)],
            root: "dut2",
            ..Default::default()
        };
        let rows = knobs.rows(&tree);
        assert_eq!(
            names(&rows),
            ["type:psu", "dut2.bay.psu", "dut2.bay", "dut2.bay.psu"]
        );
    }

    #[test]
    fn take_slot_moves_an_item_onto_another_in_either_direction() {
        let items: Vec<SharedString> = ["a", "b", "c", "d"].map(SharedString::from).to_vec();
        let names =
            |v: Vec<SharedString>| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            names(take_slot(&items, &"a".into(), &"c".into())),
            ["b", "c", "a", "d"]
        );
        assert_eq!(
            names(take_slot(&items, &"d".into(), &"b".into())),
            ["a", "d", "b", "c"]
        );
        assert_eq!(
            names(take_slot(&items, &"x".into(), &"b".into())),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn common_suffix_is_the_shared_trailing_segments() {
        assert_eq!(common_suffix(["dut1.psu", "dut2.bay.psu"]), "psu");
        assert_eq!(
            common_suffix(["cube_sat.nav.health", "cube_sat.ctrl.health"]),
            "health"
        );
        assert_eq!(common_suffix(["a.b.c", "x.b.c"]), "b.c");
        assert_eq!(common_suffix(["a.b", "c.d"]), "");
        assert_eq!(common_suffix(["only.one"]), "only.one");
    }
}
