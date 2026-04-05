use gpui::{
    App, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent, ListAlignment, ListState,
    SharedString, Window, deferred, div, list, prelude::*, px,
};

use crate::theme::DARK;

mod text_field;
pub use text_field::TextField;

/// A single selectable item in the palette.
pub struct PaletteItem {
    pub label: SharedString,
    pub action: PaletteAction,
    /// Optional pill chips rendered below the label.
    pub pills: Vec<SharedString>,
}

impl PaletteItem {
    pub fn new(label: impl Into<SharedString>, action: PaletteAction) -> Self {
        Self {
            label: label.into(),
            action,
            pills: Vec::new(),
        }
    }

    pub fn with_pills(mut self, pills: Vec<SharedString>) -> Self {
        self.pills = pills;
        self
    }
}

/// What happens when a palette item is selected.
pub enum PaletteAction {
    /// Run a one-shot callback. Receives the current filter text.
    Execute(Box<dyn FnOnce(&str, &mut Window, &mut App) + 'static>),
    /// Push a new page onto the palette stack. `label` is set on the
    /// *current* page so it renders as a pill chip in the input bar.
    NextPage {
        label: Option<SharedString>,
        page: Box<dyn FnOnce() -> PalettePage + 'static>,
    },
}

/// A page of items shown in the palette, with optional breadcrumb label and placeholder text.
pub struct PalettePage {
    pub label: Option<SharedString>,
    pub prompt: Option<SharedString>,
    pub items: Vec<PaletteItem>,
    /// An optional default action shown at the bottom and used as fallback
    /// when no items match the filter. Receives the filter text on confirm.
    pub default_action: Option<PaletteItem>,
    /// Pre-populated pages to insert into the stack before this page.
    /// Each page has a label (shown as a pill) and full items (functional when popped to).
    pub prepopulated_pills_pages: Vec<PalettePage>,
}

impl PalettePage {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        Self {
            label: None,
            prompt: None,
            items,
            default_action: None,
            prepopulated_pills_pages: Vec::new(),
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn prompt(mut self, prompt: impl Into<SharedString>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    pub fn default_action(mut self, item: PaletteItem) -> Self {
        self.default_action = Some(item);
        self
    }
}


/// The command palette view.
pub struct CommandPalette {
    text_field: TextField,
    page_stack: Vec<PalettePage>,
    selected_index: usize,
    list_state: ListState,
    last_list_count: usize,
    focus_handle: FocusHandle,
    /// Focus handle to return focus to when the palette is dismissed.
    parent_focus: Option<FocusHandle>,
    /// Set to true after an Execute action runs or Escape on root page.
    pub dismissed: bool,
}

impl CommandPalette {
    pub fn new(page: PalettePage, cx: &mut Context<Self>) -> Self {
        let prompt = page
            .prompt
            .clone()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Search...".to_string());
        Self {
            text_field: TextField::new(prompt),
            page_stack: vec![page],
            selected_index: 0,
            list_state: ListState::new(0, ListAlignment::Top, px(100.0)),
            last_list_count: 0,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
        }
    }

    /// Push additional pages onto the stack (for pre-populating selections as pills).
    pub fn push_pages(&mut self, pages: Vec<PalettePage>) {
        for page in pages {
            let prompt = page
                .prompt
                .clone()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Search...".to_string());
            self.text_field.clear();
            self.text_field.set_placeholder(prompt);
            self.page_stack.push(page);
        }
        self.selected_index = 0;
    }

    /// Set the focus handle to return focus to when the palette is dismissed.
    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    fn current_page(&self) -> Option<&PalettePage> {
        self.page_stack.last()
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let Some(page) = self.current_page() else {
            return vec![];
        };
        let filter = &self.text_field.text;
        if filter.is_empty() {
            return (0..page.items.len()).collect();
        }

        use nucleo_matcher::{
            Matcher,
            pattern::{CaseMatching, Normalization, Pattern},
        };

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(usize, u32)> = page
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let mut buf = Vec::new();
                let haystack =
                    nucleo_matcher::Utf32Str::new(&item.label, &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some((i, score))
            })
            .collect();

        // Sort by score descending (best matches first)
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut App) {
        let filter = self.text_field.text.clone();

        match self.row_to_item(self.selected_index) {
            None => {
                // Default action selected
                let page = match self.page_stack.last_mut() {
                    Some(p) => p,
                    None => return,
                };
                if let Some(default) = page.default_action.take() {
                    self.run_action(default.action, filter, window, cx);
                }
            }
            Some(item_index) => {
                let page = self.page_stack.last_mut().unwrap();
                let action = std::mem::replace(
                    &mut page.items[item_index].action,
                    PaletteAction::Execute(Box::new(|_, _, _| {})),
                );
                self.run_action(action, filter, window, cx);
            }
        }
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    fn run_action(
        &mut self,
        action: PaletteAction,
        filter: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        match action {
            PaletteAction::Execute(f) => {
                self.dismiss(window);
                f(&filter, window, cx);
            }
            PaletteAction::NextPage { label, page } => {
                // Set the label on the current page (it becomes a pill)
                if let Some(current) = self.page_stack.last_mut() {
                    if label.is_some() {
                        current.label = label;
                    }
                }
                let mut next = page();

                // Insert pre-populated pages (with items) before the active page
                let prepopulated = std::mem::take(&mut next.prepopulated_pills_pages);
                for pre_page in prepopulated {
                    self.page_stack.push(pre_page);
                }

                let prompt = next
                    .prompt
                    .clone()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Search...".to_string());
                self.text_field.clear();
                self.text_field.set_placeholder(prompt);
                self.selected_index = 0;
                self.page_stack.push(next);
            }
        }
    }

    fn pop_page(&mut self) {
        if self.page_stack.len() > 1 {
            self.page_stack.pop();
            // Clear the label on the revealed page — the selection it
            // represented was just removed by popping.
            if let Some(page) = self.page_stack.last_mut() {
                page.label = None;
            }
            self.text_field.clear();
            if let Some(page) = self.current_page() {
                let prompt = page
                    .prompt
                    .clone()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Search...".to_string());
                self.text_field.set_placeholder(prompt);
            }
            self.selected_index = 0;
        }
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut App) {
        let key = event.keystroke.key.as_str();


        match key {
            "escape" => {
                self.dismiss(window);
                return;
            }
            "up" => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.list_state.scroll_to_reveal_item(self.selected_index);
                }
                return;
            }
            "down" => {
                let total = self.visible_row_count();
                if total > 0 && self.selected_index < total - 1 {
                    self.selected_index += 1;
                    self.list_state.scroll_to_reveal_item(self.selected_index);
                }
                return;
            }
            "enter" | "return" => {
                self.confirm(window, cx);
                return;
            }
            _ => {}
        }


        let handled = self.text_field.handle_key_down(event, cx);

        if !handled && key == "backspace" {
            // Backspace on empty text — pop page (removes a pill)
            if self.text_field.text.is_empty() && self.page_stack.len() > 1 {
                self.pop_page();
            }
        } else if handled {
            self.selected_index = 0;
            self.list_state.scroll_to_reveal_item(0);
        }
    }

    fn render_input(&self) -> impl IntoElement {
        let len = self.page_stack.len();

        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .px(px(12.0))
            .py(px(8.0))
            .gap(px(4.0))
            .w_full()
            .border_b_1()
            .border_color(DARK.border_primary);

        // Back arrow pill if deeper than root
        if len > 1 {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .px(px(6.0))
                    .py(px(2.0))
                    .bg(DARK.pill_bg)
                    .border_1()
                    .border_color(DARK.pill_border)
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .text_color(DARK.text_secondary)
                    .child(SharedString::new_static("←")),
            );
        }

        // All pages except the last render as pill chips
        for page in &self.page_stack[..len.saturating_sub(1)] {
            if let Some(label) = &page.label {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.0))
                        .py(px(2.0))
                        .bg(DARK.pill_bg)
                        .border_1()
                        .border_color(DARK.pill_border)
                        .rounded(px(4.0))
                        .text_size(px(12.0))
                        .text_color(DARK.text_primary)
                        .child(label.clone()),
                );
            }
        }

        // Text field fills remaining space
        row = row.child(
            div()
                .flex_1()
                .min_w(px(100.0))
                .child(self.text_field.element()),
        );

        row
    }

    /// Total number of visible rows: default action (if any) + filtered items.
    fn visible_row_count(&self) -> usize {
        let indices = self.filtered_indices();
        let has_default = self
            .current_page()
            .map_or(false, |p| p.default_action.is_some());
        let offset = if has_default { 1 } else { 0 };
        offset + indices.len()
    }

    /// Map a visual row index to either the default action or a page item index.
    /// Returns `None` for default action, `Some(item_idx)` for page items.
    fn row_to_item(&self, row: usize) -> Option<usize> {
        let has_default = self
            .current_page()
            .map_or(false, |p| p.default_action.is_some());
        if has_default {
            if row == 0 {
                return None; // default action
            }
            self.filtered_indices().get(row - 1).copied()
        } else {
            self.filtered_indices().get(row).copied()
        }
    }

    fn render_items(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.current_page() {
            Some(p) => p,
            None => return div().into_any_element(),
        };

        let total = self.visible_row_count();
        if total == 0 {
            return div()
                .py(px(4.0))
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(DARK.text_tertiary)
                        .child(SharedString::new_static("No results")),
                )
                .into_any_element();
        }

        // Build row data for the list
        let has_default = page.default_action.is_some();
        let indices = self.filtered_indices();
        let selected_index = self.selected_index;

        let mut rows: Vec<(SharedString, bool, Vec<SharedString>)> = Vec::with_capacity(total);

        if let Some(default) = &page.default_action {
            rows.push((
                default.label.clone(),
                selected_index == 0,
                default.pills.clone(),
            ));
        }

        for (i, &item_idx) in indices.iter().enumerate() {
            let visual = if has_default { i + 1 } else { i };
            rows.push((
                page.items[item_idx].label.clone(),
                visual == selected_index,
                page.items[item_idx].pills.clone(),
            ));
        }

        if total != self.last_list_count {
            self.list_state.reset(total);
            self.last_list_count = total;
        }

        let this = cx.entity().downgrade();
        list(self.list_state.clone(), move |i, _window, _cx| {
            let (ref label, selected, ref pills) = rows[i];
            let this = this.clone();
            Self::render_item_row(label.clone(), selected, pills, i, this).into_any_element()
        })
        .flex_1()
        .py(px(4.0))
        .into_any_element()
    }

    fn render_item_row(
        label: SharedString,
        selected: bool,
        pills: &[SharedString],
        row_index: usize,
        this: gpui::WeakEntity<Self>,
    ) -> impl IntoElement {
        let bg = if selected {
            DARK.selection_bg
        } else {
            Hsla::transparent_black()
        };
        let text_color = if selected {
            DARK.text_primary
        } else {
            DARK.text_secondary
        };

        let mut row = div()
            .id(("palette-row", row_index))
            .px(px(12.0))
            .py(px(6.0))
            .w_full()
            .bg(bg)
            .cursor_pointer()
            .hover(|s| s.bg(DARK.selection_bg))
            .text_size(px(14.0))
            .text_color(text_color)
            .on_mouse_down(gpui::MouseButton::Left, move |_event, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.selected_index = row_index;
                    this.confirm(window, cx);
                    cx.notify();
                });
            })
            .child(label);

        if !pills.is_empty() {
            let mut pill_row = div().flex().flex_row().flex_wrap().gap(px(4.0)).pt(px(4.0));
            for pill in pills {
                pill_row = pill_row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(6.0))
                        .py(px(1.0))
                        .bg(DARK.pill_bg)
                        .border_1()
                        .border_color(DARK.pill_border)
                        .rounded(px(4.0))
                        .text_size(px(11.0))
                        .text_color(DARK.text_primary)
                        .child(pill.clone()),
                );
            }
            row = row.child(pill_row);
        }

        row
    }
}


impl Focusable for CommandPalette {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }

        deferred(
            div()
                .id("command-palette-overlay")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .pt(px(80.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .id("command-palette")
                        .track_focus(&self.focus_handle)
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            this.handle_key_down(event, window, cx);
                            cx.notify();
                        }))
                        .w(px(500.0))
                        .max_h(px(400.0))
                        .bg(DARK.bg_elevated)
                        .border_1()
                        .border_color(DARK.border_primary)
                        .rounded(px(8.0))
                        .overflow_hidden()
                        .child(self.render_input())
                        .child(self.render_items(cx)),
                ),
        )
        .into_any_element()
    }
}

