use std::ops::Range;

use gpui::{
    AnyElement, App, Axis, Bounds, Context, DragMoveEvent, Empty, FocusHandle, Focusable, Hsla,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollHandle,
    SharedString, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use smallvec::SmallVec;

use super::Scrollbar;
use crate::icons::Icon;
use crate::theme::theme;

const HEADER_HEIGHT: f32 = 28.0;
const ROW_HEIGHT: f32 = 24.0;
const DEFAULT_COL_WIDTH: f32 = 200.0;
const RESIZE_HANDLE_WIDTH: f32 = 6.0;
const MIN_COL_WIDTH: f32 = 120.0;
const MAX_COL_WIDTH: f32 = 800.0;
const MIN_DETAIL_WIDTH: f32 = 320.0;

/// Data source and label supplier for a [`ColumnBrowser`].
///
/// The delegate owns selection (typically stored as a path of stable keys);
/// the widget retains only view state (column widths, scroll positions,
/// focused column). `resolve_selection` runs every render so external tree
/// mutations propagate without explicit invalidation.
pub trait ColumnBrowserDelegate: Sized + 'static {
    /// One row value. Keep it cheap to clone: `Arc`-based handles work well.
    type Item: Clone + 'static;

    /// Current selection resolved against the live tree.
    ///
    /// Truncate at the first missing ancestor so the browser collapses back
    /// to a valid prefix after a tree rebuild.
    fn resolve_selection(&self, cx: &App) -> SmallVec<[Self::Item; 8]>;

    /// Items shown in column 0, honoring any [`set_root_override`](Self::set_root_override).
    fn root_items(&self, cx: &App) -> Vec<Self::Item>;

    fn children(&self, item: &Self::Item, cx: &App) -> Vec<Self::Item>;

    /// `true` when `item` should display a preview instead of a child column.
    fn is_leaf(&self, item: &Self::Item) -> bool;

    fn item_label(&self, item: &Self::Item) -> SharedString;

    /// Header text for a column. `parent` is `None` for the root column.
    fn column_label(&self, parent: Option<&Self::Item>) -> SharedString;

    /// Identity comparison used to reattach a stored selection to freshly
    /// produced items.
    fn items_equal(&self, a: &Self::Item, b: &Self::Item) -> bool;

    /// Record that the user picked `item` in `column_ix`.
    ///
    /// The delegate must truncate any deeper prior selection so the new
    /// pick becomes the tail.
    fn set_selection(
        &mut self,
        column_ix: usize,
        item: &Self::Item,
        cx: &mut Context<ColumnBrowser<Self>>,
    );

    /// Set a virtual root so the browser behaves as though a sub-node were
    /// the top of the tree.
    ///
    /// `ancestors` is the chain from the original root down to the new
    /// root; `ancestors.last()` is the node whose children become column 0.
    fn set_root_override(
        &mut self,
        ancestors: SmallVec<[Self::Item; 8]>,
        cx: &mut Context<ColumnBrowser<Self>>,
    );

    /// Undo a [`set_root_override`](Self::set_root_override).
    fn clear_root_override(&mut self, cx: &mut Context<ColumnBrowser<Self>>);

    /// Optional trailing detail column.
    ///
    /// Rendered after every navigation column. `tail` is the selected node,
    /// or `None` when nothing is selected. Return `None` to suppress.
    fn render_detail(
        &mut self,
        tail: Option<&Self::Item>,
        window: &mut Window,
        cx: &mut Context<ColumnBrowser<Self>>,
    ) -> Option<AnyElement> {
        let _ = (tail, window, cx);
        None
    }

    /// Header text above the detail column. Ignored when no detail renders.
    fn detail_label(&self, tail: Option<&Self::Item>) -> SharedString {
        let _ = tail;
        SharedString::default()
    }

    /// Triggered by Enter or a double-click on a row.
    fn on_activate(
        &mut self,
        item: &Self::Item,
        window: &mut Window,
        cx: &mut Context<ColumnBrowser<Self>>,
    ) {
        let _ = (item, window, cx);
    }

    /// Right-click on a row (`item = Some`) or the column body
    /// (`item = None`). Default: ignore.
    fn on_context_menu(
        &mut self,
        column_ix: usize,
        item: Option<&Self::Item>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<ColumnBrowser<Self>>,
    ) {
        let _ = (column_ix, item, position, window, cx);
    }

    /// Run before each render so the delegate can auto-extend the
    /// current selection through single-item columns. Default: no-op.
    ///
    /// Delegates override this to skip past columns that would only
    /// contain one row, so the user sees deeper content without having
    /// to click through every level.
    fn auto_extend_selection(&mut self, cx: &mut Context<ColumnBrowser<Self>>) {
        let _ = cx;
    }
}

/// Finder-style hierarchical browser.
///
/// Renders the root column, a child column per selected non-leaf item, and
/// a trailing detail column supplied by the delegate. Keyboard navigation
/// (arrow keys, Enter) and column resizing are built in; double-clicking a
/// column header reroots the browser at that prefix.
pub struct ColumnBrowser<D: ColumnBrowserDelegate> {
    delegate: D,
    focus_handle: FocusHandle,
    focused_column: usize,
    column_widths: SmallVec<[Pixels; 8]>,
    scroll_handles: SmallVec<[UniformListScrollHandle; 8]>,
    column_bounds: SmallVec<[Bounds<Pixels>; 8]>,
    hscroll: ScrollHandle,
}

#[derive(Clone)]
struct ResizeDrag(usize);

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl<D: ColumnBrowserDelegate> ColumnBrowser<D> {
    pub fn new(delegate: D, cx: &mut Context<Self>) -> Self {
        Self {
            delegate,
            focus_handle: cx.focus_handle(),
            focused_column: 0,
            column_widths: SmallVec::new(),
            scroll_handles: SmallVec::new(),
            column_bounds: SmallVec::new(),
            hscroll: ScrollHandle::new(),
        }
    }

    pub fn delegate(&self) -> &D {
        &self.delegate
    }

    pub fn delegate_mut(&mut self) -> &mut D {
        &mut self.delegate
    }

    fn ensure_column_state(&mut self, count: usize) {
        while self.column_widths.len() < count {
            self.column_widths.push(px(DEFAULT_COL_WIDTH));
        }
        while self.scroll_handles.len() < count {
            self.scroll_handles.push(UniformListScrollHandle::new());
        }
        while self.column_bounds.len() < count {
            self.column_bounds.push(Bounds::default());
        }
    }

    fn items_for_column(
        &self,
        column_ix: usize,
        selection: &[D::Item],
        cx: &App,
    ) -> Vec<D::Item> {
        if column_ix == 0 {
            self.delegate.root_items(cx)
        } else {
            match selection.get(column_ix - 1) {
                Some(parent) => self.delegate.children(parent, cx),
                None => Vec::new(),
            }
        }
    }

    fn selected_ix_in_column(
        &self,
        column_ix: usize,
        selection: &[D::Item],
        items: &[D::Item],
    ) -> Option<usize> {
        let picked = selection.get(column_ix)?;
        items
            .iter()
            .position(|it| self.delegate.items_equal(it, picked))
    }

    fn handle_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        let selection = self.delegate.resolve_selection(cx);
        match key {
            "up" | "down" => {
                let items = self.items_for_column(self.focused_column, &selection, cx);
                if items.is_empty() {
                    return;
                }
                let next = match self
                    .selected_ix_in_column(self.focused_column, &selection, &items)
                {
                    Some(i) => {
                        let delta: isize = if key == "up" { -1 } else { 1 };
                        (i as isize + delta).clamp(0, items.len() as isize - 1) as usize
                    }
                    None => 0,
                };
                let item = items[next].clone();
                self.delegate.set_selection(self.focused_column, &item, cx);
                cx.notify();
            }
            "right" => {
                if let Some(item) = selection.get(self.focused_column).cloned() {
                    if self.delegate.is_leaf(&item) {
                        return;
                    }
                    let children = self.delegate.children(&item, cx);
                    if let Some(first) = children.first().cloned() {
                        self.delegate
                            .set_selection(self.focused_column + 1, &first, cx);
                        self.focused_column += 1;
                        cx.notify();
                    }
                }
            }
            "left" => {
                if self.focused_column > 0 {
                    self.focused_column -= 1;
                    cx.notify();
                }
            }
            "enter" => {
                if let Some(item) = selection.last().cloned() {
                    self.delegate.on_activate(&item, window, cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    pub fn apply_root_override(&mut self, column_ix: usize, cx: &mut Context<Self>) {
        if column_ix == 0 {
            self.delegate.clear_root_override(cx);
        } else {
            let selection = self.delegate.resolve_selection(cx);
            let ancestors: SmallVec<[D::Item; 8]> =
                selection.iter().take(column_ix).cloned().collect();
            if ancestors.is_empty() {
                return;
            }
            self.delegate.set_root_override(ancestors, cx);
        }
        self.focused_column = 0;
        cx.notify();
    }

    fn render_resize_handle(&self, column_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(("browser-resize", column_ix))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .ml(px(-RESIZE_HANDLE_WIDTH / 2.0))
            .cursor_col_resize()
            .on_drag(ResizeDrag(column_ix), |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<ResizeDrag>, _window, cx| {
                    let ix = e.drag(cx).0;
                    let Some(left) = this.column_bounds.get(ix).map(|b| b.left()) else {
                        return;
                    };
                    let new_width =
                        (e.event.position.x - left).clamp(px(MIN_COL_WIDTH), px(MAX_COL_WIDTH));
                    if let Some(slot) = this.column_widths.get_mut(ix)
                        && *slot != new_width
                    {
                        *slot = new_width;
                        cx.notify();
                    }
                },
            ))
            .into_any_element()
    }

    fn render_column_header(
        &self,
        column_ix: usize,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        div()
            .id(("browser-header", column_ix))
            .flex()
            .items_center()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .px(px(8.0))
            .text_size(px(11.0))
            .text_color(theme.text_tertiary)
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    if event.click_count == 2 {
                        this.apply_root_override(column_ix, cx);
                    }
                }),
            )
            .child(label)
            .into_any_element()
    }

    fn render_row_element(
        delegate: &D,
        focus: FocusHandle,
        column_ix: usize,
        row_ix: usize,
        item: D::Item,
        is_selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let label = delegate.item_label(&item);
        let is_leaf = delegate.is_leaf(&item);

        let bg: Hsla = if is_selected {
            theme.selection_bg
        } else {
            Hsla::transparent_black()
        };
        let hover_bg = theme.selection_bg;
        let text_color = theme.text_primary;

        let item_left = item.clone();
        let item_right = item.clone();

        let mut row = div()
            .id(SharedString::from(format!(
                "browser-row-{}-{}",
                column_ix, row_ix
            )))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(8.0))
            .text_size(px(12.0))
            .text_color(text_color)
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&focus);
                    this.delegate.set_selection(column_ix, &item_left, cx);
                    this.focused_column = column_ix;
                    if event.click_count == 2 {
                        if this.delegate.is_leaf(&item_left) {
                            this.delegate.on_activate(&item_left, window, cx);
                        } else {
                            // Reroot to the clicked prefix; `column_ix + 1`
                            // so the clicked row itself becomes the new root.
                            this.apply_root_override(column_ix + 1, cx);
                        }
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.delegate
                        .on_context_menu(column_ix, Some(&item_right), event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(label),
            );

        if !is_leaf {
            row = row.child(Icon::ChevronRight.svg(8.0));
        }

        row.into_any_element()
    }

    fn render_list_column(
        &mut self,
        column_ix: usize,
        parent: Option<D::Item>,
        items: Vec<D::Item>,
        selection: &[D::Item],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let width = self.column_widths[column_ix];
        let border = theme.border_primary;
        let bg = theme.bg_primary;
        let header_label = self.delegate.column_label(parent.as_ref());

        let selected_ix = self.selected_ix_in_column(column_ix, selection, &items);
        let count = items.len();
        let items_for_list = items;
        let scroll_handle = self.scroll_handles[column_ix].clone();
        let scrollbar_handle = self.scroll_handles[column_ix].clone();
        let focus = self.focus_handle.clone();

        let view = cx.entity().clone();
        let ix = column_ix;
        let bounds_canvas = gpui::canvas(
            move |bounds, _, cx| {
                view.update(cx, |this, _| {
                    if let Some(slot) = this.column_bounds.get_mut(ix) {
                        *slot = bounds;
                    }
                });
                bounds
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let header = self.render_column_header(column_ix, header_label, cx);
        let resize_handle = self.render_resize_handle(column_ix, cx);

        let list_view = uniform_list(
            ("browser-col", column_ix),
            count,
            cx.processor(
                move |this: &mut Self,
                      range: Range<usize>,
                      _window: &mut Window,
                      cx: &mut Context<Self>| {
                    let mut out = Vec::with_capacity(range.len());
                    for row_ix in range {
                        let item = items_for_list[row_ix].clone();
                        let is_selected = selected_ix == Some(row_ix);
                        out.push(Self::render_row_element(
                            &this.delegate,
                            focus.clone(),
                            column_ix,
                            row_ix,
                            item,
                            is_selected,
                            cx,
                        ));
                    }
                    out
                },
            ),
        )
        .track_scroll(scroll_handle)
        .h_full();

        let scrollbar_overlay = {
            let state = scrollbar_handle.0.borrow();
            let offset = state.base_handle.offset();
            let max_off = state.base_handle.max_offset();
            let max_y = f32::from(max_off.height);
            let scroll_y = f32::from(-offset.y).clamp(0.0, max_y);
            let vp = f32::from(state.base_handle.bounds().size.height);
            let vp = if vp > 0.0 {
                vp
            } else {
                count as f32 * ROW_HEIGHT
            };
            div()
                .absolute()
                .top(px(HEADER_HEIGHT))
                .right_0()
                .bottom_0()
                .left_0()
                .child(Scrollbar::new(Axis::Vertical, vp, vp + max_y, scroll_y))
        };

        let mut col = div()
            .relative()
            .overflow_hidden()
            .w(width)
            .h_full()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.delegate
                        .on_context_menu(column_ix, None, event.position, window, cx);
                }),
            )
            .child(bounds_canvas)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(header)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(list_view),
                    ),
            )
            .child(scrollbar_overlay)
            .child(
                div()
                    .absolute()
                    .right(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .top_0()
                    .bottom_0()
                    .child(resize_handle),
            );
        col.style().flex_shrink = Some(0.0);
        col.into_any_element()
    }

    fn render_detail_column(
        &self,
        header_label: SharedString,
        content: AnyElement,
        cx: &App,
    ) -> AnyElement {
        let theme = theme(cx);
        let header = div()
            .flex()
            .items_center()
            .w_full()
            .h(px(HEADER_HEIGHT))
            .px(px(8.0))
            .text_size(px(11.0))
            .text_color(theme.text_tertiary)
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .child(header_label);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(MIN_DETAIL_WIDTH))
            .h_full()
            .overflow_hidden()
            .bg(theme.bg_primary)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .p(px(8.0))
                    .child(content),
            )
            .into_any_element()
    }
}

impl<D: ColumnBrowserDelegate> Focusable for ColumnBrowser<D> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<D: ColumnBrowserDelegate> Render for ColumnBrowser<D> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        self.delegate.auto_extend_selection(cx);
        let selection = self.delegate.resolve_selection(cx);
        let n = selection.len();

        self.ensure_column_state(n + 2);

        let mut column_elements: Vec<AnyElement> = Vec::with_capacity(n + 2);

        for col_ix in 0..=n {
            let parent: Option<D::Item> = if col_ix == 0 {
                None
            } else {
                selection.get(col_ix - 1).cloned()
            };
            let items = match &parent {
                None => self.delegate.root_items(cx),
                Some(p) => self.delegate.children(p, cx),
            };

            if items.is_empty() {
                continue;
            }

            let col = self.render_list_column(col_ix, parent, items, &selection, cx);
            column_elements.push(col);
        }

        let tail = selection.last().cloned();
        if let Some(detail) = self.delegate.render_detail(tail.as_ref(), window, cx) {
            let label = self.delegate.detail_label(tail.as_ref());
            let col = self.render_detail_column(label, detail, cx);
            column_elements.push(col);
        }

        let hscroll_overlay = {
            let offset = self.hscroll.offset();
            let max_off = self.hscroll.max_offset();
            let vp = f32::from(self.hscroll.bounds().size.width);
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

        let mut strip = div()
            .id("column-browser")
            .track_focus(&self.focus_handle)
            .track_scroll(&self.hscroll)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .flex()
            .flex_row()
            .size_full()
            .overflow_x_scroll()
            .bg(theme.bg_primary)
            .children(column_elements);
        strip.style().restrict_scroll_to_axis = Some(true);

        div()
            .relative()
            .size_full()
            .child(strip)
            .children(hscroll_overlay)
    }
}
