//! Row chrome shared by lists, menus and inspectors: the inset selection pill, labels, values
//! and tag pills.

use gpui::{App, Global, Hsla, Pixels, SharedString, Window, div, prelude::*, px};

use crate::{CONTROL_HEIGHT, CONTROL_RADIUS, theme};

/// Height of every row, so editing, header and ordinary rows stack uniformly and hosts can size
/// their containers from a row count.
pub const ROW_HEIGHT: f32 = CONTROL_HEIGHT;

/// The label font: what every row's text is set in.
pub const LABEL_SIZE: Pixels = px(12.0);

/// Small pill used for category and tag annotations next to a row label.
pub fn tag_pill(tag: SharedString, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    div()
        .px(px(6.0))
        .py(px(1.0))
        .bg(theme.pill_bg)
        .border_1()
        .border_color(theme.pill_border)
        .rounded(px(3.0))
        .text_size(px(10.0))
        .text_color(theme.text_secondary)
        .child(tag)
}

/// Width of `text` at `size` in the window's font.
pub fn measure(text: &str, size: Pixels, window: &Window) -> Pixels {
    let run = gpui::TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color: Hsla::transparent_black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_string()), size, &[run], None)
        .width
}

/// A row's leading label in the row font.
pub fn row_label(label: impl Into<SharedString>, color: Hsla) -> gpui::Div {
    div()
        .text_size(LABEL_SIZE)
        .text_color(color)
        .child(label.into())
}

/// A row's trailing value: right-aligned, taking the remaining width, and
/// ellipsised when it doesn't fit.
pub fn row_value(value: impl Into<SharedString>, color: Hsla) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .text_size(LABEL_SIZE)
        .text_color(color)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_right()
        .child(value.into())
}

/// Row-chrome the concrete widgets wrap: background, hover, spacing, and id.
///
/// The selection/hover highlight is painted as an inset rounded pill behind
/// the content rather than a full-bleed fill. This keeps the highlight clear
/// of the panel's rounded corners (gpui content masks are rectangular, so a
/// full-width fill would bleed past the curve).
pub fn row_base(row_ix: usize, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme(cx);
    let interactive = !passive(cx);
    let pill_bg = if selected {
        theme.selection_bg
    } else {
        Hsla::transparent_black()
    };
    div()
        .id(("inspector-row", row_ix))
        .when(interactive, |row| {
            row.group("inspector-row").cursor_pointer()
        })
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(12.0))
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .bottom(px(2.0))
                .left(px(4.0))
                .right(px(4.0))
                .rounded(px(CONTROL_RADIUS))
                .bg(pill_bg)
                .when(interactive, |pill| {
                    pill.group_hover("inspector-row", |s| s.bg(theme.selection_bg))
                }),
        )
}

#[derive(Default)]
struct PassiveRows(bool);

impl Global for PassiveRows {}

/// Whether rows are being built only as the visual of a dismissed overlay, so they show no
/// hover or pointer.
pub fn passive(cx: &App) -> bool {
    cx.try_global::<PassiveRows>().is_some_and(|mode| mode.0)
}

/// Build rows in [`passive`] mode.
pub fn with_passive<R>(cx: &mut App, build: impl FnOnce(&mut App) -> R) -> R {
    let previous = passive(cx);
    cx.set_global(PassiveRows(true));
    let result = build(cx);
    cx.set_global(PassiveRows(previous));
    result
}
