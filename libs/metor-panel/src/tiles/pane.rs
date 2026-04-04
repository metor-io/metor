use gpui::{
    AnyElement, Bounds, Context, DragMoveEvent, EventEmitter, IntoElement,
    MouseButton, Pixels, Render, Window, div, prelude::*, px,
};

use crate::theme::DARK;
use super::drag::{DraggedTab, SplitDirection, detect_split_zone};
use super::item::PaneItemHandle;

const TAB_HEIGHT: f32 = 28.0;
const TAB_CLOSE_SIZE: f32 = 16.0;

/// Events emitted by a Pane to its parent TileGroup.
pub enum PaneEvent {
    /// A tab was dropped on an edge zone, requesting a split.
    Split {
        direction: SplitDirection,
        item: Box<dyn PaneItemHandle>,
    },
    /// The pane has no more items and should be removed.
    Empty,
    /// User requested to inspect/edit a panel item (e.g. via right-click).
    Inspect {
        item: Box<dyn PaneItemHandle>,
    },
}

impl EventEmitter<PaneEvent> for Pane {}

pub struct Pane {
    items: Vec<Box<dyn PaneItemHandle>>,
    active_index: usize,
    drag_split_direction: Option<SplitDirection>,
    content_bounds: Bounds<Pixels>,
}

impl Pane {
    pub fn new(items: Vec<Box<dyn PaneItemHandle>>, _cx: &mut Context<Self>) -> Self {
        Self {
            items,
            active_index: 0,
            drag_split_direction: None,
            content_bounds: Bounds::default(),
        }
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

    /// Handle a drop on the tab bar — always insert as a tab, never split.
    fn handle_tab_bar_drop(
        &mut self,
        dragged: &DraggedTab,
        target_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_split_direction = None;
        self.move_or_insert_tab(dragged, target_ix, cx);
    }

    /// Handle a drop on the content area — check split zones first.
    fn handle_content_drop(
        &mut self,
        dragged: &DraggedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_split_direction = None;

        // Check for split zone
        if let Some(direction) = detect_split_zone(
            window.mouse_position(),
            self.content_bounds,
        ) {
            let same_pane = cx.entity().entity_id() == dragged.pane.entity_id();
            if same_pane {
                // Splitting from our own pane: remove first, then emit split.
                // This avoids re-entrant update on the same entity.
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

        // Center zone: add as a tab
        self.move_or_insert_tab(dragged, self.items.len(), cx);
    }

    fn move_or_insert_tab(
        &mut self,
        dragged: &DraggedTab,
        target_ix: usize,
        cx: &mut Context<Self>,
    ) {
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
        let new_direction = detect_split_zone(position, self.content_bounds);
        if new_direction != self.drag_split_direction {
            self.drag_split_direction = new_direction;
            cx.notify();
        }
    }

    fn render_tab(
        &self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let item = &self.items[ix];
        let title = item.tab_title(cx);
        let is_active = ix == self.active_index;
        let can_close = item.can_close(cx);

        let pane_entity = cx.entity().clone();
        let item_handle = item.clone_handle();
        let inspect_handle = item.clone_handle();

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
            .border_r_1()
            .border_color(DARK.border_primary);

        if is_active {
            tab = tab.bg(DARK.bg_primary).text_color(DARK.text_primary);
        } else {
            tab = tab
                .bg(DARK.bg_secondary)
                .text_color(DARK.text_secondary)
                .hover(|s| s.bg(DARK.bg_primary).text_color(DARK.text_primary));
        }

        // Click to activate
        tab = tab.on_click(cx.listener(move |this, _, _, cx| {
            this.activate_item(ix, cx);
        }));

        // Right-click to inspect/edit
        tab = tab.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, _, cx| {
                this.activate_item(ix, cx);
                cx.emit(PaneEvent::Inspect {
                    item: inspect_handle.clone_handle(),
                });
            }),
        );

        // Drag this tab
        tab = tab.on_drag(
            DraggedTab {
                pane: pane_entity,
                item: item_handle,
                ix,
            },
            |dragged, _, _, cx| cx.new(|_| DraggedTab {
                pane: dragged.pane.clone(),
                item: dragged.item.clone_handle(),
                ix: dragged.ix,
            }),
        );

        // Drop target for reordering (tab bar — never splits)
        tab = tab.on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
            this.handle_tab_bar_drop(dragged, ix, window, cx);
        }));

        // Drag-over highlight
        tab = tab.drag_over::<DraggedTab>(|style, _, _, _| {
            style.bg(DARK.border_primary)
        });

        // Tab label
        tab = tab.child(title);

        // Close button
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
                    .text_size(px(10.0))
                    .text_color(DARK.text_tertiary)
                    .hover(|s| s.bg(DARK.border_primary).text_color(DARK.text_primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove_item(ix, cx);
                    }))
                    .child("x"),
            );
        }

        tab
    }

    fn render_split_overlay(&self) -> Option<AnyElement> {
        let direction = self.drag_split_direction?;
        let highlight = gpui::Hsla {
            h: DARK.line_color.h,
            s: DARK.line_color.s,
            l: DARK.line_color.l,
            a: 0.15,
        };

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
        // Tab bar
        let mut tab_bar = div()
            .flex()
            .flex_row()
            .w_full()
            .h(px(TAB_HEIGHT))
            .bg(DARK.bg_secondary)
            .border_b_1()
            .border_color(DARK.border_primary)
            .overflow_x_hidden();

        // Drop target at end of tab bar (to drop as last tab)
        let tab_count = self.items.len();
        for ix in 0..tab_count {
            tab_bar = tab_bar.child(self.render_tab(ix, window, cx));
        }

        // Empty space drop target in tab bar
        tab_bar = tab_bar.child(
            div()
                .id("tab-bar-drop-zone")
                .flex_1()
                .h_full()
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.handle_tab_bar_drop(dragged, tab_count, window, cx);
                }))
                .drag_over::<DraggedTab>(|style, _, _, _| {
                    style.bg(DARK.border_primary)
                }),
        );

        // Content area
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
            .bg(DARK.bg_primary)
            .child(content_bounds_tracker);

        // Render active item
        if let Some(item) = self.items.get(self.active_index) {
            content = content.child(item.view());
        }

        // Drop on content area (split-on-drop at edges, add-as-tab in center)
        content = content
            .on_drop(cx.listener(|this, dragged: &DraggedTab, window, cx| {
                this.handle_content_drop(dragged, window, cx);
            }))
            .on_drag_move(cx.listener(Self::handle_content_drag_move));

        // Split direction overlay
        if let Some(overlay) = self.render_split_overlay() {
            content = content.child(overlay);
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(tab_bar)
            .child(content)
    }
}
