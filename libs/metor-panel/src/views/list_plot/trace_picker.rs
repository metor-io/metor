//! Single-step wizard for constructing a [`ListTrace`].
//!
//! Pages: pick component (filtered to those with vector dim > 1) →
//! commit. Trace `len` is captured from the component schema at pick
//! time; later samples are assumed to share that length (component dims
//! are fixed in this codebase).
//!
//! Typing `=` computes one instead. A list plot draws the *interior* of a
//! sample, so an expression's element count is its length — which makes the
//! rule here the same one the time-series picker follows: what an expression
//! publishes is plotted the way the channel it came from would be. Taking
//! `len` as 1 would be this plot's version of collapsing to element zero, and
//! it would draw a single point where the arithmetic produced a vector.

use std::sync::Arc;

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::expressions;
use crate::inspector::rows::{CommandRow, ExpressionRow, HeaderRow, InspectorRow, RowAction};
use crate::inspector::trace_picker::{ColorBasis, element_names_for_component};
use crate::theme::theme;

use super::ListTrace;

/// Invoked with the [`ListTrace`] the user built through the wizard.
pub type OnListTraceSelected = Arc<dyn Fn(ListTrace, &mut Window, &mut App)>;

/// Build the wizard's only page: the expression row, then a list of
/// vector-dim components.
pub fn select_list_trace_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnListTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let candidates = list_vector_components(&db);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(HeaderRow::new(
        "Pick a vector component, or type an expression",
    ))];

    // Pinned above the list, and before the empty-state note: a db with no
    // vector component of its own can still have one computed from it.
    let commit: crate::inspector::rows::OnExpression = {
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        Arc::new(move |component, text, window, cx| {
            let trace = expression_trace(&db, component, &text, &color_basis, cx);
            on_select(trace, window, cx);
            RowAction::Dismiss
        })
    };
    // Only vectors fit a list plot, so the completion provider's component
    // candidates are declined unless their schema says vector — the same
    // rule `list_vector_components` applies to the unfiltered list.
    let vector_row: crate::inspector::rows::ComponentRowBuilder = {
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        Arc::new(move |id, _item, _cx| {
            let (_, name, len) = list_vector_components(&db)
                .into_iter()
                .find(|(vid, _, _)| *vid == id)?;
            let on_select = on_select.clone();
            let color_basis = color_basis.clone();
            let label = SharedString::from(format!("{} [{}]", name, len));
            Some(Box::new(CommandRow::new(
                label,
                Arc::new(move |window, cx| {
                    let theme = theme(cx);
                    let basis = (color_basis)(cx);
                    let color = theme.line_colors[basis % theme.line_colors.len()];
                    let mut trace = ListTrace::new(id, len, color);
                    trace.label = SharedString::from(name.clone());
                    (on_select)(trace, window, cx);
                }),
            )) as Box<dyn InspectorRow>)
        })
    };
    rows.push(Box::new(ExpressionRow::new(
        db.clone(),
        commit,
        vector_row,
        None,
    )));

    if candidates.is_empty() {
        rows.push(Box::new(HeaderRow::new(
            "No vector components available — list plots need a component whose schema dim has length > 1.",
        )));
        return rows;
    }
    for (id, name, len) in candidates {
        let on_select = on_select.clone();
        let color_basis = color_basis.clone();
        let label = SharedString::from(format!("{} [{}]", name, len));
        rows.push(Box::new(CommandRow::new(
            label,
            Arc::new(move |window, cx| {
                let theme = theme(cx);
                let basis = (color_basis)(cx);
                let color = theme.line_colors[basis % theme.line_colors.len()];
                let mut trace = ListTrace::new(id, len, color);
                trace.label = SharedString::from(name.clone());
                (on_select)(trace, window, cx);
            }),
        )));
    }
    rows
}

/// One list trace over an expression's output.
///
/// The label is the text the operator typed, which is also how the expression
/// survives a save: a trace serializes its component id and its label, and the
/// component is the hash-named hidden one the expression publishes into.
pub(crate) fn expression_trace(
    db: &DB,
    component: ComponentId,
    text: &str,
    color_basis: &ColorBasis,
    cx: &App,
) -> ListTrace {
    let theme = theme(cx);
    let color = theme.line_colors[(color_basis)(cx) % theme.line_colors.len()];
    let mut trace = ListTrace::new(component, expression_len(db, component), color);
    trace.label = SharedString::from(expressions::body(text).to_string());
    trace.expression = expressions::running(component, cx);
    trace
}

/// How long an expression's list trace is: its output's element count.
///
/// Separated from the colours so the rule can be checked without a window —
/// the same split the time-series picker makes, and for the same reason.
pub(crate) fn expression_len(db: &DB, component: ComponentId) -> usize {
    element_names_for_component(db, component).len().max(1)
}

/// Components whose schema dim is more than a scalar (i.e. total
/// element count > 1). Sorted by name for stable display.
fn list_vector_components(db: &DB) -> Vec<(ComponentId, String, usize)> {
    let mut out: Vec<(ComponentId, String, usize)> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .filter(|(_, meta)| !meta.is_hidden())
            .filter_map(|(id, meta)| {
                let comp = state.get_component(*id)?;
                let len: usize = comp.schema.dim.iter().product();
                if len > 1 {
                    Some((*id, meta.name.clone(), len))
                } else {
                    None
                }
            })
            .collect()
    });
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}
