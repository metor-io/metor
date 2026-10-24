use std::sync::Arc;

use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};

use super::slider::SliderRow;
use super::{InspectorRow, RowAction, row_base, row_label};
use crate::icons::Icon;
use crate::theme::theme;

/// Row for [`Hsla`] fields.
///
/// Shows a color swatch and cascades into four slider rows (H, S, L, A) so
/// the user can tune each channel without leaving the inspector.
pub struct ColorRow {
    pub label: SharedString,
    pub read_color: Arc<dyn Fn(&App) -> Hsla>,
    pub on_change: Arc<dyn Fn(Hsla, &mut Window, &mut App)>,
}

/// The four sliders' labels and the channel each one edits.
const CHANNELS: [(&str, fn(&mut Hsla) -> &mut f32); 4] = [
    ("Hue", |c| &mut c.h),
    ("Saturation", |c| &mut c.s),
    ("Lightness", |c| &mut c.l),
    ("Alpha", |c| &mut c.a),
];

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
            .child(row_label(self.label.clone(), theme.text_primary))
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
                            .bg((self.read_color)(cx)),
                    )
                    .child(Icon::ChevronRight.svg(8.0, theme.text_secondary)),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Cascade(
            CHANNELS
                .into_iter()
                .map(|(label, channel)| {
                    let read = self.read_color.clone();
                    let write = self.read_color.clone();
                    let on_change = self.on_change.clone();
                    Box::new(SliderRow {
                        label: label.into(),
                        read_value: Arc::new(move |cx| *channel(&mut read(cx)) as f64),
                        min: 0.0,
                        max: 1.0,
                        on_change: Arc::new(move |v, w, cx| {
                            let mut color = write(cx);
                            *channel(&mut color) = v as f32;
                            on_change(color, w, cx);
                        }),
                    }) as Box<dyn InspectorRow>
                })
                .collect(),
        )
    }
}
