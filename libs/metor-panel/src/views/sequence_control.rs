//! Start/stop control for a single sequence channel.
//!
//! [`SequenceView`](super::SequenceView) lists every channel and
//! [`SequenceGrid`](super::SequenceGrid) reduces them to status dots; neither
//! is what an operator wants when one channel *is* the operation. This is
//! that third shape: one channel, its state and progress, and the verbs
//! reachable in a single click rather than through the inspector.
//!
//! No new command plumbing — the buttons drive the same
//! [`SequenceStore`](crate::sequences::SequenceStore) the list does, so the
//! command still travels panel DB → link → `UplinkSystem`.

use std::sync::Arc;

use gpui::{App, Context, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_proto_wkt::SequenceRunState;
use serde::{Deserialize, Serialize};

use super::sequence_panel::{load_picker_rows, pill_button};
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, is_resettable, run_state_index, run_state_label};
use crate::theme::theme;

/// Persisted shape of a [`SequenceControl`], shared by the tile and dashboard
/// surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SequenceControlConfig {
    /// Channel name — the address every published command carries.
    pub channel: String,
    /// Drop the loaded-sequence and progress lines, leaving state and
    /// buttons. For a tile too short to show them.
    pub compact: bool,
}

/// Single-channel sequence control.
#[derive(facet::Facet)]
pub struct SequenceControl {
    pub channel: SharedString,
    pub compact: bool,
    /// Set by the first Stop click, cleared by any other action. Stop is the
    /// unsafe hard-drop, so it takes two deliberate clicks — the same guard
    /// the channel list uses.
    #[facet(skip)]
    arming_stop: bool,
}

impl SequenceControl {
    pub fn from_config(cfg: &SequenceControlConfig, cx: &mut Context<Self>) -> Self {
        if let Some(store) = sequences::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            channel: SharedString::from(cfg.channel.clone()),
            compact: cfg.compact,
            arming_stop: false,
        }
    }

    pub fn to_config(&self) -> SequenceControlConfig {
        SequenceControlConfig {
            channel: self.channel.to_string(),
            compact: self.compact,
        }
    }

    /// Run every command through here so any action disarms a half-pressed
    /// Stop; an armed confirm left over from an earlier click is a hazard.
    fn command(&mut self, cx: &mut Context<Self>, act: impl Fn(&sequences::SequenceStore, &str)) {
        self.arming_stop = false;
        if let Some(store) = sequences::try_global(cx) {
            act(store.read(cx), &self.channel);
        }
        cx.notify();
    }
}

impl Render for SequenceControl {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = sequences::try_global(cx);
        let channel = self.channel.clone();

        let Some(ch) = store
            .as_ref()
            .and_then(|s| s.read(cx).state().channel(&channel).cloned())
        else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(if store.is_none() {
                    SharedString::new_static("Sequence store unavailable")
                } else if channel.is_empty() {
                    SharedString::new_static("No channel configured")
                } else {
                    SharedString::from(format!("Unknown channel: {channel}"))
                });
        };

        let color = theme.run_state_color(run_state_index(ch.run_state));
        let arming = self.arming_stop;

        let header = div()
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_tertiary)
                    .child("Live"),
            )
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().w(px(8.0)).h(px(8.0)).rounded(px(4.0)).bg(color))
            .child(
                div()
                    .flex_grow()
                    .truncate()
                    .text_color(theme.text_primary)
                    .text_size(px(12.0))
                    .child(ch.name.clone()),
            )
            .child(
                div()
                    .text_color(color)
                    .text_size(px(11.0))
                    .child(run_state_label(ch.run_state)),
            );

        let available = ch.available.clone();
        let load_btn = pill_button(&theme, ("seq-ctl-load", 0), "Load").on_click(cx.listener({
            let channel = channel.clone();
            move |this, _, window, cx| {
                this.arming_stop = false;
                let Some(open) = open_inspector(cx) else {
                    return;
                };
                open(
                    InspectorRequest {
                        rows: load_picker_rows(channel.clone(), &available),
                        mode: InspectorMode::Centered,
                    },
                    window,
                    cx,
                );
            }
        }));

        let start_btn = pill_button(&theme, ("seq-ctl-start", 0), "Start")
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.start(c))));
        let reset_btn = pill_button(&theme, ("seq-ctl-reset", 0), "Reset")
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.reset(c))));
        let abort_btn = pill_button(&theme, ("seq-ctl-abort", 0), "Abort")
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.abort(c))));

        let stop_btn = pill_button(
            &theme,
            ("seq-ctl-stop", 0),
            if arming { "Confirm stop" } else { "Stop" },
        )
        .text_color(theme.error_accent)
        .when(arming, |el| {
            el.bg(theme.run_state_tint(run_state_index(SequenceRunState::Failed)))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            if this.arming_stop {
                this.command(cx, |s, c| s.stop(c));
            } else {
                this.arming_stop = true;
                cx.notify();
            }
        }));

        let mut tile = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .p(px(8.0))
            .child(header);

        if !self.compact {
            tile = tile
                .child(
                    div()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary)
                        .child(
                            ch.loaded
                                .clone()
                                .unwrap_or(SharedString::new_static("nothing loaded")),
                        ),
                )
                .child(
                    div()
                        .flex_grow()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_tertiary)
                        .child(ch.last_message.clone().unwrap_or_default()),
                );
        }

        tile.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .child(load_btn)
                .when(is_resettable(ch.run_state), |el| el.child(reset_btn))
                .when(matches!(ch.run_state, SequenceRunState::Idle), |el| {
                    el.child(start_btn)
                })
                .child(abort_btn)
                .child(stop_btn),
        )
    }
}

/// Rows listing every declared channel, plus a free-text entry.
///
/// The registry arrives over the link, so a panel opened before the target
/// connects would otherwise offer an empty list; typing the channel name
/// works because the name *is* the address a command carries.
pub(crate) fn channel_picker_rows(
    cx: &App,
    on_select: impl Fn(String, &mut App) + 'static,
) -> Vec<Box<dyn InspectorRow>> {
    let on_select = Arc::new(on_select);
    let mut rows: Vec<Box<dyn InspectorRow>> = sequences::try_global(cx)
        .map(|store| {
            store
                .read(cx)
                .state()
                .channels_ordered()
                .iter()
                .map(|ch| {
                    let name = ch.name.to_string();
                    let on_select = on_select.clone();
                    Box::new(CommandRow::new(
                        ch.name.clone(),
                        Arc::new(move |_window, cx| on_select(name.clone(), cx)),
                    )) as Box<dyn InspectorRow>
                })
                .collect()
        })
        .unwrap_or_default();

    rows.push(Box::new(DefaultActionRow::new(
        "Channel name...",
        Arc::new(move |input, _window, cx| {
            if !input.is_empty() {
                on_select(input.to_string(), cx);
            }
        }),
    )));
    rows
}
