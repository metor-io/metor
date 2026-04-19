/// Unified inspector — the everything-palette and right-click panel.
///
/// Operates on [`InspectorRow`] widgets with a page-stack navigation model.
/// Can render as either an anchored right-click panel or a centered overlay.
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, Corner, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    Pixels, Point, Render, ScrollStrategy, SharedString, UniformListScrollHandle, Window, anchored,
    canvas, deferred, div, prelude::*, px, uniform_list,
};

pub mod edits;
pub mod palette;
pub mod reflect;
pub mod registry;
pub mod rows;
pub mod trace_picker;

use crate::theme::theme;
use rows::{InspectorRow, RowAction, TextField};

const ROW_HEIGHT: f32 = 28.0;

/// Action to open the inspector for an arbitrary entity at a position.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct InspectEntity {
    pub entity: gpui::AnyEntity,
    pub position: Point<Pixels>,
}

/// How the inspector is positioned on screen.
#[derive(Clone, Copy)]
pub enum InspectorMode {
    /// Anchored at a specific point (right-click menu).
    Anchored(Point<Pixels>),
    /// Centered in the window (command palette style).
    Centered,
}

/// Bundles everything needed to open an inspector from any entry point.
pub struct InspectorRequest {
    pub rows: Vec<Box<dyn InspectorRow>>,
    pub mode: InspectorMode,
}

/// Callback that opens an inspector.
pub type OpenInspectorCallback = Arc<dyn Fn(InspectorRequest, &mut Window, &mut App) + 'static>;

/// One page in the inspector's navigation stack.
struct InspectorPage {
    rows: Vec<Box<dyn InspectorRow>>,
    label: Option<SharedString>,
}

/// Unified inspector panel with page-stack navigation.
pub struct Inspector {
    pages: Vec<InspectorPage>,
    mode: InspectorMode,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    search: TextField,
    selected_index: usize,
    editing: Option<EditState>,
    panel_bounds: Option<Bounds<Pixels>>,
    scroll_handle: UniformListScrollHandle,
}

struct EditState {
    row_index: usize,
    field: TextField,
    on_commit: Option<Box<dyn FnOnce(String, &mut Window, &mut App)>>,
}

impl Inspector {
    pub fn new(
        rows: Vec<Box<dyn InspectorRow>>,
        mode: InspectorMode,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            pages: vec![InspectorPage { rows, label: None }],
            mode,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            search: TextField::new("Search...", cx),
            selected_index: 0,
            editing: None,
            panel_bounds: None,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    fn current_page(&self) -> &InspectorPage {
        self.pages.last().expect("page stack must never be empty")
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    fn push_page(
        &mut self,
        label: Option<SharedString>,
        rows: Vec<Box<dyn InspectorRow>>,
        cx: &mut Context<Self>,
    ) {
        // Set label on current page (becomes a breadcrumb pill)
        if let Some(current) = self.pages.last_mut() {
            if current.label.is_none() {
                current.label = label;
            }
        }
        self.pages.push(InspectorPage { rows, label: None });
        self.search.clear();
        self.selected_index = 0;
        cx.notify();
    }

    fn pop_page(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pages.len() > 1 {
            self.pages.pop();
            if let Some(current) = self.pages.last_mut() {
                current.label = None;
            }
            self.search.clear();
            self.selected_index = 0;
            cx.notify();
            true
        } else {
            false
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let page = self.current_page();
        let filter = &self.search.text;
        if filter.is_empty() {
            return (0..page.rows.len()).collect();
        }

        use nucleo_matcher::{
            Matcher,
            pattern::{CaseMatching, Normalization, Pattern},
        };

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(usize, u32)> = page
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let mut buf = Vec::new();
                let haystack = nucleo_matcher::Utf32Str::new(row.label(), &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some((i, score))
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    fn visible_count(&self) -> usize {
        self.filtered_indices().len()
    }

    fn activate_row(&mut self, row_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.pages.last_mut().expect("page stack empty");
        let action = page.rows[row_idx].activate(window, cx);
        self.handle_action(action, row_idx, window, cx);
    }

    fn handle_action(
        &mut self,
        action: RowAction,
        row_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RowAction::Handled => {
                cx.notify();
            }
            RowAction::Cascade(child_rows) => {
                let label = self
                    .current_page()
                    .rows
                    .get(row_idx)
                    .map(|r| SharedString::from(r.label().to_string()));
                self.push_page(label, child_rows, cx);
            }
            RowAction::Dismiss => {
                self.dismiss(window);
            }
            RowAction::StartEdit {
                current_text,
                on_commit,
            } => {
                let mut edit_field = TextField::new("", cx);
                edit_field.text = current_text;
                edit_field.cursor = edit_field.text.len();
                edit_field.mark = edit_field.cursor;
                self.editing = Some(EditState {
                    row_index: row_idx,
                    field: edit_field,
                    on_commit: Some(on_commit),
                });
                cx.notify();
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let indices = self.filtered_indices();
        let Some(&row_idx) = indices.get(self.selected_index) else {
            return;
        };
        self.activate_row(row_idx, window, cx);
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(mut edit) = self.editing.take() {
            if let Some(on_commit) = edit.on_commit.take() {
                on_commit(edit.field.text.clone(), window, cx);
            }
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();

        if self.editing.is_some() {
            match key {
                "escape" => {
                    self.editing = None;
                    cx.notify();
                    return;
                }
                "enter" | "return" => {
                    self.commit_edit(window, cx);
                    cx.notify();
                    return;
                }
                _ => {
                    if let Some(edit) = &mut self.editing {
                        edit.field.handle_key_down(event, cx);
                    }
                    cx.notify();
                    return;
                }
            }
        }

        match key {
            "escape" => {
                if !self.pop_page(cx) {
                    self.dismiss(window);
                }
                cx.notify();
                return;
            }
            "backspace" => {
                if self.search.text.is_empty() && self.pages.len() > 1 {
                    self.pop_page(cx);
                    return;
                }
            }
            "up" => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.scroll_handle
                        .scroll_to_item(self.selected_index, ScrollStrategy::Top);
                }
                cx.notify();
                return;
            }
            "down" => {
                let total = self.visible_count();
                if total > 0 && self.selected_index < total - 1 {
                    self.selected_index += 1;
                    self.scroll_handle
                        .scroll_to_item(self.selected_index, ScrollStrategy::Bottom);
                }
                cx.notify();
                return;
            }
            "enter" | "return" => {
                self.confirm(window, cx);
                cx.notify();
                return;
            }
            _ => {}
        }

        if self.search.handle_key_down(event, cx) {
            self.selected_index = 0;
        }
    }

    fn render_input_bar(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0));

        // Breadcrumb pills for stacked pages
        for page in &self.pages[..self.pages.len().saturating_sub(1)] {
            if let Some(label) = &page.label {
                bar = bar.child(
                    div()
                        .px(px(6.0))
                        .py(px(1.0))
                        .mr(px(4.0))
                        .bg(theme.pill_bg)
                        .border_1()
                        .border_color(theme.pill_border)
                        .rounded(px(3.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_secondary)
                        .child(label.clone()),
                );
            }
        }

        bar = bar.child(div().flex_1().min_w(px(60.0)).child(self.search.element()));

        bar
    }

    fn render_panel(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);
        let indices = self.filtered_indices();

        let view = cx.entity().clone();
        let bounds_tracker = canvas(
            move |bounds, _window, cx| {
                view.update(cx, |this, _| {
                    this.panel_bounds = Some(bounds);
                });
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let items_element: AnyElement = if indices.is_empty() {
            div()
                .px(px(12.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::new_static("No results"))
                .into_any_element()
        } else {
            let count = indices.len();
            let scroll_handle = self.scroll_handle.clone();
            let max_items_h = 360.0_f32;
            let items_h = (count as f32 * ROW_HEIGHT).min(max_items_h);
            uniform_list(
                "inspector-items",
                count,
                cx.processor(
                    move |this: &mut Self,
                          range: Range<usize>,
                          window: &mut Window,
                          cx: &mut Context<Self>| {
                        let theme = crate::theme::theme(cx);
                        let indices = this.filtered_indices();
                        let mut out = Vec::with_capacity(range.len());
                        for vis_ix in range {
                            let row_idx = indices[vis_ix];
                            let selected = vis_ix == this.selected_index;
                            let is_editing = this
                                .editing
                                .as_ref()
                                .is_some_and(|e| e.row_index == row_idx);

                            let element = if is_editing {
                                let edit = this.editing.as_ref().unwrap();
                                let page = this.current_page();
                                let label =
                                    SharedString::from(page.rows[row_idx].label().to_string());
                                crate::inspector::rows::row_base(vis_ix, selected, cx)
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.text_primary)
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(60.0))
                                            .max_w(px(120.0))
                                            .child(edit.field.element()),
                                    )
                                    .into_any_element()
                            } else {
                                let page = this.current_page();
                                let row_element =
                                    page.rows[row_idx].render_row(vis_ix, selected, window, cx);
                                let row_idx_click = row_idx;
                                div()
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.selected_index = vis_ix;
                                            this.activate_row(row_idx_click, window, cx);
                                        }),
                                    )
                                    .child(row_element)
                                    .into_any_element()
                            };

                            out.push(element);
                        }
                        out
                    },
                ),
            )
            .track_scroll(scroll_handle)
            .h(px(items_h))
            .into_any_element()
        };

        let width = match self.mode {
            InspectorMode::Anchored(_) => px(280.0),
            InspectorMode::Centered => px(500.0),
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .id("inspector-panel")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
                cx.notify();
            }))
            .w(width)
            .max_h(px(400.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .child(bounds_tracker)
            .child(self.render_input_bar(cx))
            .child(items_element)
    }
}

impl Focusable for Inspector {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Inspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }

        let panel = self.render_panel(window, cx);

        let panel_with_dismiss =
            panel.on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, window, _cx| {
                this.dismiss(window);
            }));

        match self.mode {
            InspectorMode::Anchored(position) => {
                let anchored_panel = anchored()
                    .position(position)
                    .anchor(Corner::TopLeft)
                    .snap_to_window_with_margin(px(8.0))
                    .child(panel_with_dismiss);

                let overlay = div()
                    .id("inspector-overlay")
                    .occlude()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .child(anchored_panel)
                    .shadow_sm();

                deferred(overlay).with_priority(1).into_any_element()
            }
            InspectorMode::Centered => {
                let centered = div()
                    .id("inspector-centered")
                    .occlude()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(80.0))
                    .child(panel_with_dismiss)
                    .shadow_sm();

                deferred(centered).with_priority(1).into_any_element()
            }
        }
    }
}
