//! Two-step wizard for constructing a single [`XyTrace`].
//!
//! Page stack: pick X component → pick X element → pick Y component →
//! pick Y element → commit. A trace's color comes from the theme's
//! categorical palette indexed by `color_basis` (the parent plot's
//! existing trace count, or zero for fresh plots).

use std::sync::{Arc, Mutex};

use gpui::{App, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{CommandRow, HeaderRow, InspectorRow, NavRow};
use crate::inspector::trace_picker::{ColorBasis, element_names_for_component, list_components};
use crate::theme::theme;

use super::XyTrace;

/// Invoked with the [`XyTrace`] the user built through the wizard.
pub type OnXyTraceSelected = Arc<dyn Fn(XyTrace, &mut Window, &mut App)>;

/// Draft state carried across the wizard pages. The Y component+element
/// reach `build_trace` directly through closure capture from the Y-axis
/// pages, so only the X selection needs to round-trip through here.
#[derive(Default)]
struct XyDraft {
    x: Option<(ComponentId, usize)>,
}

/// Build the wizard's first page: a list of components for the X axis.
pub fn select_xy_trace_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let draft = Arc::new(Mutex::new(XyDraft::default()));
    x_component_rows(db, color_basis, on_select, draft)
}

fn x_component_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
    draft: Arc<Mutex<XyDraft>>,
) -> Vec<Box<dyn InspectorRow>> {
    let header: Box<dyn InspectorRow> = Box::new(HeaderRow::new("Pick X axis component"));
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![header];
    for (id, name) in list_components(&db) {
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        let draft = draft.clone();
        let label = SharedString::from(name.clone());
        rows.push(Box::new(NavRow::new(
            label,
            SharedString::new_static(""),
            Box::new(move |_cx| {
                x_element_rows(
                    db.clone(),
                    color_basis.clone(),
                    on_select.clone(),
                    draft.clone(),
                    id,
                    name.clone(),
                )
            }),
        )));
    }
    rows
}

fn x_element_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
    draft: Arc<Mutex<XyDraft>>,
    component_id: ComponentId,
    component_name: String,
) -> Vec<Box<dyn InspectorRow>> {
    let elem_names = element_names_for_component(&db, component_id);
    let elem_names = if elem_names.is_empty() {
        vec!["value".to_string()]
    } else {
        elem_names
    };
    let header: Box<dyn InspectorRow> = Box::new(HeaderRow::new(format!(
        "Pick X element ({})",
        component_name
    )));
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![header];
    for (idx, elem_name) in elem_names.into_iter().enumerate() {
        let display = if elem_name.is_empty() {
            format!("[{}]", idx)
        } else {
            elem_name
        };
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        let draft = draft.clone();
        let label = SharedString::from(format!("{}.{}", component_name, display));
        rows.push(Box::new(NavRow::new(
            label,
            SharedString::new_static(""),
            Box::new(move |_cx| {
                {
                    let mut d = draft.lock().unwrap();
                    d.x = Some((component_id, idx));
                }
                y_component_rows(
                    db.clone(),
                    color_basis.clone(),
                    on_select.clone(),
                    draft.clone(),
                )
            }),
        )));
    }
    rows
}

fn y_component_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
    draft: Arc<Mutex<XyDraft>>,
) -> Vec<Box<dyn InspectorRow>> {
    let header: Box<dyn InspectorRow> = Box::new(HeaderRow::new("Pick Y axis component"));
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![header];
    for (id, name) in list_components(&db) {
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        let draft = draft.clone();
        let label = SharedString::from(name.clone());
        rows.push(Box::new(NavRow::new(
            label,
            SharedString::new_static(""),
            Box::new(move |_cx| {
                y_element_rows(
                    db.clone(),
                    color_basis.clone(),
                    on_select.clone(),
                    draft.clone(),
                    id,
                    name.clone(),
                )
            }),
        )));
    }
    rows
}

fn y_element_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
    draft: Arc<Mutex<XyDraft>>,
    component_id: ComponentId,
    component_name: String,
) -> Vec<Box<dyn InspectorRow>> {
    let elem_names = element_names_for_component(&db, component_id);
    let elem_names = if elem_names.is_empty() {
        vec!["value".to_string()]
    } else {
        elem_names
    };
    let header: Box<dyn InspectorRow> = Box::new(HeaderRow::new(format!(
        "Pick Y element ({})",
        component_name
    )));
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![header];
    for (idx, elem_name) in elem_names.into_iter().enumerate() {
        let display = if elem_name.is_empty() {
            format!("[{}]", idx)
        } else {
            elem_name
        };
        let db = db.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        let draft = draft.clone();
        let component_name = component_name.clone();
        let label = SharedString::from(format!("{}.{}", component_name, display));
        rows.push(Box::new(CommandRow::new(
            label,
            Arc::new(move |window, cx| {
                let trace = build_trace(
                    &db,
                    &draft,
                    component_id,
                    idx,
                    component_name.clone(),
                    display.clone(),
                    &color_basis,
                    cx,
                );
                if let Some(trace) = trace {
                    (on_select)(trace, window, cx);
                }
            }),
        )));
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn build_trace(
    db: &DB,
    draft: &Arc<Mutex<XyDraft>>,
    y_component_id: ComponentId,
    y_element_index: usize,
    y_component_name: String,
    y_element_display: String,
    color_basis: &ColorBasis,
    cx: &App,
) -> Option<XyTrace> {
    let theme = theme(cx);
    let base = (color_basis)(cx);
    let color = theme.line_colors[base % theme.line_colors.len()];

    let (x_component_id, x_element_index) = draft.lock().unwrap().x?;
    let x_component_name = list_components(db)
        .into_iter()
        .find(|(id, _)| *id == x_component_id)
        .map(|(_, n)| n)
        .unwrap_or_default();
    let x_elem_names = element_names_for_component(db, x_component_id);
    let x_elem_display = x_elem_names
        .get(x_element_index)
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("[{}]", x_element_index));

    let label = format!(
        "{}.{} vs {}.{}",
        x_component_name, x_elem_display, y_component_name, y_element_display
    );
    let mut t = XyTrace::new(
        x_component_id,
        x_element_index,
        y_component_id,
        y_element_index,
        color,
    );
    t.label = SharedString::from(label);
    Some(t)
}
