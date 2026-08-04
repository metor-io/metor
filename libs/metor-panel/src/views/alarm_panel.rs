//! Live list of the control system's alarms: active occurrences with an acknowledge
//! affordance, plus a history view. All data comes from the global
//! [`AlarmStore`](crate::alarms::AlarmStore); this view only renders and publishes acks.

use gpui::{AnyElement, Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto::types::Timestamp;

use crate::alarms::{self, AlarmEventKind, AlarmState};
use crate::theme::theme;

/// Which set the alarm list shows. Switched from the panel inspector rather than an
/// inline toggle, so the panel chrome stays minimal.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, facet::Facet)]
#[repr(u8)]
pub enum AlarmListMode {
    #[default]
    Active,
    History,
}

/// The alarm list view. Its [`AlarmListMode`] is editable through the inspector.
#[derive(facet::Facet)]
pub struct AlarmView {
    pub mode: AlarmListMode,
}

impl AlarmView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Repaint whenever the control system raises, clears, or acks an alarm.
        if let Some(store) = alarms::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            mode: AlarmListMode::default(),
        }
    }

    /// Restore a persisted panel to the history or active tab.
    pub fn set_history(&mut self, history: bool) {
        self.mode = if history {
            AlarmListMode::History
        } else {
            AlarmListMode::Active
        };
    }

    /// Whether the history tab is showing, for persistence.
    pub fn is_history(&self) -> bool {
        self.mode == AlarmListMode::History
    }
}

fn format_age(raised_at: Timestamp) -> String {
    let secs = (Timestamp::now().0 - raised_at.0).max(0) as f64 / 1e6;
    if secs < 60.0 {
        format!("{secs:.0}s ago")
    } else if secs < 3600.0 {
        format!("{:.0}m ago", secs / 60.0)
    } else {
        format!("{:.1}h ago", secs / 3600.0)
    }
}

fn severity_chip_label(idx: usize) -> &'static str {
    match idx {
        2 => "Critical",
        1 => "Warning",
        _ => "Info",
    }
}

impl Render for AlarmView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let history = self.mode == AlarmListMode::History;
        let store = alarms::try_global(cx);

        // Header: severity count chips, then an Ack-all control and the
        // Active/History toggle. No title — the tab already names the panel.
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_primary);

        if let Some(store) = &store {
            let counts = store.read(cx).state().counts_by_severity();
            for idx in (0..counts.len()).rev() {
                if counts[idx] == 0 {
                    continue;
                }
                header = header.child(
                    div()
                        .px_2()
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.alarm_tint(idx))
                        .text_color(theme.alarm_color(idx))
                        .text_size(px(11.0))
                        .child(SharedString::from(format!(
                            "{} {}",
                            counts[idx],
                            severity_chip_label(idx)
                        ))),
                );
            }
        }

        header = header
            .child(div().flex_grow())
            .child(
                div()
                    .id("alarm-ack-all")
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_elevated)
                    .text_color(theme.text_secondary)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child("Ack all")
                    .on_click(cx.listener(|_this, _, _, cx| {
                        if let Some(store) = alarms::try_global(cx) {
                            store.read(cx).acknowledge_all();
                        }
                    })),
            )
            .child(
                div()
                    .id("alarm-mode")
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_elevated)
                    .text_color(theme.text_secondary)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child(if history { "History" } else { "Active" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = if this.mode == AlarmListMode::History {
                            AlarmListMode::Active
                        } else {
                            AlarmListMode::History
                        };
                        cx.notify();
                    })),
            );

        let mut list = div()
            .id("alarm-list")
            .flex()
            .flex_col()
            .flex_grow()
            .overflow_y_scroll();

        match &store {
            None => {
                list = list.child(empty_row(&theme, "Alarm store unavailable"));
            }
            Some(store) => {
                let store = store.read(cx);
                let state = store.state();
                let rows = if history {
                    history_rows(state, &theme)
                } else {
                    active_rows(state, &theme)
                };
                if rows.is_empty() {
                    let msg = if history {
                        "No alarm history"
                    } else {
                        "No active alarms"
                    };
                    list = list.child(empty_row(&theme, msg));
                } else {
                    list = list.children(rows);
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

fn active_rows(state: &AlarmState, theme: &crate::theme::Theme) -> Vec<AnyElement> {
    state
        .active_sorted()
        .into_iter()
        .map(|alarm| {
            let idx = alarms::severity_index(alarm.severity);
            let color = theme.alarm_color(idx);
            let name = state
                .def(&alarm.def_id)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| alarm.def_id.clone());
            let acked = state.is_acked(alarm.occurrence);
            let value_str = alarm.value.map(|v| format!("{v:.3}")).unwrap_or_default();
            let name_color = if acked {
                theme.text_secondary
            } else {
                theme.text_primary
            };

            let ack: AnyElement = if acked {
                div()
                    .text_color(color)
                    .text_size(px(11.0))
                    .child("✓ acked")
                    .into_any_element()
            } else {
                let occ = alarm.occurrence;
                div()
                    .id(("ack", occ as usize))
                    .px_2()
                    .py(px(2.0))
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child("Ack")
                    .on_click(move |_, _, cx| {
                        if let Some(store) = alarms::try_global(cx) {
                            store.read(cx).acknowledge(occ);
                        }
                    })
                    .into_any_element()
            };

            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border_primary)
                .child(div().w(px(4.0)).h(px(28.0)).rounded_sm().bg(color))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_grow()
                        .child(div().text_color(name_color).child(SharedString::from(name)))
                        .child(
                            div()
                                .text_color(theme.text_secondary)
                                .text_size(px(11.0))
                                .child(SharedString::from(alarm.message.clone())),
                        ),
                )
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(value_str)),
                )
                .child(
                    div()
                        .text_color(theme.text_tertiary)
                        .text_size(px(11.0))
                        .child(SharedString::from(format_age(alarm.raised_at))),
                )
                .child(ack)
                .into_any_element()
        })
        .collect()
}

fn history_rows(state: &AlarmState, theme: &crate::theme::Theme) -> Vec<AnyElement> {
    state
        .history()
        .iter()
        .rev()
        .map(|event| {
            let (label, color) = match event.kind {
                AlarmEventKind::Raised => {
                    let idx = event.severity.map(alarms::severity_index).unwrap_or(0);
                    ("Raised", theme.alarm_color(idx))
                }
                AlarmEventKind::Cleared => ("Cleared", theme.text_secondary),
                AlarmEventKind::Acked => ("Acked", theme.text_secondary),
            };
            let name = state
                .def(&event.def_id)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| event.def_id.clone());
            let mut text = name;
            if !event.detail.is_empty() {
                text.push_str(" — ");
                text.push_str(&event.detail);
            }

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
                        .w(px(56.0))
                        .text_color(color)
                        .text_size(px(11.0))
                        .child(label),
                )
                .child(
                    div()
                        .flex_grow()
                        .text_color(theme.text_secondary)
                        .text_size(px(12.0))
                        .child(SharedString::from(text)),
                )
                .child(
                    div()
                        .text_color(theme.text_tertiary)
                        .text_size(px(11.0))
                        .child(SharedString::from(format_age(event.timestamp))),
                )
                .into_any_element()
        })
        .collect()
}
