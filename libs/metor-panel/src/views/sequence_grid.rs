//! Compact at-a-glance monitor for many sequence channels: a wrapping grid of small tiles,
//! one per channel, colored by run state. Clicking a tile opens its control page in the
//! inspector (Load / Start / Abort / Stop). All data comes from the global
//! [`SequenceStore`](crate::sequences::SequenceStore).
//!
//! Unlike the traffic-light grid, tiles need no per-cell stream tasks — the whole sequence
//! state already lives in the global store, so each render reads it directly.

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto_wkt::{ChannelId, SequenceRunState};

use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, run_state_index, run_state_label};
use crate::theme::theme;
use crate::views::TooltipText;
use crate::views::sequence_panel::channel_control_rows;

/// Compact grid of sequence channels.
#[derive(facet::Facet)]
pub struct SequenceGrid {}

impl SequenceGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {}
    }
}

struct Cell {
    id: ChannelId,
    name: SharedString,
    available: Vec<SharedString>,
    run_state: SequenceRunState,
    tooltip: SharedString,
}

impl Render for SequenceGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = sequences::try_global(cx);

        let cells: Vec<Cell> = match &store {
            None => Vec::new(),
            Some(store) => store
                .read(cx)
                .state()
                .channels_ordered()
                .iter()
                .map(|c| {
                    let mut tip = format!("{}: {}", c.name, run_state_label(c.run_state));
                    if let Some(loaded) = &c.loaded {
                        tip.push_str(&format!("\n{loaded}"));
                    }
                    if let Some(msg) = &c.last_message {
                        tip.push_str(&format!("\n{msg}"));
                    }
                    Cell {
                        id: c.id,
                        name: c.name.clone(),
                        available: c.available.clone(),
                        run_state: c.run_state,
                        tooltip: tip.into(),
                    }
                })
                .collect(),
        };

        let mut grid = div()
            .id("seq-grid")
            .size_full()
            .bg(theme.bg_primary)
            .text_color(theme.text_primary)
            .p(px(8.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(4.0))
            .content_start()
            .overflow_y_scroll();

        if cells.is_empty() {
            return grid.child(
                div()
                    .text_color(theme.text_tertiary)
                    .text_size(px(13.0))
                    .child(if store.is_none() {
                        "Sequence store unavailable"
                    } else {
                        "No channels declared"
                    }),
            );
        }

        for cell in cells {
            let color = theme.run_state_color(run_state_index(cell.run_state));
            let id = cell.id;
            let run_state = cell.run_state;
            let available = cell.available.clone();
            let tooltip = cell.tooltip.clone();

            grid = grid.child(
                div()
                    .id(("seq-grid-cell", id as usize))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_primary)
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .child(div().w(px(10.0)).h(px(10.0)).rounded(px(5.0)).bg(color))
                    .child(cell.name.clone())
                    .tooltip(move |_window, cx| TooltipText::build(tooltip.clone(), cx))
                    .on_click(cx.listener(move |_this, _, window, cx| {
                        let Some(open) = open_inspector(cx) else {
                            return;
                        };
                        let rows = channel_control_rows(id, run_state, available.clone());
                        open(
                            InspectorRequest {
                                rows,
                                mode: InspectorMode::Centered,
                            },
                            window,
                            cx,
                        );
                    })),
            );
        }

        grid
    }
}
