//! "Copy value" and "Copy name" for anything that shows a component — the
//! outline, the browser's columns and detail rows, and every value strip.
//! One builder so each surface offers the same two rows in the same order.

use std::sync::Arc;

use gpui::{ClipboardItem, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use super::format::ValueFormatter;
use crate::inspector::rows::{CommandRow, InspectorRow};

/// The latest sample of `id` as the strip would show it — the whole value,
/// or one element of a vector. `None` before the first sample lands.
pub(crate) fn latest_value_text(
    db: &DB,
    id: ComponentId,
    element: Option<usize>,
) -> Option<String> {
    let formatter = ValueFormatter::resolve(db, id);
    db.with_state(|state| {
        let component = state.get_component(id)?;
        let latest = component.time_series.latest()?;
        let (_size, view) = component.schema.parse_value(latest.data()).ok()?;
        let text = match element {
            Some(index) => {
                let cells = formatter.format_cells(&view, &[]);
                // Strings and enums collapse to one cell whatever the index.
                let cell = if cells.len() == 1 {
                    &cells[0]
                } else {
                    cells.get(index)?
                };
                cell.value.to_string()
            }
            None => formatter.format(view),
        };
        Some(text.trim().to_string())
    })
}

/// Rows copying a component's current value and its full name. With an
/// `element`, the value row copies that element and names it.
pub(crate) fn copy_rows(
    db: Arc<DB>,
    id: ComponentId,
    name: SharedString,
    element: Option<usize>,
) -> Vec<Box<dyn InspectorRow>> {
    let value_label = match element {
        Some(index) => format!("Copy value [{index}]"),
        None => "Copy value".to_string(),
    };
    let name_for_copy = name.clone();
    vec![
        Box::new(CommandRow::new(
            SharedString::from(value_label),
            Arc::new(move |_window, cx| {
                if let Some(text) = latest_value_text(&db, id, element) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }),
        )),
        Box::new(CommandRow::new(
            "Copy name",
            Arc::new(move |_window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(name_for_copy.to_string()));
            }),
        )),
    ]
}

/// A single row copying a path that isn't a component — a branch, or a
/// pivot instance.
pub(crate) fn copy_name_row(name: SharedString) -> Box<dyn InspectorRow> {
    Box::new(CommandRow::new(
        "Copy name",
        Arc::new(move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
        }),
    ))
}
