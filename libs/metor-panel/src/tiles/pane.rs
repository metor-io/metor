use gpui::{
    Bounds, ClickEvent, Context, DragMoveEvent, EventEmitter, IntoElement, MouseButton, Pixels,
    Point, Render, ScrollHandle, SharedString, Window, div, prelude::*, px,
};
use metor_proto::types::ComponentId;
use smallvec::SmallVec;
use std::rc::Rc;

use super::drag::{DraggedTab, RailResizeDrag, SplitDirection, detect_split_zone};
use super::item::PaneItemHandle;
use super::motion::{MovingTab, Rail, TabSizes};
use super::tab::{
    HEIGHT as TAB_HEIGHT, MAX_RAIL_WIDTH, MAX_WIDTH as TAB_MAX_WIDTH, MIN_RAIL_WIDTH,
    RAIL_HANDLE_WIDTH, RAIL_WIDTH as DEFAULT_RAIL_WIDTH,
};
use crate::theme::theme;

/// Dispatched by views inside a pane to spawn a new plot tab in that pane.
///
/// The payload is deliberately minimal — `ComponentId` and element indices —
/// so the action can implement `PartialEq`. Color, label, and style derive
/// from theme and metadata via [`crate::views::time_series::traces_for_component`].
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct PlotComponentAction {
    pub component_id: ComponentId,
    pub indices: SmallVec<[usize; 4]>,
}

/// Dispatched by the outline to open another outline tab in the same pane,
/// rooted on `root` (a full component path).
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct OpenOutlineAction {
    pub root: SharedString,
}

/// Dispatched while shift is held over a component name to open a transient
/// plot preview anchored at the cursor. An empty `indices` means all
/// elements; surfaces with per-element granularity (value strips) pass a
/// single index. AppRoot owns the preview lifecycle and dismisses on
/// shift release.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct PreviewPlotAction {
    pub component_id: ComponentId,
    pub indices: SmallVec<[usize; 4]>,
    pub anchor: Point<Pixels>,
}

pub use super::serial::TabOrientation;

/// Messages a pane sends to the [`TileGroup`](super::TileGroup) that owns it.
///
/// Splits and removals happen at the tile-group level so the whole tree can
/// be mutated coherently; the pane only signals what the user wanted.
pub enum PaneEvent {
    /// Drop landed on an edge zone: create a sibling pane holding `item`.
    Split {
        direction: SplitDirection,
        item: Box<dyn PaneItemHandle>,
    },
    /// Last tab was closed; tile group may drop the pane entirely.
    Empty,
    /// User requested an inspector anchored at `position` (typically from a
    /// tab's right-click).
    Inspect {
        item: Box<dyn PaneItemHandle>,
        position: Point<Pixels>,
    },
    InspectPane {
        position: Point<Pixels>,
    },
    /// A tab was double-clicked: fill the tile group with this pane, or put
    /// the layout back if it already does.
    ToggleZoom,
}

impl EventEmitter<PaneEvent> for Pane {}

/// A tabbed container for [`PaneItem`](super::PaneItem)s.
///
/// Handles local concerns only: tab switching, close, reordering, and drag
/// hit-testing against split zones. Cross-pane moves and splits are raised as
/// [`PaneEvent`]s for the tile group to resolve.
pub struct Pane {
    items: Vec<Box<dyn PaneItemHandle>>,
    active_index: usize,
    content_bounds: Bounds<Pixels>,
    tab_scroll: ScrollHandle,
    tab_sizes: TabSizes,
    tab_drag_preview: Option<TabDragPreview>,
    lifted_tab: Option<gpui::EntityId>,
    tab_orientation: TabOrientation,
    hide_tab_bar: bool,
    locked_size: Option<gpui::Size<f32>>,
    rail_width: Pixels,
    /// Where the vertical rail sat at its last paint. Its left edge doesn't
    /// move while the rail is resized, so a drag reads widths straight off
    /// the pointer instead of accumulating deltas against a stale frame.
    rail_bounds: Bounds<Pixels>,
}

#[derive(Clone, Copy, PartialEq)]
struct TabDragPreview {
    item: gpui::EntityId,
    before: Option<gpui::EntityId>,
    extent: Pixels,
}

/// Use committed slot centres, not animated hitboxes, so a stationary pointer
/// cannot repeatedly swap the target as neighbours slide past it.
fn insertion_before(
    slots: impl IntoIterator<Item = (gpui::EntityId, Pixels)>,
    dragged: gpui::EntityId,
    pointer: Pixels,
) -> Option<gpui::EntityId> {
    let mut start = px(0.0);
    for (id, extent) in slots {
        if id != dragged && pointer < start + extent / 2.0 {
            return Some(id);
        }
        start += extent;
    }
    None
}

/// New active-tab index after removing the tab at `removed_ix`, given the
/// `remaining` count (must be non-zero). Keeps the selection on the same tab
/// where possible, shifting left when the removed tab sat before it.
fn active_after_remove(active: usize, removed_ix: usize, remaining: usize) -> usize {
    if active >= remaining {
        remaining - 1
    } else if active > removed_ix {
        active - 1
    } else {
        active
    }
}

/// Insertion index when reordering a tab within one pane, after the tab has
/// been pulled out of slot `from`. Dragging rightward (`to > from`) shifts the
/// target left by one to account for the now-vacant slot.
fn reorder_insert_index(from: usize, to: usize) -> usize {
    if to > from { to - 1 } else { to }
}

impl Pane {
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, _cx: &mut Context<Self>) -> Self {
        Self {
            items,
            active_index: 0,
            content_bounds: Bounds::default(),
            tab_scroll: ScrollHandle::new(),
            tab_sizes: TabSizes::default(),
            tab_drag_preview: None,
            lifted_tab: None,
            tab_orientation: TabOrientation::default(),
            hide_tab_bar: false,
            locked_size: None,
            rail_width: px(DEFAULT_RAIL_WIDTH),
            rail_bounds: Bounds::default(),
        }
    }

    pub fn tab_orientation(&self) -> TabOrientation {
        self.tab_orientation
    }

    pub fn set_tab_orientation(&mut self, orientation: TabOrientation, cx: &mut Context<Self>) {
        self.tab_orientation = orientation;
        cx.notify();
    }

    pub fn hide_tab_bar(&self) -> bool {
        self.hide_tab_bar
    }

    pub fn set_hide_tab_bar(&mut self, hide: bool, cx: &mut Context<Self>) {
        self.hide_tab_bar = hide;
        cx.notify();
    }

    pub fn locked_size(&self) -> Option<gpui::Size<f32>> {
        self.locked_size
    }

    pub fn rail_width(&self) -> Pixels {
        self.rail_width
    }

    /// The rail width worth persisting: `None` while it is still the default.
    pub(super) fn custom_rail_width(&self) -> Option<f32> {
        (self.rail_width != px(DEFAULT_RAIL_WIDTH)).then(|| self.rail_width.into())
    }

    /// Clamped so a rail can neither swallow its titles nor its pane.
    pub fn set_rail_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = width.clamp(px(MIN_RAIL_WIDTH), px(MAX_RAIL_WIDTH));
        if self.rail_width != width {
            self.rail_width = width;
            cx.notify();
        }
    }

    /// Outer pane size = content bounds plus the tab strip (when shown).
    /// Used by the inspector toggle to capture "lock at the current size".
    pub fn current_outer_size(&self) -> gpui::Size<f32> {
        let mut size = gpui::Size {
            width: self.content_bounds.size.width.into(),
            height: self.content_bounds.size.height.into(),
        };
        if !self.hide_tab_bar {
            match self.tab_orientation {
                TabOrientation::Horizontal => size.height += TAB_HEIGHT,
                TabOrientation::Vertical => size.width += f32::from(self.rail_width),
            }
        }
        size
    }

    pub fn set_locked_size(&mut self, size: Option<gpui::Size<f32>>, cx: &mut Context<Self>) {
        self.locked_size = size;
        cx.notify();
    }

    pub(super) fn content_bounds(&self) -> Bounds<Pixels> {
        self.content_bounds
    }

    pub fn items(&self) -> &[Box<dyn PaneItemHandle>] {
        &self.items
    }

    pub(crate) fn index_of(&self, id: gpui::EntityId) -> Option<usize> {
        self.items.iter().position(|item| item.entity_id() == id)
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn add_item(&mut self, item: Box<dyn PaneItemHandle>, cx: &mut Context<Self>) {
        self.items.push(item);
        self.active_index = self.items.len() - 1;
        cx.notify();
    }

    pub fn remove_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.items.len() {
            return;
        }
        let removed = self.items.remove(ix);
        self.tab_sizes.borrow_mut().remove(&removed.entity_id());
        if self.items.is_empty() {
            cx.emit(PaneEvent::Empty);
        } else {
            self.active_index = active_after_remove(self.active_index, ix, self.items.len());
        }
        cx.notify();
    }

    pub fn activate_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.active_index = ix;
            cx.notify();
        }
    }

    /// Shared by the close button and a middle click, so both honour
    /// [`PaneItemHandle::can_close`] the same way.
    fn close_tab(&mut self, id: gpui::EntityId, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id)
            && self.items[index].can_close(cx)
        {
            self.remove_item(index, cx);
        }
    }

    pub fn cycle_forward(&mut self, cx: &mut Context<Self>) {
        if self.items.len() > 1 {
            self.active_index = (self.active_index + 1) % self.items.len();
            cx.notify();
        }
    }

    pub fn cycle_backward(&mut self, cx: &mut Context<Self>) {
        if self.items.len() > 1 {
            self.active_index = (self.active_index + self.items.len() - 1) % self.items.len();
            cx.notify();
        }
    }

    /// Tab-bar drops always insert as a new tab, regardless of cursor position.
    fn handle_tab_bar_drop(
        &mut self,
        dragged: &DraggedTab,
        target_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_ix = self
            .tab_drag_preview
            .take()
            .filter(|preview| preview.item == dragged.item.entity_id())
            .map(|preview| {
                preview
                    .before
                    .and_then(|id| self.index_of(id))
                    .unwrap_or(self.items.len())
            })
            .unwrap_or(target_ix);
        self.drop_tab(dragged, target_ix, cx);
    }

    fn handle_tab_bar_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedTab>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = event.bounds.contains(&event.event.position).then(|| {
            let dragged = event.drag(cx);
            let item = dragged.item.entity_id();
            let axis = |size: gpui::Size<Pixels>| match self.tab_orientation {
                TabOrientation::Horizontal => size.width,
                TabOrientation::Vertical => size.height,
            };
            let sizes = self.tab_sizes.borrow();
            // A tab measured in a vertical rail is as wide as that rail; as a
            // horizontal tab it would size to its title, up to the cap.
            let extent = if self.tab_orientation == TabOrientation::Vertical {
                px(TAB_HEIGHT)
            } else if dragged.pane.entity_id() == cx.entity_id() {
                sizes
                    .get(&item)
                    .copied()
                    .map(axis)
                    .unwrap_or(self.rail_width)
            } else {
                let source = dragged.pane.read(cx);
                source
                    .tab_sizes
                    .borrow()
                    .get(&item)
                    .map(|size| size.width)
                    .unwrap_or(source.rail_width)
            }
            .min(px(TAB_MAX_WIDTH));
            let relative = event.event.position - event.bounds.origin - self.tab_scroll.offset();
            let pointer = match self.tab_orientation {
                TabOrientation::Horizontal => relative.x,
                TabOrientation::Vertical => relative.y,
            };
            let before = insertion_before(
                self.items.iter().map(|item| {
                    let id = item.entity_id();
                    (id, sizes.get(&id).copied().map(axis).unwrap_or(extent))
                }),
                item,
                pointer,
            );
            TabDragPreview {
                item,
                before,
                extent,
            }
        });
        if self.tab_drag_preview != next {
            self.tab_drag_preview = next;
            cx.notify();
        }
    }

    /// Content-area drops split the pane when near an edge, otherwise insert
    /// as a tab.
    fn handle_content_drop(
        &mut self,
        dragged: &DraggedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_in_content(
            dragged,
            detect_split_zone(window.mouse_position(), self.content_bounds),
            cx,
        );
    }

    pub(super) fn drop_in_content(
        &mut self,
        dragged: &DraggedTab,
        direction: Option<SplitDirection>,
        cx: &mut Context<Self>,
    ) {
        match direction {
            Some(direction) => self.split_or_move(dragged, direction, cx),
            None => self.drop_tab(dragged, self.items.len(), cx),
        }
    }

    /// Peel `dragged` into a new sibling pane along `direction`.
    ///
    /// A same-pane edge drop of the *only* tab is a no-op: splitting it into a
    /// sibling and removing it here would empty this pane and collapse straight
    /// back to a single pane. Bailing early also dodges the ordering hazard
    /// where [`Pane::remove_item`] emits [`PaneEvent::Empty`] — dropping this
    /// pane from the tree — before the [`PaneEvent::Split`] targeting it runs.
    pub(super) fn split_or_move(
        &mut self,
        dragged: &DraggedTab,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        let same_pane = cx.entity().entity_id() == dragged.pane.entity_id();
        let source_index = if same_pane {
            self.index_of(dragged.item.entity_id())
        } else {
            dragged.pane.read(cx).index_of(dragged.item.entity_id())
        };
        let Some(source_index) = source_index else {
            return;
        };
        if same_pane {
            if self.items.len() == 1 {
                return;
            }
            // Re-entrant update on the same entity is forbidden; remove the tab
            // locally before asking the tile group to split.
            let item = dragged.item.clone_handle();
            self.remove_item(source_index, cx);
            cx.emit(PaneEvent::Split { direction, item });
        } else {
            cx.emit(PaneEvent::Split {
                direction,
                item: dragged.item.clone_handle(),
            });
            dragged.pane.update(cx, |source, cx| {
                source.remove_item(source_index, cx);
            });
        }
    }

    fn drop_tab(&mut self, dragged: &DraggedTab, target_ix: usize, cx: &mut Context<Self>) {
        let same_pane = cx.entity().entity_id() == dragged.pane.entity_id();
        let source_index = if same_pane {
            self.index_of(dragged.item.entity_id())
        } else {
            dragged.pane.read(cx).index_of(dragged.item.entity_id())
        };
        let Some(source_index) = source_index else {
            return;
        };
        if same_pane {
            let from = source_index;
            let to = target_ix.min(self.items.len());
            if from != to && from < self.items.len() {
                let item = self.items.remove(from);
                let insert_at = reorder_insert_index(from, to);
                self.items.insert(insert_at.min(self.items.len()), item);
                self.active_index = insert_at.min(self.items.len().saturating_sub(1));
                cx.notify();
            }
        } else {
            let item = dragged.item.clone_handle();
            let insert_at = target_ix.min(self.items.len());
            self.items.insert(insert_at, item);
            self.active_index = insert_at;
            cx.notify();
            dragged.pane.update(cx, |source, cx| {
                source.remove_item(source_index, cx);
            });
        }
    }

    fn render_tab(
        &mut self,
        ix: usize,
        orientation: TabOrientation,
        rail: Rc<Rail>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let item = &self.items[ix];
        let item_id = item.entity_id();
        let title = item.tab_title(cx);
        let is_active = ix == self.active_index;
        let can_close = item.can_close(cx);
        let text_style = window.text_style();
        let ghost_title = title.clone();

        let pane_entity = cx.entity().clone();
        let item_handle = item.clone_handle();
        let inspect_handle = item.clone_handle();

        let border_primary = theme.border_primary;
        let bg_primary = theme.bg_primary;
        let text_primary = theme.text_primary;

        let mut tab = super::tab::header(&theme, orientation)
            .id(("tab", item_id))
            .cursor_pointer();

        if is_active {
            tab = tab.bg(bg_primary).text_color(text_primary);
        } else {
            tab = tab
                .bg(theme.bg_secondary)
                .text_color(theme.text_secondary)
                .hover(move |s| s.bg(bg_primary).text_color(text_primary));
        }

        tab = tab.on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            if let Some(index) = this.index_of(item_id) {
                this.activate_item(index, cx);
                // The first click of the pair already activated the tab, so a
                // double click only adds the zoom.
                if event.click_count() == 2 {
                    cx.emit(PaneEvent::ToggleZoom);
                }
            }
        }));

        tab = tab.on_aux_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            if event.is_middle_click() {
                this.close_tab(item_id, cx);
            }
        }));

        tab = tab.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                let Some(index) = this.index_of(item_id) else {
                    return;
                };
                this.activate_item(index, cx);
                cx.emit(PaneEvent::Inspect {
                    item: inspect_handle.clone_handle(),
                    position: event.position,
                });
            }),
        );

        tab = tab.on_drag(
            DraggedTab {
                pane: pane_entity,
                item: item_handle,
            },
            move |dragged, _, window, cx| {
                // Mirror the drag app-side so a mouse-up outside the window
                // (which gpui otherwise swallows) can tear the tab out.
                super::drag::set_active_tab_drag(
                    super::drag::ActiveTabDrag {
                        pane: dragged.pane.clone(),
                        item: dragged.item.clone_handle(),
                        source_window: window.window_handle(),
                    },
                    cx,
                );
                let size = dragged.pane.update(cx, |pane, cx| {
                    pane.lifted_tab = Some(dragged.item.entity_id());
                    cx.notify();
                    pane.tab_sizes
                        .borrow()
                        .get(&dragged.item.entity_id())
                        .copied()
                        .unwrap_or(gpui::size(pane.rail_width, px(TAB_HEIGHT)))
                });
                cx.new(|_| super::drag::TabDragGhost {
                    title: ghost_title.clone(),
                    size,
                    text_style: text_style.clone(),
                    can_close,
                    orientation,
                })
            },
        );

        tab = tab.child(super::tab::title(title));

        if can_close {
            tab = tab.child(
                super::tab::close_icon(&theme)
                    .id(("tab-close", item_id))
                    .hover(move |s| s.bg(border_primary).text_color(text_primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(item_id, cx);
                    })),
            );
        }

        MovingTab::new(
            item_id,
            tab,
            rail,
            !cx.has_active_drag() || self.tab_drag_preview.is_some() || self.lifted_tab.is_some(),
            orientation == TabOrientation::Vertical,
        )
    }
}

impl Pane {
    /// Frames the vertical tab bar at this pane's rail width, with a grab
    /// area on its right edge that resizes it the way split handles resize
    /// panes.
    fn render_rail(&self, tab_bar: impl IntoElement, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let pane_id = cx.entity_id();
        let view = cx.entity().clone();
        let bounds_tracker = gpui::canvas(
            move |bounds, _, cx| view.update(cx, |pane, _| pane.rail_bounds = bounds),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let hover_color = theme.text_tertiary;
        let handle = div()
            .id("tab-rail-resize")
            .debug_selector(|| "tab-rail-resize".into())
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(RAIL_HANDLE_WIDTH))
            .cursor(gpui::CursorStyle::ResizeColumn)
            .group("tab-rail-resize")
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(1.0))
                    .group_hover("tab-rail-resize", move |s| s.bg(hover_color)),
            )
            .on_drag(RailResizeDrag { pane: pane_id }, |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<RailResizeDrag>, _, cx| {
                    if event.drag(cx).pane != pane_id {
                        return;
                    }
                    let width = event.event.position.x - this.rail_bounds.origin.x;
                    this.set_rail_width(width, cx);
                },
            ));
        div()
            .relative()
            .h_full()
            .w(self.rail_width)
            .flex_shrink_0()
            .child(bounds_tracker)
            .child(tab_bar)
            .child(handle)
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let orientation = self.tab_orientation;
        let tab_count = self.items.len();
        if !cx.has_active_drag() {
            self.tab_drag_preview = None;
            self.lifted_tab = None;
        }

        let tab_bar = (!self.hide_tab_bar).then(|| {
            let rail = Rc::new(Rail::new(self.tab_sizes.clone()));
            let tracker = rail.clone();
            let mut tab_bar = div()
                .id("pane-tab-bar")
                .relative()
                .child(
                    gpui::canvas(
                        move |bounds, _, _| tracker.bounds.set(bounds),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .flex()
                .bg(theme.bg_secondary)
                .border_color(theme.border_primary)
                .track_scroll(&self.tab_scroll)
                .on_drag_move(cx.listener(Self::handle_tab_bar_drag_move))
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.handle_tab_bar_drop(dragged, this.items.len(), window, cx);
                }));
            tab_bar = match orientation {
                TabOrientation::Horizontal => tab_bar
                    .flex_row()
                    .w_full()
                    .h(px(TAB_HEIGHT))
                    .border_b_1()
                    .overflow_x_scroll(),
                TabOrientation::Vertical => tab_bar
                    .flex_col()
                    .size_full()
                    .border_r_1()
                    .overflow_y_scroll(),
            };
            tab_bar.style().restrict_scroll_to_axis = Some(true);

            let preview = self.tab_drag_preview;
            let mut order: Vec<_> = (0..tab_count)
                .filter(|ix| {
                    let id = self.items[*ix].entity_id();
                    Some(id) != self.lifted_tab && preview.is_none_or(|preview| id != preview.item)
                })
                .map(Some)
                .collect();
            if let Some(preview) = preview {
                let slot = order
                    .iter()
                    .position(|ix| Some(self.items[ix.unwrap()].entity_id()) == preview.before)
                    .unwrap_or(order.len());
                let original_slot = self.index_of(preview.item);
                // Lifting closes the source gap immediately. Only open an
                // insertion gap when hovering a different slot or another pane.
                if original_slot != Some(slot) {
                    order.insert(slot, None);
                }
            }
            for ix in order {
                if let Some(ix) = ix {
                    tab_bar =
                        tab_bar.child(self.render_tab(ix, orientation, rail.clone(), window, cx));
                } else if let Some(preview) = preview {
                    let gap = div().flex_shrink_0();
                    tab_bar = tab_bar.child(match orientation {
                        TabOrientation::Horizontal => gap.w(preview.extent).h_full(),
                        TabOrientation::Vertical => gap.h(preview.extent).w_full(),
                    });
                }
            }

            let mut drop_zone = div()
                .id("tab-bar-drop-zone")
                .debug_selector(|| "tab-bar-drop-zone".into())
                .flex_1()
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_this, event: &gpui::MouseDownEvent, _window, cx| {
                        cx.emit(PaneEvent::InspectPane {
                            position: event.position,
                        });
                    }),
                );
            drop_zone = match orientation {
                TabOrientation::Horizontal => drop_zone.h_full(),
                TabOrientation::Vertical => drop_zone.w_full(),
            };
            let tab_bar = tab_bar.child(drop_zone);
            match orientation {
                TabOrientation::Horizontal => tab_bar.into_any_element(),
                TabOrientation::Vertical => self.render_rail(tab_bar, cx).into_any_element(),
            }
        });

        let view = cx.entity().clone();
        let content_bounds_tracker = gpui::canvas(
            move |bounds, _window, cx| {
                view.update(cx, |pane, _| {
                    pane.content_bounds = bounds;
                });
                bounds
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let mut content = div()
            .id("pane-content")
            .relative()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_primary)
            .child(content_bounds_tracker);

        if let Some(item) = self.items.get(self.active_index) {
            content = content.child(item.view());
        }

        // Drop on content area (split-on-drop at edges, add-as-tab in center)
        content = content.on_drop(cx.listener(|this, dragged: &DraggedTab, window, cx| {
            this.handle_content_drop(dragged, window, cx);
        }));

        let mut outer = div().flex().size_full();
        outer = match orientation {
            TabOrientation::Horizontal => outer.flex_col(),
            TabOrientation::Vertical => outer.flex_row(),
        };
        if let Some(tab_bar) = tab_bar {
            outer = outer.child(tab_bar);
        }
        outer.child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{PaneItem, TileGroup};
    use gpui::{App, SharedString};

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct TestItemConfig {}

    struct TestItem;

    impl Render for TestItem {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl PaneItem for TestItem {
        type Config = TestItemConfig;

        fn tab_title(&self, _: &App) -> SharedString {
            SharedString::new_static("test")
        }

        fn serialization_key() -> &'static str {
            "test_item"
        }

        fn to_config(&self, _: &App) -> TestItemConfig {
            TestItemConfig {}
        }
    }

    fn item(cx: &mut App) -> Box<dyn PaneItemHandle> {
        Box::new(cx.new(|_| TestItem))
    }

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct OtherItemConfig {}

    struct OtherItem;

    impl Render for OtherItem {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl PaneItem for OtherItem {
        type Config = OtherItemConfig;

        fn tab_title(&self, _: &App) -> SharedString {
            SharedString::new_static("other")
        }

        fn serialization_key() -> &'static str {
            "other_item"
        }

        fn to_config(&self, _: &App) -> OtherItemConfig {
            OtherItemConfig {}
        }

        fn can_close(&self, _: &App) -> bool {
            false
        }
    }

    fn other_item(cx: &mut App) -> Box<dyn PaneItemHandle> {
        Box::new(cx.new(|_| OtherItem))
    }

    #[test]
    fn reorder_insert_index_shifts_left_when_moving_right() {
        assert_eq!(reorder_insert_index(0, 3), 2);
        assert_eq!(reorder_insert_index(1, 2), 1);
        // Moving leftward keeps the target as-is.
        assert_eq!(reorder_insert_index(3, 1), 1);
        assert_eq!(reorder_insert_index(2, 0), 0);
    }

    #[test]
    fn active_after_remove_tracks_selection() {
        // Removing a tab before the active one shifts the selection left.
        assert_eq!(active_after_remove(2, 0, 3), 1);
        // Removing a tab after the active one leaves it put.
        assert_eq!(active_after_remove(1, 2, 3), 1);
        // Removing the active tab when it was last clamps to the new end.
        assert_eq!(active_after_remove(3, 3, 3), 2);
        // Removing the active tab mid-list keeps the index (now the next tab).
        assert_eq!(active_after_remove(1, 1, 3), 1);
    }

    #[gpui::test]
    fn dragging_previews_order_until_release_in_both_orientations(cx: &mut gpui::TestAppContext) {
        for orientation in [TabOrientation::Horizontal, TabOrientation::Vertical] {
            let (pane, cx) = cx.add_window_view(|_, cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                let mut pane = Pane::new(vec![item(cx), item(cx), item(cx)], cx);
                pane.tab_orientation = orientation;
                pane
            });
            cx.refresh().unwrap();
            let ids = cx.update(|_, cx| {
                pane.read(cx)
                    .items
                    .iter()
                    .map(|item| item.entity_id())
                    .collect::<Vec<_>>()
            });
            let start = gpui::point(px(8.0), px(12.0));
            let source_size = cx.update(|_, cx| pane.read(cx).tab_sizes.borrow()[&ids[0]]);
            let before_lift = cx.debug_bounds("tab-bar-drop-zone").unwrap();
            let end = match orientation {
                TabOrientation::Horizontal => gpui::point(px(350.0), px(12.0)),
                TabOrientation::Vertical => gpui::point(px(8.0), px(130.0)),
            };
            cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_move(
                start + gpui::point(px(4.0), px(0.0)),
                MouseButton::Left,
                gpui::Modifiers::default(),
            );
            cx.refresh().unwrap();
            let after_lift = cx.debug_bounds("tab-bar-drop-zone").unwrap();
            assert_eq!(
                cx.debug_bounds("tab-drag-ghost").unwrap().size,
                source_size,
                "floating tab retains its measured size"
            );
            match orientation {
                TabOrientation::Horizontal => assert_eq!(
                    after_lift.origin.x,
                    before_lift.origin.x - source_size.width
                ),
                TabOrientation::Vertical => assert_eq!(
                    after_lift.origin.y,
                    before_lift.origin.y - source_size.height
                ),
            }
            cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                let pane = pane.read(cx);
                let preview = pane
                    .tab_drag_preview
                    .expect("drag previews an insertion gap");
                assert_eq!(preview.item, ids[0]);
                assert_eq!(preview.before, None);
                assert_eq!(
                    pane.items[0].entity_id(),
                    ids[0],
                    "preview does not mutate saved order"
                );
            });
            // Repeated moves at the same position must not oscillate as tabs animate.
            cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                let pane = pane.read(cx);
                assert!(pane.tab_drag_preview.is_none());
                assert!(pane.lifted_tab.is_none());
                assert_eq!(
                    pane.items
                        .iter()
                        .map(|item| item.entity_id())
                        .collect::<Vec<_>>(),
                    vec![ids[1], ids[2], ids[0]]
                );
            });
        }
    }

    #[gpui::test]
    fn dragging_between_panes_previews_a_gap_and_transfers_once(cx: &mut gpui::TestAppContext) {
        struct Host {
            source: gpui::Entity<Pane>,
            destination: gpui::Entity<Pane>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .flex()
                    .size_full()
                    .child(
                        div()
                            .w(px(400.0))
                            .h_full()
                            .flex_shrink_0()
                            .child(self.source.clone()),
                    )
                    .child(
                        div()
                            .w(px(400.0))
                            .h_full()
                            .flex_shrink_0()
                            .child(self.destination.clone()),
                    )
            }
        }
        let (host, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            Host {
                source: cx.new(|cx| Pane::new(vec![item(cx), item(cx)], cx)),
                destination: cx.new(|cx| Pane::new(vec![item(cx), item(cx)], cx)),
            }
        });
        cx.refresh().unwrap();
        let (source, destination, dragged_id, width) = cx.update(|_, cx| {
            let host = host.read(cx);
            let source = host.source.read(cx);
            let id = source.items[0].entity_id();
            (
                host.source.clone(),
                host.destination.clone(),
                id,
                source.tab_sizes.borrow()[&id].width,
            )
        });
        let initial_tail = cx.debug_bounds("tab-bar-drop-zone").unwrap();
        let start = gpui::point(px(8.0), px(12.0));
        let end = gpui::point(px(410.0), px(12.0));
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            start + gpui::point(px(4.0), px(0.0)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::default());
        cx.refresh().unwrap();
        assert_eq!(
            cx.debug_bounds("tab-bar-drop-zone").unwrap().origin.x,
            initial_tail.origin.x + width
        );
        cx.update(|_, cx| {
            let target = destination.read(cx);
            assert_eq!(
                target.tab_drag_preview.unwrap().before,
                Some(target.items[0].entity_id())
            );
            assert_eq!(target.items.len(), 2);
            assert_eq!(source.read(cx).items.len(), 2);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            assert_eq!(source.read(cx).items.len(), 1);
            let target = destination.read(cx);
            assert_eq!(target.items.len(), 3);
            assert_eq!(target.items[0].entity_id(), dragged_id);
            assert!(target.tab_drag_preview.is_none());
        });
    }

    #[gpui::test]
    fn leaving_the_tab_bar_discards_the_preview(cx: &mut gpui::TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            Pane::new(vec![item(cx), item(cx), item(cx)], cx)
        });
        cx.refresh().unwrap();
        let first = cx.update(|_, cx| pane.read(cx).items[0].entity_id());
        let start = gpui::point(px(8.0), px(12.0));
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            start + gpui::point(px(4.0), px(0.0)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_move(
            gpui::point(px(350.0), px(12.0)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|_, cx| assert!(pane.read(cx).tab_drag_preview.is_some()));
        let outside = gpui::point(px(-30.0), px(-30.0));
        cx.simulate_mouse_move(outside, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(outside, MouseButton::Left, gpui::Modifiers::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(pane.tab_drag_preview.is_none());
            assert_eq!(pane.items[0].entity_id(), first);
            assert_eq!(pane.items.len(), 3);
        });
    }

    #[gpui::test]
    fn drop_at_tail_resolves_tab_identity_after_live_removal(cx: &mut gpui::TestAppContext) {
        let pane = cx.update(|cx| cx.new(|cx| Pane::new(vec![item(cx), item(cx), item(cx)], cx)));
        cx.update(|cx| {
            let dragged = DraggedTab {
                pane: pane.clone(),
                item: pane.read(cx).items()[1].clone_handle(),
            };
            let last_id = pane.read(cx).items()[2].entity_id();
            pane.update(cx, |pane, cx| {
                pane.remove_item(0, cx);
                pane.drop_tab(&dragged, pane.items.len(), cx);
                assert_eq!(pane.items.len(), 2);
                assert_eq!(pane.items[0].entity_id(), last_id);
                assert_eq!(pane.items[1].entity_id(), dragged.item.entity_id());
                assert_eq!(pane.active_index, 1);
            });
        });
    }

    #[gpui::test]
    fn dropping_a_closed_tab_does_not_resurrect_it(cx: &mut gpui::TestAppContext) {
        let source = cx.update(|cx| cx.new(|cx| Pane::new(vec![item(cx), item(cx)], cx)));
        let destination = cx.update(|cx| cx.new(|cx| Pane::new(vec![item(cx)], cx)));
        cx.update(|cx| {
            let dragged = DraggedTab {
                pane: source.clone(),
                item: source.read(cx).items()[0].clone_handle(),
            };
            source.update(cx, |pane, cx| pane.remove_item(0, cx));
            destination.update(cx, |pane, cx| {
                pane.drop_tab(&dragged, 1, cx);
                assert_eq!(pane.items.len(), 1);
            });
            assert_eq!(source.read(cx).items.len(), 1);
        });
    }

    #[gpui::test]
    fn focus_or_open_activates_existing_tab(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx), other_item(cx)], cx)));
        let pane = cx.read(|cx| tg.read(cx).panes()[0].clone());
        cx.update(|cx| {
            tg.update(cx, |tg, cx| {
                tg.focus_or_open("other_item", |cx| Some(other_item(cx)), cx);
            });
        });
        cx.read(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.items().len(), 2, "no new tab when one already exists");
            assert_eq!(pane.active_index(), 1, "existing tab becomes active");
        });
    }

    #[gpui::test]
    fn focus_or_open_adds_missing_panel(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx)], cx)));
        let pane = cx.read(|cx| tg.read(cx).panes()[0].clone());
        cx.update(|cx| {
            tg.update(cx, |tg, cx| {
                tg.focus_or_open("other_item", |cx| Some(other_item(cx)), cx);
            });
        });
        cx.read(|cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.items().len(), 2);
            assert_eq!(pane.items()[1].serialization_key(), "other_item");
            assert_eq!(pane.active_index(), 1, "new tab becomes active");
        });
    }

    #[gpui::test]
    fn split_pane_ignores_missing_target(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx)], cx)));
        let stray = cx.update(|cx| cx.new(|cx| Pane::new(vec![item(cx)], cx)));
        cx.update(|cx| {
            let new_pane = cx.new(|cx| Pane::new(vec![item(cx)], cx));
            tg.update(cx, |tg, cx| {
                tg.split_pane(&stray, new_pane, SplitDirection::Right, cx);
            });
        });
        cx.read(|cx| assert_eq!(tg.read(cx).panes().len(), 1));
    }

    #[gpui::test]
    fn single_item_same_pane_edge_drop_is_noop(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx)], cx)));
        let pane = cx.read(|cx| tg.read(cx).panes()[0].clone());
        cx.update(|cx| {
            let handle = pane.read(cx).items()[0].clone_handle();
            let dragged = DraggedTab {
                pane: pane.clone(),
                item: handle,
            };
            pane.update(cx, |p, cx| {
                p.split_or_move(&dragged, SplitDirection::Right, cx)
            });
        });
        cx.read(|cx| {
            assert_eq!(tg.read(cx).panes().len(), 1);
            assert_eq!(pane.read(cx).items().len(), 1);
        });
    }

    #[gpui::test]
    fn multi_item_same_pane_edge_drop_splits(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx), item(cx)], cx)));
        let pane = cx.read(|cx| tg.read(cx).panes()[0].clone());
        cx.update(|cx| {
            let handle = pane.read(cx).items()[1].clone_handle();
            let dragged = DraggedTab {
                pane: pane.clone(),
                item: handle,
            };
            pane.update(cx, |p, cx| {
                p.split_or_move(&dragged, SplitDirection::Right, cx)
            });
        });
        cx.read(|cx| {
            assert_eq!(pane.read(cx).items().len(), 1);
            assert_eq!(tg.read(cx).panes().len(), 2);
        });
    }
    fn click(
        cx: &mut gpui::VisualTestContext,
        position: Point<Pixels>,
        button: MouseButton,
        click_count: usize,
    ) {
        cx.simulate_event(gpui::MouseDownEvent {
            button,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count,
        });
    }

    /// A tile group already split in two, with the original pane on the left.
    fn split_group(cx: &mut Context<TileGroup>) -> TileGroup {
        crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
        let mut tg = TileGroup::new(vec![item(cx), item(cx)], cx);
        let left = tg.panes()[0].clone();
        let right = cx.new(|cx| Pane::new(vec![other_item(cx)], cx));
        tg.split_pane(&left, right, SplitDirection::Right, cx);
        tg
    }

    #[gpui::test]
    fn double_clicking_a_tab_zooms_its_pane_and_again_restores(cx: &mut gpui::TestAppContext) {
        let (tg, cx) = cx.add_window_view(|_, cx| split_group(cx));
        cx.refresh().unwrap();
        let (left, second) = cx.update(|_, cx| {
            let left = tg.read(cx).panes()[0].clone();
            let second = left.read(cx).items[1].entity_id();
            (left, second)
        });
        let second_tab = cx.update(|_, cx| left.read(cx).tab_sizes.borrow()[&second]);
        let tab = gpui::point(second_tab.width + px(8.0), px(12.0));

        click(cx, tab, MouseButton::Left, 1);
        cx.update(|_, cx| {
            assert_eq!(left.read(cx).active_index, 1, "a single click activates");
            assert!(tg.read(cx).zoomed().is_none(), "a single click never zooms");
        });

        click(cx, tab, MouseButton::Left, 2);
        cx.refresh().unwrap();
        cx.update(|window, cx| {
            assert_eq!(tg.read(cx).zoomed(), Some(&left));
            let pane = left.read(cx);
            assert_eq!(pane.items[1].entity_id(), second, "items survive the zoom");
            assert_eq!(pane.active_index, 1);
            assert_eq!(
                pane.content_bounds.size.width,
                window.viewport_size().width,
                "the zoomed pane spans the whole group"
            );
        });

        click(cx, tab, MouseButton::Left, 1);
        click(cx, tab, MouseButton::Left, 2);
        cx.update(|_, cx| assert!(tg.read(cx).zoomed().is_none()));
    }

    #[gpui::test]
    fn zoom_clears_when_the_layout_changes_under_it(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(split_group));
        let (left, right) = cx.read(|cx| {
            (
                tg.read(cx).panes()[0].clone(),
                tg.read(cx).panes()[1].clone(),
            )
        });
        cx.update(|cx| {
            tg.update(cx, |tg, cx| {
                tg.toggle_zoom(&right, cx);
                assert_eq!(tg.zoomed(), Some(&right));
                tg.remove_pane(&right, cx);
                assert!(tg.zoomed().is_none(), "removing the zoomed pane un-zooms");

                let lone = tg.panes()[0].clone();
                tg.toggle_zoom(&lone, cx);
                assert!(tg.zoomed().is_none(), "a lone pane has nothing to hide");

                let extra = cx.new(|cx| Pane::new(vec![item(cx)], cx));
                tg.split_pane(&left, extra.clone(), SplitDirection::Down, cx);
                tg.toggle_zoom(&left, cx);
                let fresh = cx.new(|cx| Pane::new(vec![item(cx)], cx));
                tg.split_pane(&extra, fresh, SplitDirection::Right, cx);
                assert!(tg.zoomed().is_none(), "a split reveals its new pane");

                tg.toggle_zoom(&left, cx);
                tg.exit_zoom(cx);
                assert!(tg.zoomed().is_none());
            });
        });
    }

    #[gpui::test]
    fn middle_click_closes_only_closable_tabs(cx: &mut gpui::TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            Pane::new(vec![item(cx), other_item(cx)], cx)
        });
        cx.refresh().unwrap();
        let (survivor, first_width) = cx.update(|_, cx| {
            let pane = pane.read(cx);
            let first = pane.items[0].entity_id();
            (
                pane.items[1].entity_id(),
                pane.tab_sizes.borrow()[&first].width,
            )
        });
        click(
            cx,
            gpui::point(first_width + px(8.0), px(12.0)),
            MouseButton::Middle,
            1,
        );
        cx.update(|_, cx| assert_eq!(pane.read(cx).items.len(), 2));
        click(cx, gpui::point(px(8.0), px(12.0)), MouseButton::Middle, 1);
        cx.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.items.len(), 1);
            assert_eq!(pane.items[0].entity_id(), survivor);
        });
    }

    #[gpui::test]
    fn dragging_the_rail_edge_resizes_within_bounds(cx: &mut gpui::TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            let mut pane = Pane::new(vec![item(cx)], cx);
            pane.tab_orientation = TabOrientation::Vertical;
            pane
        });
        cx.refresh().unwrap();
        let handle = cx.debug_bounds("tab-rail-resize").unwrap();
        assert_eq!(handle.right(), px(DEFAULT_RAIL_WIDTH));
        let start = handle.center();
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        for x in [start.x + px(8.0), px(300.0)] {
            cx.simulate_mouse_move(
                gpui::point(x, start.y),
                MouseButton::Left,
                gpui::Modifiers::default(),
            );
        }
        cx.refresh().unwrap();
        cx.update(|_, cx| assert_eq!(pane.read(cx).rail_width, px(300.0)));
        let content = cx.update(|_, cx| pane.read(cx).content_bounds);
        assert_eq!(content.origin.x, px(300.0), "content starts past the rail");
        cx.update(|_, cx| {
            assert_eq!(
                pane.read(cx).current_outer_size().width,
                f32::from(content.right()),
                "outer size counts the resized rail"
            )
        });

        cx.simulate_mouse_move(
            gpui::point(px(2000.0), start.y),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            gpui::point(px(2000.0), start.y),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.update(|_, cx| assert_eq!(pane.read(cx).rail_width, px(MAX_RAIL_WIDTH)));
    }

    #[gpui::test]
    fn rail_width_round_trips_through_the_layout(cx: &mut gpui::TestAppContext) {
        let tg = cx.update(|cx| cx.new(|cx| TileGroup::new(vec![item(cx)], cx)));
        let json = cx.update(|cx| {
            let pane = tg.read(cx).panes()[0].clone();
            assert!(
                !tg.read(cx).to_json(cx).contains("tab_rail_width"),
                "a default rail leaves the document as it was"
            );
            pane.update(cx, |pane, cx| {
                pane.set_tab_orientation(TabOrientation::Vertical, cx);
                pane.set_rail_width(px(10.0), cx);
                assert_eq!(pane.rail_width(), px(MIN_RAIL_WIDTH));
                pane.set_rail_width(px(224.0), cx);
            });
            tg.read(cx).to_json(cx)
        });
        let mut registry = crate::tiles::ItemRegistry::default();
        registry.register::<TestItem>(|_, cx| Some(cx.new(|_| TestItem)));
        cx.update(|cx| {
            tg.update(cx, |tg, cx| {
                *tg = TileGroup::from_json(&json, &registry, cx).unwrap();
            })
        });
        cx.read(|cx| {
            let pane = tg.read(cx).panes()[0].read(cx);
            assert_eq!(pane.rail_width(), px(224.0));
            assert_eq!(pane.tab_orientation(), TabOrientation::Vertical);
        });
    }
}
