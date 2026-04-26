use gpui::{
    AnyElement, App, Bounds, Context, DragMoveEvent, EventEmitter, IntoElement, MouseButton,
    Pixels, Point, Render, ScrollHandle, Window, div, prelude::*, px,
};
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use super::drag::{DraggedTab, SplitDirection, detect_split_zone};
use super::item::PaneItemHandle;
use crate::icons::Icon;
use crate::theme::theme;

/// Dispatched by views inside a pane to spawn a new plot tab in that pane.
///
/// The payload is deliberately minimal — `ComponentId` and element indices —
/// so the action can implement `PartialEq`. Color, label, and style derive
/// from theme and metadata inside `PlotPanel::new`, matching the
/// `TimeSeriesPlot::from_component` path used by the palette wizard.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct PlotComponentAction {
    pub component_id: ComponentId,
    pub indices: SmallVec<[usize; 4]>,
}

const TAB_HEIGHT: f32 = 28.0;
const TAB_CLOSE_SIZE: f32 = 16.0;
const TAB_RAIL_WIDTH: f32 = 160.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, facet::Facet)]
#[repr(u8)]
pub enum TabOrientation {
    #[default]
    Horizontal,
    Vertical,
}

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
    drag_split_direction: Option<SplitDirection>,
    content_bounds: Bounds<Pixels>,
    tab_scroll: ScrollHandle,
    tab_orientation: TabOrientation,
    hide_tab_bar: bool,
    locked_size: Option<gpui::Size<f32>>,
}

impl Pane {
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, _cx: &mut Context<Self>) -> Self {
        Self {
            items,
            active_index: 0,
            drag_split_direction: None,
            content_bounds: Bounds::default(),
            tab_scroll: ScrollHandle::new(),
            tab_orientation: TabOrientation::default(),
            hide_tab_bar: false,
            locked_size: None,
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
                TabOrientation::Vertical => size.width += TAB_RAIL_WIDTH,
            }
        }
        size
    }

    pub fn set_locked_size(&mut self, size: Option<gpui::Size<f32>>, cx: &mut Context<Self>) {
        self.locked_size = size;
        cx.notify();
    }

    pub fn items(&self) -> &[Box<dyn PaneItemHandle>] {
        &self.items
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
        self.items.remove(ix);
        if self.items.is_empty() {
            cx.emit(PaneEvent::Empty);
        } else if self.active_index >= self.items.len() {
            self.active_index = self.items.len() - 1;
        } else if self.active_index > ix {
            self.active_index -= 1;
        }
        cx.notify();
    }

    pub fn activate_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.active_index = ix;
            cx.notify();
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
        self.drag_split_direction = None;
        self.drop_tab(dragged, target_ix, cx);
    }

    /// Content-area drops split the pane when near an edge, otherwise insert
    /// as a tab.
    fn handle_content_drop(
        &mut self,
        dragged: &DraggedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_split_direction = None;

        if let Some(direction) = detect_split_zone(window.mouse_position(), self.content_bounds) {
            let same_pane = cx.entity().entity_id() == dragged.pane.entity_id();
            if same_pane {
                // Re-entrant update on the same entity is forbidden; remove
                // the tab locally before asking the tile group to split.
                let item = dragged.item.clone_handle();
                self.remove_item(dragged.ix, cx);
                cx.emit(PaneEvent::Split { direction, item });
            } else {
                cx.emit(PaneEvent::Split {
                    direction,
                    item: dragged.item.clone_handle(),
                });
                dragged.pane.update(cx, |source, cx| {
                    source.remove_item(dragged.ix, cx);
                });
            }
            return;
        }

        self.drop_tab(dragged, self.items.len(), cx);
    }

    fn drop_tab(&mut self, dragged: &DraggedTab, target_ix: usize, cx: &mut Context<Self>) {
        let same_pane = cx.entity().entity_id() == dragged.pane.entity_id();
        if same_pane {
            let from = dragged.ix;
            let to = target_ix.min(self.items.len().saturating_sub(1));
            if from != to && from < self.items.len() {
                let item = self.items.remove(from);
                let insert_at = if to > from { to - 1 } else { to };
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
                source.remove_item(dragged.ix, cx);
            });
        }
    }

    fn handle_content_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedTab>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = event.event.position;
        let new_direction = if self.content_bounds.contains(&position) {
            detect_split_zone(position, self.content_bounds)
        } else {
            None
        };
        if new_direction != self.drag_split_direction {
            self.drag_split_direction = new_direction;
            cx.notify();
        }
    }

    fn render_tab(
        &self,
        ix: usize,
        orientation: TabOrientation,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let item = &self.items[ix];
        let title = item.tab_title(cx);
        let is_active = ix == self.active_index;
        let can_close = item.can_close(cx);

        let pane_entity = cx.entity().clone();
        let item_handle = item.clone_handle();
        let inspect_handle = item.clone_handle();

        let border_primary = theme.border_primary;
        let bg_primary = theme.bg_primary;
        let text_primary = theme.text_primary;

        let mut tab = div()
            .id(("tab", ix))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .h(px(TAB_HEIGHT))
            .text_size(px(12.0))
            .cursor_pointer()
            .border_color(border_primary);

        tab = match orientation {
            TabOrientation::Horizontal => tab.border_r_1(),
            TabOrientation::Vertical => tab.w_full().justify_between().border_b_1(),
        };

        if is_active {
            tab = tab.bg(bg_primary).text_color(text_primary);
        } else {
            tab = tab
                .bg(theme.bg_secondary)
                .text_color(theme.text_secondary)
                .hover(move |s| s.bg(bg_primary).text_color(text_primary));
        }

        tab = tab.on_click(cx.listener(move |this, _, _, cx| {
            this.activate_item(ix, cx);
        }));

        tab = tab.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _window, cx| {
                this.activate_item(ix, cx);
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
                ix,
            },
            |dragged, _, _, cx| {
                cx.new(|_| DraggedTab {
                    pane: dragged.pane.clone(),
                    item: dragged.item.clone_handle(),
                    ix: dragged.ix,
                })
            },
        );

        tab = tab.on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
            this.handle_tab_bar_drop(dragged, ix, window, cx);
        }));

        tab = tab.drag_over::<DraggedTab>(move |style, _, _, _| style.bg(border_primary));

        tab = tab.child(title);

        if can_close {
            tab = tab.child(
                div()
                    .id(("tab-close", ix))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(TAB_CLOSE_SIZE))
                    .h(px(TAB_CLOSE_SIZE))
                    .rounded(px(3.0))
                    .text_color(theme.text_tertiary)
                    .hover(move |s| s.bg(border_primary).text_color(text_primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove_item(ix, cx);
                    }))
                    .child(Icon::Close.svg(10.0)),
            );
        }

        tab
    }

    fn render_split_overlay(&self, cx: &App) -> Option<AnyElement> {
        let theme = theme(cx);
        let direction = self.drag_split_direction?;
        let highlight = theme.drop_target;

        let overlay = match direction {
            SplitDirection::Left => div().w_1_2().h_full().bg(highlight),
            SplitDirection::Right => div()
                .flex()
                .flex_row_reverse()
                .w_full()
                .h_full()
                .child(div().w_1_2().h_full().bg(highlight)),
            SplitDirection::Up => div().w_full().h_1_2().bg(highlight),
            SplitDirection::Down => div()
                .flex()
                .flex_col_reverse()
                .w_full()
                .h_full()
                .child(div().w_full().h_1_2().bg(highlight)),
        };

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(overlay)
                .into_any_element(),
        )
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let orientation = self.tab_orientation;
        let tab_count = self.items.len();

        let tab_bar = (!self.hide_tab_bar).then(|| {
            let mut tab_bar = div()
                .id("pane-tab-bar")
                .flex()
                .bg(theme.bg_secondary)
                .border_color(theme.border_primary)
                .track_scroll(&self.tab_scroll);
            tab_bar = match orientation {
                TabOrientation::Horizontal => tab_bar
                    .flex_row()
                    .w_full()
                    .h(px(TAB_HEIGHT))
                    .border_b_1()
                    .overflow_x_scroll(),
                TabOrientation::Vertical => tab_bar
                    .flex_col()
                    .h_full()
                    .w(px(TAB_RAIL_WIDTH))
                    .border_r_1()
                    .overflow_y_scroll(),
            };
            tab_bar.style().restrict_scroll_to_axis = Some(true);

            for ix in 0..tab_count {
                tab_bar = tab_bar.child(self.render_tab(ix, orientation, window, cx));
            }

            let mut drop_zone = div()
                .id("tab-bar-drop-zone")
                .flex_1()
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.handle_tab_bar_drop(dragged, tab_count, window, cx);
                }))
                .drag_over::<DraggedTab>({
                    let border = theme.border_primary;
                    move |style, _, _, _| style.bg(border)
                })
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
            tab_bar.child(drop_zone)
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
        content = content
            .on_drop(cx.listener(|this, dragged: &DraggedTab, window, cx| {
                this.handle_content_drop(dragged, window, cx);
            }))
            .on_drag_move(cx.listener(Self::handle_content_drag_move));

        if let Some(overlay) = self.render_split_overlay(cx) {
            content = content.child(overlay);
        }

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
