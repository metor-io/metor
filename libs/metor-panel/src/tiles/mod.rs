pub(crate) mod drag;
pub(crate) mod item;
pub(crate) mod pane;
pub mod panels;
pub(crate) mod serial;

use gpui::{
    AnyElement, App, Axis, Context, DragMoveEvent, Entity, EventEmitter, IntoElement, Pixels,
    Point, Render, Window, div, prelude::*, px, relative,
};

use smallvec::SmallVec;

use crate::theme::theme;
use drag::ResizeDrag;
use serial::{TileItem, TileNode, TilePane, TileSplit};

pub use drag::SplitDirection;
pub use item::{PaneItem, PaneItemHandle};
pub use pane::{Pane, PaneEvent, PlotComponentAction, PreviewPlotAction, TabOrientation};
pub use serial::{ItemRegistry, TileLayout};

/// Sequence of member indices locating a node in the split tree.
///
/// `SmallVec` inlines the first four levels since the UI is rarely nested
/// deeper and the path is cloned on every handle render.
pub(crate) type SplitPath = SmallVec<[usize; 4]>;

/// Events the tile group forwards to its owning view.
pub enum TileGroupEvent {
    /// User asked to inspect a pane item (typically via right-click on a tab).
    /// The position marks where to anchor the inspector.
    Inspect {
        item: Box<dyn PaneItemHandle>,
        position: Point<Pixels>,
    },
    InspectPane {
        pane: Entity<Pane>,
        position: Point<Pixels>,
    },
}

impl EventEmitter<TileGroupEvent> for TileGroup {}

const RESIZE_HANDLE_SIZE: f32 = 1.0;

/// Layout version this binary writes and accepts on read. Lives in
/// `metor-proto-wkt` so targets shipping preset layouts share it; bump in
/// lockstep with [`TileGroup::serialize`] when the document shape changes.
///
/// Version history:
/// - 1: initial.
/// - 2: `PlotPanelConfig` gains measurement `cursors` for right-click-drag
///   measurement cursors.
/// - 3: `PlotPanelConfig` gains `measurement_panel` (track/pinned position
///   for the native measurement readout panel).
/// - 4: facet-json → serde_json migration; colors re-encode as RGBA hex and
///   `PrimType` values as kebab-case. Items from older documents that hold
///   either fall back to their config defaults.
/// - 5: drop all legacy-layout fallbacks; plot bounds live only on `axes` and
///   dashboard allocation counters are derived from item ids.
const SUPPORTED_LAYOUT_VERSION: u32 = serial::TILE_LAYOUT_VERSION;

/// Failure modes when loading a layout from JSON.
#[derive(Debug)]
pub enum LoadError {
    /// The document isn't valid layout JSON.
    Parse(serde_json::Error),
    /// The document's layout version differs from the one this build writes.
    UnsupportedVersion(u32),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported layout version {v}; this build requires version {SUPPORTED_LAYOUT_VERSION}"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<serde_json::Error> for LoadError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e)
    }
}

/// A node in the split tree: a leaf [`Pane`] or an interior [`SplitAxis`].
enum Member {
    Pane(Entity<Pane>),
    Axis(SplitAxis),
}

/// Interior node laying children out along a single axis.
///
/// `flexes[i]` is the flex-grow weight for `members[i]`; during resize these
/// values shift within a pair so the total for an axis stays constant.
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
    /// Insert `new_pane` next to `target` along `direction`.
    ///
    /// If `target` already sits in a split along the matching axis, the new
    /// pane is appended as a sibling; otherwise a new `SplitAxis` wraps both.
    /// Returns `true` if `target` was found.
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
                for i in 0..axis.members.len() {
                    if let Member::Pane(pane) = &axis.members[i]
                        && pane.entity_id() == target.entity_id()
                        && axis.axis == direction.axis()
                    {
                        let new = Member::Pane(new_pane.clone());
                        let insert_at = if direction.increasing() { i + 1 } else { i };
                        axis.members.insert(insert_at, new);
                        axis.flexes.insert(insert_at, 1.0);
                        return true;
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

    /// Remove `target` from the tree. Returns `true` when found.
    ///
    /// Leaves empty axis nodes behind; run [`Member::collapse`] afterwards to
    /// unwrap single-child axes.
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

    /// Bottom-up pass that replaces any single-child axis with its child.
    ///
    /// Called after [`Member::remove`] so a 2-way split holding one pane
    /// doesn't render an empty slot.
    fn collapse(&mut self) {
        if let Member::Axis(axis) = self {
            for member in &mut axis.members {
                member.collapse();
            }
            if axis.members.len() == 1 {
                *self = axis.members.remove(0);
            }
        }
    }

    fn serialize(&self, cx: &App) -> TileNode {
        match self {
            Member::Pane(pane) => {
                let pane = pane.read(cx);
                let items = pane
                    .items()
                    .iter()
                    .map(|item| TileItem {
                        kind: item.serialization_key().to_string(),
                        state: item.serialize(cx),
                    })
                    .collect();
                TileNode::Pane(TilePane {
                    active_index: pane.active_index(),
                    items,
                    tab_orientation: pane.tab_orientation(),
                    hide_tab_bar: pane.hide_tab_bar(),
                    locked_size: pane.locked_size().map(|s| (s.width, s.height)),
                })
            }
            Member::Axis(axis) => TileNode::Split(TileSplit {
                axis: serial::split_axis(axis.axis),
                flexes: axis.flexes.clone(),
                children: axis.members.iter().map(|m| m.serialize(cx)).collect(),
            }),
        }
    }

    // `window` is unused at the leaf today, but forwarded so future render
    // paths (focus, scroll into view) can reach gpui's window APIs without
    // a signature churn.
    #[allow(clippy::only_used_in_recursion)]
    fn render(
        &self,
        path: SplitPath,
        tile_group: &Entity<TileGroup>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        match self {
            Member::Pane(pane) => {
                // Record this pane as current on any click within it. Mouse
                // events bubble, so this fires after inner handlers (tab clicks,
                // content interactions) without intercepting them.
                let tg = tile_group.clone();
                let focus_pane = pane.clone();
                div()
                    .size_full()
                    .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                        tg.update(cx, |this, _cx| {
                            this.focused_pane = Some(focus_pane.clone());
                        });
                    })
                    .child(pane.clone())
                    .into_any_element()
            }
            Member::Axis(axis) => {
                let container = match axis.axis {
                    Axis::Horizontal => div().flex().flex_row(),
                    Axis::Vertical => div().flex().flex_col(),
                };

                let mut children: Vec<AnyElement> = Vec::new();

                for (ix, member) in axis.members.iter().enumerate() {
                    if ix > 0 {
                        let prev_locked = locked_along_axis(&axis.members[ix - 1], axis.axis, cx);
                        let cur_locked = locked_along_axis(member, axis.axis, cx);
                        let handle = if prev_locked.is_some() || cur_locked.is_some() {
                            render_static_resize_handle(axis.axis, cx).into_any_element()
                        } else {
                            render_resize_handle(path.clone(), ix, axis.axis, tile_group, cx)
                                .into_any_element()
                        };
                        children.push(handle);
                    }

                    let flex = axis.flexes[ix];
                    let mut child_path = path.clone();
                    child_path.push(ix);

                    let child_element = member.render(child_path, tile_group, window, cx);
                    let locked_px = locked_along_axis(member, axis.axis, cx);

                    let mut child_div = div()
                        .flex_basis(relative(0.))
                        .overflow_hidden()
                        .min_w(px(50.0))
                        .min_h(px(50.0))
                        .child(child_element);
                    {
                        let style = child_div.style();
                        if let Some(px_val) = locked_px {
                            style.flex_grow = Some(0.0);
                            style.flex_shrink = Some(0.0);
                            style.flex_basis = Some(gpui::Length::Definite(px(px_val).into()));
                            style.min_size.width = Some(px(0.).into());
                            style.min_size.height = Some(px(0.).into());
                        } else {
                            style.flex_grow = Some(flex);
                            style.flex_shrink = Some(flex);
                        }
                    }
                    children.push(child_div.into_any_element());
                }

                // Resize handles need the axis's on-screen extent; capture it here.
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

/// Returns the locked-size pixel value of `member` along `axis`, or `None`
/// if the member is unlocked or is a sub-axis (only panes can lock).
fn locked_along_axis(member: &Member, axis: Axis, cx: &App) -> Option<f32> {
    match member {
        Member::Pane(pane) => pane.read(cx).locked_size().map(|s| match axis {
            Axis::Horizontal => s.width,
            Axis::Vertical => s.height,
        }),
        Member::Axis(_) => None,
    }
}

/// Static placeholder rendered in place of a draggable resize handle when one
/// of its neighbors is a locked pane. Same on-screen size as the live handle
/// so layout doesn't shift when locking is toggled.
fn render_static_resize_handle(axis: Axis, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    let mut handle = div().bg(theme.border_primary);
    handle = match axis {
        Axis::Horizontal => handle.w(px(RESIZE_HANDLE_SIZE)).h_full(),
        Axis::Vertical => handle.w_full().h(px(RESIZE_HANDLE_SIZE)),
    };
    handle.style().flex_shrink = Some(0.);
    handle
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

    // Hash path + handle index so gpui element IDs stay unique across nested splits.
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

/// Owns the split tree that fills the main viewport.
///
/// Panes are flat-indexed for fast iteration; the tree is only consulted for
/// layout and structural edits. Axis bounds are cached during paint so
/// resize drags can convert pixel positions into flex weights without a
/// second traversal.
pub struct TileGroup {
    root: Member,
    panes: Vec<Entity<Pane>>,
    axis_bounds: std::collections::HashMap<SplitPath, gpui::Bounds<gpui::Pixels>>,
    /// The pane that last received a click, i.e. the "current" tile that
    /// keyboard-driven commands (the transient chord menu) act on. Falls back
    /// to the first pane via [`TileGroup::active_pane`] when unset or stale.
    focused_pane: Option<Entity<Pane>>,
}

impl TileGroup {
    /// Build a tile group containing a single pane seeded with `items`.
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(items, cx));
        cx.subscribe(&pane, Self::handle_pane_event).detach();
        let panes = vec![pane.clone()];
        Self {
            root: Member::Pane(pane.clone()),
            panes,
            axis_bounds: Default::default(),
            focused_pane: Some(pane),
        }
    }

    /// Adopt an already-constructed pane as the sole member of the tree.
    ///
    /// Use when the caller needs to hold the pane entity before handing
    /// ownership of the layout to the tile group.
    pub fn from_pane(pane: Entity<Pane>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&pane, Self::handle_pane_event).detach();
        let panes = vec![pane.clone()];
        Self {
            root: Member::Pane(pane.clone()),
            panes,
            axis_bounds: Default::default(),
            focused_pane: Some(pane),
        }
    }

    pub fn panes(&self) -> &[Entity<Pane>] {
        &self.panes
    }

    /// The "current" pane keyboard commands target: the last-clicked pane if it
    /// still exists, otherwise the first pane. `None` only when the tree somehow
    /// holds no panes (which never happens — the layout keeps at least one).
    pub fn active_pane(&self, _cx: &App) -> Option<Entity<Pane>> {
        self.focused_pane
            .as_ref()
            .filter(|p| self.panes.iter().any(|q| q.entity_id() == p.entity_id()))
            .cloned()
            .or_else(|| self.panes.first().cloned())
    }

    /// Reveal the panel of `kind` (a [`PaneItem::serialization_key`]): when one
    /// is already open anywhere in the layout, activate its tab and make its
    /// pane current; otherwise build a fresh item with `make` and add it as a
    /// new tab in the active pane.
    ///
    /// This is the entry point for "jump to X" affordances (the titlebar alarm
    /// summary, and any future status-bar shortcut) — unlike the palette's
    /// explicit "New Panel" commands, revealing must not multiply tabs.
    pub fn focus_or_open(
        &mut self,
        kind: &str,
        make: impl FnOnce(&mut Context<Pane>) -> Box<dyn PaneItemHandle>,
        cx: &mut Context<Self>,
    ) {
        let existing = self.panes.iter().find_map(|pane| {
            pane.read(cx)
                .items()
                .iter()
                .position(|item| item.serialization_key() == kind)
                .map(|ix| (pane.clone(), ix))
        });
        match existing {
            Some((pane, ix)) => {
                pane.update(cx, |pane, cx| pane.activate_item(ix, cx));
                self.focused_pane = Some(pane);
                cx.notify();
            }
            None => {
                if let Some(pane) = self.active_pane(cx) {
                    pane.update(cx, |pane, cx| {
                        let item = make(cx);
                        pane.add_item(item, cx);
                    });
                }
            }
        }
    }

    /// Split `target` along `direction`, placing `new_pane` beside it.
    pub fn split_pane(
        &mut self,
        target: &Entity<Pane>,
        new_pane: Entity<Pane>,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        // A missing `target` makes the split a no-op; tracking `new_pane`
        // anyway would leave a pane that's counted but never rendered — and
        // swallow whatever item it was carrying.
        if !self.root.split(target, &new_pane, direction) {
            return;
        }
        cx.subscribe(&new_pane, Self::handle_pane_event).detach();
        // A freshly-split pane becomes the current one so a follow-up chord
        // (e.g. "new tile") lands in the pane the user just created.
        self.focused_pane = Some(new_pane.clone());
        self.panes.push(new_pane);
        cx.notify();
    }

    /// Drop a pane from the layout; usually triggered by a pane emitting
    /// [`PaneEvent::Empty`].
    pub fn remove_pane(&mut self, target: &Entity<Pane>, cx: &mut Context<Self>) {
        self.root.remove(target);
        self.root.collapse();
        self.panes.retain(|p| p.entity_id() != target.entity_id());
        cx.notify();
    }

    /// Whether any pane holds an item. A fresh group keeps one empty pane
    /// around, so "no items anywhere" is the real test for an untouched
    /// layout (used to decide if loading a saved one needs consent).
    pub fn has_items(&self, cx: &App) -> bool {
        self.panes
            .iter()
            .any(|pane| !pane.read(cx).items().is_empty())
    }

    /// Snapshot the full layout (tree shape, flexes, and each item's own
    /// persisted state) for saving to disk.
    pub fn serialize(&self, cx: &App) -> TileLayout {
        TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: crate::views::time_series::time_range::GlobalTimeRange::get(cx)
                .to_string(),
            root: self.root.serialize(cx),
        }
    }

    /// Convenience: snapshot to a JSON string ready to write to disk.
    ///
    /// Panics on serialization failure — every field in the serialized tree
    /// is plain data, so the only way `serde_json::to_string` can fail here
    /// is a programmer error.
    pub fn to_json(&self, cx: &App) -> String {
        serde_json::to_string(&self.serialize(cx)).expect("tile layout always serializes")
    }

    /// Convenience: load a layout from a JSON string. The application picks
    /// the file path; this layer is format-only.
    pub fn from_json(
        json: &str,
        registry: &ItemRegistry,
        cx: &mut Context<Self>,
    ) -> Result<Self, LoadError> {
        let serialized: TileLayout = serde_json::from_str(json)?;
        if serialized.version != SUPPORTED_LAYOUT_VERSION {
            return Err(LoadError::UnsupportedVersion(serialized.version));
        }
        Ok(Self::deserialize(serialized, registry, cx))
    }

    /// Parse `json` and swap the layout in place.
    ///
    /// Pulls the panel-item registry from gpui globals so callers don't have
    /// to thread it. The clone keeps the global borrow short so the rest of
    /// the deserialization can take `&mut Context<Self>` freely.
    pub fn replace_from_json(
        &mut self,
        json: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), LoadError> {
        let registry = cx.global::<ItemRegistry>().clone();
        let new_self = Self::from_json(json, &registry, cx)?;
        *self = new_self;
        cx.notify();
        Ok(())
    }

    /// Rebuild a layout from a [`TileLayout`]. Items whose serialization
    /// key is absent from `registry` are silently dropped.
    pub fn deserialize(
        serialized: TileLayout,
        registry: &ItemRegistry,
        cx: &mut Context<Self>,
    ) -> Self {
        // Always reset: a layout that never narrowed the global range
        // (or predates it) must not inherit the previous layout's window.
        crate::views::time_series::time_range::GlobalTimeRange::set(
            cx,
            serialized.global_time_range.parse().unwrap_or_default(),
        );
        let mut panes = Vec::new();
        let root = Self::deserialize_member(&serialized.root, registry, &mut panes, cx);
        let mut this = Self {
            root,
            panes: Vec::new(),
            axis_bounds: Default::default(),
            focused_pane: panes.first().cloned(),
        };
        for pane in &panes {
            cx.subscribe(pane, Self::handle_pane_event).detach();
        }
        this.panes = panes;
        this
    }

    fn deserialize_member(
        serialized: &TileNode,
        registry: &ItemRegistry,
        panes: &mut Vec<Entity<Pane>>,
        cx: &mut Context<Self>,
    ) -> Member {
        match serialized {
            TileNode::Pane(sp) => {
                let pane = cx.new(|cx| {
                    let items: Vec<Box<dyn PaneItemHandle>> = sp
                        .items
                        .iter()
                        .filter_map(|si| registry.deserialize(&si.kind, &si.state, cx))
                        .collect();
                    let mut pane = Pane::new(items, cx);
                    if sp.active_index < pane.items().len() {
                        pane.activate_item(sp.active_index, cx);
                    }
                    pane.set_tab_orientation(sp.tab_orientation, cx);
                    pane.set_hide_tab_bar(sp.hide_tab_bar, cx);
                    pane.set_locked_size(
                        sp.locked_size.map(|(w, h)| gpui::Size {
                            width: w,
                            height: h,
                        }),
                        cx,
                    );
                    pane
                });
                panes.push(pane.clone());
                Member::Pane(pane)
            }
            TileNode::Split(ss) => {
                let members: Vec<Member> = ss
                    .children
                    .iter()
                    .map(|child| Self::deserialize_member(child, registry, panes, cx))
                    .collect();
                let mut axis = SplitAxis::new(serial::gpui_axis(ss.axis), members);
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
                // Keep one empty pane around so the layout is never void.
                if self.panes.len() > 1 {
                    self.remove_pane(&pane, cx);
                }
            }
            PaneEvent::Inspect { item, position } => {
                cx.emit(TileGroupEvent::Inspect {
                    item: item.clone_handle(),
                    position: *position,
                });
            }
            PaneEvent::InspectPane { position } => {
                cx.emit(TileGroupEvent::InspectPane {
                    pane: pane.clone(),
                    position: *position,
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

            let ratio = (rel / total).clamp(0.0, 1.0);

            let left = handle_ix - 1;
            let right = handle_ix;
            if right >= axis.flexes.len() {
                return;
            }

            // Only the two panes adjacent to the handle redistribute flex;
            // everything else keeps its current weight so unrelated panes
            // don't visibly shift when the user drags one boundary.
            let pair_flex = axis.flexes[left] + axis.flexes[right];
            let flex_before_pair: f32 = axis.flexes[..left].iter().sum();
            let flex_sum: f32 = axis.flexes.iter().sum();

            let target_left = (ratio * flex_sum - flex_before_pair).clamp(0.05, pair_flex - 0.05);
            let target_right = pair_flex - target_left;

            axis.flexes[left] = target_left;
            axis.flexes[right] = target_right;
            cx.notify();
        }
    }

    fn find_axis_mut(&mut self, path: &[usize]) -> Option<&mut SplitAxis> {
        let mut current = &mut self.root;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::pane::TabOrientation;
    use crate::views::dashboard::{
        DashboardPanelConfig, DashboardWidget, WidgetId, WidgetKind, WidgetLive, WidgetRect,
        WidgetRegistry, WidgetSpec, deserialize_dashboard,
    };
    use gpui::{AnyView, IntoElement, Render, Window};
    use std::sync::Arc;

    struct RegisteredTestView {
        value: String,
    }

    impl Render for RegisteredTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.value.clone())
        }
    }

    fn empty_pane_layout(version: u32) -> String {
        let layout = TileLayout {
            version,
            global_time_range: String::new(),
            root: TileNode::Pane(TilePane {
                active_index: 0,
                tab_orientation: TabOrientation::Horizontal,
                hide_tab_bar: false,
                locked_size: None,
                items: vec![],
            }),
        };
        serde_json::to_string(&layout).expect("serialize")
    }

    #[gpui::test]
    fn unknown_pane_round_trips_raw_kind_and_state(cx: &mut gpui::TestAppContext) {
        let state = r#"{"plugin":"temporarily absent"}"#;
        let layout = TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: String::new(),
            root: TileNode::Pane(TilePane {
                active_index: 0,
                tab_orientation: TabOrientation::Horizontal,
                hide_tab_bar: false,
                locked_size: None,
                items: vec![serial::TileItem {
                    kind: "downstream.plugin".into(),
                    state: state.into(),
                }],
            }),
        };
        let json = serde_json::to_string(&layout).unwrap();
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        cx.update(|cx| {
            tg.update(cx, |this, cx| {
                *this = TileGroup::from_json(&json, &ItemRegistry::default(), cx).unwrap();
            })
        });
        cx.read(|cx| {
            let item = &tg.read(cx).panes[0].read(cx).items()[0];
            assert_eq!(item.serialization_key(), "downstream.plugin");
            assert_eq!(item.serialize(cx), state);
        });
    }

    #[gpui::test]
    fn one_view_spec_builds_snapshots_and_inspects_in_both_hosts(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        let spec = Arc::new(
            WidgetSpec::new(
                (120.0, 80.0),
                |_| "Registered test".into(),
                |config, _db, cx| {
                    let value: String = serde_json::from_str(config).unwrap();
                    let entity = cx.new(|_| RegisteredTestView { value });
                    let any = entity.clone().into_any();
                    WidgetLive {
                        view: AnyView::from(entity),
                        inspect: any.clone(),
                        state: any,
                    }
                },
                |entity, _, cx| {
                    let entity = entity.clone().downcast::<RegisteredTestView>().ok()?;
                    serde_json::to_string(&entity.read(cx).value).ok()
                },
            )
            .with_tile("registered_test", |_| "Registered test".into()),
        );
        let mut panes = ItemRegistry::default();
        panes.register_view(spec.clone(), db.clone());
        let config = serde_json::to_string("hello").unwrap();

        let dashboard = cx.update(|cx| {
            WidgetRegistry::init(cx);
            cx.global_mut::<WidgetRegistry>()
                .register_shared(WidgetKind::new("registered_test"), spec);
            deserialize_dashboard(
                db,
                &serde_json::to_string(&DashboardPanelConfig {
                    title: "Downstream".into(),
                    widgets: vec![DashboardWidget {
                        id: WidgetId(1),
                        rect: WidgetRect {
                            x: 0.0,
                            y: 0.0,
                            w: 120.0,
                            h: 80.0,
                        },
                        kind: WidgetKind::new("registered_test"),
                        config: config.clone(),
                    }],
                    connectors: Vec::new(),
                })
                .unwrap(),
                cx,
            )
        });
        cx.read(|cx| {
            let cfg = dashboard.read(cx).to_config(cx);
            assert_eq!(cfg.widgets[0].config, config);
            assert!(
                dashboard.read(cx).inspectable_widgets(cx)[0]
                    .1
                    .clone()
                    .downcast::<RegisteredTestView>()
                    .is_ok()
            );
        });

        let layout = TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: String::new(),
            root: TileNode::Pane(TilePane {
                active_index: 0,
                tab_orientation: TabOrientation::Horizontal,
                hide_tab_bar: false,
                locked_size: None,
                items: vec![serial::TileItem {
                    kind: "registered_test".into(),
                    state: config.clone(),
                }],
            }),
        };
        let json = serde_json::to_string(&layout).unwrap();
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        cx.update(|cx| {
            tg.update(cx, |this, cx| {
                *this = TileGroup::from_json(&json, &panes, cx).unwrap();
            })
        });
        cx.read(|cx| {
            let item = &tg.read(cx).panes[0].read(cx).items()[0];
            assert_eq!(item.serialization_key(), "registered_test");
            assert_eq!(item.serialize(cx), config);
            assert!(item.entity_any(cx).downcast::<RegisteredTestView>().is_ok());
        });
    }

    #[gpui::test]
    fn from_json_rejects_older_version(cx: &mut gpui::TestAppContext) {
        let version = SUPPORTED_LAYOUT_VERSION - 1;
        let json = empty_pane_layout(version);
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        let rejected = cx.update(|cx| {
            tg.update(cx, |_this, cx| {
                let registry = ItemRegistry::default();
                matches!(
                    TileGroup::from_json(&json, &registry, cx),
                    Err(LoadError::UnsupportedVersion(v)) if v == version
                )
            })
        });
        assert!(rejected, "an older layout version must be rejected");
    }

    #[gpui::test]
    fn from_json_rejects_newer_version(cx: &mut gpui::TestAppContext) {
        let json = empty_pane_layout(SUPPORTED_LAYOUT_VERSION + 1);
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        let rejected = cx.update(|cx| {
            tg.update(cx, |_this, cx| {
                let registry = ItemRegistry::default();
                matches!(
                    TileGroup::from_json(&json, &registry, cx),
                    Err(LoadError::UnsupportedVersion(v)) if v == SUPPORTED_LAYOUT_VERSION + 1
                )
            })
        });
        assert!(rejected, "a version above the ceiling must be rejected");
    }
}
