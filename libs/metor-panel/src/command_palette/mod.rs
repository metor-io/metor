use gpui::{
    deferred, div, prelude::*, px, App, ClipboardItem, FocusHandle, Focusable, Hsla, IntoElement,
    KeyDownEvent, SharedString, Window,
};

use crate::theme::DARK;

/// A single selectable item in the palette.
pub struct PaletteItem {
    pub label: SharedString,
    pub action: PaletteAction,
}

/// What happens when a palette item is selected.
pub enum PaletteAction {
    /// Run a one-shot callback. Receives the current filter text.
    Execute(Box<dyn FnOnce(&str, &mut Window, &mut App) + 'static>),
    /// Push a new page onto the palette stack.
    NextPage(Box<dyn FnOnce() -> PalettePage + 'static>),
}

/// A page of items shown in the palette, with optional breadcrumb label and placeholder text.
pub struct PalettePage {
    pub label: Option<SharedString>,
    pub prompt: Option<SharedString>,
    pub items: Vec<PaletteItem>,
    /// An optional default action shown at the bottom and used as fallback
    /// when no items match the filter. Receives the filter text on confirm.
    pub default_action: Option<PaletteItem>,
}

impl PalettePage {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        Self {
            label: None,
            prompt: None,
            items,
            default_action: None,
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
    filter: String,
    /// Cursor byte offset into `filter`.
    cursor: usize,
    /// Selection anchor byte offset. When equal to `cursor`, nothing is selected.
    mark: usize,
    page_stack: Vec<PalettePage>,
    selected_index: usize,
    focus_handle: FocusHandle,
}

impl CommandPalette {
    pub fn new(page: PalettePage, cx: &mut Context<Self>) -> Self {
        Self {
            filter: String::new(),
            cursor: 0,
            mark: 0,
            page_stack: vec![page],
            selected_index: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn current_page(&self) -> Option<&PalettePage> {
        self.page_stack.last()
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let Some(page) = self.current_page() else {
            return vec![];
        };
        let filter = self.filter.to_lowercase();
        if filter.is_empty() {
            return (0..page.items.len()).collect();
        }
        page.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let label = item.label.to_lowercase();
                fuzzy_match(&label, &filter)
            })
            .map(|(i, _)| i)
            .collect()
    }

    // ── Text editing helpers ──────────────────────────────────────────

    fn has_selection(&self) -> bool {
        self.mark != self.cursor
    }

    fn selection_range(&self) -> std::ops::Range<usize> {
        let start = self.mark.min(self.cursor);
        let end = self.mark.max(self.cursor);
        start..end
    }

    fn selected_text(&self) -> &str {
        &self.filter[self.selection_range()]
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.cursor = 0;
        self.mark = 0;
    }

    fn prev_char_boundary(&self, offset: usize) -> usize {
        let mut o = offset.saturating_sub(1);
        while o > 0 && !self.filter.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn next_char_boundary(&self, offset: usize) -> usize {
        let mut o = (offset + 1).min(self.filter.len());
        while o < self.filter.len() && !self.filter.is_char_boundary(o) {
            o += 1;
        }
        o
    }

    fn delete_selection(&mut self) {
        let range = self.selection_range();
        self.filter.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.mark = self.cursor;
    }

    fn insert_text(&mut self, text: &str) {
        if self.has_selection() {
            self.delete_selection();
        }
        self.filter.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.mark = self.cursor;
    }

    fn copy(&self, cx: &mut App) {
        if self.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_string()));
        }
    }

    fn cut(&mut self, cx: &mut App) {
        if self.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_string()));
            self.delete_selection();
            self.selected_index = 0;
        }
    }

    fn paste(&mut self, cx: &mut App) {
        if let Some(item) = cx.read_from_clipboard() {
            if let Some(text) = item.text() {
                let text = text.replace('\n', "");
                self.insert_text(&text);
                self.selected_index = 0;
            }
        }
    }

    // ── Core logic ──────────────────────────────────────────────────

    fn confirm(&mut self, window: &mut Window, cx: &mut App) {
        let indices = self.filtered_indices();
        let filter = self.filter.clone();
        let default_idx = indices.len();

        // Selected a filtered item.
        if let Some(&item_index) = indices.get(self.selected_index) {
            let page = self.page_stack.last_mut().unwrap();
            let action = std::mem::replace(
                &mut page.items[item_index].action,
                PaletteAction::Execute(Box::new(|_, _, _| {})),
            );
            self.run_action(action, filter, window, cx);
            return;
        }

        // Selected the default action (either explicitly or no matches).
        let page = match self.page_stack.last_mut() {
            Some(p) => p,
            None => return,
        };
        let is_default_selected = self.selected_index == default_idx || indices.is_empty();
        if is_default_selected {
            if let Some(default) = page.default_action.take() {
                self.run_action(default.action, filter, window, cx);
            }
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
                f(&filter, window, cx);
            }
            PaletteAction::NextPage(make_page) => {
                let next = make_page();
                self.clear_filter();
                self.selected_index = 0;
                self.page_stack.push(next);
            }
        }
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut App) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // ── Platform (Cmd) shortcuts ────────────────────────────────
        if mods.platform {
            match key {
                "a" => {
                    self.mark = 0;
                    self.cursor = self.filter.len();
                }
                "c" => self.copy(cx),
                "x" => self.cut(cx),
                "v" => self.paste(cx),
                "left" => {
                    if mods.shift {
                        self.cursor = 0;
                    } else {
                        self.cursor = 0;
                        self.mark = 0;
                    }
                }
                "right" => {
                    if mods.shift {
                        self.cursor = self.filter.len();
                    } else {
                        self.cursor = self.filter.len();
                        self.mark = self.cursor;
                    }
                }
                "backspace" => {
                    if self.has_selection() {
                        self.delete_selection();
                    } else if self.cursor > 0 {
                        self.filter.replace_range(0..self.cursor, "");
                        self.cursor = 0;
                        self.mark = 0;
                    }
                    self.selected_index = 0;
                }
                _ => {}
            }
            return;
        }

        // ── Regular keys ────────────────────────────────────────────
        match key {
            "escape" => {
                if self.page_stack.len() > 1 {
                    self.page_stack.pop();
                    self.clear_filter();
                    self.selected_index = 0;
                }
            }
            "backspace" => {
                if self.has_selection() {
                    self.delete_selection();
                    self.selected_index = 0;
                } else if self.cursor > 0 {
                    let prev = self.prev_char_boundary(self.cursor);
                    self.filter.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                    self.mark = self.cursor;
                    self.selected_index = 0;
                } else if self.filter.is_empty() && self.page_stack.len() > 1 {
                    self.page_stack.pop();
                    self.selected_index = 0;
                }
            }
            "delete" => {
                if self.has_selection() {
                    self.delete_selection();
                } else if self.cursor < self.filter.len() {
                    let next = self.next_char_boundary(self.cursor);
                    self.filter.replace_range(self.cursor..next, "");
                }
                self.selected_index = 0;
            }
            "left" => {
                if mods.shift {
                    self.cursor = self.prev_char_boundary(self.cursor);
                } else if self.has_selection() {
                    self.cursor = self.selection_range().start;
                    self.mark = self.cursor;
                } else {
                    self.cursor = self.prev_char_boundary(self.cursor);
                    self.mark = self.cursor;
                }
            }
            "right" => {
                if mods.shift {
                    self.cursor = self.next_char_boundary(self.cursor);
                } else if self.has_selection() {
                    self.cursor = self.selection_range().end;
                    self.mark = self.cursor;
                } else {
                    self.cursor = self.next_char_boundary(self.cursor);
                    self.mark = self.cursor;
                }
            }
            "up" => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
            }
            "down" => {
                let filtered_count = self.filtered_indices().len();
                let has_default = self
                    .current_page()
                    .map_or(false, |p| p.default_action.is_some());
                let total = filtered_count + if has_default { 1 } else { 0 };
                if total > 0 && self.selected_index < total - 1 {
                    self.selected_index += 1;
                }
            }
            "enter" | "return" => {
                self.confirm(window, cx);
            }
            _ => {
                if let Some(ch) = &event.keystroke.key_char {
                    if !mods.control && !mods.alt {
                        self.insert_text(ch);
                        self.selected_index = 0;
                    }
                }
            }
        }
    }

    fn render_breadcrumbs(&self) -> impl IntoElement {
        let mut crumbs = div().flex().flex_row().gap(px(4.0));
        for (i, page) in self.page_stack.iter().enumerate() {
            if let Some(label) = &page.label {
                if i > 0 {
                    crumbs = crumbs.child(
                        div()
                            .text_color(DARK.text_tertiary)
                            .text_size(px(12.0))
                            .child(SharedString::new_static(">")),
                    );
                }
                crumbs = crumbs.child(
                    div()
                        .text_color(DARK.text_secondary)
                        .text_size(px(12.0))
                        .child(label.clone()),
                );
            }
        }
        crumbs
    }

    fn render_input(&self) -> impl IntoElement {
        let prompt = self
            .current_page()
            .and_then(|p| p.prompt.clone())
            .unwrap_or_else(|| SharedString::new_static("Search..."));

        let text_field = if self.filter.is_empty() {
            // Empty: show placeholder with cursor at the left
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(Self::render_cursor())
                .child(
                    div()
                        .text_color(DARK.text_tertiary)
                        .child(prompt),
                )
        } else {
            // Split text around cursor/selection and render spans
            let sel = self.selection_range();
            let cursor_at_start = self.cursor <= self.mark;

            let before = &self.filter[..sel.start];
            let selected = &self.filter[sel.clone()];
            let after = &self.filter[sel.end..];

            let mut row = div().flex().flex_row().items_center();

            if cursor_at_start {
                // Cursor is at left edge of selection
                if !before.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .child(SharedString::from(before.to_string())),
                    );
                }
                row = row.child(Self::render_cursor());
                if !selected.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .bg(SELECTION_BG)
                            .child(SharedString::from(selected.to_string())),
                    );
                }
                if !after.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .child(SharedString::from(after.to_string())),
                    );
                }
            } else {
                // Cursor is at right edge of selection
                if !before.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .child(SharedString::from(before.to_string())),
                    );
                }
                if !selected.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .bg(SELECTION_BG)
                            .child(SharedString::from(selected.to_string())),
                    );
                }
                row = row.child(Self::render_cursor());
                if !after.is_empty() {
                    row = row.child(
                        div()
                            .text_color(DARK.text_primary)
                            .child(SharedString::from(after.to_string())),
                    );
                }
            }

            row
        };

        div()
            .flex()
            .flex_row()
            .px(px(12.0))
            .py(px(8.0))
            .gap(px(8.0))
            .w_full()
            .border_b_1()
            .border_color(DARK.border_primary)
            .child(self.render_breadcrumbs())
            .child(text_field.text_size(px(14.0)).flex_1())
    }

    fn render_cursor() -> impl IntoElement {
        div().w(px(1.0)).h(px(16.0)).bg(DARK.text_primary)
    }

    fn render_items(&self) -> impl IntoElement {
        let indices = self.filtered_indices();
        let page = match self.current_page() {
            Some(p) => p,
            None => return div(),
        };
        let has_default = page.default_action.is_some();
        // The default action occupies the slot right after the filtered items.
        let default_visual_idx = indices.len();

        let mut list = div().flex().flex_col().w_full().py(px(4.0));
        for (visual_idx, &item_idx) in indices.iter().enumerate() {
            let item = &page.items[item_idx];
            let selected = visual_idx == self.selected_index;
            list = list.child(self.render_item_row(&item.label, selected));
        }

        if let Some(default) = &page.default_action {
            let selected = if indices.is_empty() {
                // No matches — default is always selected.
                true
            } else {
                self.selected_index == default_visual_idx
            };
            list = list.child(self.render_item_row(&default.label, selected));
        }

        if indices.is_empty() && !has_default {
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(DARK.text_tertiary)
                    .child(SharedString::new_static("No results")),
            );
        }

        list
    }

    fn render_item_row(&self, label: &SharedString, selected: bool) -> impl IntoElement {
        let bg = if selected {
            ITEM_SELECTED_BG
        } else {
            Hsla::transparent_black()
        };
        div()
            .px(px(12.0))
            .py(px(6.0))
            .w_full()
            .bg(bg)
            .rounded(px(4.0))
            .text_size(px(14.0))
            .text_color(if selected {
                DARK.text_primary
            } else {
                DARK.text_secondary
            })
            .child(label.clone())
    }
}

const SELECTION_BG: Hsla = Hsla {
    h: 0.583,
    s: 0.5,
    l: 0.35,
    a: 0.6,
};

const ITEM_SELECTED_BG: Hsla = Hsla {
    h: 0.083,
    s: 0.10,
    l: 0.20,
    a: 1.0,
};

const PALETTE_BG: Hsla = Hsla {
    h: 0.083,
    s: 0.08,
    l: 0.14,
    a: 1.0,
};

impl Focusable for CommandPalette {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        deferred(
            div()
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
                        .bg(PALETTE_BG)
                        .border_1()
                        .border_color(DARK.border_primary)
                        .rounded(px(8.0))
                        .overflow_hidden()
                        .child(self.render_input())
                        .child(self.render_items()),
                ),
        )
    }
}

/// Simple subsequence fuzzy match. Returns true if all characters of
/// `pattern` appear in `text` in order.
fn fuzzy_match(text: &str, pattern: &str) -> bool {
    let mut text_chars = text.chars();
    for p in pattern.chars() {
        loop {
            match text_chars.next() {
                Some(t) if t == p => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}
