use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, DragMoveEvent, Empty, Render, SharedString, Window, canvas,
    div, fill, point, prelude::*, px, size,
};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

const SLIDER_HEIGHT: f32 = 14.0;
const SLIDER_TRACK_HEIGHT: f32 = 4.0;
const SLIDER_HANDLE_SIZE: f32 = 10.0;

struct SliderDrag {
    min: f64,
    max: f64,
    on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
}

impl Render for SliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        Empty
    }
}

/// Inspector row for a bounded numeric field, rendered as a draggable slider.
///
/// Values are rounded to two decimal places on each drag tick to keep
/// callbacks debounced and the displayed value stable.
pub struct SliderRow {
    pub label: SharedString,
    pub read_value: Arc<dyn Fn(&App) -> f64>,
    pub min: f64,
    pub max: f64,
    pub on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
}

impl InspectorRow for SliderRow {
    fn supports_exit_fade(&self) -> bool {
        true
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let min = self.min;
        let max = self.max;
        let val = (self.read_value)(cx);
        let fraction = if max > min {
            ((val - min) / (max - min)).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        let track_color = theme.border_primary;
        let fill_color = theme.line_color;
        let handle_color = theme.text_primary;

        let on_change_drag = self.on_change.clone();

        let slider = div()
            .id(("slider", row_ix))
            .w(px(100.0))
            .h(px(SLIDER_HEIGHT))
            .when(!super::passive(cx), |slider| {
                slider
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_drag(
                        SliderDrag {
                            min,
                            max,
                            on_change: on_change_drag,
                        },
                        |drag, _, _, cx| {
                            cx.new(|_| SliderDrag {
                                min: drag.min,
                                max: drag.max,
                                on_change: drag.on_change.clone(),
                            })
                        },
                    )
                    .on_drag_move(move |event: &DragMoveEvent<SliderDrag>, window, cx| {
                        let drag = event.drag(cx);
                        let bounds = event.bounds;
                        let rel_x = f32::from(event.event.position.x - bounds.origin.x);
                        let width = f32::from(bounds.size.width);
                        let frac = (rel_x / width).clamp(0.0, 1.0) as f64;
                        let new_val = drag.min + frac * (drag.max - drag.min);
                        let rounded = (new_val * 100.0).round() / 100.0;
                        let cb = drag.on_change.clone();
                        cb(rounded, window, cx);
                    })
            })
            .child(
                canvas(
                    move |bounds, _window, _cx| (bounds, fraction),
                    move |_, (bounds, fraction), window, _cx| {
                        let track_y =
                            bounds.origin.y + px((SLIDER_HEIGHT - SLIDER_TRACK_HEIGHT) / 2.0);
                        let track_bounds = Bounds::new(
                            point(bounds.origin.x, track_y),
                            size(bounds.size.width, px(SLIDER_TRACK_HEIGHT)),
                        );
                        window.paint_quad(fill(track_bounds, track_color));

                        let fill_w = bounds.size.width * fraction;
                        if fill_w > px(0.0) {
                            let fill_bounds = Bounds::new(
                                point(bounds.origin.x, track_y),
                                size(fill_w, px(SLIDER_TRACK_HEIGHT)),
                            );
                            window.paint_quad(fill(fill_bounds, fill_color));
                        }

                        let handle_x = bounds.origin.x + fill_w - px(SLIDER_HANDLE_SIZE / 2.0);
                        let handle_y =
                            bounds.origin.y + px((SLIDER_HEIGHT - SLIDER_HANDLE_SIZE) / 2.0);
                        let handle_bounds = Bounds::new(
                            point(handle_x, handle_y),
                            size(px(SLIDER_HANDLE_SIZE), px(SLIDER_HANDLE_SIZE)),
                        );
                        let mut handle_quad = fill(handle_bounds, handle_color);
                        handle_quad.corner_radii = gpui::Corners::all(px(SLIDER_HANDLE_SIZE / 2.0));
                        window.paint_quad(handle_quad);
                    },
                )
                .w_full()
                .h(px(SLIDER_HEIGHT)),
            );

        let value_text = SharedString::from(format!("{:.2}", val));

        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(slider)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_secondary)
                            .min_w(px(36.0))
                            .child(value_text),
                    ),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Handled
    }
}
