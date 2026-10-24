//! Start/stop control for a single sequence channel.
//!
//! [`SequenceView`](super::SequenceView) lists every channel and
//! [`SequenceGrid`](super::SequenceGrid) reduces them to status dots; neither
//! is what an operator wants when one channel *is* the operation. This is
//! that third shape: one channel, its state and progress, and the verbs
//! reachable in a single click rather than through the inspector.
//!
//! The shape is one strip-height card: the channel and its run state over
//! the loaded sequence's name on the left, transport-style icon buttons on
//! the right. The protocol has no pause, so the slot a transport gives pause
//! holds Abort (the commanded safe stop, under its own glyph) rather than a
//! hold the control system couldn't honor; the play slot turns into Reset
//! once a run has ended.
//!
//! No new command plumbing — the buttons drive the same
//! [`SequenceStore`](crate::sequences::SequenceStore) the list does, so the
//! command still travels panel DB → link → `UplinkSystem`.

use std::sync::Arc;

use gpui::{
    App, Context, Div, FontWeight, Hsla, IntoElement, SharedString, Stateful, Window, div,
    prelude::*,
};
use metor_proto_wkt::SequenceRunState;
use serde::{Deserialize, Serialize};

use super::sequence_panel::load_picker_rows;
use crate::icons::Icon;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, is_resettable, run_state_index, run_state_label};
use crate::theme::{Theme, theme, ui_px};
use crate::views::TooltipText;

/// Persisted shape of a [`SequenceControl`], shared by the tile and dashboard
/// surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SequenceControlConfig {
    /// Channel name — the address every published command carries.
    pub channel: String,
    /// Drop the latest status line from beside the run state. For a strip
    /// too narrow to show both.
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
            return card(&theme).justify_center().child(
                div()
                    .truncate()
                    .text_size(ui_px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(if store.is_none() {
                        SharedString::new_static("Sequence store unavailable")
                    } else if channel.is_empty() {
                        SharedString::new_static("No channel configured")
                    } else {
                        SharedString::from(format!("Unknown channel: {channel}"))
                    }),
            );
        };

        let state_color = theme.run_state_color(run_state_index(ch.run_state));
        let arming = self.arming_stop;

        // The armed confirm takes the status slot even when compact: it is
        // the one line the operator must not miss.
        let status = if arming {
            Some(SharedString::new_static("press stop again to hard-stop"))
        } else if self.compact {
            None
        } else {
            ch.last_message.clone()
        };
        let subtitle = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(ui_px(4.0))
            .min_w_0()
            .text_size(ui_px(10.0))
            .child(
                div()
                    .flex_none()
                    .size(ui_px(6.0))
                    .rounded(ui_px(3.0))
                    .bg(state_color),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_tertiary)
                    .child(ch.name.to_lowercase()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(state_color)
                    .child(run_state_label(ch.run_state).to_lowercase()),
            )
            .when_some(status, |el, status| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(if arming {
                            theme.error_accent
                        } else {
                            theme.text_tertiary
                        })
                        .child(status),
                )
            });

        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(ui_px(6.0))
            .min_w_0()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if ch.loaded.is_some() {
                        theme.text_primary
                    } else {
                        theme.text_tertiary
                    })
                    .child(match &ch.loaded {
                        Some(name) => name.to_uppercase(),
                        None => "NOTHING LOADED".to_string(),
                    }),
            )
            .child(Icon::Link.svg(12.0, theme.text_tertiary).flex_none());

        let available = ch.available.clone();
        let label = div()
            .id("seq-ctl-load")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(ui_px(2.0))
            .cursor_pointer()
            .tooltip(|_window, cx| TooltipText::build("Load sequence".into(), cx))
            .child(subtitle)
            .child(title)
            .on_click(cx.listener({
                let channel = channel.clone();
                move |this, _, window, cx| {
                    this.arming_stop = false;
                    cx.notify();
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

        let abort_btn = icon_button(
            &theme,
            "seq-ctl-abort",
            Icon::Abort,
            theme.text_secondary,
            theme.bg_elevated,
            "Abort (safe stop)",
        )
        .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.abort(c))));

        let primary_btn = match ch.run_state {
            SequenceRunState::Running => icon_button(
                &theme,
                "seq-ctl-primary",
                Icon::Play,
                theme.control_active,
                Theme::dim(theme.control_active, 0.3),
                "Running",
            )
            .cursor_default(),
            SequenceRunState::Idle => icon_button(
                &theme,
                "seq-ctl-primary",
                Icon::Play,
                theme.control_active,
                theme.run_state_tint(run_state_index(SequenceRunState::Running)),
                "Start",
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.start(c)))),
            state if is_resettable(state) => icon_button(
                &theme,
                "seq-ctl-primary",
                Icon::Loop,
                theme.text_secondary,
                theme.bg_elevated,
                "Reset",
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.reset(c)))),
            _ => icon_button(
                &theme,
                "seq-ctl-primary",
                Icon::Play,
                theme.text_tertiary,
                theme.bg_elevated,
                "Not startable",
            )
            .cursor_default(),
        };

        let stop_btn = icon_button(
            &theme,
            "seq-ctl-stop",
            Icon::Stop,
            if arming {
                theme.error_accent
            } else {
                theme.text_secondary
            },
            if arming {
                Theme::dim(theme.error_accent, 0.25)
            } else {
                theme.bg_elevated
            },
            if arming {
                "Confirm hard stop"
            } else {
                "Stop (hard, click twice)"
            },
        )
        .when(arming, |el| el.border_1().border_color(theme.error_accent))
        .on_click(cx.listener(move |this, _, _, cx| {
            if this.arming_stop {
                this.command(cx, |s, c| s.stop(c));
            } else {
                this.arming_stop = true;
                cx.notify();
            }
        }));

        card(&theme).child(label).child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(ui_px(4.0))
                .child(abort_btn)
                .child(primary_btn)
                .child(stop_btn),
        )
    }
}

/// The rounded strip every state of the control renders into, so an
/// unbound control keeps the footprint of a live one.
fn card(theme: &Theme) -> Div {
    div()
        .size_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(ui_px(8.0))
        .px(ui_px(10.0))
        .overflow_hidden()
        .rounded(ui_px(6.0))
        .bg(theme.bg_primary)
        .border_1()
        .border_color(theme.border_primary)
}

fn icon_button(
    theme: &Theme,
    id: &'static str,
    icon: Icon,
    color: Hsla,
    bg: Hsla,
    tooltip: &'static str,
) -> Stateful<Div> {
    let hover = theme.selection_bg;
    div()
        .id(id)
        .flex_none()
        .size(ui_px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(ui_px(4.0))
        .bg(bg)
        .hover(move |s| s.bg(hover))
        .cursor_pointer()
        .tooltip(move |_window, cx| TooltipText::build(tooltip.into(), cx))
        .child(icon.svg(13.0, color))
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
