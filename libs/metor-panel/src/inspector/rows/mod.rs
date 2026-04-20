//! Row widgets rendered inside an [`Inspector`](crate::inspector::Inspector).
//!
//! Every concrete widget lives in its own submodule and captures the
//! closures needed to read and write its bound data. The inspector itself
//! only sees [`InspectorRow`] trait objects and a [`RowAction`] reply when
//! a row is activated, so adding a new widget doesn't touch shared code.
use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};

use crate::theme::theme;

pub mod bool;
pub mod checkbox;
pub mod color;
pub mod command;
pub mod default_action;
pub mod enum_;
pub mod nav;
pub mod scalar;
pub mod slider;
pub mod text;
pub mod text_field;

pub use bool::BoolRow;
pub use checkbox::{check_square, checkbox};
pub use color::ColorRow;
pub use command::CommandRow;
pub use default_action::DefaultActionRow;
pub use enum_::EnumRow;
pub use nav::NavRow;
pub use scalar::ScalarRow;
pub use slider::SliderRow;
pub use text::TextRow;
pub use text_field::TextField;

/// Contract every row in the inspector satisfies.
pub trait InspectorRow: 'static {
    /// Text matched by the inspector's fuzzy search.
    fn label(&self) -> &str;

    /// Paint the row. `selected` reflects keyboard focus and drives the
    /// selection-background highlight.
    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;

    /// Respond to Enter or a click. Returning a [`RowAction`] lets the row
    /// mutate its bound data directly while delegating navigation and
    /// dismissal to the host.
    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction;
}

/// Reply from [`InspectorRow::activate`] directing what the host should do next.
pub enum RowAction {
    /// Row mutated its own state; refresh only.
    Handled,
    /// Drill into a sub-page with the provided rows.
    Cascade(Vec<Box<dyn InspectorRow>>),
    /// Close the inspector.
    Dismiss,
    /// Hand off to inline text editing seeded with `current_text`.
    StartEdit {
        current_text: String,
        on_commit: Box<dyn FnOnce(String, &mut Window, &mut App)>,
    },
}

/// Small pill used for category and tag annotations next to a row label.
pub fn tag_pill(tag: SharedString, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    div()
        .px(px(6.0))
        .py(px(1.0))
        .bg(theme.pill_bg)
        .border_1()
        .border_color(theme.pill_border)
        .rounded(px(3.0))
        .text_size(px(10.0))
        .text_color(theme.text_secondary)
        .child(tag)
}

/// Row-chrome the concrete widgets wrap: background, hover, spacing, and id.
pub fn row_base(row_ix: usize, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme(cx);
    let bg = if selected {
        theme.selection_bg
    } else {
        Hsla::transparent_black()
    };
    div()
        .id(("inspector-row", row_ix))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(28.0))
        .px(px(12.0))
        .bg(bg)
        .cursor_pointer()
        .hover(|s| s.bg(theme.selection_bg))
}
