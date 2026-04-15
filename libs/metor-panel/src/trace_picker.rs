use std::sync::Arc;

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::elements::time_series::Trace;
use crate::widgets::{CommandRow, InspectorRow, NavRow};

/// List all components from the DB, sorted by name.
pub(crate) fn list_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut components: Vec<_> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));
    components
}

/// Return element names for a component from the DB, or an empty vec if not found.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| element_names(c.schema.dim.as_slice()))
            .unwrap_or_default()
    })
}

/// Callback invoked once the user has selected one or more elements; receives
/// the constructed traces ready to be inserted into a plot.
pub type OnTracesSelected = Arc<dyn Fn(Vec<Trace>, &mut Window, &mut App)>;

/// Returns the next color index to assign, given the current `App`. Used so
/// the wizard can either start fresh (return `0`) or continue an existing
/// trace list (return `parent.read(cx).traces.len()`).
pub type ColorBasis = Arc<dyn Fn(&App) -> usize>;

/// Build a component → element wizard that constructs `Trace`s from the user's
/// pick and forwards them to `on_select`. Mirrors the layout of the in-plot
/// "Add Trace" wizard so the picker UX is consistent across entry points.
pub fn select_traces_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnTracesSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let components = list_components(&db);

    components
        .into_iter()
        .map(|(comp_id, comp_name)| {
            let db = db.clone();
            let color_basis = color_basis.clone();
            let on_select = on_select.clone();

            Box::new(NavRow::new(
                SharedString::from(comp_name.clone()),
                SharedString::new_static(""),
                Box::new(move |_cx| {
                    let elem_names = element_names_for_component(&db, comp_id);
                    let elem_names = if elem_names.is_empty() {
                        vec!["value".to_string()]
                    } else {
                        elem_names
                    };

                    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

                    // "All" option — emit one trace per element at once.
                    if elem_names.len() > 1 {
                        let comp_name = comp_name.clone();
                        let names = elem_names.clone();
                        let color_basis = color_basis.clone();
                        let on_select = on_select.clone();
                        rows.push(Box::new(CommandRow::new(
                            SharedString::from(format!("{} (all)", comp_name)),
                            Arc::new(move |w, cx| {
                                let theme = crate::theme::theme(cx);
                                let base_idx = (color_basis)(cx);
                                let traces: Vec<Trace> = names
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, elem_name)| {
                                        let color = theme.line_colors
                                            [(base_idx + idx) % theme.line_colors.len()];
                                        let display = if elem_name.is_empty() {
                                            format!("[{}]", idx)
                                        } else {
                                            elem_name.clone()
                                        };
                                        let mut t = Trace::new(comp_id, idx, color);
                                        t.label = SharedString::from(format!(
                                            "{}.{}",
                                            comp_name, display
                                        ));
                                        t
                                    })
                                    .collect();
                                on_select(traces, w, cx);
                            }),
                        )));
                    }

                    // Individual element options.
                    for (idx, elem_name) in elem_names.into_iter().enumerate() {
                        let comp_name = comp_name.clone();
                        let display = if elem_name.is_empty() {
                            format!("[{}]", idx)
                        } else {
                            elem_name
                        };
                        let label_text = format!("{}.{}", comp_name, display);
                        let color_basis = color_basis.clone();
                        let on_select = on_select.clone();
                        rows.push(Box::new(CommandRow::new(
                            SharedString::from(label_text),
                            Arc::new(move |w, cx| {
                                let theme = crate::theme::theme(cx);
                                let color_idx = (color_basis)(cx);
                                let color =
                                    theme.line_colors[color_idx % theme.line_colors.len()];
                                let mut t = Trace::new(comp_id, idx, color);
                                t.label = SharedString::from(format!(
                                    "{}.{}",
                                    comp_name, display
                                ));
                                on_select(vec![t], w, cx);
                            }),
                        )));
                    }

                    rows
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Generate default element names from a shape (e.g. [3] -> ["x", "y", "z"]).
pub(crate) fn element_names(shape: &[usize]) -> Vec<String> {
    fn walk(shape: &[usize], prefix: &str, out: &mut Vec<String>) {
        if shape.is_empty() {
            out.push(prefix.to_string());
            return;
        }
        const NAMES: [char; 8] = ['x', 'y', 'z', 'w', 'u', 'v', 's', 't'];
        for x in 0..shape[0] {
            let mut elem = prefix.to_string();
            if let Some(c) = NAMES.get(x) {
                elem.push(*c);
            } else {
                elem.push_str(&x.to_string());
            }
            walk(&shape[1..], &elem, out);
        }
    }
    let mut out = Vec::new();
    walk(shape, "", &mut out);
    out
}
