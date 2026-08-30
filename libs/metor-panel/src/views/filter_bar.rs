//! The strip an exploration panel shows above its content when the
//! operator wants to narrow it: a search glyph, a single-line field, and
//! the `shown / total` count. Hosts own visibility and matching; the bar
//! owns the text, parses it into a [`Query`] on every keystroke, and says so
//! through [`FilterBarEvent`].
//!
//! Keys are handled here and stopped, so a host that also reads arrows
//! (the column browser) never sees the ones typed into the field. Escape
//! clears the query and hands focus back to the host.

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString,
    Window, div, prelude::*, px,
};

use crate::icons::Icon;
use crate::inspector::rows::TextField;
use crate::query::Query;
use crate::theme::theme;

const BAR_HEIGHT: f32 = 28.0;

/// What the bar just did with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterBarEvent {
    /// The query text changed, possibly to empty; re-read [`FilterBar::query`].
    Changed,
    /// Enter, with whatever the query is now.
    Submitted,
}

pub struct FilterBar {
    field: TextField,
    query: Query,
    focus_handle: FocusHandle,
    /// Where Escape sends focus — the host's own handle.
    parent_focus: FocusHandle,
    /// `shown / total`, or whatever the host wants beside the field.
    pub status: Option<SharedString>,
    /// A trailing affordance the host offers for Enter, such as `↵ save`.
    pub hint: Option<SharedString>,
}

impl FilterBar {
    pub fn new(
        placeholder: impl Into<String>,
        parent_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            field: TextField::new(placeholder, cx),
            query: Query::default(),
            focus_handle: cx.focus_handle(),
            parent_focus,
            status: None,
            hint: None,
        }
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    pub fn text(&self) -> &str {
        &self.field.text
    }

    /// Replace the text as if it had been typed, so the host hears about it.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        self.field.set_text(text.clone());
        self.field.cursor = text.len();
        self.field.mark = text.len();
        self.query = Query::parse(&text);
        cx.emit(FilterBarEvent::Changed);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.field.text.is_empty() {
            return;
        }
        self.field.clear();
        self.query = Query::default();
        cx.emit(FilterBarEvent::Changed);
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.clear(cx);
                window.focus(&self.parent_focus);
            }
            "enter" | "return" => cx.emit(FilterBarEvent::Submitted),
            _ => {
                if !self.field.handle_key_down(event, cx) {
                    return;
                }
                self.query = Query::parse(&self.field.text);
                cx.emit(FilterBarEvent::Changed);
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl EventEmitter<FilterBarEvent> for FilterBar {}

impl Focusable for FilterBar {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FilterBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let has_text = !self.field.text.is_empty();
        div()
            .id("filter-bar")
            .key_context("FilterBar TextInput")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .w_full()
            .h(px(BAR_HEIGHT))
            .px(px(8.0))
            .gap(px(6.0))
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(11.0))
            .text_color(theme.text_tertiary)
            .cursor_text()
            .child(Icon::Search.svg_color(11.0, theme.text_tertiary))
            .child(div().flex_1().h_full().child(self.field.element()))
            .children(
                self.status
                    .clone()
                    .map(|status| div().whitespace_nowrap().child(status)),
            )
            .children(
                has_text
                    .then(|| self.hint.clone())
                    .flatten()
                    .map(|hint| div().whitespace_nowrap().child(hint)),
            )
            .children(has_text.then(|| {
                div()
                    .id("filter-bar-clear")
                    .px(px(4.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.selection_bg))
                    .child(Icon::Close.svg_color(9.0, theme.text_secondary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.clear(cx);
                        window.focus(&this.parent_focus);
                        cx.stop_propagation();
                    }))
            }))
    }
}
