//! Glue for hosting a transient [`TimeSeriesPlot`] inside an inspector
//! view page. Lives here so the palette "Plot Component" flow and the
//! shift-hover surfaces produce visually identical previews.

use std::sync::Arc;

use gpui::{App, AppContext, MouseMoveEvent, Pixels, Size, Window, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use crate::inspector::rows::PreviewSpec;
use crate::tiles::PreviewPlotAction;
use crate::views::time_series::{TimeSeriesPlot, Trace, traces_for_component};

pub const PREVIEW_SIZE: Size<Pixels> = Size {
    width: px(560.0),
    height: px(320.0),
};

/// Build the inspector page spec for a plot of `traces`.
pub fn build_plot_preview(db: Arc<DB>, traces: Vec<Trace>, cx: &mut App) -> PreviewSpec {
    let entity = cx.new(|cx| TimeSeriesPlot::new(db, traces, cx));
    let label = entity.read(cx).title(cx);
    PreviewSpec {
        view: entity.into(),
        size: PREVIEW_SIZE,
        label,
    }
}

/// Resolve `indices` to a trace list, defaulting to *all* elements when
/// `indices` is empty (the convention for whole-component previews).
pub fn preview_traces(
    db: &DB,
    component_id: ComponentId,
    indices: &[usize],
    cx: &App,
) -> Vec<Trace> {
    if indices.is_empty() {
        let elem_count = crate::inspector::trace_picker::element_names_for_component(db, component_id)
            .len()
            .max(1);
        let all: Vec<usize> = (0..elem_count).collect();
        traces_for_component(db, component_id, &all, cx)
    } else {
        traces_for_component(db, component_id, indices, cx)
    }
}

/// Returns an `on_mouse_move` listener that dispatches [`PreviewPlotAction`]
/// while shift is held. Empty `indices` previews every element of the
/// component; pass `smallvec![idx]` for per-element granularity.
///
/// Sharing this factory keeps every shift-hover surface byte-identical so
/// the AppRoot dedup key stays consistent.
pub fn shift_hover_listener(
    component_id: ComponentId,
    indices: SmallVec<[usize; 4]>,
) -> impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        if event.modifiers.shift {
            window.dispatch_action(
                Box::new(PreviewPlotAction {
                    component_id,
                    indices: indices.clone(),
                    anchor: event.position,
                }),
                cx,
            );
        }
    }
}
