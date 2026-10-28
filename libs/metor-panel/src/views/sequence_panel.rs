//! Detailed per-channel control panel for sequences (best for a handful of channels).
//! Each row is a [`SequenceControl`] for one channel, so the list and the single-channel
//! widget stage loads, arm Stop and lay out their buttons the same way; a header carries a
//! global Reload and channel-state count chips. Channels come from the global
//! [`SequenceStore`](crate::sequences::SequenceStore), their states from their shared
//! [`ChannelStatusReader`](crate::sequences::ChannelStatusReader)s, and the History tab
//! from [`SequenceHistory`]; this view only renders and publishes commands.

use std::sync::Arc;

use gpui::{AnyElement, Context, Entity, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{SequenceAccepts, SequenceChannelState};

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::sequences::{
    self, ChannelReaders, HistoryEntry, SequenceHistory, format_age, state_label,
};
use crate::theme::{theme, ui_px};
use crate::views::sequence_control::{SequenceControl, SequenceControlConfig, SequenceLayout};
use crate::views::table;

/// Size of one square control in the channel grid, in unscaled pixels.
const SQUARE_TILE: (f32, f32) = (260.0, 132.0);

/// Channel-state chips to surface in the header, in display order. `Unloaded` is
/// omitted as noise.
const CHIP_STATES: [SequenceChannelState; 7] = [
    SequenceChannelState::Running,
    SequenceChannelState::Quiescing,
    SequenceChannelState::Failed,
    SequenceChannelState::Panicked,
    SequenceChannelState::Stopped,
    SequenceChannelState::Aborted,
    SequenceChannelState::Completed,
];

/// Which set the sequence panel shows. Switched from the inspector or the header toggle.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, facet::Facet)]
#[repr(u8)]
pub enum SequenceListMode {
    #[default]
    Channels,
    History,
}

/// The sequence control view. Its [`SequenceListMode`] is editable through the inspector.
#[derive(facet::Facet)]
pub struct SequenceView {
    pub mode: SequenceListMode,
    /// How each channel's control lays out. Square controls tile in a wrapping grid;
    /// the others stack as rows.
    pub layout: SequenceLayout,
    /// One control per declared channel, in display order. Kept across renders so a
    /// staged load or an armed Stop survives the store's next event.
    #[facet(opaque)]
    controls: Vec<(SharedString, Entity<SequenceControl>)>,
    /// Every declared channel's status, for the header's counts.
    #[facet(opaque)]
    readers: ChannelReaders,
}

impl SequenceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Repaint whenever the control system declares channels or a channel
        // raises a message.
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        if let Some(history) = SequenceHistory::global(cx) {
            cx.observe(&history, |_, _, cx| cx.notify()).detach();
        }
        Self {
            mode: SequenceListMode::default(),
            layout: SequenceLayout::Medium,
            controls: Vec::new(),
            readers: ChannelReaders::default(),
        }
    }

    /// Restore a persisted panel to the history or channels tab.
    pub fn set_history(&mut self, history: bool) {
        self.mode = if history {
            SequenceListMode::History
        } else {
            SequenceListMode::Channels
        };
    }

    /// Whether the history tab is showing, for persistence.
    pub fn is_history(&self) -> bool {
        self.mode == SequenceListMode::History
    }
}

/// Inspector rows listing the sequences loadable into the channel named `channel`;
/// selecting one publishes a load command. Shared by this panel's `Load` button and the
/// grid's control page.
pub(crate) fn load_picker_rows(
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
/// into a confirmation row since it is the unsafe hard-drop. Used by the grid's
/// click-through.
pub(crate) fn channel_control_rows(
    channel: SharedString,
    accepts: SequenceAccepts,
    available: Vec<SharedString>,
) -> Vec<Box<dyn InspectorRow>> {
    let allows = |bit| accepts.0 & bit != 0;
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
    if allows(SequenceAccepts::LOAD) && !available.is_empty() {
        let channel = channel.clone();
        rows.push(Box::new(NavRow::new(
            "Load",
            "",
            Box::new(move |_cx| load_picker_rows(channel.clone(), &available)),
        )));
    }
    if allows(SequenceAccepts::START) {
        rows.push(command("Start", |s, c| s.start(c)));
    }
    if allows(SequenceAccepts::RESET) {
        rows.push(command("Reset", |s, c| s.reset(c)));
    }
    if allows(SequenceAccepts::ABORT) {
        rows.push(command("Abort", |s, c| s.abort(c)));
    }
    if allows(SequenceAccepts::STOP) {
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

impl Render for SequenceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let history = self.mode == SequenceListMode::History;
        let store = sequences::try_global(cx);

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(table::HEADER_HEIGHT))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_primary)
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_tertiary)
                    // Scrubbed, the rows show the selected time's state and
                    // their controls are disabled; the header says which.
                    .child(if crate::temporal::is_live(cx) {
                        "Live"
                    } else {
                        "Past · read-only"
                    }),
            );

        let channels: Vec<SharedString> = store
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
        self.readers.sync(&channels, cx);
        let states: Vec<Option<SequenceChannelState>> = self
            .readers
            .iter()
            .map(|reader| reader.read(cx).status().state)
            .collect();
        for chip in CHIP_STATES {
            let n = states.iter().filter(|s| **s == Some(chip)).count();
            if n == 0 {
                continue;
            }
            header = header.child(
                div()
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.run_state_tint(Some(chip)))
                    .text_color(theme.run_state_color(Some(chip)))
                    .text_size(px(11.0))
                    .child(SharedString::from(format!(
                        "{n} {}",
                        state_label(Some(chip))
                    ))),
            );
        }

        header = header
            .child(div().flex_grow_1())
            .child(
                div()
                    .id("seq-reload")
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_elevated)
                    .text_color(theme.text_secondary)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child("Reload")
                    .on_click(cx.listener(|_this, _, _, cx| {
                        if let Some(store) = sequences::try_global(cx) {
                            store.read(cx).reload();
                        }
                    })),
            )
            .child(
                div()
                    .id("seq-mode")
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_elevated)
                    .text_color(theme.text_secondary)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child(if history { "History" } else { "Channels" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = if this.mode == SequenceListMode::History {
                            SequenceListMode::Channels
                        } else {
                            SequenceListMode::History
                        };
                        cx.notify();
                    })),
            );

        let mut list = div()
            .id("seq-list")
            .flex()
            .flex_col()
            .flex_grow_1()
            .overflow_y_scroll();

        match &store {
            None => {
                list = list.child(empty_row(&theme, "Sequence store unavailable"));
            }
            Some(_) if history => {
                let now = Timestamp::now();
                let rows: Vec<AnyElement> = SequenceHistory::global(cx)
                    .map(|history| {
                        history
                            .read(cx)
                            .newest_first()
                            .into_iter()
                            .map(|e| history_row(e, now, &theme))
                            .collect()
                    })
                    .unwrap_or_default();
                if rows.is_empty() {
                    list = list.child(empty_row(&theme, "No sequence history"));
                } else {
                    list = list.children(rows);
                }
            }
            Some(_) => {
                self.sync_controls(&channels, cx);
                if self.controls.is_empty() {
                    list = list.child(empty_row(&theme, "No channels declared"));
                } else if self.layout == SequenceLayout::Square {
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(ui_px(8.0))
                            .p(ui_px(8.0))
                            .children(self.controls.iter().map(|(_, control)| {
                                div()
                                    .flex()
                                    .w(ui_px(SQUARE_TILE.0))
                                    .min_h(ui_px(SQUARE_TILE.1))
                                    .rounded(ui_px(6.0))
                                    .border_1()
                                    .border_color(theme.border_primary)
                                    .child(control.clone())
                            })),
                    );
                } else {
                    list = list.children(self.controls.iter().map(|(_, control)| {
                        div()
                            .flex_none()
                            .py(ui_px(8.0))
                            .border_b_1()
                            .border_color(theme.border_primary)
                            .child(control.clone())
                    }));
                }
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .text_color(theme.text_primary)
            .text_size(px(13.0))
            .child(header)
            .child(list)
    }
}

impl SequenceView {
    /// Match the controls to the declared channels, keeping each surviving
    /// channel's control (and whatever it has staged or armed).
    fn sync_controls(&mut self, channels: &[SharedString], cx: &mut Context<Self>) {
        for (_, control) in &self.controls {
            if control.read(cx).layout != self.layout {
                let layout = self.layout;
                control.update(cx, |control, cx| {
                    control.layout = layout;
                    cx.notify();
                });
            }
        }
        if self.controls.len() == channels.len()
            && self.controls.iter().zip(channels).all(|((a, _), b)| a == b)
        {
            return;
        }
        let mut old = std::mem::take(&mut self.controls);
        self.controls = channels
            .iter()
            .map(|name| {
                let control = match old.iter().position(|(n, _)| n == name) {
                    Some(ix) => old.swap_remove(ix).1,
                    None => {
                        let cfg = SequenceControlConfig {
                            channel: name.to_string(),
                            layout: self.layout,
                        };
                        cx.new(|cx| SequenceControl::from_config(&cfg, cx))
                    }
                };
                (name.clone(), control)
            })
            .collect();
    }
}

fn empty_row(theme: &crate::theme::Theme, msg: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_color(theme.text_tertiary)
        .child(msg)
        .into_any_element()
}

/// One message a channel raised: the channel, the message in the color of the
/// state it left the channel in, and how long ago.
fn history_row(entry: &HistoryEntry, now: Timestamp, theme: &crate::theme::Theme) -> AnyElement {
    let label = if entry.message.is_empty() {
        SharedString::new_static(state_label(entry.state))
    } else {
        entry.message.clone()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(theme.border_primary)
        .child(
            div()
                .w(px(120.0))
                .text_color(theme.text_secondary)
                .text_size(px(12.0))
                .child(entry.channel.clone()),
        )
        .child(
            div()
                .flex_grow_1()
                .text_color(theme.run_state_color(entry.state))
                .text_size(px(12.0))
                .child(label),
        )
        .child(
            div()
                .text_color(theme.text_tertiary)
                .text_size(px(11.0))
                .child(SharedString::from(format_age(entry.at, now))),
        )
        .into_any_element()
}
