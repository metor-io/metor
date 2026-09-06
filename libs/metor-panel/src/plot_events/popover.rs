//! Shared event readout styling for plots and timeline previews.
use super::PlotEvent;
use crate::views::time_series::LABEL_FONT_SIZE;
use gpui::{SharedString, div, prelude::*, px};

pub(crate) fn event_card<'a>(
    header: SharedString,
    events: impl Iterator<Item = &'a PlotEvent>,
    theme: &crate::theme::Theme,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_y_0()
        .px(px(6.0))
        .py(px(4.0))
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(3.0))
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_secondary)
                .child(header),
        )
        .children(events.map(|event| event_summary_row(event, theme)))
}

pub(crate) fn event_summary_row(ev: &PlotEvent, theme: &crate::theme::Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(crate::views::format::format_time(
                    ev.ts.0,
                ))),
        )
        .child(div().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(ev.color))
        .child(
            div()
                .flex_1()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_secondary)
                .child(ev.label.clone()),
        )
}
