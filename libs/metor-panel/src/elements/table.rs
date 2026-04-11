use std::ops::Range;

use gpui::{
    AnyElement, App, Bounds, Context, DragMoveEvent, Empty, IntoElement, Pixels, Render,
    SharedString, Window, div, prelude::*, px, uniform_list,
};

use crate::icons::Icon;
use crate::theme::theme;

const HEADER_HEIGHT: f32 = 32.0;
const RESIZE_HANDLE_WIDTH: f32 = 6.0;

/// Sort state for a table column.
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

/// Definition for a table column: name, sizing, and behavior.
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

/// Provides data and rendering for a [`Table`]: columns, row count, cell rendering, and sorting.
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

/// Generic table widget with resizable/sortable columns and virtual scrolling.
pub struct Table<D: TableDelegate> {
    delegate: D,
    col_states: Vec<ColState>,
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
        }
    }

    pub fn delegate(&self) -> &D {
        &self.delegate
    }

    pub fn delegate_mut(&mut self) -> &mut D {
        &mut self.delegate
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

    fn render_resize_handle(
        &self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("resize-handle", col_ix))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .ml(px(-RESIZE_HANDLE_WIDTH / 2.0))
            .cursor_col_resize()
            .on_drag(ResizeDrag(col_ix), |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<ResizeDrag>, _window, cx| {
                    let columns = this.delegate.columns();
                    let col_ix = e.drag(cx).0;
                    if let Some(col) = columns.get(col_ix) {
                        let col_left = this.col_states[col_ix].bounds.left();
                        let new_width =
                            (e.event.position.x - col_left).clamp(col.min_width, col.max_width);
                        if let Some(state) = this.col_states.get_mut(col_ix) {
                            if state.width != new_width {
                                state.width = new_width;
                                cx.notify();
                            }
                        }
                    }
                },
            ))
    }
}

impl<D: TableDelegate> Render for Table<D> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_col_states();
        let theme = theme(cx);
        let columns = self.delegate.columns();
        let row_count = self.delegate.rows_count();
        let row_height = self.delegate.row_height();

        // Build header
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .border_b_1()
            .border_color(theme.border_primary);

        let view = cx.entity().clone();
        for (ix, col) in columns.iter().enumerate() {
            let width = self.col_states[ix].width;
            let is_flex = col.flex;
            let is_resizable = col.resizable;

            // Capture column bounds via canvas
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

            let mut col_div = div().relative().overflow_hidden();
            if is_flex {
                col_div = col_div.flex_1();
            } else {
                col_div = col_div.w(width);
            }

            let cell = self.render_header_cell(ix, col, window, cx);
            col_div = col_div.child(bounds_canvas).child(cell);

            if is_resizable && !is_flex {
                let handle = self.render_resize_handle(ix, window, cx);
                col_div = col_div.child(
                    div()
                        .absolute()
                        .right(px(-RESIZE_HANDLE_WIDTH / 2.0))
                        .top_0()
                        .bottom_0()
                        .child(handle),
                );
            }

            header = header.child(col_div);
        }

        // Build body with uniform_list
        let col_count = columns.len();
        let col_flex: Vec<bool> = columns.iter().map(|c| c.flex).collect();

        let border_color = theme.border_primary;
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_primary)
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(
                uniform_list(
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

                                for col_ix in 0..col_count {
                                    let cell =
                                        this.delegate.render_cell(row_ix, col_ix, window, cx);

                                    let mut cell_div = div().overflow_hidden();
                                    if col_flex[col_ix] {
                                        cell_div = cell_div.flex_1().h(row_height);
                                    } else {
                                        cell_div =
                                            cell_div.w(this.col_states[col_ix].width);
                                    }

                                    row = row.child(cell_div.child(cell));
                                }

                                items.push(row.into_any_element());
                            }
                            items
                        },
                    ),
                )
                .flex_1(),
            )
    }
}
