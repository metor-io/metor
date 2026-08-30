use std::ops::Range;

use gpui::{
    AnyElement, App, Axis, Bounds, Context, DragMoveEvent, ElementId, Empty, IntoElement, Pixels,
    Render, ScrollHandle, SharedString, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use super::Scrollbar;

use crate::icons::Icon;
use crate::theme::theme;

const HEADER_HEIGHT: f32 = 32.0;
const RESIZE_HANDLE_WIDTH: f32 = 6.0;

/// Sort state of a column in a [`Table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnSort {
    Default,
    Ascending,
    Descending,
}

impl ColumnSort {
    fn cycle(self) -> Self {
        match self {
            Self::Default => Self::Descending,
            Self::Descending => Self::Ascending,
            Self::Ascending => Self::Default,
        }
    }
}

/// Declarative description of one table column.
///
/// Width is pixel-based unless `flex` is set, in which case the column
/// absorbs remaining horizontal space in proportion with other flex columns.
pub struct Column {
    pub name: SharedString,
    pub width: Pixels,
    pub min_width: Pixels,
    pub max_width: Pixels,
    pub resizable: bool,
    pub sortable: bool,
    pub flex: bool,
}

impl Column {
    pub fn new(name: impl Into<SharedString>, width: f32) -> Self {
        Self {
            name: name.into(),
            width: px(width),
            min_width: px(50.0),
            max_width: px(f32::MAX),
            resizable: true,
            sortable: false,
            flex: false,
        }
    }

    pub fn sortable(mut self) -> Self {
        self.sortable = true;
        self
    }

    pub fn flex(mut self) -> Self {
        self.flex = true;
        self
    }

    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    pub fn min_width(mut self, min: f32) -> Self {
        self.min_width = px(min);
        self
    }

    pub fn max_width(mut self, max: f32) -> Self {
        self.max_width = px(max);
        self
    }
}

/// Data source for a [`Table`].
///
/// Implementers own the row model and paint each cell on demand. Sorting is
/// delegated so backends can use whatever comparator makes sense.
pub trait TableDelegate: Sized + 'static {
    fn columns(&self) -> Vec<Column>;
    fn rows_count(&self) -> usize;
    fn row_height(&self) -> Pixels {
        px(60.0)
    }
    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement;
    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, cx: &App);
    /// Called once after the visible row range has been painted each frame.
    /// Delegates that materialize rows lazily use it to release entities that
    /// scrolled out of view.
    fn frame_rendered(&mut self) {}
}

struct ColState {
    width: Pixels,
    sort: ColumnSort,
    bounds: Bounds<Pixels>,
}

#[derive(Clone)]
struct ResizeDrag(usize);

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Virtualized table widget driven by a [`TableDelegate`].
///
/// Column widths and sort state live here; the delegate just supplies rows.
/// Scrolling uses gpui's `uniform_list` so the row count scales to
/// thousands without per-row element cost.
pub struct Table<D: TableDelegate> {
    delegate: D,
    col_states: Vec<ColState>,
    scroll_handle: UniformListScrollHandle,
    hscroll: ScrollHandle,
}

impl<D: TableDelegate> Table<D> {
    pub fn new(delegate: D) -> Self {
        let col_states = delegate
            .columns()
            .iter()
            .map(|col| ColState {
                width: col.width,
                sort: ColumnSort::Default,
                bounds: Bounds::default(),
            })
            .collect();
        Self {
            delegate,
            col_states,
            scroll_handle: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
        }
    }

    pub fn delegate(&self) -> &D {
        &self.delegate
    }

    pub fn delegate_mut(&mut self) -> &mut D {
        &mut self.delegate
    }

    /// Scroll the body so row `ix` sits at the bottom of the viewport; the
    /// log viewer's follow-tail.
    pub fn scroll_to_row(&mut self, ix: usize) {
        self.scroll_handle
            .scroll_to_item(ix, gpui::ScrollStrategy::Bottom);
    }

    fn sync_col_states(&mut self) {
        let columns = self.delegate.columns();
        if self.col_states.len() != columns.len() {
            self.col_states = columns
                .iter()
                .map(|col| ColState {
                    width: col.width,
                    sort: ColumnSort::Default,
                    bounds: Bounds::default(),
                })
                .collect();
        }
    }

    fn apply_sort(&mut self, col_ix: usize, cx: &App) {
        let new_sort = self.col_states[col_ix].sort.cycle();
        for (ix, state) in self.col_states.iter_mut().enumerate() {
            if ix == col_ix {
                state.sort = new_sort;
            } else {
                state.sort = ColumnSort::Default;
            }
        }
        self.delegate.sort_column(col_ix, new_sort, cx);
    }

    fn render_header_cell(
        &self,
        col_ix: usize,
        col: &Column,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sort = self.col_states[col_ix].sort;
        let sortable = col.sortable;

        let theme = theme(cx);
        let mut cell = div()
            .id(("header-cell", col_ix))
            .flex()
            .flex_row()
            .items_center()
            .h_full()
            .gap(px(4.0))
            .px(px(12.0))
            .text_size(px(12.0))
            .text_color(theme.text_tertiary)
            .child(col.name.clone());

        match sort {
            ColumnSort::Ascending => {
                cell = cell.child(Icon::ChevronUp.svg(12.0));
            }
            ColumnSort::Descending => {
                cell = cell.child(Icon::ChevronDown.svg(12.0));
            }
            ColumnSort::Default => {}
        }

        if sortable {
            cell = cell
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_sort(col_ix, cx);
                    cx.notify();
                }));
        }

        cell
    }

    /// The grab zone along a column's right edge. Every cell in the column
    /// carries one, not just the header, so the divider is draggable down
    /// the whole table; `id` keeps each instance distinct.
    fn render_resize_handle(
        &self,
        id: impl Into<ElementId>,
        col_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .absolute()
            .right_0()
            .top_0()
            .bottom_0()
            .w(px(RESIZE_HANDLE_WIDTH))
            .cursor_col_resize()
            .on_drag(ResizeDrag(col_ix), |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<ResizeDrag>, _window, cx| {
                    let columns = this.delegate.columns();
                    let col_ix = e.drag(cx).0;
                    if let Some(col) = columns.get(col_ix) {
                        let col_left = this.col_states[col_ix].bounds.left();
                        let new_width =
                            (e.event.position.x - col_left).clamp(col.min_width, col.max_width);
                        if let Some(state) = this.col_states.get_mut(col_ix)
                            && state.width != new_width
                        {
                            state.width = new_width;
                            cx.notify();
                        }
                    }
                }),
            )
    }
}

impl<D: TableDelegate> Render for Table<D> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_col_states();
        let theme = theme(cx);
        let columns = self.delegate.columns();
        let row_count = self.delegate.rows_count();
        let row_height = self.delegate.row_height();

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary);

        let view = cx.entity().clone();
        let last_col = columns.len().saturating_sub(1);
        for (ix, col) in columns.iter().enumerate() {
            let width = self.col_states[ix].width;
            let is_flex = col.flex;
            let is_resizable = col.resizable;

            // Capture column bounds via canvas
            // Resize drags convert pixel deltas against the column's
            // on-screen origin; capture the bounds here during paint.
            let view = view.clone();
            let bounds_canvas = gpui::canvas(
                move |bounds, _window, cx| {
                    view.update(cx, |table, _| {
                        if let Some(state) = table.col_states.get_mut(ix) {
                            state.bounds = bounds;
                        }
                    });
                    bounds
                },
                |_, _, _, _| {},
            )
            .size_full()
            .absolute();

            let mut col_div = div().relative().h_full().overflow_hidden();
            if ix < last_col {
                col_div = col_div.border_r_1().border_color(theme.border_primary);
            }
            if is_flex {
                col_div = col_div.flex_1();
            } else {
                col_div = col_div.w(width);
                col_div.style().flex_shrink = Some(0.0);
            }

            let cell = self.render_header_cell(ix, col, window, cx);
            col_div = col_div.child(bounds_canvas).child(cell);

            if is_resizable && !is_flex {
                col_div = col_div.child(self.render_resize_handle(("resize-handle", ix), ix, cx));
            }

            header = header.child(col_div);
        }

        let col_count = columns.len();
        let col_flex: Vec<bool> = columns.iter().map(|c| c.flex).collect();
        let col_resizable: Vec<bool> = columns.iter().map(|c| c.resizable).collect();
        let any_flex = col_flex.iter().any(|f| *f);

        let total_width: f32 = self
            .col_states
            .iter()
            .zip(columns.iter())
            .filter(|(_, c)| !c.flex)
            .map(|(s, _)| f32::from(s.width))
            .sum();

        let border_color = theme.border_primary;
        let scroll_handle = self.scroll_handle.clone();
        let scrollbar_handle = self.scroll_handle.clone();
        let hscroll_handle = self.hscroll.clone();
        let hscroll_indicator = self.hscroll.clone();

        let body = uniform_list(
            "table-body",
            row_count,
            cx.processor(
                move |this: &mut Self,
                      range: Range<usize>,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
                    let mut items = Vec::new();
                    for row_ix in range {
                        let mut row = div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .w_full()
                            .h(row_height)
                            .border_b_1()
                            .border_color(border_color);

                        // `col_ix` indexes both `col_flex` and `col_states`,
                        // so a plain index loop reads better than zipping
                        // two slices together.
                        #[allow(clippy::needless_range_loop)]
                        for col_ix in 0..col_count {
                            let cell = this.delegate.render_cell(row_ix, col_ix, window, cx);

                            let mut cell_div = div().relative().overflow_hidden();
                            if col_ix + 1 < col_count {
                                cell_div = cell_div.border_r_1().border_color(border_color);
                            }
                            if col_flex[col_ix] {
                                cell_div = cell_div.flex_1().h(row_height);
                            } else {
                                // Without explicit flex_shrink=0 the cell
                                // collapses below its column width and
                                // the strip's flex_wrap breaks element
                                // boxes onto a new line.
                                cell_div = cell_div.w(this.col_states[col_ix].width).h(row_height);
                                cell_div.style().flex_shrink = Some(0.0);
                            }

                            cell_div = cell_div.child(cell);
                            if col_resizable[col_ix] && !col_flex[col_ix] {
                                cell_div = cell_div.child(this.render_resize_handle(
                                    ("cell-resize", row_ix * col_count + col_ix),
                                    col_ix,
                                    cx,
                                ));
                            }
                            row = row.child(cell_div);
                        }

                        items.push(row.into_any_element());
                    }
                    this.delegate.frame_rendered();
                    items
                },
            ),
        )
        .track_scroll(scroll_handle)
        .flex_1();

        let mut inner = div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body);
        if !any_flex {
            inner = inner.w(px(total_width));
        }

        let mut scroll_wrap = div().id("table-hscroll").size_full().child(inner);
        if !any_flex {
            scroll_wrap = scroll_wrap
                .overflow_x_scroll()
                .track_scroll(&hscroll_handle);
            scroll_wrap.style().restrict_scroll_to_axis = Some(true);
        }

        let vertical_scrollbar = {
            let state = scrollbar_handle.0.borrow();
            let offset = state.base_handle.offset();
            let max_off = state.base_handle.max_offset();
            let max_y = f32::from(max_off.height);
            let scroll_y = f32::from(-offset.y).clamp(0.0, max_y);
            let vp = f32::from(state.base_handle.bounds().size.height);
            let vp = if vp > 0.0 {
                vp
            } else {
                row_count as f32 * f32::from(row_height)
            };
            div()
                .absolute()
                .top(px(HEADER_HEIGHT))
                .right_0()
                .bottom_0()
                .left_0()
                .child(Scrollbar::new(Axis::Vertical, vp, vp + max_y, scroll_y))
        };

        let horizontal_scrollbar = if any_flex {
            None
        } else {
            let offset = hscroll_indicator.offset();
            let max_off = hscroll_indicator.max_offset();
            let vp = f32::from(hscroll_indicator.bounds().size.width);
            let max_x = f32::from(max_off.width);
            if max_x > 0.0 && vp > 0.0 {
                let scroll_x = f32::from(-offset.x).clamp(0.0, max_x);
                Some(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(10.0))
                        .child(Scrollbar::new(Axis::Horizontal, vp, vp + max_x, scroll_x)),
                )
            } else {
                None
            }
        };

        let mut outer = div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(theme.bg_primary)
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(scroll_wrap)
            .child(vertical_scrollbar);
        if let Some(h) = horizontal_scrollbar {
            outer = outer.child(h);
        }
        outer
    }
}
