//! Floating inspectors: the frosted card the panel's context menus and palette are drawn on,
//! with rows of choices, opened at a point over everything else.
//!
//! The host owns whether a menu is open and where. It draws the card through [`floating`] and
//! closes it from the card's `on_mouse_down_out`.

use gpui::{
    Anchor, AnimationExt, AnyElement, App, Div, ElementId, IntoElement, Pixels, Point,
    SharedString, Stateful, anchored, deferred, div, prelude::*, px,
};

use crate::row::{ROW_HEIGHT, row_base, row_label};
use crate::{Icon, SURFACE_BLUR, motion, theme};

/// Corner radius of a floating inspector, as the panel's.
pub const MENU_RADIUS: f32 = 6.0;

/// The inspector card: a frosted, bordered column of rows.
pub fn menu_card(cx: &App) -> Div {
    let theme = theme(cx);
    div()
        .flex()
        .flex_col()
        .py(px(2.0))
        .min_w(px(200.0))
        .rounded(px(MENU_RADIUS))
        .border_1()
        .border_color(theme.border_primary)
        .bg(theme.surface_bg())
        .backdrop_blur(SURFACE_BLUR)
        .shadow_sm()
        .text_color(theme.text_secondary)
}

/// A small heading over a group of rows.
pub fn menu_heading(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT - 6.0))
        .px(px(12.0))
        .text_size(px(11.0))
        .text_color(theme(cx).text_tertiary)
        .child(label.into())
}

/// A rule between groups of rows.
pub fn menu_separator(cx: &App) -> Div {
    div()
        .h(px(1.0))
        .mx(px(8.0))
        .my(px(3.0))
        .bg(theme(cx).border_primary)
}

/// A row that acts on click, labelled `label`, with a check at its end when `checked`.
///
/// `ix` keys its hover highlight; rows in one card need distinct ones.
pub fn menu_item(
    ix: usize,
    label: impl Into<SharedString>,
    checked: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = theme(cx);
    row_base(ix, false, cx)
        .child(row_label(label, theme.text_primary).relative())
        .when(checked, |row| {
            row.child(
                div()
                    .relative()
                    .child(Icon::Check.svg(12.0, theme.text_secondary)),
            )
        })
}

/// `card` at `position`, in window coordinates, or where it is laid out when `None`, drawn above
/// the rest of the window and kept inside it, fading in as it opens.
///
/// `id` keys the fade; give each opening a new one, such as one counting the openings.
pub fn floating(
    id: impl Into<ElementId>,
    position: Option<Point<Pixels>>,
    anchor: Anchor,
    card: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let card = div().child(card);
    let card = if motion::enabled(cx) {
        card.with_animation(
            id,
            gpui::Animation::new(motion::MENU_ENTER).with_easing(gpui::ease_out_quint()),
            |card, t| card.opacity(t),
        )
        .into_any_element()
    } else {
        card.into_any_element()
    };
    let anchored = match position {
        Some(position) => anchored().position(position),
        None => anchored(),
    };
    deferred(
        anchored
            .anchor(anchor)
            .snap_to_window_with_margin(px(8.0))
            .child(card),
    )
    .with_priority(1)
    .into_any_element()
}
