//! Live list of the control system's alarms: everything pending an operator (live
//! occurrences and latched ones) with acknowledge and shelve affordances, plus history
//! and shelf tabs. All data comes from the global
//! [`AlarmStore`](crate::alarms::AlarmStore); this view only renders and publishes the
//! operator's side.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, IntoElement, Pixels, Point, SharedString, Window, div, prelude::*, px,
};
use metor_proto::types::Timestamp;
use metor_proto_wkt::AlarmId;

use crate::alarms::latch::TileState;
use crate::alarms::{self, AlarmEventKind, AlarmState, MAX_SHELF_DURATION};
use crate::inspector::rows::{DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::theme::theme;
use crate::views::table;

/// Which set the alarm list shows.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug, facet::Facet,
)]
#[repr(u8)]
pub enum AlarmListMode {
    #[default]
    Active,
    History,
    Shelved,
}

impl AlarmListMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Active => Self::History,
            Self::History => Self::Shelved,
            Self::Shelved => Self::Active,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::History => "History",
            Self::Shelved => "Shelved",
        }
    }
}

/// Shelf durations offered by the row affordance. ISA-18.2 caps a shelf; there is no
/// "forever" row, so a point needing permanent suppression gets a config change.
const SHELF_DURATIONS: [(&str, Duration); 3] = [
    ("15 minutes", Duration::from_secs(15 * 60)),
    ("1 hour", Duration::from_secs(60 * 60)),
    ("8 hours", MAX_SHELF_DURATION),
];

/// The alarm list view. Its [`AlarmListMode`] is editable through the inspector.
#[derive(facet::Facet)]
pub struct AlarmView {
    pub mode: AlarmListMode,
}

impl AlarmView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Repaint whenever the control system raises, clears, or acks an alarm — and on
        // the store's shelf ticker, which is what makes the countdowns run.
        if let Some(store) = alarms::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            mode: AlarmListMode::default(),
        }
    }

    /// Restore a persisted panel to the history or active tab. The fallback for layouts
    /// saved before the mode was persisted outright.
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

/// How much of a shelf is left, counting down to zero.
fn format_remaining(until: Timestamp) -> String {
    let secs = (until.0 - Timestamp::now().0).max(0) as f64 / 1e6;
    if secs < 60.0 {
        format!("{secs:.0}s left")
    } else if secs < 3600.0 {
        format!("{:.0}m left", secs / 60.0)
    } else {
        format!("{:.1}h left", secs / 3600.0)
    }
}

fn severity_chip_label(idx: usize) -> &'static str {
    match idx {
        2 => "Critical",
        1 => "Warning",
        _ => "Info",
    }
}

/// Anchored duration picker for shelving `def_id`. Text typed into the inspector's
/// search field before choosing a duration becomes the shelf reason.
fn open_shelve_page(def_id: AlarmId, position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let Some(open) = open_inspector(cx) else {
        return;
    };
    let rows: Vec<Box<dyn InspectorRow>> = SHELF_DURATIONS
        .iter()
        .map(|(label, duration)| {
            let def_id = def_id.clone();
            let duration = *duration;
            Box::new(DefaultActionRow::optional(
                SharedString::from(format!("Shelve for {label}")),
                Arc::new(move |reason, _window, cx| {
                    if let Some(store) = alarms::try_global(cx) {
                        let reason = (!reason.trim().is_empty()).then(|| reason.trim().to_string());
                        store.read(cx).shelve(def_id.clone(), duration, reason);
                    }
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect();
    open(
        InspectorRequest {
            rows,
            mode: InspectorMode::Anchored(position),
        },
        window,
        cx,
    );
}

impl Render for AlarmView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mode = self.mode;
        let store = alarms::try_global(cx);

        // Header: severity count chips, then an Ack-all control and the mode toggle.
        // No title — the tab already names the panel.
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
            let state = store.read(cx).state();
            let counts = state.counts_by_severity();
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
            // A shelved point must never be invisible, so its count shows from every
            // tab and jumps to the shelf list.
            let shelved = state.shelves_sorted().len();
            if shelved > 0 {
                header = header.child(
                    div()
                        .id("alarm-shelved-count")
                        .px_2()
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(theme.bg_elevated)
                        .text_color(theme.text_tertiary)
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .child(SharedString::from(format!("{shelved} shelved")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mode = AlarmListMode::Shelved;
                            cx.notify();
                        })),
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
                    .child(mode.label())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = this.mode.cycle();
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
                let rows = match mode {
                    AlarmListMode::Active => pending_rows(state, &theme),
                    AlarmListMode::History => history_rows(state, &theme),
                    AlarmListMode::Shelved => shelved_rows(state, &theme),
                };
                if rows.is_empty() {
                    let msg = match mode {
                        AlarmListMode::Active => "No active alarms",
                        AlarmListMode::History => "No alarm history",
                        AlarmListMode::Shelved => "No shelved alarms",
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

/// Rows for everything awaiting an operator. A latched row — the alarm cleared before
/// anyone looked — keeps its place with a dimmed severity bar and a "cleared" badge, and
/// its `Ack` retires the latch outright.
fn pending_rows(state: &AlarmState, theme: &crate::theme::Theme) -> Vec<AnyElement> {
    state
        .pending_sorted()
        .into_iter()
        .map(|pending| {
            let alarm = pending.alarm;
            let idx = alarms::severity_index(alarm.severity);
            let color = theme.alarm_color(idx);
            let name = state
                .def(&alarm.def_id)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| alarm.def_id.clone());
            let acked = pending.state == TileState::AlarmAcked;
            let value_str = alarm.value.map(|v| format!("{v:.3}")).unwrap_or_default();
            let name_color = if acked {
                theme.text_secondary
            } else {
                theme.text_primary
            };
            let bar_color = match pending.cleared_at {
                Some(_) => theme.alarm_tint(idx),
                None => color,
            };
            let cleared_badge = pending.cleared_at.map(|cleared_at| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py(px(1.0))
                            .rounded(px(3.0))
                            .bg(theme.alarm_tint(idx))
                            .text_color(color)
                            .text_size(px(10.0))
                            .child("cleared"),
                    )
                    .child(
                        div()
                            .text_color(theme.text_tertiary)
                            .text_size(px(11.0))
                            .child(SharedString::from(format_age(cleared_at))),
                    )
            });

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

            let def_id = alarm.def_id.clone();
            let shelve = div()
                .id(("shelve", alarm.occurrence as usize))
                .px_2()
                .py(px(2.0))
                .rounded_md()
                .bg(theme.bg_elevated)
                .text_color(theme.text_secondary)
                .text_size(px(11.0))
                .cursor_pointer()
                .child("Shelve")
                .on_click(move |event, window, cx| {
                    open_shelve_page(def_id.clone(), event.position(), window, cx)
                });

            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border_primary)
                .child(div().w(px(4.0)).h(px(28.0)).rounded_sm().bg(bar_color))
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
                .children(cleared_badge)
                .child(
                    div()
                        .text_color(theme.text_tertiary)
                        .text_size(px(11.0))
                        .child(SharedString::from(format_age(alarm.raised_at))),
                )
                .child(shelve)
                .child(ack)
                .into_any_element()
        })
        .collect()
}

/// The shelf list: what is suppressed, for how long, by whom, and why.
fn shelved_rows(state: &AlarmState, theme: &crate::theme::Theme) -> Vec<AnyElement> {
    state
        .shelves_sorted()
        .into_iter()
        .map(|(def_id, shelf)| {
            let name = state
                .def(def_id)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| def_id.clone());
            let reason = shelf.reason.clone().unwrap_or_default();
            let def_id = def_id.clone();

            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border_primary)
                .child(
                    div()
                        .flex_grow()
                        .text_color(theme.text_primary)
                        .child(SharedString::from(name)),
                )
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .text_size(px(11.0))
                        .child(SharedString::from(reason)),
                )
                .child(
                    div()
                        .text_color(theme.text_tertiary)
                        .text_size(px(11.0))
                        .child(SharedString::from(shelf.operator.clone())),
                )
                .child(
                    div()
                        .text_color(theme.text_secondary)
                        .text_size(px(11.0))
                        .child(SharedString::from(format_remaining(shelf.until))),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("unshelve-{def_id}")))
                        .px_2()
                        .py(px(2.0))
                        .rounded_md()
                        .bg(theme.bg_elevated)
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .child("Unshelve")
                        .on_click(move |_, _, cx| {
                            if let Some(store) = alarms::try_global(cx) {
                                store.read(cx).unshelve(def_id.clone());
                            }
                        }),
                )
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
                AlarmEventKind::Shelved => ("Shelved", theme.text_secondary),
                AlarmEventKind::Unshelved => ("Unshelved", theme.text_secondary),
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
