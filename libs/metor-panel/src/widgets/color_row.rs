use std::sync::Arc;

use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};

use super::slider_row::SliderRow;
use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// Color swatch row that cascades to H/S/L/A slider sub-rows.
pub struct ColorRow {
    pub label: SharedString,
    pub color: Hsla,
    pub on_change: Arc<dyn Fn(Hsla, &mut Window, &mut App) + Send + Sync>,
}

impl InspectorRow for ColorRow {
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
                    .child(
                        div()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded(px(3.0))
                            .bg(self.color),
                    )
                    .child(Icon::ChevronRight.svg(8.0)),
            )
            .into_any_element()
    }

    fn activate(&self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let color = self.color;
        let on_change = self.on_change.clone();

        let hue_cb = on_change.clone();
        let sat_cb = on_change.clone();
        let lit_cb = on_change.clone();
        let alpha_cb = on_change.clone();

        RowAction::Cascade(vec![
            Box::new(SliderRow {
                label: "Hue".into(),
                value: color.h as f64,
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    hue_cb(Hsla { h: v as f32, ..color }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Saturation".into(),
                value: color.s as f64,
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    sat_cb(Hsla { s: v as f32, ..color }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Lightness".into(),
                value: color.l as f64,
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    lit_cb(Hsla { l: v as f32, ..color }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Alpha".into(),
                value: color.a as f64,
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    alpha_cb(Hsla { a: v as f32, ..color }, w, cx);
                }),
            }),
        ])
    }
}
