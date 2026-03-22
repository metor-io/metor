use gpui::{
    deferred, div, prelude::*, px, App, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent,
    SharedString, Window,
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
    page_stack: Vec<PalettePage>,
    selected_index: usize,
    focus_handle: FocusHandle,
}

impl CommandPalette {
    pub fn new(page: PalettePage, cx: &mut Context<Self>) -> Self {
        Self {
            filter: String::new(),
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
                self.filter.clear();
                self.selected_index = 0;
                self.page_stack.push(next);
            }
        }
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut App) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                if self.page_stack.len() > 1 {
                    self.page_stack.pop();
                    self.filter.clear();
                    self.selected_index = 0;
                }
            }
            "backspace" => {
                if self.filter.is_empty() && self.page_stack.len() > 1 {
                    self.page_stack.pop();
                    self.selected_index = 0;
                } else {
                    self.filter.pop();
                    self.selected_index = 0;
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
                    if !event.keystroke.modifiers.platform
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                    {
                        self.filter.push_str(ch);
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

        let display_text = if self.filter.is_empty() {
            div().text_color(DARK.text_tertiary).child(prompt)
        } else {
            div()
                .text_color(DARK.text_primary)
                .child(SharedString::from(self.filter.clone()))
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
            .child(display_text.text_size(px(14.0)).flex_1())
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
