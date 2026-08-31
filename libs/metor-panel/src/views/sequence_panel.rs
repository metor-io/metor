//! Detailed per-channel control panel for sequences (best for a handful of channels).
//! Each row shows a channel's loaded sequence and run state with inline Load / Start /
//! Abort / Stop controls; a header carries a global Reload and run-state count chips. All
//! data comes from the global [`SequenceStore`](crate::sequences::SequenceStore); this view
//! only renders and publishes commands.
//!
//! `Stop` is the unsafe hard-drop, so it is guarded by an inline two-step confirm
//! ([`arming_stop`](SequenceView::arming_stop)) rather than firing on first click.

use std::sync::Arc;

use gpui::{AnyElement, Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto::types::Timestamp;
use metor_proto_wkt::SequenceRunState;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, is_resettable, run_state_index, run_state_label};
use crate::theme::theme;
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
    /// The channel (by name) whose `Stop` is armed for confirmation, if any. Transient
    /// UI state.
    #[facet(skip)]
    arming_stop: Option<SharedString>,
}

impl SequenceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Repaint whenever the control system declares channels or reports an event.
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            mode: SequenceListMode::default(),
            arming_stop: None,
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

/// Snapshot of one channel for rendering, owned so the store borrow is released before we
/// build listeners that need `&mut Context`. The name is the channel's identity — the
/// address every published command carries.
struct ChannelRow {
    name: SharedString,
    loaded: Option<SharedString>,
    available: Vec<SharedString>,
    run_state: SequenceRunState,
    last_message: Option<SharedString>,
    updated_at: Timestamp,
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
            .border_color(theme.border_primary);

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
            .child(div().flex_grow())
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
                        this.arming_stop = None;
                        cx.notify();
                    })),
            );

        let mut list = div()
            .id("seq-list")
            .flex()
            .flex_col()
            .flex_grow()
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
                let channels: Vec<ChannelRow> = store
                    .read(cx)
                    .state()
                    .channels_ordered()
                    .iter()
                    .map(|c| ChannelRow {
                        name: c.name.clone(),
                        loaded: c.loaded.clone(),
                        available: c.available.clone(),
                        run_state: c.run_state,
                        last_message: c.last_message.clone(),
                        updated_at: c.updated_at,
                    })
                    .collect();
                if channels.is_empty() {
                    list = list.child(empty_row(&theme, "No channels declared"));
                } else {
                    for (ix, ch) in channels.into_iter().enumerate() {
                        list = list.child(self.channel_row(ix, ch, &theme, cx));
                    }
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
    fn channel_row(
        &self,
        ix: usize,
        ch: ChannelRow,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let idx = run_state_index(ch.run_state);
        let color = theme.run_state_color(idx);
        let name = ch.name.clone();
        let arming = self.arming_stop.as_ref() == Some(&name);

        let loaded_line = ch
            .loaded
            .clone()
            .unwrap_or_else(|| SharedString::new_static("— no sequence loaded"));
        let detail = ch
            .last_message
            .clone()
            .unwrap_or_else(|| SharedString::from(run_state_label(ch.run_state)));

        let available = ch.available.clone();
        let load_btn = pill_button(theme, ("seq-load", ix), "Load").on_click(cx.listener({
            let name = name.clone();
            move |this, _, window, cx| {
                this.arming_stop = None;
                let Some(open) = open_inspector(cx) else {
                    return;
                };
                let rows = load_picker_rows(name.clone(), &available);
                open(
                    InspectorRequest {
                        rows,
                        mode: InspectorMode::Centered,
                    },
                    window,
                    cx,
                );
            }
        }));

        let resettable = is_resettable(ch.run_state);
        let reset_btn = pill_button(theme, ("seq-reset", ix), "Reset").on_click(cx.listener({
            let name = name.clone();
            move |this, _, _, cx| {
                this.arming_stop = None;
                if let Some(store) = sequences::try_global(cx) {
                    store.read(cx).reset(&name);
                }
                cx.notify();
            }
        }));

        let startable = matches!(ch.run_state, SequenceRunState::Idle);
        let start_btn = pill_button(theme, ("seq-start", ix), "Start").on_click(cx.listener({
            let name = name.clone();
            move |this, _, _, cx| {
                this.arming_stop = None;
                if let Some(store) = sequences::try_global(cx) {
                    store.read(cx).start(&name);
                }
                cx.notify();
            }
        }));

        let abort_btn = pill_button(theme, ("seq-abort", ix), "Abort").on_click(cx.listener({
            let name = name.clone();
            move |this, _, _, cx| {
                this.arming_stop = None;
                if let Some(store) = sequences::try_global(cx) {
                    store.read(cx).abort(&name);
                }
                cx.notify();
            }
        }));

        let stop_label = if arming { "Confirm stop" } else { "Stop" };
        let stop_btn = pill_button(theme, ("seq-stop", ix), stop_label)
            .text_color(theme.error_accent)
            .when(arming, |el| {
                el.bg(theme.run_state_tint(run_state_index(SequenceRunState::Failed)))
            })
            .on_click(cx.listener({
                let name = name.clone();
                move |this, _, _, cx| {
                    if this.arming_stop.as_ref() == Some(&name) {
                        if let Some(store) = sequences::try_global(cx) {
                            store.read(cx).stop(&name);
                        }
                        this.arming_stop = None;
                    } else {
                        this.arming_stop = Some(name.clone());
                    }
                    cx.notify();
                }
            }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border_primary)
            .child(div().w(px(4.0)).h(px(34.0)).rounded_sm().bg(color))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow()
                    .child(div().text_color(theme.text_primary).child(ch.name.clone()))
                    .child(
                        div()
                            .text_color(theme.text_secondary)
                            .text_size(px(11.0))
                            .child(loaded_line),
                    )
                    .child(
                        div()
                            .text_color(theme.text_tertiary)
                            .text_size(px(11.0))
                            .child(detail),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(
                        div()
                            .text_color(color)
                            .text_size(px(11.0))
                            .child(run_state_label(ch.run_state)),
                    )
                    .child(
                        div()
                            .text_color(theme.text_tertiary)
                            .text_size(px(11.0))
                            .child(SharedString::from(format_age(ch.updated_at))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(load_btn)
                    .when(resettable, |el| el.child(reset_btn))
                    .when(startable, |el| el.child(start_btn))
                    .child(abort_btn)
                    .child(stop_btn),
            )
            .into_any_element()
    }
}

/// A small flat pill-styled button. Caller attaches `.on_click` and any color overrides.
pub(crate) fn pill_button(
    theme: &crate::theme::Theme,
    id: (&'static str, usize),
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py(px(2.0))
        .rounded_md()
        .bg(theme.bg_elevated)
        .text_color(theme.text_secondary)
        .text_size(px(11.0))
        .cursor_pointer()
        .child(label)
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
                .flex_grow()
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
