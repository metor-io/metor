/// Unified inspector and command palette.
///
/// Operates on [`InspectorRow`] widgets with a page-stack navigation model.
/// Can render as either an anchored right-click panel or a centered overlay.
use std::sync::Arc;

use gpui::{
    anchored, canvas, deferred, div, prelude::*, px, App, Bounds, Context, Corner, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, Pixels, Point, Render, SharedString, Window,
};

use crate::command_palette::{PaletteAction, PalettePage};
use crate::theme::theme;
use crate::widgets::{CommandRow, InspectorRow, NavRow, RowAction, TextField};

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
pub type OpenInspectorCallback =
    Arc<dyn Fn(InspectorRequest, &mut Window, &mut App) + 'static>;

/// One page in the inspector's navigation stack.
struct InspectorPage {
    rows: Vec<Box<dyn InspectorRow>>,
    label: Option<SharedString>,
}

/// Unified inspector panel with page-stack navigation.
///
/// Replaces both `PropertyInspector` and `CommandPalette`. Supports:
/// - Fuzzy search
/// - Keyboard navigation (up/down/enter/escape)
/// - Page stack with breadcrumb pills (cascade pushes a page)
/// - Inline text editing
/// - Anchored (right-click) or centered (Cmd-P) positioning
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
        let page = self.pages.last().expect("page stack empty");
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

    fn render_input_bar(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(3.0))
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

        bar = bar.child(
            div()
                .flex_1()
                .min_w(px(60.0))
                .child(self.search.element()),
        );

        bar
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
                    let page = self.current_page();
                    let label = SharedString::from(page.rows[row_idx].label().to_string());
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
                    let page = self.current_page();
                    let row_element =
                        page.rows[row_idx].render_row(vis_ix, selected, window, cx);
                    let row_idx_click = row_idx;
                    items_col = items_col.child(
                        div()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.selected_index = vis_ix;
                                    this.activate_row(row_idx_click, window, cx);
                                }),
                            )
                            .child(row_element),
                    );
                }
            }
        }

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
            .overflow_y_scroll()
            .child(bounds_tracker)
            .child(self.render_input_bar(cx))
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

        let panel = self.render_panel(window, cx);

        let panel_with_dismiss = panel.on_mouse_down_out(cx.listener(
            |this, _: &gpui::MouseDownEvent, window, _cx| {
                this.dismiss(window);
            },
        ));

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
                    .child(anchored_panel);

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
                    .child(panel_with_dismiss);

                deferred(centered).with_priority(1).into_any_element()
            }
        }
    }
}

/// A row that prompts for text input via inline editing, then forwards
/// the committed text to a callback. Used for `PalettePage::default_action`.
struct DefaultActionRow {
    label: SharedString,
    callback: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl InspectorRow for DefaultActionRow {
    fn label(&self) -> &str {
        &self.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let theme = theme(cx);
        crate::widgets::row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .into_any_element()
    }

    fn activate(&self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let cb = self.callback.clone();
        RowAction::StartEdit {
            current_text: String::new(),
            on_commit: Box::new(move |text, w, cx| {
                cb(text, w, cx);
            }),
        }
    }
}

/// Wrap inspector rows in a [`PalettePage`] so they can be opened via
/// the `on_open_page` callback chain. Each row becomes a `PaletteItem`
/// whose `Execute` action is a no-op — the real behaviour comes from
/// `palette_page_to_rows` converting back on the receiving end.
///
/// This is intentionally a thin wrapper: the page's items list is empty
/// and the rows are stashed in a `NextPage` so that `palette_page_to_rows`
/// can recover them.
pub fn inspector_rows_to_page(rows: Vec<Box<dyn InspectorRow>>) -> PalettePage {
    let items: Vec<crate::command_palette::PaletteItem> = rows
        .into_iter()
        .map(|row| {
            let label = SharedString::from(row.label().to_string());
            let row = std::sync::Mutex::new(Some(row));
            crate::command_palette::PaletteItem::new(
                label,
                PaletteAction::Execute(Box::new(move |_, window, cx| {
                    if let Some(row) = row.lock().unwrap().as_ref() {
                        row.activate(window, cx);
                    }
                })),
            )
        })
        .collect();
    PalettePage::new(items)
}

/// Convert a [`PalettePage`] into inspector rows.
///
/// This bridges the old CommandPalette page model to the new unified
/// Inspector. `PaletteAction::Execute` becomes a `CommandRow`,
/// `PaletteAction::NextPage` becomes a `NavRow` that cascades.
pub fn palette_page_to_rows(page: PalettePage) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = page.items
        .into_iter()
        .map(|item| {
            let label = item.label;
            match item.action {
                PaletteAction::Execute(cb) => {
                    let cb = std::sync::Mutex::new(Some(cb));
                    Box::new(CommandRow {
                        label,
                        callback: Arc::new(move |w, cx| {
                            if let Some(cb) = cb.lock().unwrap().take() {
                                cb("", w, cx);
                            }
                        }),
                    }) as Box<dyn InspectorRow>
                }
                PaletteAction::NextPage { label: page_label, page: page_fn } => {
                    let page_fn = std::sync::Mutex::new(Some(page_fn));
                    Box::new(NavRow {
                        label,
                        summary: page_label.unwrap_or_default(),
                        build_children: Arc::new(move |_cx| {
                            if let Some(page_fn) = page_fn.lock().unwrap().take() {
                                let page = page_fn();
                                palette_page_to_rows(page)
                            } else {
                                vec![]
                            }
                        }),
                    }) as Box<dyn InspectorRow>
                }
            }
        })
        .collect();

    // Convert default_action into a row that starts inline text editing.
    // The committed text is forwarded to the action's callback.
    if let Some(default) = page.default_action {
        let label = default.label;
        match default.action {
            PaletteAction::Execute(cb) => {
                let cb = std::sync::Mutex::new(Some(cb));
                rows.push(Box::new(DefaultActionRow {
                    label,
                    callback: Arc::new(move |text, w, cx| {
                        if let Some(cb) = cb.lock().unwrap().take() {
                            cb(&text, w, cx);
                        }
                    }),
                }));
            }
            _ => {}
        }
    }

    rows
}
