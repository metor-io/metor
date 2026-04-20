use std::sync::Arc;

use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};

use super::slider::SliderRow;
use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// Row for [`Hsla`] fields.
///
/// Shows a color swatch and cascades into four slider rows (H, S, L, A) so
/// the user can tune each channel without leaving the inspector.
pub struct ColorRow {
    pub label: SharedString,
    pub color: Hsla,
    pub read_color: Arc<dyn Fn(&App) -> Hsla>,
    pub on_change: Arc<dyn Fn(Hsla, &mut Window, &mut App)>,
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

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        let read = self.read_color.clone();

        let (hue_read, hue_set) = (read.clone(), read.clone());
        let (sat_read, sat_set) = (read.clone(), read.clone());
        let (lit_read, lit_set) = (read.clone(), read.clone());
        let (alpha_read, alpha_set) = (read.clone(), read.clone());
        let hue_cb = on_change.clone();
        let sat_cb = on_change.clone();
        let lit_cb = on_change.clone();
        let alpha_cb = on_change.clone();

        RowAction::Cascade(vec![
            Box::new(SliderRow {
                label: "Hue".into(),
                read_value: Arc::new(move |cx| hue_read(cx).h as f64),
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    let cur = hue_set(cx);
                    hue_cb(Hsla { h: v as f32, ..cur }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Saturation".into(),
                read_value: Arc::new(move |cx| sat_read(cx).s as f64),
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    let cur = sat_set(cx);
                    sat_cb(Hsla { s: v as f32, ..cur }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Lightness".into(),
                read_value: Arc::new(move |cx| lit_read(cx).l as f64),
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    let cur = lit_set(cx);
                    lit_cb(Hsla { l: v as f32, ..cur }, w, cx);
                }),
            }),
            Box::new(SliderRow {
                label: "Alpha".into(),
                read_value: Arc::new(move |cx| alpha_read(cx).a as f64),
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    let cur = alpha_set(cx);
                    alpha_cb(Hsla { a: v as f32, ..cur }, w, cx);
                }),
            }),
        ])
    }
}
