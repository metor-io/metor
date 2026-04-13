/// Reusable inspector container for the right-click property panel.
///
/// Operates on [`InspectorRow`] widgets — does not know about specific
/// field types. Handles layout, fuzzy search, keyboard navigation,
/// cascading children, and inline text editing.
use std::sync::Arc;

use gpui::{
    anchored, canvas, deferred, div, point, prelude::*, px, App, Bounds, Context, Corner, Entity,
    FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent, Pixels, Point, Render, SharedString,
    Window,
};

use crate::command_palette::{PalettePage, TextField};
use crate::theme::theme;
use crate::widgets::{InspectorRow, RowAction};

/// Searchable right-click inspector panel with cascading submenus.
///
/// Constructed with a `Vec<Box<dyn InspectorRow>>` — each row renders
/// and handles activation independently. The container provides the
/// shell: search, keyboard nav, cascade management, inline editing.
pub struct Inspector {
    rows: Vec<Box<dyn InspectorRow>>,
    position: Point<Pixels>,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    search: TextField,
    selected_index: usize,
    editing: Option<EditState>,
    child: Option<Entity<Inspector>>,
    panel_bounds: Option<Bounds<Pixels>>,
    is_root: bool,
    on_open_palette: Option<Arc<dyn Fn(PalettePage, &mut Window, &mut App)>>,
}

struct EditState {
    row_index: usize,
    field: TextField,
    on_commit: Option<Box<dyn FnOnce(String, &mut Window, &mut App)>>,
}

impl Inspector {
    pub fn new(
        rows: Vec<Box<dyn InspectorRow>>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            rows,
            position,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            search: TextField::new("Search...", cx),
            selected_index: 0,
            editing: None,
            child: None,
            panel_bounds: None,
            is_root: true,
            on_open_palette: None,
        }
    }

    fn new_child(
        rows: Vec<Box<dyn InspectorRow>>,
        position: Point<Pixels>,
        on_open_palette: Option<Arc<dyn Fn(PalettePage, &mut Window, &mut App)>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            rows,
            position,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            search: TextField::new("Search...", cx),
            selected_index: 0,
            editing: None,
            child: None,
            panel_bounds: None,
            is_root: false,
            on_open_palette,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    pub fn set_on_open_palette(
        &mut self,
        cb: impl Fn(PalettePage, &mut Window, &mut App) + 'static,
    ) {
        self.on_open_palette = Some(Arc::new(cb));
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.child = None;
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    fn child_position(&self) -> Point<Pixels> {
        self.panel_bounds
            .map(|b| {
                let row_y = px(self.selected_index as f32 * 26.0);
                point(b.origin.x + b.size.width, b.origin.y + row_y)
            })
            .unwrap_or(self.position)
    }

    fn open_cascade(
        &mut self,
        child_rows: Vec<Box<dyn InspectorRow>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let child_pos = self.child_position();
        let on_open_palette = self.on_open_palette.clone();
        let parent_focus = self.focus_handle.clone();
        let child = cx.new(|cx| {
            let mut c = Self::new_child(child_rows, child_pos, on_open_palette, cx);
            c.set_parent_focus(parent_focus);
            c
        });
        child.focus_handle(cx).focus(window);
        self.child = Some(child);
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let filter = &self.search.text;
        if filter.is_empty() {
            return (0..self.rows.len()).collect();
        }

        use nucleo_matcher::{
            Matcher,
            pattern::{CaseMatching, Normalization, Pattern},
        };

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(usize, u32)> = self
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

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let indices = self.filtered_indices();
        let Some(&row_idx) = indices.get(self.selected_index) else {
            return;
        };
        let action = self.rows[row_idx].activate(window, cx);
        match action {
            RowAction::Handled => {
                cx.notify();
            }
            RowAction::Cascade(child_rows) => {
                self.open_cascade(child_rows, window, cx);
            }
            RowAction::OpenPalette(page) => {
                if let Some(cb) = &self.on_open_palette {
                    cb(page, window, cx);
                }
                self.dismiss(window);
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
                if self.child.is_some() {
                    self.child = None;
                } else {
                    self.dismiss(window);
                }
                cx.notify();
                return;
            }
            "up" => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                cx.notify();
                return;
            }
            "down" => {
                let total = self.visible_count();
                if total > 0 && self.selected_index < total - 1 {
                    self.selected_index += 1;
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

    fn render_search_bar(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(3.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(60.0))
                    .child(self.search.element()),
            )
    }

    fn render_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
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

        let mut items_col = div().flex().flex_col().py(px(4.0));

        if indices.is_empty() {
            items_col = items_col.child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No results")),
            );
        } else {
            for (vis_ix, &row_idx) in indices.iter().enumerate() {
                let selected = vis_ix == self.selected_index;
                let is_editing = self
                    .editing
                    .as_ref()
                    .is_some_and(|e| e.row_index == row_idx);

                if is_editing {
                    let edit = self.editing.as_ref().unwrap();
                    let label = SharedString::from(self.rows[row_idx].label().to_string());
                    items_col = items_col.child(
                        crate::widgets::row_base(vis_ix, selected, cx)
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
                            ),
                    );
                } else {
                    let row_element =
                        self.rows[row_idx].render_row(vis_ix, selected, window, cx);
                    let row_idx_click = row_idx;
                    items_col = items_col.child(
                        div()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.selected_index = vis_ix;
                                    let action = this.rows[row_idx_click].activate(window, cx);
                                    match action {
                                        RowAction::Handled => cx.notify(),
                                        RowAction::Cascade(rows) => {
                                            this.open_cascade(rows, window, cx);
                                        }
                                        RowAction::OpenPalette(page) => {
                                            if let Some(cb) = &this.on_open_palette {
                                                cb(page, window, cx);
                                            }
                                            this.dismiss(window);
                                        }
                                        RowAction::Dismiss => this.dismiss(window),
                                        RowAction::StartEdit {
                                            current_text,
                                            on_commit,
                                        } => {
                                            let mut edit_field = TextField::new("", cx);
                                            edit_field.text = current_text;
                                            edit_field.cursor = edit_field.text.len();
                                            edit_field.mark = edit_field.cursor;
                                            this.editing = Some(EditState {
                                                row_index: row_idx_click,
                                                field: edit_field,
                                                on_commit: Some(on_commit),
                                            });
                                            cx.notify();
                                        }
                                    }
                                }),
                            )
                            .child(row_element),
                    );
                }
            }
        }

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
            .w(px(280.0))
            .max_h(px(400.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .overflow_y_scroll()
            .child(bounds_tracker)
            .child(self.render_search_bar(cx))
            .child(items_col)
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

        if let Some(child) = &self.child {
            if child.read(cx).dismissed {
                self.child = None;
                self.focus_handle.focus(window);
            }
        }

        let position = self.position;
        let panel = self.render_panel(window, cx);

        let anchored_panel = anchored()
            .position(position)
            .anchor(Corner::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(
                panel.on_mouse_down_out(cx.listener(
                    |this, _: &gpui::MouseDownEvent, window, _cx| {
                        if this.child.is_none() {
                            this.dismiss(window);
                        }
                    },
                )),
            );

        if self.is_root {
            let mut overlay = div()
                .id("inspector-overlay")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(anchored_panel);

            if let Some(child) = &self.child {
                overlay = overlay.child(child.clone());
            }

            deferred(overlay).with_priority(1).into_any_element()
        } else {
            let mut container = div().child(anchored_panel);
            if let Some(child) = &self.child {
                container = container.child(child.clone());
            }
            container.into_any_element()
        }
    }
}
