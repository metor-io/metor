//! Detailed per-channel control panel for sequences (best for a handful of channels).
//! Each row is a [`SequenceControl`] for one channel, so the list and the single-channel
//! widget stage loads, arm Stop and lay out their buttons the same way; a header carries a
//! global Reload and run-state count chips. All data comes from the global
//! [`SequenceStore`](crate::sequences::SequenceStore); this view only renders and
//! publishes commands.

use std::sync::Arc;

use gpui::{AnyElement, Context, Entity, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto::types::Timestamp;
use metor_proto_wkt::SequenceRunState;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::sequences::{self, is_resettable, run_state_index, run_state_label};
use crate::theme::{theme, ui_px};
use crate::views::sequence_control::{SequenceControl, SequenceControlConfig};
use crate::views::table;

/// Run-state chips to surface in the header, in display order. `Idle` is omitted as noise.
const CHIP_STATES: [SequenceRunState; 5] = [
    SequenceRunState::Running,
    SequenceRunState::Failed,
    SequenceRunState::Stopped,
    SequenceRunState::Aborted,
    SequenceRunState::Completed,
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
    /// One control per declared channel, in display order. Kept across renders so a
    /// staged load or an armed Stop survives the store's next event.
    #[facet(opaque)]
    controls: Vec<(SharedString, Entity<SequenceControl>)>,
}

impl SequenceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Repaint whenever the control system declares channels or reports an event.
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            mode: SequenceListMode::default(),
            controls: Vec::new(),
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

struct HistoryRow {
    channel_name: SharedString,
    run_state: SequenceRunState,
    label: SharedString,
    timestamp: Timestamp,
}

fn format_age(at: Timestamp) -> String {
    let secs = (Timestamp::now().0 - at.0).max(0) as f64 / 1e6;
    if secs < 60.0 {
        format!("{secs:.0}s ago")
    } else if secs < 3600.0 {
        format!("{:.0}m ago", secs / 60.0)
    } else {
        format!("{:.1}h ago", secs / 3600.0)
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

/// Inspector control page for one channel: Load / Reset / Start / Abort / Stop. `Reset` only
/// appears from a terminal state; `Stop` drills into a confirmation row since it is the unsafe
/// hard-drop. Used by the grid's click-through.
pub(crate) fn channel_control_rows(
    channel: SharedString,
    run_state: SequenceRunState,
    available: Vec<SharedString>,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    rows.push(Box::new(NavRow::new(
        "Load",
        "",
        Box::new({
            let channel = channel.clone();
            move |_cx| load_picker_rows(channel.clone(), &available)
        }),
    )));
    if is_resettable(run_state) {
        rows.push(Box::new(CommandRow::new(
            "Reset",
            Arc::new({
                let channel = channel.clone();
                move |_window, cx| {
                    if let Some(store) = sequences::try_global(cx) {
                        store.read(cx).reset(&channel);
                    }
                }
            }),
        )));
    }
    if let SequenceRunState::Idle = run_state {
        rows.push(Box::new(CommandRow::new(
            "Start",
            Arc::new({
                let channel = channel.clone();
                move |_window, cx| {
                    if let Some(store) = sequences::try_global(cx) {
                        store.read(cx).start(&channel);
                    }
                }
            }),
        )));
    }
    rows.push(Box::new(CommandRow::new(
        "Abort",
        Arc::new({
            let channel = channel.clone();
            move |_window, cx| {
                if let Some(store) = sequences::try_global(cx) {
                    store.read(cx).abort(&channel);
                }
            }
        }),
    )));
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
                    .child("Live"),
            );

        if let Some(store) = &store {
            let store = store.read(cx);
            let state = store.state();
            for run_state in CHIP_STATES {
                let n = state.count_in_state(run_state);
                if n == 0 {
                    continue;
                }
                let idx = run_state_index(run_state);
                header = header.child(
                    div()
                        .px_2()
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.run_state_tint(idx))
                        .text_color(theme.run_state_color(idx))
                        .text_size(px(11.0))
                        .child(SharedString::from(format!(
                            "{n} {}",
                            run_state_label(run_state)
                        ))),
                );
            }
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
            Some(store) if history => {
                let entries: Vec<HistoryRow> = store
                    .read(cx)
                    .state()
                    .history()
                    .iter()
                    .rev()
                    .map(|e| HistoryRow {
                        channel_name: e.channel_name.clone(),
                        run_state: e.run_state,
                        label: e.label.clone(),
                        timestamp: e.timestamp,
                    })
                    .collect();
                if entries.is_empty() {
                    list = list.child(empty_row(&theme, "No sequence history"));
                } else {
                    list = list.children(entries.into_iter().map(|e| history_row(e, &theme)));
                }
            }
            Some(store) => {
                let channels: Vec<SharedString> = store
                    .read(cx)
                    .state()
                    .channels_ordered()
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                self.sync_controls(&channels, cx);
                if self.controls.is_empty() {
                    list = list.child(empty_row(&theme, "No channels declared"));
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
                            compact: false,
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

fn history_row(entry: HistoryRow, theme: &crate::theme::Theme) -> AnyElement {
    let color = theme.run_state_color(run_state_index(entry.run_state));
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
                .child(entry.channel_name),
        )
        .child(
            div()
                .flex_grow_1()
                .text_color(color)
                .text_size(px(12.0))
                .child(entry.label),
        )
        .child(
            div()
                .text_color(theme.text_tertiary)
                .text_size(px(11.0))
                .child(SharedString::from(format_age(entry.timestamp))),
        )
        .into_any_element()
}
