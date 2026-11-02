//! The grid model: alike subtrees rotated into instances × fields.
//!
//! Every pivot is the same rotation whatever chose its instances: each
//! instance is a subtree, its leaf paths relative to itself are its
//! fields, and the grid's columns are the union of them all, so an
//! instance missing a field simply gets an empty cell. Instances come
//! from a branch's [`children`] or from every subtree [`alike`] an
//! exemplar's shape; [`Grid::new`] lays either out the same way.
//!
//! An [`Arrangement`] records how the user arranged a grid — field order,
//! hidden fields, instance order — apart from the grid itself, so it
//! survives fields and instances coming and going.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use gpui::SharedString;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use crate::views::component_browser::component_tree::{
    ComponentNode, component_count, natural_cmp,
};

/// Subtrees rotated into instances × fields.
pub(crate) struct Grid {
    /// What the grid belongs to — a branch path or a type key — so state
    /// kept per grid, such as an arrangement or a scroll offset, can find
    /// it again.
    pub key: SharedString,
    /// Leaf paths relative to an instance, in display order.
    pub fields: Vec<SharedString>,
    /// Widest strip each field needs, in element cells.
    pub cells: Vec<usize>,
    pub instances: Vec<Instance>,
    /// Fields the arrangement hides, so a menu can offer them back.
    pub hidden: Vec<SharedString>,
}

/// Values a row pattern's named captures took for one instance, in the
/// pattern's capture order.
pub(crate) type Captures = SmallVec<[SharedString; 2]>;

/// One subtree chosen to be a row, before it's laid out on the grid.
#[derive(Clone)]
pub(crate) struct Frame {
    pub node: Arc<ComponentNode>,
    pub label: SharedString,
    pub captures: Captures,
}

impl Frame {
    /// A row labelled `label` with nothing captured.
    pub fn labelled(node: Arc<ComponentNode>, label: SharedString) -> Self {
        Self {
            node,
            label,
            captures: Captures::new(),
        }
    }
}

pub(crate) struct Instance {
    pub node: Arc<ComponentNode>,
    /// Row label: the segment under a branch, the full path in a type.
    pub label: SharedString,
    pub captures: Captures,
    /// One slot per field; `None` where this instance lacks it.
    pub ids: Vec<Option<ComponentId>>,
}

impl Grid {
    /// Lay `instances` out on the union of their fields, arranged by
    /// `arrangement` when the grid has one. `cell_count` sizes a column by
    /// the widest strip any instance renders in it.
    pub fn new(
        key: SharedString,
        instances: Vec<Frame>,
        arrangement: Option<&Arrangement>,
        cell_count: &dyn Fn(ComponentId) -> usize,
    ) -> Self {
        let instances = match arrangement {
            Some(a) => lead_by(instances, &a.rows, |f| &f.label),
            None => instances,
        };
        let per_instance: Vec<BTreeMap<SharedString, ComponentId>> = instances
            .iter()
            .map(|f| {
                let mut leaves = BTreeMap::new();
                collect_leaves(&f.node, f.node.full_name.len() + 1, &mut leaves);
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
        let instances: Vec<Instance> = instances
            .into_iter()
            .zip(&per_instance)
            .map(|(f, leaves)| Instance {
                node: f.node,
                label: f.label,
                captures: f.captures,
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
        Self {
            key,
            fields,
            cells,
            instances,
            hidden,
        }
    }
}

/// How the user arranged one grid. Names that no longer exist are kept,
/// not pruned, so a field that comes back keeps its slot.
#[derive(Default, Clone, PartialEq, Eq)]
pub(crate) struct Arrangement {
    /// Fields first, in this order; the rest follow in natural order.
    pub order: Vec<SharedString>,
    pub hidden: HashSet<SharedString>,
    /// Instance labels first, in this order; the rest keep their sort.
    pub rows: Vec<SharedString>,
}

impl Arrangement {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.hidden.is_empty() && self.rows.is_empty()
    }

    /// Drop field `from` of `grid` onto field `to`: it takes that slot.
    pub fn move_field(&mut self, grid: &Grid, from: &SharedString, to: &SharedString) {
        self.order = take_slot(&grid.fields, from, to);
    }

    /// Drop instance `from` of `grid` onto instance `to`: it takes that
    /// slot.
    pub fn move_instance(&mut self, grid: &Grid, from: &SharedString, to: &SharedString) {
        let labels: Vec<SharedString> = grid.instances.iter().map(|i| i.label.clone()).collect();
        self.rows = take_slot(&labels, from, to);
    }

    pub fn set_hidden(&mut self, field: &SharedString, hidden: bool) {
        if hidden {
            self.hidden.insert(field.clone());
        } else {
            self.hidden.remove(field);
        }
    }
}

/// A branch's instances: every child that is itself a branch, labelled by
/// its segment, in display order.
pub(crate) fn children(branch: &ComponentNode, descending: bool) -> Vec<Frame> {
    let mut out: Vec<_> = branch
        .children
        .values()
        .filter(|c| !c.children.is_empty())
        .map(|c| Frame::labelled(c.clone(), c.segment.clone()))
        .collect();
    if descending {
        out.reverse();
    }
    out
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
fn take_slot(items: &[SharedString], from: &SharedString, to: &SharedString) -> Vec<SharedString> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::views::component_browser::component_tree::Children;

    pub(crate) fn leaf(name: &str) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: Some(ComponentId::new(name)),
            children: Children::default(),
        })
    }

    pub(crate) fn branch(name: &str, children: Vec<Arc<ComponentNode>>) -> Arc<ComponentNode> {
        Arc::new(ComponentNode {
            segment: SharedString::from(name.rsplit('.').next().unwrap().to_string()),
            full_name: SharedString::from(name.to_string()),
            component_id: None,
            children: children.into_iter().collect(),
        })
    }

    /// Two PSUs under different DUTs, plus a decoy with one field fewer.
    pub(crate) fn duts() -> Arc<ComponentNode> {
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

    /// The exemplar PSU's shape.
    pub(crate) fn psu_fields(tree: &Arc<ComponentNode>) -> Vec<SharedString> {
        let exemplar = tree
            .children
            .get("dut1")
            .unwrap()
            .children
            .get("psu")
            .unwrap();
        signature(exemplar)
    }

    fn label_all(nodes: Vec<Arc<ComponentNode>>) -> Vec<Frame> {
        nodes
            .into_iter()
            .map(|n| {
                let label = n.full_name.clone();
                Frame::labelled(n, label)
            })
            .collect()
    }

    #[test]
    fn alike_finds_the_shape_at_any_depth_and_nothing_else() {
        let tree = duts();
        let fields = psu_fields(&tree);
        assert_eq!(fields, ["current", "voltage"]);
        let found: Vec<String> = alike(&tree, &fields)
            .iter()
            .map(|n| n.full_name.to_string())
            .collect();
        assert_eq!(found, ["dut1.psu", "dut2.bay.psu"]);
    }

    #[test]
    fn a_grid_leaves_gaps_where_an_instance_lacks_a_field() {
        let tree = duts();
        let psus = vec![
            tree.children
                .get("dut1")
                .unwrap()
                .children
                .get("psu")
                .unwrap()
                .clone(),
            tree.children
                .get("dut3")
                .unwrap()
                .children
                .get("psu")
                .unwrap()
                .clone(),
        ];
        let grid = Grid::new("psus".into(), label_all(psus), None, &|_| 2);
        assert_eq!(grid.fields, ["current", "voltage"]);
        assert_eq!(grid.cells, [2, 2]);
        assert!(grid.instances[0].ids.iter().all(Option::is_some));
        assert!(grid.instances[1].ids[0].is_some());
        assert_eq!(grid.instances[1].ids[1], None);
    }

    #[test]
    fn an_arrangement_leads_rows_and_fields_and_hides_the_rest() {
        let tree = duts();
        let instances = label_all(alike(&tree, &psu_fields(&tree)));
        let arrangement = Arrangement {
            order: vec!["voltage".into()],
            hidden: ["current".into()].into_iter().collect(),
            rows: vec!["dut2.bay.psu".into(), "gone".into()],
        };
        let grid = Grid::new("psus".into(), instances, Some(&arrangement), &|_| 1);
        assert_eq!(grid.fields, ["voltage"]);
        assert_eq!(grid.hidden, ["current"]);
        let labels: Vec<&str> = grid.instances.iter().map(|i| i.label.as_ref()).collect();
        assert_eq!(labels, ["dut2.bay.psu", "dut1.psu"]);
    }

    #[test]
    fn children_are_the_branch_children_in_display_order() {
        let tree = duts();
        let labels = |descending| -> Vec<String> {
            children(&tree, descending)
                .into_iter()
                .map(|f| f.label.to_string())
                .collect()
        };
        assert_eq!(labels(false), ["dut1", "dut2", "dut3"]);
        assert_eq!(labels(true), ["dut3", "dut2", "dut1"]);
        let dut1 = tree.children.get("dut1").unwrap();
        assert_eq!(children(dut1, false).len(), 1);
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
