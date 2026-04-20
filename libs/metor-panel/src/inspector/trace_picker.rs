use std::sync::Arc;

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::views::time_series::Trace;
use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

/// Every known component sorted by name, suitable for palette display.
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

/// Element labels for `component_id`, derived from its schema dimension.
/// Empty when the component is missing from the DB.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| element_names(c.schema.dim.as_slice()))
            .unwrap_or_default()
    })
}

/// Invoked with the [`Trace`]s the user built through the wizard.
pub type OnTracesSelected = Arc<dyn Fn(Vec<Trace>, &mut Window, &mut App)>;

/// Starting index into the theme's categorical color palette.
///
/// Lets the wizard continue an existing plot's color sequence (return the
/// current trace count) or restart from zero (return `0`).
pub type ColorBasis = Arc<dyn Fn(&App) -> usize>;

/// Two-step component-then-element picker for constructing [`Trace`]s.
///
/// Shared between the "New Panel → Time Series" flow and the in-plot
/// "Add Trace" button so both entry points present the same UX.
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

                    // A vector component gets an extra row that adds every
                    // element in one pick; colors step through the palette.
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

/// Default element names for a tensor shape.
///
/// Axes consume letters in order from `NAMES` (x, y, z, w, u, v, s, t);
/// once exhausted the numeric index is used. A rank-0 scalar yields a
/// single empty string so callers can substitute `"value"`.
///
/// Example: `[3]` → `["x", "y", "z"]`; `[2, 2]` → `["xx", "xy", "yx", "yy"]`.
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
