//! Single-step wizard for constructing a [`SpectrogramTrace`].
//!
//! One page: the expression row, then the vector components. A spectrogram is
//! almost always driven by an expression — `= fft(window(sig, 64))` — so the
//! typed field is the primary path and the component list is the fallback for
//! a target that already publishes spectra.
//!
//! Bin count is captured from the schema at pick time, the same rule the list
//! plot follows: what an expression publishes is plotted the way the channel
//! it came from would be.

use std::sync::Arc;

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::expressions;
use crate::inspector::rows::{CommandRow, ExpressionRow, HeaderRow, InspectorRow, RowAction};
use crate::views::list_plot::trace_picker::{expression_len, list_vector_components};

use super::SpectrogramTrace;

/// Invoked with the [`SpectrogramTrace`] the user built through the wizard.
pub type OnSpectrogramTraceSelected = Arc<dyn Fn(SpectrogramTrace, &mut Window, &mut App)>;

/// Build the wizard's only page.
pub fn select_spectrogram_trace_wizard_rows(
    db: Arc<DB>,
    on_select: OnSpectrogramTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let candidates = list_vector_components(&db);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(HeaderRow::new(
        "Pick a spectrum component, or type an expression",
    ))];

    // Pinned above the list, and before the empty-state note: a db with no
    // vector component of its own can still have a spectrum computed from one.
    let commit: crate::inspector::rows::OnExpression = {
        let db = db.clone();
        let on_select = on_select.clone();
        Arc::new(move |component, text, window, cx| {
            let trace = expression_trace(&db, component, &text, cx);
            on_select(trace, window, cx);
            RowAction::Dismiss
        })
    };
    // Only vectors have bins, so the completion provider's component
    // candidates are declined unless their schema says vector.
    let vector_row: crate::inspector::rows::ComponentRowBuilder = {
        let db = db.clone();
        let on_select = on_select.clone();
        Arc::new(move |id, _item, _cx| {
            let (_, name, len) = list_vector_components(&db)
                .into_iter()
                .find(|(vid, _, _)| *vid == id)?;
            let on_select = on_select.clone();
            let label = SharedString::from(format!("{} [{}]", name, len));
            Some(Box::new(CommandRow::new(
                label,
                Arc::new(move |window, cx| {
                    let mut trace = SpectrogramTrace::new(id, len);
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
            "No vector components available — a spectrogram needs a component whose schema dim has length > 1, such as the output of `fft`.",
        )));
        return rows;
    }
    for (id, name, len) in candidates {
        let on_select = on_select.clone();
        let label = SharedString::from(format!("{} [{}]", name, len));
        rows.push(Box::new(CommandRow::new(
            label,
            Arc::new(move |window, cx| {
                let mut trace = SpectrogramTrace::new(id, len);
                trace.label = SharedString::from(name.clone());
                (on_select)(trace, window, cx);
            }),
        )));
    }
    rows
}

/// One spectrogram source over an expression's output.
///
/// The label is the text the operator typed, which is also how the expression
/// survives a save: the source serializes its component id and its label, and
/// the component is the hash-named hidden one the expression publishes into.
pub(crate) fn expression_trace(
    db: &DB,
    component: ComponentId,
    text: &str,
    cx: &App,
) -> SpectrogramTrace {
    let mut trace = SpectrogramTrace::new(component, expression_len(db, component));
    trace.label = SharedString::from(expressions::body(text).to_string());
    trace.source = crate::data_binding::Binding::selected(component, text, cx);
    trace
}
