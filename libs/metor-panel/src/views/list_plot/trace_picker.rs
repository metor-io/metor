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
    let expression_basis = color_basis.clone();
    vector_trace_rows(
        db,
        "Pick a vector component, or type an expression",
        "No vector components available — list plots need a component whose schema dim has length > 1.",
        move |db, component, text, cx| expression_trace(db, component, text, &expression_basis, cx),
        move |id, name, len, cx| {
            let theme = theme(cx);
            let basis = (color_basis)(cx);
            let color = theme.line_colors[basis % theme.line_colors.len()];
            let mut trace = ListTrace::new(id, len, color);
            trace.label = SharedString::from(name.to_string());
            trace
        },
        on_select,
    )
}

/// The page every vector-trace wizard shares: `header`, the expression row,
/// then the vector components (or `empty_note` when there are none).
///
/// The expression row is pinned above the list, and before the empty-state
/// note: a db with no vector component of its own can still have one
/// computed from it. Only vectors fit, so the completion provider's component
/// candidates are declined unless their schema says vector — the same rule
/// [`list_vector_components`] applies to the unfiltered list.
pub(crate) fn vector_trace_rows<T: 'static>(
    db: Arc<DB>,
    header: &'static str,
    empty_note: &'static str,
    from_expression: impl Fn(&DB, ComponentId, &str, &App) -> T + 'static,
    from_component: impl Fn(ComponentId, &str, usize, &App) -> T + 'static,
    on_select: Arc<dyn Fn(T, &mut Window, &mut App)>,
) -> Vec<Box<dyn InspectorRow>> {
    let from_component: Arc<dyn Fn(ComponentId, &str, usize, &App) -> T> =
        Arc::new(from_component);
    let pick_row = {
        let on_select = on_select.clone();
        let from_component = from_component.clone();
        move |id: ComponentId, name: String, len: usize| -> Box<dyn InspectorRow> {
            let on_select = on_select.clone();
            let from_component = from_component.clone();
            let label = SharedString::from(format!("{} [{}]", name, len));
            Box::new(CommandRow::new(
                label,
                Arc::new(move |window, cx| {
                    let trace = from_component(id, &name, len, cx);
                    (on_select)(trace, window, cx);
                }),
            ))
        }
    };

    let commit: crate::inspector::rows::OnExpression = {
        let db = db.clone();
        let on_select = on_select.clone();
        Arc::new(move |component, text, window, cx| {
            let trace = from_expression(&db, component, &text, cx);
            on_select(trace, window, cx);
            RowAction::Dismiss
        })
    };
    let vector_row: crate::inspector::rows::ComponentRowBuilder = {
        let db = db.clone();
        let pick_row = pick_row.clone();
        Arc::new(move |id, _item, _cx| {
            let (_, name, len) = list_vector_components(&db)
                .into_iter()
                .find(|(vid, _, _)| *vid == id)?;
            Some(pick_row(id, name, len))
        })
    };

    let candidates = list_vector_components(&db);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![
        Box::new(HeaderRow::new(header)),
        Box::new(ExpressionRow::new(db.clone(), commit, vector_row, None)),
    ];
    if candidates.is_empty() {
        rows.push(Box::new(HeaderRow::new(empty_note)));
        return rows;
    }
    rows.extend(
        candidates
            .into_iter()
            .map(|(id, name, len)| pick_row(id, name, len)),
    );
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
    trace.source = crate::data_binding::Binding::selected(component, text, cx);
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
pub(crate) fn list_vector_components(db: &DB) -> Vec<(ComponentId, String, usize)> {
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
