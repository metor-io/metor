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
use crate::inspector::rows::InspectorRow;
use crate::views::list_plot::trace_picker::{expression_len, vector_trace_rows};

use super::SpectrogramTrace;

/// Invoked with the [`SpectrogramTrace`] the user built through the wizard.
pub type OnSpectrogramTraceSelected = Arc<dyn Fn(SpectrogramTrace, &mut Window, &mut App)>;

/// Build the wizard's only page.
pub fn select_spectrogram_trace_wizard_rows(
    db: Arc<DB>,
    on_select: OnSpectrogramTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    vector_trace_rows(
        db,
        "Pick a spectrum component, or type an expression",
        "No vector components available — a spectrogram needs a component whose schema dim has length > 1, such as the output of `fft`.",
        expression_trace,
        |id, name, len, _cx| {
            let mut trace = SpectrogramTrace::new(id, len);
            trace.label = SharedString::from(name.to_string());
            trace
        },
        on_select,
    )
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
