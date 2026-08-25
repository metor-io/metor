use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;

use super::{InspectorRow, RowAction, row_base};
use crate::dynamic::expressions::{self, SIGIL};
use crate::theme::theme;

/// The spreadsheet convention, in a component picker: type `=` and the search
/// field becomes an expression field.
///
/// The row is pinned and always visible ([`consumes_search`]), because a
/// picker's search text *is* the expression — there is no second field to open
/// and nothing to fuzzy-match against.
///
/// Activating compiles the text and starts a view-owned system, handing its
/// component id to the same callback a picked component would have gone to.
/// Every consumer therefore gains computed channels without learning what an
/// expression is.
///
/// An expression that does not compile leaves the row open with the compiler's
/// first complaint in it, so the text stays where it can be fixed. The
/// inspector paints rows without handing them the query, so this is what the
/// row knows: what happened the last time the operator committed.
///
/// [`consumes_search`]: InspectorRow::consumes_search
pub struct ExpressionRow {
    db: Arc<DB>,
    on_select: OnExpression,
    /// What the last commit made of the query, if there has been one.
    status: Option<String>,
}
/// What a picker does with a committed expression.
///
/// It receives the hidden component the expression publishes into and the text
/// that produced it, and decides what happens next — most callers dismiss,
/// while a multi-select wizard hands back a trace and closes itself.
pub type OnExpression = Arc<dyn Fn(ComponentId, String, &mut Window, &mut App) -> RowAction>;

impl ExpressionRow {
    pub fn new(db: Arc<DB>, on_select: OnExpression) -> Self {
        Self {
            db,
            on_select,
            status: None,
        }
    }
}

impl InspectorRow for ExpressionRow {
    fn label(&self) -> &str {
        "Expression"
    }

    fn consumes_search(&self) -> bool {
        true
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let (detail, tint) = match &self.status {
            Some(why) => (SharedString::from(why.clone()), theme.error_accent),
            None => (
                SharedString::from(format!("type {SIGIL} to compute a channel")),
                theme.text_tertiary,
            ),
        };
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_primary)
                            .child(SharedString::new_static("Expression")),
                    )
                    .child(div().text_size(px(11.0)).text_color(tint).child(detail)),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        // Reached only when the query is not an expression, in which case
        // there is nothing to commit.
        RowAction::Handled
    }

    fn activate_with_search(
        &mut self,
        search: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> RowAction {
        if !expressions::is_expression(search) {
            return RowAction::Handled;
        }
        match expressions::resolve(search, &self.db, cx) {
            Ok(expression) => {
                self.status = None;
                (self.on_select)(expression.component_id(), search.to_string(), window, cx)
            }
            // Staying open leaves the text where it can be fixed, with the
            // compiler's first complaint next to it.
            Err(why) => {
                self.status = Some(why.to_string());
                RowAction::Handled
            }
        }
    }
}
