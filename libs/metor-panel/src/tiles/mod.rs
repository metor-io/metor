pub(crate) mod dock;
pub(crate) mod drag;
pub(crate) mod item;
mod motion;
pub(crate) mod pane;
pub mod panels;
pub(crate) mod serial;
mod split_preview;
pub(crate) mod starter;
pub(crate) mod tab_bar;

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    AnyElement, App, Axis, Context, DragMoveEvent, Entity, EntityId, EventEmitter, IntoElement,
    Pixels, Point, Render, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
    relative,
};

use smallvec::SmallVec;

use crate::theme::theme;
use drag::ResizeDrag;
use serial::{TileItem, TileNode, TilePane, TileSplit, TileTab};

pub use dock::Edge;
pub use drag::SplitDirection;
pub(crate) use drag::take_active_drag;
pub use item::{PaneItem, PaneItemHandle};
pub use pane::{
    OpenImageAction, OpenOutlineAction, OpenPivotTableAction, Pane, PaneEvent, PlotComponentAction,
    PreviewPlotAction,
};
pub use serial::{ItemRegistry, TileLayout};
pub use tab_bar::TabId;

/// Sequence of member indices locating a node in the split tree.
///
/// `SmallVec` inlines the first four levels since the UI is rarely nested
/// deeper and the path is cloned on every handle render.
pub(crate) type SplitPath = SmallVec<[usize; 4]>;

/// Which of a group's split trees a [`SplitPath`] runs through: the tab on
/// screen's, or a dock's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Region {
    Tab,
    Dock(Edge),
}

actions!(
    metor_panel,
    [
        /// Put back the split layout a zoomed pane is covering. Bound to a
        /// bare key, so the app only offers it while a pane is zoomed.
        ExitZoom,
    ]
);

/// Events the tile group forwards to its owning view.
pub enum TileGroupEvent {
    /// User asked to inspect a pane item (typically via right-click on a
    /// pane header). The position marks where to anchor the inspector.
    Inspect {
        item: Box<dyn PaneItemHandle>,
        pane: Entity<Pane>,
        position: Point<Pixels>,
    },
    InspectPane {
        pane: Entity<Pane>,
        position: Point<Pixels>,
    },
    /// An empty pane's Time Series button: open the trace wizard aimed at it.
    NewTimeSeries { pane: Entity<Pane> },
    /// A top-level tab was right-clicked.
    InspectTab { tab: TabId, position: Point<Pixels> },
}

impl EventEmitter<TileGroupEvent> for TileGroup {}

const RESIZE_HANDLE_SIZE: f32 = 1.0;

/// How long a dragged pane must hover a background tab before it opens, so
/// the pane can then be dropped at a precise spot in that tab.
const SPRING_LOAD: Duration = Duration::from_millis(450);

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
/// - 6: tabs move to the top of the window: the document is a list of tabs,
///   each a split tree, and a pane holds at most one item. Panes pinned
///   along the top and bottom edges sit in optional docks beside the tabs.
pub(crate) const SUPPORTED_LAYOUT_VERSION: u32 = serial::TILE_LAYOUT_VERSION;

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
#[derive(Clone)]
enum Member {
    Pane(Entity<Pane>),
    Axis(SplitAxis),
}

/// Interior node laying children out along a single axis.
///
/// `flexes[i]` is the flex-grow weight for `members[i]`; during resize these
/// values shift within a pair so the total for an axis stays constant.
#[derive(Clone)]
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
    /// Insert `new` (a pane, or a whole tab's tree) next to `target` along
    /// `direction`.
    ///
    /// If `target` already sits in a split along the matching axis, `new`
    /// is inserted as a sibling; otherwise a new `SplitAxis` wraps both.
    /// Returns `true` if `target` was found.
    fn split(&mut self, target: &Entity<Pane>, new: &Member, direction: SplitDirection) -> bool {
        match self {
            Member::Pane(pane) => {
                if pane.entity_id() != target.entity_id() {
                    return false;
                }
                let old = Member::Pane(pane.clone());
                let new = new.clone();
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
                        let new = new.clone();
                        let insert_at = if direction.increasing() { i + 1 } else { i };
                        axis.members.insert(insert_at, new);
                        axis.flexes.insert(insert_at, 1.0);
                        return true;
                    }
                }
                for member in &mut axis.members {
                    if member.split(target, new, direction) {
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
                TileNode::Pane(TilePane {
                    item: pane.item().map(|item| TileItem {
                        kind: item.serialization_key().to_string(),
                        state: item.serialize(cx),
                    }),
                    hide_header: pane.hide_header(),
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

    /// `moving` names the pane a zoom is drawing over the layout, whose slot
    /// is left empty here.
    fn render(
        &self,
        region: Region,
        path: SplitPath,
        tile_group: &Entity<TileGroup>,
        gaps: &split_preview::GapWeights,
        moving: Option<gpui::EntityId>,
        cx: &mut App,
    ) -> AnyElement {
        match self {
            Member::Pane(pane) => {
                let tg = tile_group.clone();
                let id = pane.entity_id();
                let slot = gpui::canvas(
                    move |bounds, _, cx| {
                        tg.update(cx, |this, _| {
                            this.pane_bounds.insert(id, bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full();
                if moving == Some(id) {
                    return div().relative().size_full().child(slot).into_any_element();
                }
                if gaps.contains_key(&pane.entity_id()) {
                    let placeholder = pane.read(cx).is_empty();
                    let landing = gaps.landing == Some(id);
                    let theme = theme(cx);
                    return div()
                        .size_full()
                        .when(!landing, |gap| gap.bg(theme.bg_secondary))
                        .when(placeholder, |gap| {
                            gap.debug_selector(|| "split-preview-gap".into())
                        })
                        .when(landing, |gap| {
                            gap.bg(theme.drop_fill())
                                .border_1()
                                .border_color(crate::theme::Theme::dim(theme.drop_accent(), 0.6))
                        })
                        .into_any_element();
                }
                // Record this pane as current on any click within it. Mouse
                // events bubble, so this fires after inner handlers (tab clicks,
                // content interactions) without intercepting them.
                let tg = tile_group.clone();
                let focus_pane = pane.clone();
                div()
                    .relative()
                    .size_full()
                    .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                        tg.update(cx, |this, _cx| this.focus_pane(&focus_pane));
                        crate::inspector::target::select_pane(&focus_pane, cx);
                    })
                    .child(slot)
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
                        // Keep exactly one divider between surviving members,
                        // including when a sole-tab source disappears mid-axis.
                        let gap_amount = (!gaps.is_empty()).then(|| {
                            let preceding = axis.members[..ix]
                                .iter()
                                .map(|member| gap_weight(member, gaps).unwrap_or(1.0))
                                .fold(0.0_f32, f32::max);
                            gap_weight(member, gaps).unwrap_or(1.0) * preceding
                        });
                        let handle = if !gaps.is_empty()
                            || prev_locked.is_some()
                            || cur_locked.is_some()
                        {
                            render_static_resize_handle(axis.axis, cx)
                                .when_some(gap_amount, |handle, amount| match axis.axis {
                                    Axis::Horizontal => handle.w(px(RESIZE_HANDLE_SIZE * amount)),
                                    Axis::Vertical => handle.h(px(RESIZE_HANDLE_SIZE * amount)),
                                })
                                .into_any_element()
                        } else {
                            render_resize_handle(
                                region,
                                path.clone(),
                                ix,
                                axis.axis,
                                tile_group,
                                cx,
                            )
                            .into_any_element()
                        };
                        children.push(handle);
                    }

                    let gap_amount = gap_weight(member, gaps);
                    let flex = axis.flexes[ix] * gap_amount.unwrap_or(1.0);
                    let mut child_path = path.clone();
                    child_path.push(ix);

                    let child_element =
                        member.render(region, child_path, tile_group, gaps, moving, cx);
                    let locked_px = gap_amount
                        .is_none()
                        .then(|| locked_along_axis(member, axis.axis, cx))
                        .flatten();

                    let mut child_div = div()
                        .flex_basis(relative(0.))
                        .overflow_hidden()
                        .min_w(px(50.0 * gap_amount.unwrap_or(1.0)))
                        .min_h(px(50.0 * gap_amount.unwrap_or(1.0)))
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
                let committed_layout = gaps.is_empty();
                let bounds_tracker = gpui::canvas(
                    move |bounds, _window, cx| {
                        if committed_layout {
                            tg.update(cx, |this, _| {
                                this.axis_bounds.insert((region, p), bounds);
                            });
                        }
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

fn gap_weight(member: &Member, gaps: &split_preview::GapWeights) -> Option<f32> {
    match member {
        Member::Pane(pane) => gaps.get(&pane.entity_id()).copied(),
        Member::Axis(_) => None,
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
fn render_static_resize_handle(axis: Axis, cx: &App) -> gpui::Div {
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
    region: Region,
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
    id_hash = id_hash.wrapping_mul(31).wrapping_add(match region {
        Region::Tab => 0,
        Region::Dock(Edge::Top) => 1,
        Region::Dock(Edge::Bottom) => 2,
    });
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
                region,
                path: path.clone(),
                handle_ix,
            },
            |drag, _, _, cx| cx.new(|_| drag.clone()),
        )
        .on_drag_move({
            let tg = tg.clone();
            move |event: &DragMoveEvent<ResizeDrag>, _window, cx| {
                // Every handle hears every resize drag; only the grabbed one
                // answers.
                let drag = event.drag(cx);
                if drag.region != region || drag.path != path || drag.handle_ix != handle_ix {
                    return;
                }
                let position = event.event.position;
                let path = path.clone();
                tg.update(cx, |this, cx| {
                    this.handle_resize(region, path, handle_ix, position, cx);
                });
            }
        })
}

/// One top-level tab: a split tree of panes with its own focus and zoom.
///
/// Panes are flat-indexed for fast iteration; the tree is only consulted for
/// layout and structural edits.
struct Tab {
    id: TabId,
    root: Member,
    panes: Vec<Entity<Pane>>,
    /// The pane that last received a click, i.e. the "current" tile that
    /// keyboard-driven commands (the transient chord menu) act on. Falls back
    /// to the first pane via [`Tab::active_pane`] when unset or stale.
    focused_pane: Option<Entity<Pane>>,
    /// The pane filling the whole tab in place of the tree, toggled from a
    /// header double-click. The tree is left untouched underneath and the
    /// pane keeps its entity, so its item keeps its state across the
    /// toggle. Transient: never serialized, and cleared by any edit that
    /// changes which panes exist so the user is never left staring past a
    /// new one.
    zoomed: Option<Entity<Pane>>,
}

impl Tab {
    fn new(id: TabId, root: Member, panes: Vec<Entity<Pane>>) -> Self {
        Self {
            id,
            focused_pane: panes.first().cloned(),
            root,
            panes,
            zoomed: None,
        }
    }

    fn active_pane(&self) -> Option<Entity<Pane>> {
        self.focused_pane
            .as_ref()
            .filter(|pane| self.panes.contains(pane))
            .cloned()
            .or_else(|| self.panes.first().cloned())
    }

    /// What the tab bar calls this tab: whatever its current pane shows, so
    /// a tab reads as the view the user last worked in.
    fn title(&self, cx: &App) -> SharedString {
        let title = |pane: &Entity<Pane>| pane.read(cx).item().map(|item| item.tab_title(cx));
        self.active_pane()
            .and_then(|pane| title(&pane))
            .or_else(|| self.panes.iter().find_map(title))
            .unwrap_or_else(|| SharedString::new_static("New tab"))
    }

    fn can_close(&self, cx: &App) -> bool {
        self.panes
            .iter()
            .all(|pane| pane.read(cx).item().is_none_or(|item| item.can_close(cx)))
    }
}

/// Owns a window's top-level tabs and the split tree each one lays out.
///
/// Tabs exist only here, at the top of the window — a pane shows exactly one
/// item — so there is one place to look for everything open, and a dragged
/// pane either splits the pane it is dropped on or moves into the tab it is
/// dropped on. The tab bar itself is drawn by the window chrome (see
/// [`tab_bar`]); this view draws the active tab's tree. Axis bounds are
/// cached during paint so resize drags can convert pixel positions into flex
/// weights without a second traversal.
pub struct TileGroup {
    tabs: Vec<Tab>,
    active_tab: usize,
    /// Panes pinned along the window's top and bottom, shown whichever tab
    /// is.
    top_dock: Option<dock::Dock>,
    bottom_dock: Option<dock::Dock>,
    /// Where the whole group, docks included, was last drawn.
    bounds: gpui::Bounds<Pixels>,
    /// One per pane this group holds. Dropping it is how a pane that moved
    /// to another group stops reporting here.
    subscriptions: HashMap<EntityId, Subscription>,
    axis_bounds: HashMap<(Region, SplitPath), gpui::Bounds<Pixels>>,
    /// The zoomed pane growing over the layout, or shrinking back into it.
    zoom_motion: Option<motion::ZoomMotion>,
    /// Where each pane was last drawn, which is where a zoom grows from and
    /// shrinks back to.
    pane_bounds: HashMap<EntityId, gpui::Bounds<Pixels>>,
    split_preview: split_preview::SplitPreview,
    /// The background tab a dragged pane is hovering, and the timer that
    /// opens it.
    spring: Option<(TabId, Task<()>)>,
    bar: tab_bar::BarState,
}

impl TileGroup {
    /// Build a group with one tab per item, or a single empty tab when
    /// `items` is empty.
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty();
        if items.is_empty() {
            let pane = cx.new(|cx| Pane::new(None, cx));
            this.push_tab(pane, cx);
        }
        for item in items {
            let pane = cx.new(|cx| Pane::new(Some(item), cx));
            this.push_tab(pane, cx);
        }
        this
    }

    /// Adopt an already-constructed pane as the sole member of one tab.
    ///
    /// Use when the caller needs to hold the pane entity before handing
    /// ownership of the layout to the tile group.
    pub fn from_pane(pane: Entity<Pane>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty();
        this.push_tab(pane, cx);
        this
    }

    fn empty() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            top_dock: None,
            bottom_dock: None,
            bounds: Default::default(),
            subscriptions: HashMap::new(),
            axis_bounds: HashMap::new(),
            zoom_motion: None,
            pane_bounds: HashMap::new(),
            split_preview: Default::default(),
            spring: None,
            bar: Default::default(),
        }
    }

    /// Unique across every window's group, so a tab dragged between windows
    /// is never mistaken for one of its new bar's own.
    fn next_id(&mut self) -> TabId {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        TabId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// Start reporting `pane`'s events here and mark this group its owner.
    fn adopt(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let group = cx.weak_entity();
        pane.update(cx, |pane, cx| {
            pane.group = Some(group);
            pane.set_docked(false, cx);
        });
        self.subscriptions.insert(
            pane.entity_id(),
            cx.subscribe(pane, Self::handle_pane_event),
        );
    }

    fn release(&mut self, pane: &Entity<Pane>) {
        self.subscriptions.remove(&pane.entity_id());
        self.pane_bounds.remove(&pane.entity_id());
    }

    fn push_tab(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) -> usize {
        self.adopt(&pane, cx);
        let id = self.next_id();
        self.tabs
            .push(Tab::new(id, Member::Pane(pane.clone()), vec![pane]));
        self.tabs.len() - 1
    }

    fn insert_tab(&mut self, mut tab: Tab, at: usize, cx: &mut Context<Self>) -> usize {
        for pane in &tab.panes {
            self.adopt(pane, cx);
        }
        tab.id = self.next_id();
        tab.zoomed = None;
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, tab);
        if at <= self.active_tab && self.tabs.len() > 1 {
            self.active_tab += 1;
        }
        at
    }

    /// Take tab `ix` out of the group, keeping one empty tab behind when it
    /// was the last.
    fn remove_tab_at(&mut self, ix: usize, cx: &mut Context<Self>) -> Tab {
        let tab = self.tabs.remove(ix);
        for pane in &tab.panes {
            self.release(pane);
        }
        if self.tabs.is_empty() {
            let pane = cx.new(|cx| Pane::new(None, cx));
            self.push_tab(pane, cx);
        }
        if ix < self.active_tab || self.active_tab >= self.tabs.len() {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
        self.forget_layout();
        cx.notify();
        tab
    }

    /// Drop what was measured for the tree on screen, for a switch to
    /// another tab's tree.
    fn forget_layout(&mut self) {
        self.axis_bounds
            .retain(|(region, _), _| *region != Region::Tab);
        self.zoom_motion = None;
        self.split_preview.forget_regions();
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    fn tab_of(&self, pane: &Entity<Pane>) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.panes.contains(pane))
    }

    pub fn tab_index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    pub fn active_tab(&self) -> TabId {
        self.tab().id
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The active tab's panes.
    pub fn panes(&self) -> &[Entity<Pane>] {
        &self.tab().panes
    }

    /// Every pane in every tab, active tab first, then the docked ones.
    pub fn all_panes(&self) -> impl Iterator<Item = &Entity<Pane>> {
        self.tab()
            .panes
            .iter()
            .chain(
                self.tabs
                    .iter()
                    .enumerate()
                    .filter(|(ix, _)| *ix != self.active_tab)
                    .flat_map(|(_, tab)| tab.panes.iter()),
            )
            .chain(self.docked_panes())
    }

    pub fn zoomed(&self) -> Option<&Entity<Pane>> {
        self.tab().zoomed.as_ref()
    }

    pub fn activate_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_index(id)
            && ix != self.active_tab
        {
            self.active_tab = ix;
            self.forget_layout();
            cx.notify();
        }
    }

    /// Open a tab holding one empty pane and switch to it.
    pub fn new_tab(&mut self, cx: &mut Context<Self>) -> Entity<Pane> {
        let pane = cx.new(|cx| Pane::new(None, cx));
        self.active_tab = self.push_tab(pane.clone(), cx);
        self.forget_layout();
        cx.notify();
        pane
    }

    /// Close tab `id` and every pane in it, unless one of its items refuses
    /// to close.
    pub fn close_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_index(id)
            && self.tabs[ix].can_close(cx)
        {
            self.remove_tab_at(ix, cx);
        }
    }

    pub fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if len > 1 {
            let ix = if forward {
                (self.active_tab + 1) % len
            } else {
                (self.active_tab + len - 1) % len
            };
            self.activate_tab(self.tabs[ix].id, cx);
        }
    }

    /// Move tab `id` so it sits before `before`, or last without one.
    pub fn move_tab(&mut self, id: TabId, before: Option<TabId>, cx: &mut Context<Self>) {
        if Some(id) == before {
            return;
        }
        let Some(from) = self.tab_index(id) else {
            return;
        };
        let active = self.tab().id;
        let tab = self.tabs.remove(from);
        let to = before
            .and_then(|before| self.tab_index(before))
            .unwrap_or(self.tabs.len());
        self.tabs.insert(to, tab);
        self.active_tab = self.tab_index(active).unwrap_or(0);
        cx.notify();
    }

    /// Bring tab `id` of `source` (another window's group) into this one
    /// before `before`, and show it.
    pub fn adopt_tab(
        &mut self,
        source: &Entity<TileGroup>,
        id: TabId,
        before: Option<TabId>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = source.update(cx, |source, cx| {
            let ix = source.tab_index(id)?;
            Some(source.remove_tab_at(ix, cx))
        }) else {
            return;
        };
        let at = before
            .and_then(|before| self.tab_index(before))
            .unwrap_or(self.tabs.len());
        self.active_tab = self.insert_tab(tab, at, cx);
        self.forget_layout();
        cx.notify();
    }

    /// Lift tab `id` out into a group of its own, for a new window.
    pub fn split_off_tab(&mut self, id: TabId, cx: &mut Context<Self>) -> Option<Entity<Self>> {
        let ix = self.tab_index(id)?;
        let tab = self.remove_tab_at(ix, cx);
        Some(cx.new(|cx| {
            let mut group = Self::empty();
            group.insert_tab(tab, 0, cx);
            group
        }))
    }

    /// Zoom `pane` over its tab's layout, or restore the layout when it is
    /// the one already zoomed. A lone pane already fills the tab, so there
    /// is nothing to zoom.
    pub fn toggle_zoom(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_of(pane) else {
            return;
        };
        if self.tabs[ix].zoomed.as_ref() == Some(pane) {
            self.exit_zoom(cx);
        } else if self.tabs[ix].panes.len() > 1 && ix == self.active_tab {
            self.animate_zoom(pane.clone(), true, cx);
            let tab = &mut self.tabs[ix];
            tab.zoomed = Some(pane.clone());
            tab.focused_pane = Some(pane.clone());
            cx.notify();
        }
    }

    pub fn exit_zoom(&mut self, cx: &mut Context<Self>) {
        let ix = self.active_tab;
        if let Some(pane) = self.tabs[ix].zoomed.take() {
            self.animate_zoom(pane, false, cx);
            cx.notify();
        }
    }

    /// Grow `pane` from its slot (`zooming`) or shrink it back, unless
    /// motion is turned down or the pane has not been drawn yet.
    fn animate_zoom(&mut self, pane: Entity<Pane>, zooming: bool, cx: &App) {
        let drawn = self.pane_bounds.contains_key(&pane.entity_id());
        self.zoom_motion = (drawn && crate::motion::enabled(cx)).then(|| {
            motion::ZoomMotion::new(
                pane,
                zooming,
                self.zoom_motion.as_ref(),
                std::time::Instant::now(),
            )
        });
    }

    /// The pane moving between its slot and the whole tab this frame, and
    /// how much of the tab it covers. Ends the motion once it has arrived,
    /// when a drag starts, or when an edit has since changed the zoom out
    /// from under it.
    fn zoom_frame(&mut self, cx: &App) -> Option<(Entity<Pane>, f32)> {
        let motion = self.zoom_motion.as_ref()?;
        let tab = self.tab();
        let current = motion.zooming() == (tab.zoomed.as_ref() == Some(&motion.pane));
        let frame = (current && tab.panes.contains(&motion.pane) && !cx.has_active_drag())
            .then(|| motion.coverage(std::time::Instant::now()))
            .flatten()
            .map(|coverage| (motion.pane.clone(), coverage));
        if frame.is_none() {
            self.zoom_motion = None;
        }
        frame
    }

    /// The "current" pane keyboard commands target: the active tab's
    /// last-clicked pane if it still exists, otherwise its first pane.
    pub fn active_pane(&self, _cx: &App) -> Option<Entity<Pane>> {
        self.tab().active_pane()
    }

    /// Record `pane` as current in its tab.
    fn focus_pane(&mut self, pane: &Entity<Pane>) {
        if let Some(ix) = self.tab_of(pane) {
            self.tabs[ix].focused_pane = Some(pane.clone());
        }
    }

    /// Show `pane`: switch to its tab, make it current, and lift any zoom
    /// that hides it.
    fn reveal(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_of(pane) else {
            return;
        };
        let id = self.tabs[ix].id;
        self.activate_tab(id, cx);
        let tab = &mut self.tabs[ix];
        if tab.zoomed.as_ref().is_some_and(|zoomed| zoomed != pane) {
            tab.zoomed = None;
        }
        tab.focused_pane = Some(pane.clone());
        cx.notify();
    }

    /// Reveal the panel of `kind` (a [`PaneItem::serialization_key`]): when one
    /// is already open in any tab, switch to it and make its pane current;
    /// otherwise build a fresh item with `make` and open it from the active
    /// pane.
    ///
    /// This is the entry point for "jump to X" affordances (the titlebar alarm
    /// summary, and any future status-bar shortcut) — unlike the palette's
    /// explicit "New Panel" commands, revealing must not multiply panels.
    pub fn focus_or_open(
        &mut self,
        kind: &str,
        make: impl FnOnce(&mut Context<Pane>) -> Option<Box<dyn PaneItemHandle>>,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .all_panes()
            .find(|pane| {
                pane.read(cx)
                    .item()
                    .is_some_and(|item| item.serialization_key() == kind)
            })
            .cloned();
        match existing {
            Some(pane) => self.reveal(&pane, cx),
            None => {
                if let Some(pane) = self.active_pane(cx) {
                    pane.update(cx, |pane, cx| {
                        if let Some(item) = make(cx) {
                            pane.add_item(item, cx);
                        }
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
        if let Some(edge) = self.docked(target) {
            if let Some(dock) = self.dock_mut(edge)
                && dock
                    .root
                    .split(target, &Member::Pane(new_pane.clone()), direction)
            {
                dock.panes.push(new_pane.clone());
                self.adopt(&new_pane, cx);
                new_pane.update(cx, |pane, cx| pane.set_docked(true, cx));
                cx.notify();
            }
            return;
        }
        // A missing `target` makes the split a no-op; tracking `new_pane`
        // anyway would leave a pane that's counted but never rendered — and
        // swallow whatever item it was carrying.
        let Some(ix) = self.tab_of(target) else {
            return;
        };
        if !self.tabs[ix]
            .root
            .split(target, &Member::Pane(new_pane.clone()), direction)
        {
            return;
        }
        self.adopt(&new_pane, cx);
        let tab = &mut self.tabs[ix];
        tab.zoomed = None;
        // A freshly-split pane becomes the current one so a follow-up chord
        // (e.g. "new tile") lands in the pane the user just created.
        tab.focused_pane = Some(new_pane.clone());
        tab.panes.push(new_pane);
        cx.notify();
    }

    /// Drop a pane from the layout. A tab left with no panes closes, unless
    /// it was the last, which gets a fresh empty pane. Returns whether the
    /// pane was here.
    pub fn remove_pane(&mut self, target: &Entity<Pane>, cx: &mut Context<Self>) -> bool {
        if self.undock(target, cx) {
            return true;
        }
        let Some(ix) = self.tab_of(target) else {
            return false;
        };
        self.release(target);
        let tab = &mut self.tabs[ix];
        tab.root.remove(target);
        tab.root.collapse();
        tab.panes.retain(|pane| pane != target);
        if tab.zoomed.as_ref() == Some(target) || tab.panes.len() < 2 {
            tab.zoomed = None;
        }
        if tab.panes.is_empty() {
            if self.tabs.len() > 1 {
                self.remove_tab_at(ix, cx);
            } else {
                let pane = cx.new(|cx| Pane::new(None, cx));
                self.adopt(&pane, cx);
                let id = self.tabs[ix].id;
                self.tabs[ix] = Tab::new(id, Member::Pane(pane.clone()), vec![pane]);
            }
        }
        cx.notify();
        true
    }

    /// Take `pane` out of whichever group holds it — this one, or another
    /// window's. Returns whether it was found.
    fn detach(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) -> bool {
        let owner = pane.read(cx).group.as_ref().and_then(|g| g.upgrade());
        match owner {
            Some(owner) if owner.entity_id() != cx.entity_id() => {
                owner.update(cx, |owner, cx| owner.remove_pane(pane, cx))
            }
            _ => self.remove_pane(pane, cx),
        }
    }

    /// Move `pane` beside `target`, splitting toward `direction`. The pane
    /// keeps its entity, so its item keeps its state.
    pub fn move_pane(
        &mut self,
        pane: Entity<Pane>,
        target: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        if &pane == target || self.tab_of(target).is_none() || !self.detach(&pane, cx) {
            return;
        }
        self.split_pane(target, pane.clone(), direction, cx);
        self.reveal(&pane, cx);
    }

    /// Move `pane` into tab `id`: in place of the tab's lone empty pane, or
    /// beside the pane current there. The tab then shows.
    pub fn move_pane_to_tab(&mut self, pane: Entity<Pane>, id: TabId, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(id) else {
            return;
        };
        if self.tabs[ix].panes.contains(&pane) {
            self.activate_tab(id, cx);
            return;
        }
        if !self.detach(&pane, cx) {
            return;
        }
        let Some(ix) = self.tab_index(id) else {
            return;
        };
        let tab = &self.tabs[ix];
        let lone_empty = match tab.panes.as_slice() {
            [only] if only.read(cx).is_empty() => Some(only.clone()),
            _ => None,
        };
        if let Some(empty) = lone_empty {
            self.release(&empty);
            self.adopt(&pane, cx);
            self.tabs[ix] = Tab::new(id, Member::Pane(pane.clone()), vec![pane.clone()]);
        } else if let Some(anchor) = tab.active_pane() {
            let direction = SplitDirection::along_longer(anchor.read(cx).bounds().size);
            self.split_pane(&anchor, pane.clone(), direction, cx);
        }
        self.reveal(&pane, cx);
    }

    /// Move `pane` into a tab of its own at the end of the bar.
    pub fn move_pane_to_new_tab(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_of(&pane)
            && self.tabs[ix].panes.len() == 1
        {
            let id = self.tabs[ix].id;
            self.move_tab(id, None, cx);
            self.activate_tab(id, cx);
            return;
        }
        if !self.detach(&pane, cx) {
            return;
        }
        self.active_tab = self.push_tab(pane, cx);
        self.forget_layout();
        cx.notify();
    }

    /// Lift `pane` out into a group of its own, for a new window.
    pub fn split_off_pane(
        &mut self,
        pane: Entity<Pane>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Self>> {
        if !self.detach(&pane, cx) {
            return None;
        }
        Some(cx.new(|cx| Self::from_pane(pane, cx)))
    }

    /// A dragged pane is over tab `id` (or over no tab): after a moment,
    /// open the tab so the pane can be dropped into its layout.
    pub(crate) fn spring_load(&mut self, id: Option<TabId>, cx: &mut Context<Self>) {
        let id = id.filter(|id| *id != self.tab().id);
        if self.spring.as_ref().map(|(tab, _)| *tab) == id {
            return;
        }
        self.spring = id.map(|id| {
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SPRING_LOAD).await;
                let _ = this.update(cx, |this, cx| {
                    if this.spring.as_ref().is_some_and(|(tab, _)| *tab == id)
                        && cx.has_active_drag()
                    {
                        // Detached rather than dropped: this is that task.
                        if let Some((_, task)) = this.spring.take() {
                            task.detach();
                        }
                        this.activate_tab(id, cx);
                    }
                });
            });
            (id, task)
        });
    }

    /// Whether any pane holds an item. A fresh group keeps one empty pane
    /// around, so "no items anywhere" is the real test for an untouched
    /// layout (used to decide if loading a saved one needs consent).
    pub fn has_items(&self, cx: &App) -> bool {
        self.all_panes().any(|pane| !pane.read(cx).is_empty())
    }

    /// Snapshot the full layout (tabs, tree shapes, flexes, and each item's
    /// own persisted state) for saving to disk.
    pub fn serialize(&self, cx: &App) -> TileLayout {
        TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: crate::temporal::legacy_range(cx),
            temporal: Some(crate::temporal::save_layout(cx)),
            tabs: self
                .tabs
                .iter()
                .map(|tab| TileTab {
                    root: tab.root.serialize(cx),
                })
                .collect(),
            active_tab: self.active_tab,
            top_dock: self.serialize_dock(Edge::Top, cx),
            bottom_dock: self.serialize_dock(Edge::Bottom, cx),
        }
    }

    fn serialize_dock(&self, edge: Edge, cx: &App) -> Option<serial::TileDock> {
        self.dock(edge).map(|dock| serial::TileDock {
            root: dock.root.serialize(cx),
            height: dock.height.into(),
        })
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
    pub fn replace_from_json(
        &mut self,
        json: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), LoadError> {
        self.replace_from_layout(serde_json::from_str(json)?, cx)
    }

    /// Swap the layout in place from an already-parsed document.
    ///
    /// Pulls the panel-item registry from gpui globals so callers don't have
    /// to thread it. The clone keeps the global borrow short so the rest of
    /// the deserialization can take `&mut Context<Self>` freely.
    pub fn replace_from_layout(
        &mut self,
        layout: TileLayout,
        cx: &mut Context<Self>,
    ) -> Result<(), LoadError> {
        if layout.version != SUPPORTED_LAYOUT_VERSION {
            return Err(LoadError::UnsupportedVersion(layout.version));
        }
        let registry = cx.global::<ItemRegistry>().clone();
        *self = Self::deserialize(layout, &registry, cx);
        cx.notify();
        Ok(())
    }

    /// Rebuild a layout from a [`TileLayout`]. Items whose serialization
    /// key is absent from `registry` load as inert placeholders.
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
        if let Some(controller) = crate::temporal::controller(cx) {
            let config = crate::temporal::TemporalConfig {
                range: crate::temporal::config(cx).range,
                ..Default::default()
            };
            controller.update(cx, |c, cx| c.restore(config, cx));
        }
        if let Some(time) = serialized.temporal {
            crate::temporal::restore_layout(&time, cx);
        }
        let mut this = Self::empty();
        for tab in &serialized.tabs {
            let mut panes = Vec::new();
            let root = Self::deserialize_member(&tab.root, registry, &mut panes, cx);
            for pane in &panes {
                this.adopt(pane, cx);
            }
            let id = this.next_id();
            this.tabs.push(Tab::new(id, root, panes));
        }
        if this.tabs.is_empty() {
            let pane = cx.new(|cx| Pane::new(None, cx));
            this.push_tab(pane, cx);
        }
        this.active_tab = serialized.active_tab.min(this.tabs.len() - 1);
        for (edge, dock) in [
            (Edge::Top, &serialized.top_dock),
            (Edge::Bottom, &serialized.bottom_dock),
        ] {
            let Some(dock) = dock else {
                continue;
            };
            let mut panes = Vec::new();
            let root = Self::deserialize_member(&dock.root, registry, &mut panes, cx);
            this.dock_in(edge, root, panes, px(dock.height), cx);
        }
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
                    let item = sp
                        .item
                        .as_ref()
                        .and_then(|si| registry.deserialize(&si.kind, &si.state, cx));
                    let mut pane = Pane::new(item, cx);
                    pane.set_hide_header(sp.hide_header, cx);
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

    fn handle_dock_drop(
        &mut self,
        dragged: &drag::DraggedPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.split_preview.drop_target(window.mouse_position());
        self.split_preview.clear();
        if let Some((pane, direction)) = target
            && self.tab().panes.contains(&pane)
        {
            self.move_pane(dragged.pane.clone(), &pane, direction, cx);
        }
        cx.notify();
    }

    fn handle_tab_dock_drop(
        &mut self,
        dragged: &drag::DraggedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.split_preview.drop_target(window.mouse_position());
        self.split_preview.clear();
        if let Some((pane, direction)) = target
            && self.tab().panes.contains(&pane)
        {
            self.merge_tab(&dragged.group, dragged.tab, &pane, direction, cx);
        }
        cx.notify();
    }

    /// The panes a drag can land on: the zoomed one alone while zoomed,
    /// since hidden panes keep stale bounds from before the zoom.
    fn droppable_panes(&self) -> Vec<Entity<Pane>> {
        let tab = self.tab();
        match &tab.zoomed {
            Some(pane) => vec![pane.clone()],
            None => tab.panes.clone(),
        }
    }

    /// Fold tab `id` of `source` (this group or another window's) into the
    /// layout beside `target`, splitting toward `direction`. The tab's own
    /// split tree comes along whole, and the tab closes.
    pub fn merge_tab(
        &mut self,
        source: &Entity<TileGroup>,
        id: TabId,
        target: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        let take = |group: &mut TileGroup, cx: &mut Context<TileGroup>| {
            let ix = group.tab_index(id)?;
            (!group.tabs[ix].panes.contains(target)).then(|| group.remove_tab_at(ix, cx))
        };
        let tab = if source.entity_id() == cx.entity_id() {
            take(self, cx)
        } else {
            source.update(cx, take)
        };
        let Some(tab) = tab else {
            return;
        };
        let Some(ix) = self.tab_of(target) else {
            return;
        };
        if !self.tabs[ix].root.split(target, &tab.root, direction) {
            return;
        }
        for pane in &tab.panes {
            self.adopt(pane, cx);
        }
        let focus = tab.active_pane();
        let into = &mut self.tabs[ix];
        into.zoomed = None;
        into.panes.extend(tab.panes);
        into.focused_pane = focus;
        let id = into.id;
        self.activate_tab(id, cx);
        cx.notify();
    }

    fn handle_pane_event(&mut self, pane: Entity<Pane>, event: &PaneEvent, cx: &mut Context<Self>) {
        match event {
            PaneEvent::Split { direction, item } => {
                let item = item.as_ref().map(|item| item.clone_handle());
                let new_pane = cx.new(|cx| Pane::new(item, cx));
                self.split_pane(&pane, new_pane, *direction, cx);
            }
            PaneEvent::Empty => {
                // The last pane of the last tab stays, showing its starters,
                // so the window is never void.
                let lone = self.docked(&pane).is_none()
                    && self.tabs.len() == 1
                    && self.tabs[0].panes.len() == 1;
                if !lone {
                    self.remove_pane(&pane, cx);
                }
            }
            PaneEvent::Inspect { item, position } => {
                cx.emit(TileGroupEvent::Inspect {
                    item: item.clone_handle(),
                    pane,
                    position: *position,
                });
            }
            PaneEvent::InspectPane { position } => {
                cx.emit(TileGroupEvent::InspectPane {
                    pane: pane.clone(),
                    position: *position,
                });
            }
            PaneEvent::ToggleZoom => self.toggle_zoom(&pane, cx),
            PaneEvent::NewTimeSeries => cx.emit(TileGroupEvent::NewTimeSeries { pane }),
        }
    }

    fn handle_resize(
        &mut self,
        region: Region,
        path: SplitPath,
        handle_ix: usize,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let bounds = match self.axis_bounds.get(&(region, path.clone())) {
            Some(b) => *b,
            None => return,
        };

        if let Some(axis) = self.find_axis_mut(region, &path) {
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

            // Saved layouts can hold a pair thinner than two minimum panes;
            // split what there is rather than let the bounds cross.
            let min_flex = 0.05f32.min(pair_flex / 2.0);
            let target_left =
                (ratio * flex_sum - flex_before_pair).clamp(min_flex, pair_flex - min_flex);
            let target_right = pair_flex - target_left;

            axis.flexes[left] = target_left;
            axis.flexes[right] = target_right;
            cx.notify();
        }
    }

    fn find_axis_mut(&mut self, region: Region, path: &[usize]) -> Option<&mut SplitAxis> {
        let ix = self.active_tab;
        let mut current = match region {
            Region::Tab => &mut self.tabs[ix].root,
            Region::Dock(edge) => &mut self.dock_mut(edge).as_mut()?.root,
        };
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
        if !cx.has_active_drag()
            && let Some((_, task)) = self.spring.take()
        {
            drop(task);
        }
        // While zoomed the pane stands in for the whole tree, so a drag's
        // split preview opens its gap inside the zoomed area; the drop then
        // edits the real tree and un-zooms.
        let moving = self.zoom_frame(cx);
        if moving.is_some() {
            window.request_animation_frame();
        }
        // While a zoom grows or shrinks the layout shows underneath it, with
        // the pane drawn on top at its in-between size.
        let zoomed = match moving {
            Some(_) => None,
            None => self.tab().zoomed.clone().map(Member::Pane),
        };
        let committed = zoomed.unwrap_or_else(|| self.tab().root.clone());
        let (layout, gaps) = self.split_preview.layout(&committed, window, cx);
        let tracker = tile_group.clone();
        let mut root = div()
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .bg(theme.bg_secondary)
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<drag::DraggedPane>, _, cx| {
                    let panes = this.droppable_panes();
                    let source = Some(event.drag(cx).pane.clone());
                    if this
                        .split_preview
                        .drag_move(&panes, source, event.event.position, cx)
                    {
                        cx.notify();
                    }
                }),
            )
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<drag::DraggedTab>, _, cx| {
                    // A tab can't split into itself.
                    let dragged = event.drag(cx);
                    let own = dragged.group == cx.entity() && dragged.tab == this.active_tab();
                    let panes = if own {
                        Vec::new()
                    } else {
                        this.droppable_panes()
                    };
                    if this
                        .split_preview
                        .drag_move(&panes, None, event.event.position, cx)
                    {
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        tracker.update(cx, |this, _| this.split_preview.bounds = bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(layout.as_ref().unwrap_or(&committed).render(
                Region::Tab,
                SplitPath::new(),
                &tile_group,
                &gaps,
                moving.as_ref().map(|(pane, _)| pane.entity_id()),
                cx,
            ));
        if let Some((pane, coverage)) = moving
            && let Some(slot) = self.pane_bounds.get(&pane.entity_id())
        {
            let group = self.split_preview.bounds;
            let toward = |from: Pixels, to: Pixels| from + (to - from) * coverage;
            root = root.child(
                div()
                    .absolute()
                    .left(toward(slot.origin.x - group.origin.x, px(0.)))
                    .top(toward(slot.origin.y - group.origin.y, px(0.)))
                    .w(toward(slot.size.width, group.size.width))
                    .h(toward(slot.size.height, group.size.height))
                    .overflow_hidden()
                    .bg(theme.bg_secondary)
                    .shadow_lg()
                    .child(pane),
            );
        }
        for (pane, bounds) in self.split_preview.drop_regions() {
            if !self.tab().panes.contains(pane) {
                continue;
            }
            let bounds = *bounds;
            let origin = bounds.origin - self.split_preview.bounds.origin;
            // Original pane bounds stay fixed while the visual tree reflows.
            // This also catches drops into the newly opened gap.
            root = root.child(
                div()
                    .absolute()
                    .left(origin.x)
                    .top(origin.y)
                    .w(bounds.size.width)
                    .h(bounds.size.height)
                    .occlude()
                    .on_drop(cx.listener(Self::handle_dock_drop))
                    .on_drop(cx.listener(Self::handle_tab_dock_drop)),
            );
        }
        let dock = |edge: Edge, cx: &mut App| {
            let dock = self.dock(edge)?;
            let tree = dock.root.render(
                Region::Dock(edge),
                SplitPath::new(),
                &tile_group,
                &Default::default(),
                None,
                cx,
            );
            Some(
                div()
                    .flex_none()
                    .w_full()
                    .h(dock.height)
                    .overflow_hidden()
                    .bg(theme.bg_secondary)
                    .child(tree),
            )
        };
        let top = dock(Edge::Top, cx);
        let bottom = dock(Edge::Bottom, cx);
        let measure = tile_group.clone();
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
                gpui::canvas(
                    move |bounds, _, cx| measure.update(cx, |this, _| this.bounds = bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when_some(top, |group, top| {
                group
                    .child(top)
                    .child(dock::render_resize_handle(Edge::Top, &tile_group, cx))
            })
            .child(root)
            .when_some(bottom, |group, bottom| {
                group
                    .child(dock::render_resize_handle(Edge::Bottom, &tile_group, cx))
                    .child(bottom)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::dashboard::{
        DashboardPanelConfig, DashboardWidget, WidgetId, WidgetKind, WidgetLive, WidgetRect,
        WidgetRegistry, WidgetSpec, deserialize_dashboard,
    };
    use gpui::{AnyView, IntoElement, MouseButton, Render, TestAppContext, Window, point};
    use std::sync::Arc;

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct TestItemConfig {
        title: String,
    }

    struct TestItem {
        title: SharedString,
        closable: bool,
    }

    impl Render for TestItem {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl PaneItem for TestItem {
        type Config = TestItemConfig;

        fn tab_title(&self, _: &App) -> SharedString {
            self.title.clone()
        }

        fn serialization_key() -> &'static str {
            "test_item"
        }

        fn to_config(&self, _: &App) -> TestItemConfig {
            TestItemConfig {
                title: self.title.to_string(),
            }
        }

        fn can_close(&self, _: &App) -> bool {
            self.closable
        }
    }

    fn item(title: &'static str, cx: &mut App) -> Box<dyn PaneItemHandle> {
        Box::new(cx.new(|_| TestItem {
            title: title.into(),
            closable: true,
        }))
    }

    fn fixed(title: &'static str, cx: &mut App) -> Box<dyn PaneItemHandle> {
        Box::new(cx.new(|_| TestItem {
            title: title.into(),
            closable: false,
        }))
    }

    fn pane(title: &'static str, cx: &mut App) -> Entity<Pane> {
        cx.new(|cx| Pane::new(Some(item(title, cx)), cx))
    }

    fn registry() -> ItemRegistry {
        let mut registry = ItemRegistry::default();
        registry.register::<TestItem>(|state, cx| {
            let config: TestItemConfig = serde_json::from_str(state).ok()?;
            Some(cx.new(|_| TestItem {
                title: config.title.into(),
                closable: true,
            }))
        });
        registry
    }

    fn titles(group: &Entity<TileGroup>, cx: &App) -> Vec<SharedString> {
        let group = group.read(cx);
        group.tabs.iter().map(|tab| tab.title(cx)).collect()
    }

    fn tab_with(kind: &str, state: &str) -> TileTab {
        TileTab {
            root: TileNode::Pane(TilePane {
                item: Some(serial::TileItem {
                    kind: kind.into(),
                    state: state.into(),
                }),
                ..Default::default()
            }),
        }
    }

    fn empty_layout(version: u32) -> String {
        serde_json::to_string(&TileLayout {
            version,
            global_time_range: String::new(),
            temporal: None,
            tabs: vec![TileTab {
                root: TileNode::Pane(TilePane::default()),
            }],
            active_tab: 0,
            top_dock: None,
            bottom_dock: None,
        })
        .expect("serialize")
    }

    #[gpui::test]
    fn each_item_opens_as_its_own_tab(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        cx.read(|cx| {
            assert_eq!(titles(&group, cx), ["a", "b"]);
            assert_eq!(group.read(cx).panes().len(), 1);
            assert!(group.read(cx).has_items(cx));
        });
        let blank = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        cx.read(|cx| {
            assert_eq!(titles(&blank, cx), ["New tab"]);
            assert!(!blank.read(cx).has_items(cx));
        });
    }

    #[gpui::test]
    fn a_tab_is_named_for_its_current_pane(cx: &mut TestAppContext) {
        let group = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("plot", cx)], cx)));
        cx.update(|cx| {
            let first = group.read(cx).panes()[0].clone();
            let outline = pane("outline", cx);
            group.update(cx, |group, cx| {
                group.split_pane(&first, outline, SplitDirection::Right, cx)
            });
            assert_eq!(titles(&group, cx), ["outline"], "the new pane is current");
            group.update(cx, |group, _| group.focus_pane(&first));
            assert_eq!(titles(&group, cx), ["plot"]);
        });
    }

    #[gpui::test]
    fn adding_to_a_full_pane_splits_beside_it(cx: &mut TestAppContext) {
        let group = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        // Pane events reach the group as each update's effects flush.
        let first = cx.read(|cx| group.read(cx).panes()[0].clone());
        cx.update(|cx| first.update(cx, |pane, cx| pane.add_item(item("a", cx), cx)));
        cx.read(|cx| {
            assert_eq!(
                group.read(cx).panes().len(),
                1,
                "an empty pane fills in place"
            )
        });
        cx.update(|cx| first.update(cx, |pane, cx| pane.add_item(item("b", cx), cx)));
        cx.read(|cx| {
            let group = group.read(cx);
            assert_eq!(group.panes().len(), 2, "a full pane splits");
            assert_eq!(
                group
                    .active_pane(cx)
                    .unwrap()
                    .read(cx)
                    .item()
                    .unwrap()
                    .tab_title(cx),
                "b"
            );
        });
    }

    #[gpui::test]
    fn closing_a_tabs_last_pane_closes_the_tab_but_never_the_last(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        let a = cx.read(|cx| group.read(cx).panes()[0].clone());
        cx.update(|cx| a.update(cx, |pane, cx| pane.close(cx)));
        let b = cx.read(|cx| {
            assert_eq!(titles(&group, cx), ["b"]);
            group.read(cx).panes()[0].clone()
        });
        cx.update(|cx| b.update(cx, |pane, cx| pane.close(cx)));
        cx.read(|cx| {
            assert_eq!(titles(&group, cx), ["New tab"]);
            assert_eq!(
                group.read(cx).panes()[0],
                b,
                "the last pane stays to show its starters"
            );
        });
    }

    #[gpui::test]
    fn a_tab_holding_an_item_that_refuses_to_close_stays(cx: &mut TestAppContext) {
        let group = cx
            .update(|cx| cx.new(|cx| TileGroup::new(vec![fixed("pinned", cx), item("b", cx)], cx)));
        cx.update(|cx| {
            let id = group.read(cx).tabs[0].id;
            group.update(cx, |group, cx| group.close_tab(id, cx));
            assert_eq!(titles(&group, cx), ["pinned", "b"]);
            let id = group.read(cx).tabs[1].id;
            group.update(cx, |group, cx| group.close_tab(id, cx));
            assert_eq!(titles(&group, cx), ["pinned"]);
        });
    }

    #[gpui::test]
    fn tabs_cycle_and_reorder_keeping_the_active_one(cx: &mut TestAppContext) {
        let group = cx.update(|cx| {
            cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx), item("c", cx)], cx))
        });
        cx.update(|cx| {
            group.update(cx, |group, cx| {
                group.cycle_tab(false, cx);
                assert_eq!(group.active_tab, 2, "backward from the first wraps");
                let (a, c) = (group.tabs[0].id, group.tabs[2].id);
                group.move_tab(a, None, cx);
                group.move_tab(c, Some(group.tabs[0].id), cx);
                assert_eq!(group.active_tab(), c);
            });
            assert_eq!(titles(&group, cx), ["c", "b", "a"]);
        });
    }

    #[gpui::test]
    fn moving_a_pane_keeps_its_entity_and_drops_an_emptied_tab(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        cx.update(|cx| {
            let a = group.read(cx).tabs[0].panes[0].clone();
            let b = group.read(cx).tabs[1].panes[0].clone();
            group.update(cx, |group, cx| {
                group.move_pane(b.clone(), &a, SplitDirection::Down, cx)
            });
            let group = group.read(cx);
            assert_eq!(group.tabs.len(), 1, "b's emptied tab closed");
            assert_eq!(group.panes(), [a, b.clone()]);
            assert_eq!(group.active_pane(cx), Some(b));
            let Member::Axis(axis) = &group.tab().root else {
                panic!("expected a split");
            };
            assert_eq!(axis.axis, Axis::Vertical);
        });
    }

    #[gpui::test]
    fn dropping_a_pane_on_a_tab_moves_it_there(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        cx.update(|cx| {
            let a = group.read(cx).tabs[0].panes[0].clone();
            let extra = pane("extra", cx);
            group.update(cx, |group, cx| {
                group.split_pane(&a, extra.clone(), SplitDirection::Right, cx);
                let b_tab = group.tabs[1].id;
                group.move_pane_to_tab(extra.clone(), b_tab, cx);
                assert_eq!(group.active_tab(), b_tab, "the tab it landed in shows");
                assert_eq!(group.tabs[0].panes, std::slice::from_ref(&a));
                assert_eq!(group.tabs[1].panes.len(), 2);

                let blank = group.new_tab(cx);
                let blank_tab = group.active_tab();
                group.move_pane_to_tab(extra.clone(), blank_tab, cx);
                let ix = group.tab_index(blank_tab).unwrap();
                assert_eq!(
                    group.tabs[ix].panes,
                    std::slice::from_ref(&extra),
                    "the pane replaces a lone empty pane"
                );
                assert!(!group.subscriptions.contains_key(&blank.entity_id()));

                group.move_pane_to_new_tab(a.clone(), cx);
                assert_eq!(group.tabs.last().unwrap().panes, std::slice::from_ref(&a));
            });
            assert_eq!(titles(&group, cx), ["b", "extra", "a"]);
        });
    }

    #[gpui::test]
    fn panes_and_tabs_move_between_windows_and_report_only_to_their_new_group(
        cx: &mut TestAppContext,
    ) {
        let (left, right) = cx.update(|cx| {
            (
                cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)),
                cx.new(|cx| TileGroup::new(vec![item("c", cx)], cx)),
            )
        });
        let (a, c) = cx.update(|cx| {
            let a = left.read(cx).tabs[0].panes[0].clone();
            let c = right.read(cx).panes()[0].clone();
            right.update(cx, |right, cx| {
                right.move_pane(a.clone(), &c, SplitDirection::Left, cx)
            });
            assert_eq!(titles(&left, cx), ["b"]);
            assert_eq!(right.read(cx).panes(), [c.clone(), a.clone()]);
            assert_eq!(a.read(cx).owner(), Some(right.clone()));

            let b_tab = left.read(cx).tabs[0].id;
            right.update(cx, |right, cx| right.adopt_tab(&left, b_tab, None, cx));
            assert_eq!(titles(&right, cx), ["a", "b"]);
            assert_eq!(right.read(cx).active_tab, 1, "the adopted tab shows");
            assert_eq!(titles(&left, cx), ["New tab"]);
            assert!(!left.read(cx).has_items(cx));
            (a, c)
        });
        // Closing the moved pane is the new group's business only.
        cx.update(|cx| a.update(cx, |pane, cx| pane.close(cx)));
        cx.update(|cx| {
            assert_eq!(right.read(cx).tabs[0].panes, std::slice::from_ref(&c));
            assert_eq!(titles(&left, cx), ["New tab"]);

            let torn = right
                .update(cx, |right, cx| right.split_off_pane(c.clone(), cx))
                .unwrap();
            assert_eq!(torn.read(cx).panes(), std::slice::from_ref(&c));
            assert_eq!(titles(&right, cx), ["b"]);
            let b_tab = right.read(cx).active_tab();
            let torn_tab = right
                .update(cx, |right, cx| right.split_off_tab(b_tab, cx))
                .unwrap();
            assert_eq!(titles(&torn_tab, cx), ["b"]);
            assert_eq!(titles(&right, cx), ["New tab"]);
        });
    }

    #[gpui::test]
    fn focus_or_open_reveals_a_panel_in_another_tab(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), fixed("b", cx)], cx)));
        cx.update(|cx| {
            group.update(cx, |group, cx| {
                group.focus_or_open("test_item", |_| None, cx);
                assert_eq!(group.active_tab, 0, "the first match wins");
                let b = group.tabs[1].id;
                group.activate_tab(b, cx);
                group.focus_or_open("missing", |cx| Some(item("new", cx)), cx);
            });
        });
        cx.read(|cx| {
            assert_eq!(
                group.read(cx).panes().len(),
                2,
                "a missing panel opens beside"
            )
        });
    }

    #[gpui::test]
    fn zoom_clears_when_the_layout_changes_under_it(cx: &mut TestAppContext) {
        let group = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx)], cx)));
        cx.update(|cx| {
            group.update(cx, |group, cx| {
                let left = group.panes()[0].clone();
                group.toggle_zoom(&left, cx);
                assert!(group.zoomed().is_none(), "a lone pane has nothing to hide");
                let right = pane("b", cx);
                group.split_pane(&left, right.clone(), SplitDirection::Right, cx);
                group.toggle_zoom(&right, cx);
                assert_eq!(group.zoomed(), Some(&right));
                group.remove_pane(&right, cx);
                assert!(
                    group.zoomed().is_none(),
                    "removing the zoomed pane un-zooms"
                );

                let extra = pane("c", cx);
                group.split_pane(&left, extra.clone(), SplitDirection::Down, cx);
                group.toggle_zoom(&left, cx);
                group.split_pane(&extra, pane("d", cx), SplitDirection::Right, cx);
                assert!(group.zoomed().is_none(), "a split reveals its new pane");
            });
        });
    }

    #[gpui::test]
    fn the_layout_round_trips_tabs_and_the_active_one(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        let json = cx.update(|cx| {
            group.update(cx, |group, cx| {
                let a = group.tabs[0].panes[0].clone();
                group.split_pane(
                    &a,
                    cx.new(|cx| Pane::new(None, cx)),
                    SplitDirection::Right,
                    cx,
                );
                let b = group.tabs[1].id;
                group.activate_tab(b, cx);
            });
            group.read(cx).to_json(cx)
        });
        let restored =
            cx.update(|cx| cx.new(|cx| TileGroup::from_json(&json, &registry(), cx).unwrap()));
        cx.read(|cx| {
            assert_eq!(titles(&restored, cx), ["a", "b"]);
            let restored = restored.read(cx);
            assert_eq!(restored.active_tab, 1);
            assert_eq!(restored.tabs[0].panes.len(), 2);
            assert!(restored.tabs[0].panes[1].read(cx).is_empty());
        });
    }

    #[gpui::test]
    fn a_pinned_pane_stays_on_screen_across_tabs_until_unpinned(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        let timeline = cx.update(|cx| {
            group.update(cx, |group, cx| {
                let a = group.tabs[0].panes[0].clone();
                let timeline = cx.new(|cx| Pane::new(Some(item("timeline", cx)), cx));
                group.split_pane(&a, timeline.clone(), SplitDirection::Down, cx);
                group.pin(timeline.clone(), Edge::Bottom, cx);
                let b = group.tabs[1].id;
                group.activate_tab(b, cx);
                timeline
            })
        });
        cx.read(|cx| {
            let group = group.read(cx);
            assert_eq!(group.docked(&timeline), Some(Edge::Bottom));
            assert!(timeline.read(cx).is_docked());
            assert_eq!(group.tabs[0].panes.len(), 1);
            assert!(group.all_panes().any(|pane| pane == &timeline));
        });
        cx.update(|cx| group.update(cx, |group, cx| group.pin(timeline.clone(), Edge::Top, cx)));
        cx.read(|cx| {
            let group = group.read(cx);
            assert_eq!(group.docked(&timeline), Some(Edge::Top));
            assert!(group.bottom_dock.is_none());
        });
        cx.update(|cx| group.update(cx, |group, cx| group.unpin(timeline.clone(), cx)));
        cx.read(|cx| {
            let group = group.read(cx);
            assert_eq!(group.docked(&timeline), None);
            assert!(!timeline.read(cx).is_docked());
            assert!(group.top_dock.is_none());
            assert!(group.tabs[1].panes.contains(&timeline));
        });
    }

    #[gpui::test]
    fn a_pinned_tab_closes_and_docks_its_layout_which_round_trips(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        let json = cx.update(|cx| {
            group.update(cx, |group, cx| {
                let b = group.tabs[1].panes[0].clone();
                group.split_pane(
                    &b,
                    cx.new(|cx| Pane::new(Some(item("c", cx)), cx)),
                    SplitDirection::Right,
                    cx,
                );
                let b = group.tabs[1].id;
                group.pin_tab(b, Edge::Top, cx);
            });
            group.read(cx).to_json(cx)
        });
        cx.read(|cx| {
            assert_eq!(titles(&group, cx), ["a"]);
            assert_eq!(group.read(cx).top_dock.as_ref().unwrap().panes.len(), 2);
        });
        let restored =
            cx.update(|cx| cx.new(|cx| TileGroup::from_json(&json, &registry(), cx).unwrap()));
        cx.read(|cx| {
            let restored = restored.read(cx);
            let dock = restored.top_dock.as_ref().unwrap();
            let docked: Vec<_> = dock
                .panes
                .iter()
                .map(|pane| pane.read(cx).item().unwrap().tab_title(cx))
                .collect();
            assert_eq!(docked, ["b", "c"]);
            assert_eq!(dock.height, dock::DEFAULT_HEIGHT);
            assert!(dock.panes.iter().all(|pane| pane.read(cx).is_docked()));
            assert!(restored.bottom_dock.is_none());
        });
    }

    #[gpui::test]
    fn closing_a_docks_last_pane_closes_the_dock(cx: &mut TestAppContext) {
        let group =
            cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item("a", cx), item("b", cx)], cx)));
        let pinned = cx.update(|cx| {
            group.update(cx, |group, cx| {
                let pinned = group.tabs[1].panes[0].clone();
                group.pin(pinned.clone(), Edge::Bottom, cx);
                pinned
            })
        });
        cx.read(|cx| assert_eq!(titles(&group, cx), ["a"]));
        cx.update(|cx| pinned.update(cx, |pane, cx| pane.close(cx)));
        cx.run_until_parked();
        cx.read(|cx| {
            assert!(group.read(cx).bottom_dock.is_none());
            assert_eq!(titles(&group, cx), ["a"]);
        });
    }

    #[gpui::test]
    fn unknown_pane_round_trips_raw_kind_and_state(cx: &mut TestAppContext) {
        let state = r#"{"plugin":"temporarily absent"}"#;
        let json = serde_json::to_string(&TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: String::new(),
            temporal: None,
            tabs: vec![tab_with("downstream.plugin", state)],
            active_tab: 0,
            top_dock: None,
            bottom_dock: None,
        })
        .unwrap();
        let tg = cx.update(|cx| {
            cx.new(|cx| TileGroup::from_json(&json, &ItemRegistry::default(), cx).unwrap())
        });
        cx.read(|cx| {
            let item = tg.read(cx).panes()[0].read(cx).item().unwrap();
            assert_eq!(item.serialization_key(), "downstream.plugin");
            assert_eq!(item.serialize(cx), state);
        });
    }

    #[gpui::test]
    fn from_json_rejects_other_versions(cx: &mut TestAppContext) {
        for version in [SUPPORTED_LAYOUT_VERSION - 1, SUPPORTED_LAYOUT_VERSION + 1] {
            let json = empty_layout(version);
            let rejected = cx.update(|cx| {
                cx.new(|cx| {
                    let rejected = matches!(
                        TileGroup::from_json(&json, &ItemRegistry::default(), cx),
                        Err(LoadError::UnsupportedVersion(v)) if v == version
                    );
                    assert!(rejected, "version {version} must be rejected");
                    TileGroup::new(Vec::new(), cx)
                })
            });
            drop(rejected);
        }
    }

    struct RegisteredTestView {
        value: String,
    }

    impl Render for RegisteredTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.value.clone())
        }
    }

    /// A window drawing a group's tab bar above its layout, as the
    /// titlebar does.
    struct Host {
        group: Entity<TileGroup>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(div().h(px(36.0)).flex().child(tab_bar::tab_list(
                    &self.group,
                    crate::config::TabPlacement::Top,
                    cx,
                )))
                .child(div().flex_1().child(self.group.clone()))
        }
    }

    fn host(
        items: impl FnOnce(&mut App) -> Vec<Box<dyn PaneItemHandle>> + 'static,
        cx: &mut TestAppContext,
    ) -> (Entity<TileGroup>, &mut gpui::VisualTestContext) {
        let (host, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            let items = items(cx);
            Host {
                group: cx.new(|cx| TileGroup::new(items, cx)),
            }
        });
        cx.refresh().unwrap();
        let group = cx.update(|_, cx| host.read(cx).group.clone());
        (group, cx)
    }

    #[gpui::test]
    fn a_headerless_docked_panes_own_menu_carries_its_pane_rows(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("timeline", cx)], cx);
        let (pinned, tabbed) = cx.update(|_, cx| {
            group.update(cx, |group, cx| {
                let pinned = group.tabs[1].panes[0].clone();
                group.pin(pinned.clone(), Edge::Bottom, cx);
                (pinned, group.tabs[0].panes[0].clone())
            })
        });
        cx.refresh().unwrap();
        let labels = |pane: &Entity<Pane>, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                let at = pane.read(cx).bounds().center();
                dock::headerless_rows_at(&group, at, cx)
                    .iter()
                    .map(|row| row.label().to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert!(labels(&pinned, cx).is_empty());
        cx.update(|_, cx| pinned.update(cx, |pane, cx| pane.set_hide_header(true, cx)));
        cx.refresh().unwrap();
        assert!(
            labels(&pinned, cx)
                .iter()
                .any(|label| label == "Hide Header")
        );
        assert!(labels(&tabbed, cx).is_empty());
    }

    fn drag(
        cx: &mut gpui::VisualTestContext,
        start: gpui::Point<Pixels>,
        end: gpui::Point<Pixels>,
    ) {
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            start + point(px(6.0), px(0.0)),
            MouseButton::Left,
            Default::default(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
    }

    fn header_of(pane: &Entity<Pane>, cx: &mut gpui::VisualTestContext) -> gpui::Point<Pixels> {
        let bounds = cx.update(|_, cx| pane.read(cx).bounds());
        bounds.origin + point(px(40.0), px(pane::HEADER_HEIGHT / 2.0))
    }

    #[gpui::test]
    fn dragging_a_header_onto_a_pane_splits_toward_the_nearest_edge(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx)], cx);
        let (a, b) = cx.update(|_, cx| {
            let a = group.read(cx).panes()[0].clone();
            let b = pane("b", cx);
            group.update(cx, |group, cx| {
                group.split_pane(&a, b.clone(), SplitDirection::Right, cx)
            });
            (a, b)
        });
        cx.refresh().unwrap();
        let target = cx.update(|_, cx| a.read(cx).bounds());
        let end = point(target.center().x, target.bottom() - px(10.0));
        let start = header_of(&b, cx);
        drag(cx, start, end);
        cx.update(|_, cx| {
            let preview = group.read(cx).split_preview.target.clone();
            assert_eq!(preview, Some((a.clone(), SplitDirection::Down)));
            assert_eq!(group.read(cx).panes().len(), 2, "preview leaves the tree");
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            let group = group.read(cx);
            assert_eq!(group.panes(), [a.clone(), b.clone()]);
            let Member::Axis(axis) = &group.tab().root else {
                panic!("expected a split");
            };
            assert_eq!(axis.axis, Axis::Vertical, "b now sits below a");
            assert!(group.split_preview.target.is_none());
        });
    }

    #[gpui::test]
    fn dragging_a_header_onto_a_tab_moves_the_pane_into_it(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx)], cx);
        let (a, b_tab) = cx.update(|_, cx| {
            let group = group.read(cx);
            (group.tabs[0].panes[0].clone(), group.tabs[1].id)
        });
        let other = cx.update(|_, cx| {
            let other = pane("extra", cx);
            group.update(cx, |group, cx| {
                group.split_pane(&a, other.clone(), SplitDirection::Right, cx)
            });
            other
        });
        cx.refresh().unwrap();
        let tab = cx.debug_bounds("top-tab-1").unwrap().center();
        let start = header_of(&other, cx);
        drag(cx, start, tab);
        cx.simulate_mouse_up(tab, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            let group = group.read(cx);
            assert_eq!(group.active_tab(), b_tab);
            assert!(group.panes().contains(&other));
            assert_eq!(group.tabs[0].panes, std::slice::from_ref(&a));
        });
    }

    #[gpui::test]
    fn hovering_a_tab_with_a_pane_opens_it(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx)], cx);
        let a = cx.update(|_, cx| group.read(cx).panes()[0].clone());
        cx.update(|_, cx| {
            group.update(cx, |group, cx| {
                group.split_pane(&a, pane("extra", cx), SplitDirection::Right, cx)
            })
        });
        cx.refresh().unwrap();
        let tab = cx.debug_bounds("top-tab-1").unwrap().center();
        let start = header_of(&a, cx);
        drag(cx, start, tab);
        cx.executor().advance_clock(SPRING_LOAD * 2);
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(group.read(cx).active_tab, 1));
    }

    #[gpui::test]
    fn dragging_a_tab_reorders_the_bar(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx), item("c", cx)], cx);
        let first = cx.debug_bounds("top-tab-0").unwrap().center();
        // Once a lifts, b slides into its slot and c follows, so the seam
        // between them shows just inside where b began.
        let b = cx.debug_bounds("top-tab-1").unwrap();
        let seam = point(b.left() + px(4.0), b.center().y);
        drag(cx, first, seam);
        cx.simulate_mouse_up(seam, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            assert_eq!(
                titles(&group, cx),
                ["b", "a", "c"],
                "a lands in the gap between b and c as the bar shows them"
            );
            assert_eq!(group.read(cx).active_tab, 1, "the moved tab shows");
        });
    }

    #[gpui::test]
    fn a_lifted_tab_leaves_a_gap_that_holds_while_the_pointer_is_off_the_bar(
        cx: &mut TestAppContext,
    ) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx), item("c", cx)], cx);
        let a = cx.debug_bounds("top-tab-0").unwrap();
        let b_before = cx.debug_bounds("top-tab-1").unwrap();
        let away = point(a.center().x, a.bottom() + px(200.0));
        drag(cx, a.center(), away);
        cx.update(|_, cx| assert!(group.read(cx).bar.lifted.is_some()));
        assert_eq!(
            cx.debug_bounds("top-tab-1").unwrap().origin,
            b_before.origin,
            "b holds its place beside the gap"
        );

        // Once the pointer has picked a slot on the bar, leaving the bar
        // keeps the gap there.
        let c = cx.debug_bounds("top-tab-2").unwrap();
        let end = point(c.right() - px(4.0), c.center().y);
        cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.simulate_mouse_move(away, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            let preview = group.read(cx).bar.preview.expect("the gap stays");
            assert_eq!(preview.before, None, "at the end, where it was last");
        });

        cx.simulate_mouse_up(away, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| assert_eq!(titles(&group, cx), ["a", "b", "c"]));
    }

    #[gpui::test]
    fn a_dragged_tab_opens_a_gap_and_its_neighbours_slide(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx), item("c", cx)], cx);
        let ids: Vec<TabId> = cx.update(|_, cx| group.read(cx).tabs.iter().map(|t| t.id).collect());
        let b_before = cx.debug_bounds("top-tab-1").unwrap();
        let first = cx.debug_bounds("top-tab-0").unwrap().center();
        let last = cx.debug_bounds("top-tab-2").unwrap();
        let end = point(last.right() - px(4.0), last.center().y);
        drag(cx, first, end);
        cx.update(|_, cx| {
            let group = group.read(cx);
            assert_eq!(group.bar.lifted, Some(ids[0]));
            let preview = group.bar.preview.expect("the bar previews a landing gap");
            assert_eq!(preview.before, None);
            assert_eq!(
                group.tabs[0].id, ids[0],
                "the preview leaves the order alone"
            );
        });
        let b_moving = cx.debug_bounds("top-tab-1").unwrap();
        assert!(
            b_moving.origin.x < b_before.origin.x && b_moving.origin.x > px(0.0),
            "b slides left into the lifted tab's slot rather than jumping"
        );
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            assert_eq!(titles(&group, cx), ["b", "c", "a"]);
            assert!(group.read(cx).bar.preview.is_none());
            assert!(group.read(cx).bar.lifted.is_none());
        });
    }

    #[gpui::test]
    fn a_crowded_bar_scrolls_the_active_tab_into_view(cx: &mut TestAppContext) {
        let (group, cx) = host(
            |cx| (0..30).map(|_| item("a long tab title", cx)).collect(),
            cx,
        );
        let last = cx.update(|_, cx| group.read(cx).tabs.last().unwrap().id);
        cx.update(|_, cx| group.update(cx, |group, cx| group.activate_tab(last, cx)));
        cx.refresh().unwrap();
        cx.refresh().unwrap();
        let viewport = cx.update(|window, _| window.viewport_size());
        let shown = cx.debug_bounds("top-tab-29").unwrap();
        assert!(
            shown.right() <= viewport.width,
            "the active tab is on screen"
        );
        let scroll = cx.update(|_, cx| group.read(cx).bar.scroll.offset());
        assert!(scroll.x < px(0.0), "the bar scrolled sideways");
    }

    #[gpui::test]
    fn a_tab_dropped_on_a_pane_folds_its_layout_in_beside_it(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx), item("c", cx)], cx);
        let (a, b, c_tab) = cx.update(|_, cx| {
            let b = group.read(cx).tabs[1].panes[0].clone();
            let extra = pane("b2", cx);
            group.update(cx, |group, cx| {
                group.split_pane(&b, extra, SplitDirection::Down, cx)
            });
            let group = group.read(cx);
            (
                group.tabs[0].panes[0].clone(),
                group.tabs[1].id,
                group.tabs[2].id,
            )
        });
        cx.refresh().unwrap();
        let target = cx.update(|_, cx| a.read(cx).bounds());
        let end = point(target.right() - px(10.0), target.center().y);
        let tab = cx.debug_bounds("top-tab-1").unwrap().center();
        drag(cx, tab, end);
        cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        cx.update(|_, cx| {
            let preview = group.read(cx).split_preview.target.clone();
            assert_eq!(preview, Some((a.clone(), SplitDirection::Right)));
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            let group = group.read(cx);
            assert_eq!(group.tab_count(), 2, "the dropped tab closed");
            assert!(group.tab_index(b).is_none());
            assert!(group.tab_index(c_tab).is_some());
            assert_eq!(group.panes().len(), 3, "both of its panes came along");
            let Member::Axis(axis) = &group.tab().root else {
                panic!("expected a split");
            };
            assert_eq!(axis.axis, Axis::Horizontal);
            assert!(
                matches!(axis.members[1], Member::Axis(_)),
                "its own split stays whole"
            );
        });
    }

    #[gpui::test]
    fn a_tab_cannot_fold_into_itself(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx)], cx);
        let a = cx.update(|_, cx| group.read(cx).panes()[0].clone());
        let target = cx.update(|_, cx| a.read(cx).bounds());
        let tab = cx.debug_bounds("top-tab-0").unwrap().center();
        let end = point(target.right() - px(10.0), target.center().y);
        drag(cx, tab, end);
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        cx.update(|_, cx| assert_eq!(group.read(cx).tab_count(), 2));
    }

    #[gpui::test]
    fn the_new_tab_button_slides_with_the_tabs(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx), item("b", cx)], cx);
        let before = cx.debug_bounds("new-top-tab").unwrap();
        let first = cx.update(|_, cx| group.read(cx).tabs[0].id);
        cx.update(|_, cx| group.update(cx, |group, cx| group.close_tab(first, cx)));
        cx.refresh().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        // A test window draws as an update's effects flush.
        cx.update(|window, _| window.refresh());
        let moving = cx.debug_bounds("new-top-tab").unwrap();
        assert!(
            moving.origin.x < before.origin.x && moving.origin.x > px(100.0),
            "the button glides left rather than jumping"
        );
    }

    #[gpui::test]
    fn the_new_tab_button_and_middle_click_open_and_close_tabs(cx: &mut TestAppContext) {
        let (group, cx) = host(|cx| vec![item("a", cx)], cx);
        let plus = cx.debug_bounds("new-top-tab").unwrap().center();
        cx.simulate_click(plus, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| assert_eq!(titles(&group, cx), ["a", "New tab"]));
        let tab = cx.debug_bounds("top-tab-0").unwrap().center();
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Middle,
            position: tab,
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: MouseButton::Middle,
            position: tab,
            modifiers: Default::default(),
            click_count: 1,
        });
        cx.update(|_, cx| assert_eq!(titles(&group, cx), ["New tab"]));
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
                        frame: true,
                    }],
                    zoom: 1.0,
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
            temporal: None,
            tabs: vec![tab_with("registered_test", &config)],
            active_tab: 0,
            top_dock: None,
            bottom_dock: None,
        };
        let json = serde_json::to_string(&layout).unwrap();
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        cx.update(|cx| {
            tg.update(cx, |this, cx| {
                *this = TileGroup::from_json(&json, &panes, cx).unwrap();
            })
        });
        cx.read(|cx| {
            let item = tg.read(cx).panes()[0].read(cx).item().unwrap();
            assert_eq!(item.serialization_key(), "registered_test");
            assert_eq!(item.serialize(cx), config);
            assert!(item.entity_any(cx).downcast::<RegisteredTestView>().is_ok());
        });
    }
}
