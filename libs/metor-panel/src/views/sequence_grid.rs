//! Compact at-a-glance monitor for many sequence channels: a wrapping grid of small tiles,
//! one per channel, colored by channel state. Clicking a tile opens its control page in the
//! inspector, offering exactly the commands the channel would accept. Channels come from
//! the global [`SequenceStore`](crate::sequences::SequenceStore) and each one's state from
//! its shared [`ChannelStatusReader`], at the selected time.

use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto_wkt::{SequenceAccepts, SequenceChannelState};

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, ChannelReaders, ChannelStatusReader, state_label};
use crate::theme::theme;
use crate::views::TooltipText;

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
        if let Some(message) = &status.message {
            tip.push_str(&format!("\n{message}"));
        }
        Some(Self {
            name: channel.name.clone(),
            available: channel.available.clone(),
            accepts: reader.accepts(),
            state: status.state,
            tooltip: tip.into(),
        })
    }
}

impl Render for SequenceGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = sequences::try_global(cx);
        self.readers.sync(&sequences::channel_names(cx), cx);
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

/// Inspector rows listing the sequences loadable into the channel named `channel`;
/// selecting one publishes a load command.
fn load_picker_rows(
    channel: SharedString,
    available: &[SharedString],
) -> Vec<Box<dyn InspectorRow>> {
    available
        .iter()
        .map(|name| {
            let name = name.clone();
            let channel = channel.clone();
            Box::new(CommandRow::new(
                name.clone(),
                Arc::new(move |_window, cx| {
                    if let Some(store) = sequences::try_global(cx) {
                        store.read(cx).load(&channel, name.to_string());
                    }
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Inspector control page for one channel: exactly the commands it would accept now
/// (`accepts`), so the page never offers what the target would refuse. `Stop` drills
/// into a confirmation row since it is the unsafe hard-drop.
fn channel_control_rows(
    channel: SharedString,
    accepts: SequenceAccepts,
    available: Vec<SharedString>,
) -> Vec<Box<dyn InspectorRow>> {
    let command = |label: &'static str, act: fn(&sequences::SequenceStore, &str)| {
        let channel = channel.clone();
        Box::new(CommandRow::new(
            label,
            Arc::new(move |_window, cx| {
                if let Some(store) = sequences::try_global(cx) {
                    act(store.read(cx), &channel);
                }
            }),
        )) as Box<dyn InspectorRow>
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    if accepts.contains(SequenceAccepts::LOAD) && !available.is_empty() {
        let channel = channel.clone();
        rows.push(Box::new(NavRow::new(
            "Load",
            "",
            Box::new(move |_cx| load_picker_rows(channel.clone(), &available)),
        )));
    }
    if accepts.contains(SequenceAccepts::START) {
        rows.push(command("Start", |s, c| s.start(c)));
    }
    if accepts.contains(SequenceAccepts::RESET) {
        rows.push(command("Reset", |s, c| s.reset(c)));
    }
    if accepts.contains(SequenceAccepts::ABORT) {
        rows.push(command("Abort", |s, c| s.abort(c)));
    }
    if accepts.contains(SequenceAccepts::STOP) {
        rows.push(Box::new(NavRow::new(
            "Stop",
            "",
            Box::new(move |_cx| {
                let channel = channel.clone();
                vec![Box::new(CommandRow::new(
                    "Confirm stop — may leave the system unsafe",
                    Arc::new(move |_window, cx| {
                        if let Some(store) = sequences::try_global(cx) {
                            store.read(cx).stop(&channel);
                        }
                    }),
                )) as Box<dyn InspectorRow>]
            }),
        )));
    }
    rows
}
