/// Per-type widget elements for the property inspector.
///
/// Each field type rendered in the inspector is a self-contained struct
/// implementing [`InspectorRow`]. The [`Inspector`](crate::inspector::Inspector)
/// container composes these rows without knowing their concrete types.
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

/// One row in an inspector panel.
///
/// Each widget struct captures its own entity handle and setter closure
/// at construction time, making mutation self-contained.
pub trait InspectorRow: 'static {
    /// Searchable label text for fuzzy filtering.
    fn label(&self) -> &str;

    /// Render this row. `selected` indicates keyboard focus.
    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;

    /// What happens when this row is activated (Enter / click).
    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction;
}

/// Result of activating an inspector row.
pub enum RowAction {
    /// The row handled the action internally (e.g., bool toggled).
    Handled,
    /// Push a new page onto the inspector's page stack with these rows.
    Cascade(Vec<Box<dyn InspectorRow>>),
    /// Dismiss the inspector.
    Dismiss,
    /// Start inline text editing with the given initial value.
    StartEdit {
        current_text: String,
        /// Called when editing is committed.
        on_commit: Box<dyn FnOnce(String, &mut Window, &mut App)>,
    },
}

/// Small category pill rendered alongside a row label.
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

/// Shared base styling for an inspector row.
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
