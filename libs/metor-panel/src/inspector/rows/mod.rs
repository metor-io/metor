//! Row widgets rendered inside an [`Inspector`](crate::inspector::Inspector).
//!
//! Every concrete widget lives in its own submodule and captures the
//! closures needed to read and write its bound data. The inspector itself
//! only sees [`InspectorRow`] trait objects and a [`RowAction`] reply when
//! a row is activated, so adding a new widget doesn't touch shared code.
use gpui::{
    AnyElement, AnyView, App, Global, Hsla, Pixels, SharedString, Size, Window, div, prelude::*, px,
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
pub use expression::{ComponentRowBuilder, ExpressionRow, OnExpression, TailRowBuilder};
pub use header::HeaderRow;
pub use nav::NavRow;
pub use scalar::ScalarRow;
pub use slider::SliderRow;
pub use text::TextRow;
pub use text_field::TextField;

/// Contract every row in the inspector satisfies.
pub trait InspectorRow: 'static {
    /// Persistent companion content belongs to the provider page, outside its row list.
    fn accessory(&self, _query: &str, _cx: &mut App) -> Option<AccessorySpec> {
        None
    }
    /// Seed a provider page identically when opened directly or through navigation.
    fn initial_query(&self) -> Option<String> {
        None
    }

    /// Domain-specific hint for query pages; ordinary palettes keep Search.
    fn query_placeholder(&self) -> Option<&str> {
        None
    }

    /// Apply a user edit to a provider's query. Seeding a page and refreshing
    /// previews do not call this hook, so rendering never commits a value.
    fn query_edited(&self, _query: &str, _cx: &mut App) {}

    /// Rebuild dynamic previews without replacing the query or keyboard selection.
    fn query_revision(&self, _cx: &App) -> u64 {
        0
    }

    /// Text matched by the inspector's fuzzy search.
    fn label(&self) -> &str;

    /// Stable selection key when a live palette rebuilds its rows.
    fn identity(&self) -> SharedString {
        self.label().to_owned().into()
    }

    /// Opt in to exit rendering under [`passive`]. Such rows must omit all
    /// input handlers and interactive children when that flag is set.
    /// Custom rows default to immediate dismissal until they support this.
    fn supports_exit_fade(&self) -> bool {
        false
    }

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

    /// Reinterpret the whole query, replacing the page's list.
    ///
    /// A completion provider lives on its page as an ordinary row — the same
    /// way [`ExpressionRow`] already rides along in every picker — and this is
    /// how it takes over: given the query and the search field's cursor, it
    /// returns the rows to show *instead of* the fuzzy-filtered page. `None`
    /// leaves the page to the default filter, so a page without a provider
    /// behaves exactly as before.
    ///
    /// The first row on a page that answers wins; the inspector re-asks on
    /// every query change and owns the returned rows until the next one.
    fn query_rows(
        &self,
        _query: &str,
        _cursor: usize,
        _cx: &mut App,
    ) -> Option<Vec<Box<dyn InspectorRow>>> {
        None
    }

    /// Respond to Tab: insert rather than commit.
    ///
    /// Completion rows override this to push their candidate into the search
    /// field ([`RowAction::ReplaceQuery`]) without activating the row. Providers
    /// can apply the resulting edit through [`Self::query_edited`]. Rows with nothing to insert leave the default no-op.
    fn insert(&mut self, _search: &str, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Handled
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

/// A page-owned view, embedded in palettes and floating beneath anchored menus.
#[derive(Clone)]
pub struct AccessorySpec {
    pub view: AnyView,
    pub focus: gpui::FocusHandle,
    pub dragging: std::sync::Arc<dyn Fn(&App) -> bool>,
}

/// Reply from [`InspectorRow::activate`] directing what the host should do next.
pub enum RowAction {
    /// Row mutated its own state; refresh only.
    Handled,
    /// Drill into a sub-page with the provided rows.
    Cascade(Vec<Box<dyn InspectorRow>>),
    /// Drill into a sub-page with its search field already holding `query`
    /// — how a page that edits an existing binding opens on that binding
    /// rather than on a blank field.
    CascadeWith {
        rows: Vec<Box<dyn InspectorRow>>,
        query: String,
    },
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
    /// Rewrite the search field — how an accepted completion lands in the
    /// query without committing anything. `cursor` is a byte offset into
    /// `text`.
    ReplaceQuery { text: String, cursor: usize },
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
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    let mut budget = label_budget(cx);
    if let (Some(budget), Some(tag)) = (budget.as_mut(), &tag) {
        *budget -= measure(tag, px(10.0), window) + px(24.0);
    }
    let mut row =
        row_base(row_ix, selected, cx).child(path_label(&label, color, budget, window, cx));
    if let Some(tag) = tag {
        row = row.child(tag_pill(tag, cx));
    }
    row.into_any_element()
}

/// The room a row's label has, set by the inspector for the panel it is
/// rendering. Rows read it to elide a dotted path from the front instead of
/// letting the layout cut off its tail — the end of a path is the part that
/// tells two components apart.
pub struct LabelFit {
    /// Width inside the row's padding, when the panel knows it.
    pub row_width: Option<Pixels>,
}

#[derive(Default)]
struct PassiveRows(bool);
impl Global for PassiveRows {}

/// Whether a row is being built only as the visual of a dismissed inspector.
pub fn passive(cx: &App) -> bool {
    cx.try_global::<PassiveRows>().is_some_and(|mode| mode.0)
}

pub(super) fn with_passive<R>(cx: &mut App, build: impl FnOnce(&mut App) -> R) -> R {
    let previous = passive(cx);
    cx.set_global(PassiveRows(true));
    let result = build(cx);
    cx.set_global(PassiveRows(previous));
    result
}

impl Global for LabelFit {}

/// The label width a row may use before eliding, if the panel said.
pub fn label_budget(cx: &App) -> Option<Pixels> {
    cx.try_global::<LabelFit>()?.row_width
}

/// The label font: what every row's text is set in.
pub const LABEL_SIZE: Pixels = px(12.0);

/// Width of `text` at `size` in the window's font.
pub fn measure(text: &str, size: Pixels, window: &Window) -> Pixels {
    let run = gpui::TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color: Hsla::transparent_black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_string()), size, &[run], None)
        .width
}

/// A dotted path as a label: the leaf in `color`, the namespaces before it
/// dimmed, and — when `budget` is too narrow for the whole — leading
/// namespaces dropped behind an ellipsis so the leaf survives.
///
/// Text without a dot is just the leaf.
pub fn path_label(
    text: &str,
    color: Hsla,
    budget: Option<Pixels>,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    let theme = theme(cx);
    let (prefix, leaf) = elide_front(text, budget, |s| measure(s, LABEL_SIZE, window));
    let mut label = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(LABEL_SIZE);
    if !prefix.is_empty() {
        label = label.child(
            div()
                .flex_none()
                .text_color(theme.text_tertiary)
                .child(SharedString::from(prefix)),
        );
    }
    label.child(
        div()
            .min_w_0()
            .truncate()
            .text_color(color)
            .child(SharedString::from(leaf)),
    )
}

/// Split `text` into the namespaces to dim and the leaf to keep, dropping
/// namespaces from the front until `width` of the whole fits `budget`.
fn elide_front(
    text: &str,
    budget: Option<Pixels>,
    width: impl Fn(&str) -> Pixels,
) -> (String, String) {
    let Some(dot) = text.rfind('.') else {
        return (String::new(), text.to_string());
    };
    let leaf = &text[dot + 1..];
    let namespaces: Vec<&str> = text[..dot].split('.').collect();
    let Some(budget) = budget else {
        return (format!("{}.", namespaces.join(".")), leaf.to_string());
    };
    for skip in 0..namespaces.len() {
        let mut prefix = String::new();
        if skip > 0 {
            prefix.push('…');
        }
        for ns in &namespaces[skip..] {
            prefix.push_str(ns);
            prefix.push('.');
        }
        if width(&format!("{prefix}{leaf}")) <= budget {
            return (prefix, leaf.to_string());
        }
    }
    (String::from("…"), leaf.to_string())
}

/// Row-chrome the concrete widgets wrap: background, hover, spacing, and id.
///
/// The selection/hover highlight is painted as an inset rounded pill behind
/// the content rather than a full-bleed fill. This keeps the highlight clear
/// of the panel's rounded corners (gpui content masks are rectangular, so a
/// full-width fill would bleed past the curve).
pub fn row_base(row_ix: usize, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme(cx);
    let interactive = !passive(cx);
    let pill_bg = if selected {
        theme.selection_bg
    } else {
        Hsla::transparent_black()
    };
    div()
        .id(("inspector-row", row_ix))
        .when(interactive, |row| {
            row.group("inspector-row").cursor_pointer()
        })
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(28.0))
        .px(px(12.0))
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .bottom(px(2.0))
                .left(px(4.0))
                .right(px(4.0))
                .rounded(px(4.0))
                .bg(pill_bg)
                .when(interactive, |pill| {
                    pill.group_hover("inspector-row", |s| s.bg(theme.selection_bg))
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per character stands in for the text system.
    fn by_chars(s: &str) -> Pixels {
        px(s.chars().count() as f32)
    }

    #[test]
    fn a_path_keeps_its_leaf_and_sheds_namespaces_from_the_front() {
        let text = "cube_sat.plant.body.omega_b";
        assert_eq!(
            elide_front(text, None, by_chars),
            ("cube_sat.plant.body.".into(), "omega_b".into())
        );
        assert_eq!(
            elide_front(text, Some(px(100.0)), by_chars),
            ("cube_sat.plant.body.".into(), "omega_b".into())
        );
        assert_eq!(
            elide_front(text, Some(px(20.0)), by_chars),
            ("…plant.body.".into(), "omega_b".into()),
            "the first namespace goes first"
        );
        assert_eq!(
            elide_front(text, Some(px(9.0)), by_chars),
            ("…".into(), "omega_b".into()),
            "the leaf is never given up"
        );
        assert_eq!(
            elide_front("Add Model", Some(px(3.0)), by_chars),
            (String::new(), "Add Model".into()),
            "no dots, no prefix"
        );
    }
}
