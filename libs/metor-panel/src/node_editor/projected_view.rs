//! Painting a [`Projection`].
//!
//! Read-only, and it looks it: cards carry no drag affordance, sockets no drop
//! target, edges no selection. That restraint is the phase's point — the
//! projection has to be right on real programs before any gesture is allowed
//! to rewrite the source behind it.
//!
//! Chrome is native `div` and theme tokens; only the edges are painted, because
//! only they are raw geometry.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, Bounds, Hsla, IntoElement, Pixels, canvas, div, point, prelude::*, px,
};

use crate::graph_canvas::paint_bezier;
use crate::node_editor::projection::{Card, Projection};
use crate::theme::theme;

const CARD_WIDTH: f32 = 200.0;
const HEADER_HEIGHT: f32 = 24.0;
const SOCKET_ROW_HEIGHT: f32 = 18.0;

/// Where an edge attaches, in graph coordinates.
fn socket_anchor(card: &Card, row: usize, right: bool) -> gpui::Point<Pixels> {
    let inputs = card.inputs.len();
    let offset = HEADER_HEIGHT
        + SOCKET_ROW_HEIGHT * (if right { inputs + row } else { row }) as f32
        + SOCKET_ROW_HEIGHT / 2.0;
    point(
        px(card.position.x + if right { CARD_WIDTH } else { 0.0 }),
        px(card.position.y + offset),
    )
}

/// The whole projection as one element: edges under cards, both offset by the
/// pane's viewport.
pub fn render(
    projection: &Projection,
    viewport: (f32, f32),
    cx: &mut App,
) -> AnyElement {
    let theme = theme(cx);
    let (dx, dy) = viewport;

    let mut wires: Vec<(gpui::Point<Pixels>, gpui::Point<Pixels>)> = Vec::new();
    for edge in &projection.edges {
        let (Some(from), Some(to)) = (
            projection.cards.get(edge.producer),
            projection.cards.get(edge.consumer),
        ) else {
            continue;
        };
        let source = socket_anchor(from, edge.producer_field, true);
        let target = socket_anchor(to, edge.consumer_port, false);
        wires.push((
            point(source.x - px(dx), source.y - px(dy)),
            point(target.x - px(dx), target.y - px(dy)),
        ));
    }

    let edge_color = theme.text_tertiary;
    let mut root = div().relative().size_full().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds: Bounds<Pixels>, window, _cx| {
                for (source, target) in &wires {
                    paint_bezier(bounds.origin, *source, *target, edge_color, false, window);
                }
            },
        )
        .absolute()
        .size_full(),
    );

    for card in &projection.cards {
        root = root.child(
            div()
                .absolute()
                .left(px(card.position.x - dx))
                .top(px(card.position.y - dy))
                .w(px(CARD_WIDTH))
                .child(card_element(card, cx)),
        );
    }
    root.into_any_element()
}

fn card_element(card: &Card, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    let mut body = div()
        .flex()
        .flex_col()
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(4.0))
        .child(
            div()
                .h(px(HEADER_HEIGHT))
                .px_2()
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(theme.border_primary)
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_primary)
                        .child(gpui::SharedString::from(card.name.clone())),
                )
                // Says what this canvas is: derived from text, and not
                // editable until the phase that makes it so.
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(theme.text_tertiary)
                        .child(gpui::SharedString::new_static("read-only")),
                ),
        );

    for socket in &card.inputs {
        body = body.child(socket_row(&socket.name, &socket.detail, false, theme.line_color, cx));
    }
    for socket in &card.outputs {
        body = body.child(socket_row(
            &socket.name,
            &socket.detail,
            true,
            theme.control_active,
            cx,
        ));
    }
    body
}

fn socket_row(
    name: &str,
    detail: &str,
    right: bool,
    dot: Hsla,
    cx: &App,
) -> impl IntoElement {
    let theme = theme(cx);
    let label = div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_secondary)
                .child(gpui::SharedString::from(name.to_string())),
        )
        .child(
            div()
                .text_size(px(9.0))
                .text_color(theme.text_tertiary)
                .child(gpui::SharedString::from(detail.to_string())),
        );
    let marker = div().w(px(6.0)).h(px(6.0)).rounded_full().bg(dot);

    let row = div()
        .h(px(SOCKET_ROW_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden();
    match right {
        true => row.justify_end().child(label).child(marker),
        false => row.child(marker).child(label),
    }
}

/// Positions a pane remembers, keyed by system name.
pub type Placements = HashMap<String, crate::node_editor::projection::Position>;
