//! Single-step wizard for constructing a [`ListTrace`].
//!
//! Pages: pick component (filtered to those with vector dim > 1) →
//! commit. Trace `len` is captured from the component schema at pick
//! time; later samples are assumed to share that length (component dims
//! are fixed in this codebase).

use std::sync::Arc;

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{CommandRow, HeaderRow, InspectorRow};
use crate::inspector::trace_picker::ColorBasis;
use crate::theme::theme;

use super::ListTrace;

/// Invoked with the [`ListTrace`] the user built through the wizard.
pub type OnListTraceSelected = Arc<dyn Fn(ListTrace, &mut Window, &mut App)>;

/// Build the wizard's only page: a list of vector-dim components.
pub fn select_list_trace_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnListTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let candidates = list_vector_components(&db);
    let header: Box<dyn InspectorRow> = Box::new(HeaderRow::new("Pick a vector component"));
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![header];
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
