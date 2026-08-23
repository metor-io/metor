//! Row widgets rendered inside an [`Inspector`](crate::inspector::Inspector).
//!
//! Every concrete widget lives in its own submodule and captures the
//! closures needed to read and write its bound data. The inspector itself
//! only sees [`InspectorRow`] trait objects and a [`RowAction`] reply when
//! a row is activated, so adding a new widget doesn't touch shared code.
use gpui::{
    AnyElement, AnyView, App, Hsla, Pixels, SharedString, Size, Window, div, prelude::*, px,
};

use crate::theme::theme;

pub mod bool;
pub mod checkbox;
pub mod color;
pub mod command;
pub mod default_action;
pub mod enum_;
pub mod expression;
pub mod header;
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
pub use expression::{ExpressionRow, OnExpression};
pub use header::HeaderRow;
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

    /// Variant of [`Self::activate`] that receives the inspector's
    /// current search text. Rows that accept free-form text (prompts,
    /// filter builders) override this to consume the typed query as
    /// their input rather than opening a secondary edit field.
    ///
    /// The default delegates to `activate`, so existing rows keep their
    /// current behaviour.
    fn activate_with_search(
        &mut self,
        _search: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> RowAction {
        self.activate(window, cx)
    }

    /// Rows that consume the search field itself return `true` so the
    /// fuzzy filter keeps them visible even when the query doesn't
    /// match their label.
    fn consumes_search(&self) -> bool {
        false
    }

    /// Non-interactive section headers return `true` so the inspector skips
    /// them during arrow-key selection and drops them from filtered results
    /// (an empty header is more confusing than none).
    fn is_header(&self) -> bool {
        false
    }
}

/// A view-hosting inspector page: arbitrary gpui widget, the panel size
/// to allocate for it, and a header label shown in the chrome.
///
/// Shared between [`RowAction::CascadeView`] (drill-in) and
/// [`Inspector::with_view`](crate::inspector::Inspector::with_view) (open
/// directly) so the three fields don't have to agree on positional order
/// at every call site.
pub struct PreviewSpec {
    pub view: AnyView,
    pub size: Size<Pixels>,
    pub label: SharedString,
}

/// Reply from [`InspectorRow::activate`] directing what the host should do next.
pub enum RowAction {
    /// Row mutated its own state; refresh only.
    Handled,
    /// Drill into a sub-page with the provided rows.
    Cascade(Vec<Box<dyn InspectorRow>>),
    /// Pop the current page off the stack, returning to the parent.
    Pop,
    /// Close the inspector.
    Dismiss,
    /// Hand off to inline text editing seeded with `current_text`.
    StartEdit {
        current_text: String,
        on_commit: Box<dyn FnOnce(String, &mut Window, &mut App)>,
    },
    /// Drill into a sub-page that hosts an arbitrary widget instead of a
    /// row list. Used for transient previews (impromptu plots) that benefit
    /// from the inspector's overlay chrome and page stack.
    CascadeView(PreviewSpec),
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

/// A label row: [`row_base`] chrome wrapping a single text label in `color`,
/// with an optional trailing [`tag_pill`]. Shared by the callback rows
/// ([`CommandRow`], [`DefaultActionRow`]) so they don't each re-spell the
/// same div.
pub fn render_label_row(
    row_ix: usize,
    selected: bool,
    label: SharedString,
    tag: Option<SharedString>,
    color: Hsla,
    cx: &App,
) -> AnyElement {
    let mut row = row_base(row_ix, selected, cx).child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(px(12.0))
            .text_color(color)
            .child(label),
    );
    if let Some(tag) = tag {
        row = row.child(tag_pill(tag, cx));
    }
    row.into_any_element()
}

/// Row-chrome the concrete widgets wrap: background, hover, spacing, and id.
///
/// The selection/hover highlight is painted as an inset rounded pill behind
/// the content rather than a full-bleed fill. This keeps the highlight clear
/// of the panel's rounded corners (gpui content masks are rectangular, so a
/// full-width fill would bleed past the curve).
pub fn row_base(row_ix: usize, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme(cx);
    let pill_bg = if selected {
        theme.selection_bg
    } else {
        Hsla::transparent_black()
    };
    div()
        .id(("inspector-row", row_ix))
        .group("inspector-row")
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(28.0))
        .px(px(12.0))
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .bottom(px(2.0))
                .left(px(4.0))
                .right(px(4.0))
                .rounded(px(4.0))
                .bg(pill_bg)
                .group_hover("inspector-row", |s| s.bg(theme.selection_bg)),
        )
}
