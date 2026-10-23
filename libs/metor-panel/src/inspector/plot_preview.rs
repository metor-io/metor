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
use crate::views::image_view::expands;
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
/// `indices` is empty (the convention for whole-component previews). A
/// component that does not [`expands`] has no whole-component preview.
pub fn preview_traces(
    db: &DB,
    component_id: ComponentId,
    indices: &[usize],
    cx: &App,
) -> Vec<Trace> {
    if indices.is_empty() && !expands(db, component_id) {
        return Vec::new();
    }
    if indices.is_empty() {
        let elem_count =
            crate::inspector::trace_picker::element_names_for_component(db, component_id)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::views::image_view::tests::{COLLAPSED, image_db};

    #[gpui::test]
    fn collapsed_components_have_no_whole_preview(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = image_db();
        cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            for name in COLLAPSED {
                let id = ComponentId::new(name);
                assert!(preview_traces(&db, id, &[], cx).is_empty(), "{name}");
                assert_eq!(preview_traces(&db, id, &[3], cx).len(), 1, "{name}");
            }
            let exposure = ComponentId::new("cam.exposure");
            assert_eq!(preview_traces(&db, exposure, &[], cx).len(), 1);
        });
    }
}
