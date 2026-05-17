//! Build the inspector row list for a [`MeasurementCursor`].
//!
//! Readouts (Δt, Δy, Mean, …) live in the on-plot badge — the inspector is
//! a thin config panel: toggle which measurements appear and delete the
//! cursor. Cursors persist by default; there is no lock/unlock toggle.

use std::sync::Arc;

use gpui::{Entity, SharedString};

use super::cursor::MeasurementCursor;
use super::measurements::MeasurementKind;
use crate::inspector::rows::{ActionRow, BoolRow, InspectorRow, NavRow, RowAction};

/// Top-level rows for `cursor`. Returned via the registered type builder, so
/// the caller is `rows_for_any_entity` and the inspector handles cascade /
/// dismiss as usual.
pub fn build_cursor_rows(
    cursor: Entity<MeasurementCursor>,
    _cx: &gpui::App,
) -> Vec<Box<dyn InspectorRow>> {
    vec![
        Box::new(measurements_nav_row(cursor.clone())),
        Box::new(delete_action_row(cursor)),
    ]
}

fn measurements_nav_row(cursor: Entity<MeasurementCursor>) -> NavRow {
    let cursor_for_summary = cursor.clone();
    NavRow::with_dynamic_summary(
        "Measurements",
        Box::new(move |cx| {
            let count = cursor_for_summary.read(cx).enabled.len();
            SharedString::from(format!("{count} enabled"))
        }),
        Box::new(move |_cx| {
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
            for kind in MeasurementKind::ALL {
                let read_cursor = cursor.clone();
                let write_cursor = cursor.clone();
                rows.push(Box::new(BoolRow::dynamic(
                    kind.label(),
                    Arc::new(move |cx| read_cursor.read(cx).has_kind(kind)),
                    Arc::new(move |on, _window, cx| {
                        write_cursor.update(cx, |c, cx| c.toggle_kind(kind, on, cx));
                    }),
                )));
            }
            rows
        }),
    )
}

fn delete_action_row(cursor: Entity<MeasurementCursor>) -> ActionRow {
    ActionRow::new(
        "Delete cursor",
        Arc::new(move |_window, cx| {
            let (host, id) = {
                let c = cursor.read(cx);
                (c.host.clone(), c.id)
            };
            if let Some(host) = host.upgrade() {
                host.update(cx, |plot, cx| plot.remove_cursor(id, cx));
            }
            RowAction::Dismiss
        }),
    )
}
