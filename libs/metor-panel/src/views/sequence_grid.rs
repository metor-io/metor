//! Compact at-a-glance monitor for many sequence channels: a wrapping grid of small tiles,
//! one per channel, colored by channel state. Clicking a tile opens its control page in the
//! inspector, offering exactly the commands the channel would accept. Channels come from
//! the global [`SequenceStore`](crate::sequences::SequenceStore) and each one's state from
//! its shared [`ChannelStatusReader`], at the selected time.

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto_wkt::{SequenceAccepts, SequenceChannelState};

use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, ChannelReaders, ChannelStatusReader, state_label};
use crate::theme::theme;
use crate::views::TooltipText;
use crate::views::sequence_panel::channel_control_rows;

/// Compact grid of sequence channels.
#[derive(facet::Facet)]
pub struct SequenceGrid {
    #[facet(opaque)]
    readers: ChannelReaders,
}

impl SequenceGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            readers: ChannelReaders::default(),
        }
    }
}

/// One channel tile. The name is the channel's identity — the address every published
/// command carries.
struct Cell {
    name: SharedString,
    available: Vec<SharedString>,
    /// What the click-through page offers. Scrubbed, the tile shows the
    /// past's state but offers no commands against it.
    accepts: SequenceAccepts,
    state: Option<SequenceChannelState>,
    tooltip: SharedString,
}

impl Cell {
    fn of(reader: &ChannelStatusReader) -> Option<Self> {
        let channel = reader.channel()?;
        let status = reader.status();
        let mut tip = format!("{}: {}", channel.name, state_label(status.state));
        match channel.sequence(status.loaded) {
            Some(loaded) => tip.push_str(&format!("\n{loaded}")),
            None if channel.available.is_empty() => tip.push_str(&format!("\n{}", channel.system)),
            None => {}
        }
        if let Some(message) = status.message.as_ref().filter(|m| !m.is_empty()) {
            tip.push_str(&format!("\n{message}"));
        }
        Some(Self {
            name: channel.name.clone(),
            available: channel.available.clone(),
            accepts: if reader.live() {
                status.accepts
            } else {
                SequenceAccepts::default()
            },
            state: status.state,
            tooltip: tip.into(),
        })
    }
}

impl Render for SequenceGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = sequences::try_global(cx);
        let names: Vec<SharedString> = store
            .as_ref()
            .map(|store| {
                store
                    .read(cx)
                    .state()
                    .channels_ordered()
                    .iter()
                    .map(|c| c.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.readers.sync(&names, cx);
        let cells: Vec<Cell> = self
            .readers
            .iter()
            .filter_map(|reader| Cell::of(reader.read(cx)))
            .collect();

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

        for (ix, cell) in cells.into_iter().enumerate() {
            let Cell {
                name,
                available,
                accepts,
                state,
                tooltip,
            } = cell;
            grid = grid.child(
                div()
                    .id(("seq-grid-cell", ix))
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
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded(px(5.0))
                            .bg(theme.run_state_color(state)),
                    )
                    .child(name.clone())
                    .tooltip(move |_window, cx| TooltipText::build(tooltip.clone(), cx))
                    .on_click(cx.listener(move |_this, _, window, cx| {
                        let Some(open) = open_inspector(cx) else {
                            return;
                        };
                        let rows = channel_control_rows(name.clone(), accepts, available.clone());
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
