pub mod dashboard;
pub(crate) mod drag;
pub(crate) mod item;
pub(crate) mod pane;
pub mod panels;
pub(crate) mod serial;

use gpui::{
    AnyElement, App, Axis, Context, DragMoveEvent, Entity, EventEmitter, IntoElement, Render,
    Window, div, prelude::*, px, relative,
};

use smallvec::SmallVec;

use crate::theme::theme;
use drag::ResizeDrag;
use serial::{SerializedItem, SerializedMember, SerializedPane, SerializedSplit};

pub use drag::SplitDirection;
pub use item::{PaneItem, PaneItemHandle};
pub use pane::{Pane, PaneEvent};
pub use serial::{ItemRegistry, SerializedTileGroup};

/// Path of member indices through the tile split tree.
pub(crate) type SplitPath = SmallVec<[usize; 4]>;

/// Events emitted by TileGroup to its parent.
pub enum TileGroupEvent {
    /// A panel item requested inspection/editing (e.g. via right-click on tab).
    Inspect { item: Box<dyn PaneItemHandle> },
}

impl EventEmitter<TileGroupEvent> for TileGroup {}

const RESIZE_HANDLE_SIZE: f32 = 1.0;

/// A recursive split tree member: either a leaf pane or an axis split.
enum Member {
    Pane(Entity<Pane>),
    Axis(SplitAxis),
}

struct SplitAxis {
    axis: Axis,
    members: Vec<Member>,
    flexes: Vec<f32>,
}

impl SplitAxis {
    fn new(axis: Axis, members: Vec<Member>) -> Self {
        let len = members.len();
        Self {
            axis,
            members,
            flexes: vec![1.0; len],
        }
    }
}

impl Member {
    /// Find the target pane and replace it with a split containing old + new pane.
    fn split(
        &mut self,
        target: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
    ) -> bool {
        match self {
            Member::Pane(pane) => {
                if pane.entity_id() != target.entity_id() {
                    return false;
                }
                let old = Member::Pane(pane.clone());
                let new = Member::Pane(new_pane.clone());
                let (first, second) = if direction.increasing() {
                    (old, new)
                } else {
                    (new, old)
                };
                *self = Member::Axis(SplitAxis::new(direction.axis(), vec![first, second]));
                true
            }
            Member::Axis(axis) => {
                // Check if we can append to this axis (same direction)
                for i in 0..axis.members.len() {
                    if let Member::Pane(pane) = &axis.members[i] {
                        if pane.entity_id() == target.entity_id() && axis.axis == direction.axis() {
                            let new = Member::Pane(new_pane.clone());
                            let insert_at = if direction.increasing() { i + 1 } else { i };
                            axis.members.insert(insert_at, new);
                            axis.flexes.insert(insert_at, 1.0);
                            return true;
                        }
                    }
                }
                for member in &mut axis.members {
                    if member.split(target, new_pane, direction) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Remove a pane from the tree. Returns true if found and removed.
    fn remove(&mut self, target: &Entity<Pane>) -> bool {
        match self {
            Member::Pane(pane) => pane.entity_id() == target.entity_id(),
            Member::Axis(axis) => {
                if let Some(ix) = axis.members.iter().position(
                    |m| matches!(m, Member::Pane(p) if p.entity_id() == target.entity_id()),
                ) {
                    axis.members.remove(ix);
                    axis.flexes.remove(ix);
                    true
                } else {
                    for member in &mut axis.members {
                        if member.remove(target) {
                            return true;
                        }
                    }
                    false
                }
            }
        }
    }

    /// Collapse single-child axes into their child.
    fn collapse(&mut self) {
        if let Member::Axis(axis) = self {
            // First collapse children
            for member in &mut axis.members {
                member.collapse();
            }
            if axis.members.len() == 1 {
                *self = axis.members.remove(0);
            }
        }
    }

    fn serialize(&self, cx: &App) -> SerializedMember {
        match self {
            Member::Pane(pane) => {
                let pane = pane.read(cx);
                let items = pane
                    .items()
                    .iter()
                    .map(|item| SerializedItem {
                        kind: item.serialization_key().to_string(),
                        state: item.serialize(cx),
                    })
                    .collect();
                SerializedMember::Pane(SerializedPane {
                    active_index: pane.active_index(),
                    items,
                })
            }
            Member::Axis(axis) => SerializedMember::Split(SerializedSplit {
                axis: axis.axis.into(),
                flexes: axis.flexes.clone(),
                children: axis.members.iter().map(|m| m.serialize(cx)).collect(),
            }),
        }
    }

    fn render(
        &self,
        path: SplitPath,
        tile_group: &Entity<TileGroup>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        match self {
            Member::Pane(pane) => div().size_full().child(pane.clone()).into_any_element(),
            Member::Axis(axis) => {
                let container = match axis.axis {
                    Axis::Horizontal => div().flex().flex_row(),
                    Axis::Vertical => div().flex().flex_col(),
                };

                let mut children: Vec<AnyElement> = Vec::new();

                for (ix, member) in axis.members.iter().enumerate() {
                    // Resize handle between children
                    if ix > 0 {
                        let handle =
                            render_resize_handle(path.clone(), ix, axis.axis, tile_group, cx);
                        children.push(handle.into_any_element());
                    }

                    let flex = axis.flexes[ix];
                    let mut child_path = path.clone();
                    child_path.push(ix);

                    let child_element = member.render(child_path, tile_group, window, cx);

                    let mut child_div = div()
                        .flex_basis(relative(0.))
                        .overflow_hidden()
                        .min_w(px(50.0))
                        .min_h(px(50.0))
                        .child(child_element);
                    {
                        let style = child_div.style();
                        style.flex_grow = Some(flex);
                        style.flex_shrink = Some(flex);
                    }
                    children.push(child_div.into_any_element());
                }

                // Track axis bounds for resize calculations
                let tg = tile_group.clone();
                let p = path.clone();
                let bounds_tracker = gpui::canvas(
                    move |bounds, _window, cx| {
                        tg.update(cx, |this, _| {
                            this.axis_bounds.insert(p, bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .size_full()
                .absolute();

                div()
                    .relative()
                    .size_full()
                    .child(bounds_tracker)
                    .child(container.size_full().children(children))
                    .into_any_element()
            }
        }
    }
}

fn render_resize_handle(
    path: SplitPath,
    handle_ix: usize,
    axis: Axis,
    tile_group: &Entity<TileGroup>,
    cx: &mut App,
) -> impl IntoElement {
    let theme = theme(cx);
    let tg = tile_group.clone();

    // Build a unique ID from the full path + handle index to avoid collisions in nested splits
    let mut id_hash: u64 = handle_ix as u64;
    for &segment in path.as_slice() {
        id_hash = id_hash.wrapping_mul(31).wrapping_add(segment as u64);
    }
    let mut handle = div().id(("resize-handle", id_hash as usize));

    handle = match axis {
        Axis::Horizontal => handle
            .w(px(RESIZE_HANDLE_SIZE))
            .h_full()
            .cursor(gpui::CursorStyle::ResizeColumn),
        Axis::Vertical => handle
            .w_full()
            .h(px(RESIZE_HANDLE_SIZE))
            .cursor(gpui::CursorStyle::ResizeRow),
    };

    handle.style().flex_shrink = Some(0.);

    let hover_color = theme.text_tertiary;
    handle
        .bg(theme.border_primary)
        .hover(move |s| s.bg(hover_color))
        .on_drag(
            ResizeDrag {
                path: path.clone(),
                handle_ix,
            },
            |drag, _, _, cx| {
                cx.new(|_| ResizeDrag {
                    path: drag.path.clone(),
                    handle_ix: drag.handle_ix,
                })
            },
        )
        .on_drag_move({
            let tg = tg.clone();
            move |event: &DragMoveEvent<ResizeDrag>, _window, cx| {
                let drag = event.drag(cx);
                let path = drag.path.clone();
                let handle_ix = drag.handle_ix;
                let position = event.event.position;

                tg.update(cx, |this, cx| {
                    this.handle_resize(path, handle_ix, position, cx);
                });
            }
        })
}

/// Root of the tile system. Create with `cx.new(|cx| TileGroup::new(..., cx))`.
pub struct TileGroup {
    root: Member,
    panes: Vec<Entity<Pane>>,
    /// Cached bounds for each axis path, updated during render.
    axis_bounds: std::collections::HashMap<SplitPath, gpui::Bounds<gpui::Pixels>>,
}

impl TileGroup {
    /// Create a new TileGroup with a single pane.
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(items, cx));
        cx.subscribe(&pane, Self::handle_pane_event).detach();
        let panes = vec![pane.clone()];
        Self {
            root: Member::Pane(pane),
            panes,
            axis_bounds: Default::default(),
        }
    }

    /// Create a TileGroup with a pre-built pane entity.
    pub fn from_pane(pane: Entity<Pane>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&pane, Self::handle_pane_event).detach();
        let panes = vec![pane.clone()];
        Self {
            root: Member::Pane(pane),
            panes,
            axis_bounds: Default::default(),
        }
    }

    pub fn panes(&self) -> &[Entity<Pane>] {
        &self.panes
    }

    /// Add a pane by splitting an existing one.
    pub fn split_pane(
        &mut self,
        target: &Entity<Pane>,
        new_pane: Entity<Pane>,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe(&new_pane, Self::handle_pane_event).detach();
        self.root.split(target, &new_pane, direction);
        self.panes.push(new_pane);
        cx.notify();
    }

    /// Remove a pane from the tree (called when pane becomes empty).
    pub fn remove_pane(&mut self, target: &Entity<Pane>, cx: &mut Context<Self>) {
        self.root.remove(target);
        self.root.collapse();
        self.panes.retain(|p| p.entity_id() != target.entity_id());
        cx.notify();
    }

    /// Serialize the entire tile layout to a JSON-compatible structure.
    pub fn serialize(&self, cx: &App) -> SerializedTileGroup {
        SerializedTileGroup {
            root: self.root.serialize(cx),
        }
    }

    /// Deserialize a tile layout from a serialized structure.
    pub fn deserialize(
        serialized: SerializedTileGroup,
        registry: &ItemRegistry,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panes = Vec::new();
        let root = Self::deserialize_member(&serialized.root, registry, &mut panes, cx);
        let mut this = Self {
            root,
            panes: Vec::new(),
            axis_bounds: Default::default(),
        };
        for pane in &panes {
            cx.subscribe(pane, Self::handle_pane_event).detach();
        }
        this.panes = panes;
        this
    }

    fn deserialize_member(
        serialized: &SerializedMember,
        registry: &ItemRegistry,
        panes: &mut Vec<Entity<Pane>>,
        cx: &mut Context<Self>,
    ) -> Member {
        match serialized {
            SerializedMember::Pane(sp) => {
                let pane = cx.new(|cx| {
                    let items: Vec<Box<dyn PaneItemHandle>> = sp
                        .items
                        .iter()
                        .filter_map(|si| registry.deserialize(&si.kind, si.state.clone(), cx))
                        .collect();
                    let mut pane = Pane::new(items, cx);
                    if sp.active_index < pane.items().len() {
                        pane.activate_item(sp.active_index, cx);
                    }
                    pane
                });
                panes.push(pane.clone());
                Member::Pane(pane)
            }
            SerializedMember::Split(ss) => {
                let members: Vec<Member> = ss
                    .children
                    .iter()
                    .map(|child| Self::deserialize_member(child, registry, panes, cx))
                    .collect();
                let mut axis = SplitAxis::new(ss.axis.into(), members);
                if ss.flexes.len() == axis.members.len() {
                    axis.flexes = ss.flexes.clone();
                }
                Member::Axis(axis)
            }
        }
    }

    fn handle_pane_event(&mut self, pane: Entity<Pane>, event: &PaneEvent, cx: &mut Context<Self>) {
        match event {
            PaneEvent::Split { direction, item } => {
                let new_pane = cx.new(|cx| Pane::new(vec![item.clone_handle()], cx));
                self.split_pane(&pane, new_pane, *direction, cx);
            }
            PaneEvent::Empty => {
                // Only remove if there's more than one pane
                if self.panes.len() > 1 {
                    self.remove_pane(&pane, cx);
                }
            }
            PaneEvent::Inspect { item } => {
                cx.emit(TileGroupEvent::Inspect {
                    item: item.clone_handle(),
                });
            }
        }
    }

    fn handle_resize(
        &mut self,
        path: SplitPath,
        handle_ix: usize,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let bounds = match self.axis_bounds.get(&path) {
            Some(b) => *b,
            None => return,
        };

        if let Some(axis) = self.find_axis_mut(&path) {
            if handle_ix == 0 || handle_ix > axis.members.len() {
                return;
            }

            let total = match axis.axis {
                Axis::Horizontal => f32::from(bounds.size.width),
                Axis::Vertical => f32::from(bounds.size.height),
            };

            let rel = match axis.axis {
                Axis::Horizontal => f32::from(position.x - bounds.origin.x),
                Axis::Vertical => f32::from(position.y - bounds.origin.y),
            };

            if total <= 0.0 {
                return;
            }

            // Compute the fraction of total space that should be before the handle
            let ratio = (rel / total).clamp(0.0, 1.0);

            let left = handle_ix - 1;
            let right = handle_ix;
            if right >= axis.flexes.len() {
                return;
            }

            // The two panes sharing this handle own a combined flex budget
            let pair_flex = axis.flexes[left] + axis.flexes[right];
            // How much of the total flex is before the left pane?
            let flex_before_pair: f32 = axis.flexes[..left].iter().sum();
            let flex_sum: f32 = axis.flexes.iter().sum();

            // Target flex for the left pane: map the mouse ratio into the pair's budget
            let target_left = (ratio * flex_sum - flex_before_pair).clamp(0.05, pair_flex - 0.05);
            let target_right = pair_flex - target_left;

            axis.flexes[left] = target_left;
            axis.flexes[right] = target_right;
            cx.notify();
        }
    }

    fn find_axis_mut(&mut self, path: &[usize]) -> Option<&mut SplitAxis> {
        let mut current = &mut self.root;
        // Navigate along the full path
        for &ix in path {
            current = match current {
                Member::Axis(axis) => axis.members.get_mut(ix)?,
                _ => return None,
            };
        }
        match current {
            Member::Axis(axis) => Some(axis),
            _ => None,
        }
    }
}

impl Render for TileGroup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let tile_group = cx.entity().clone();

        div()
            .size_full()
            .bg(theme.bg_secondary)
            .child(self.root.render(SplitPath::new(), &tile_group, window, cx))
    }
}
