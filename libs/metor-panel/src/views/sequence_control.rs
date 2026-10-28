//! Start/stop control for a single sequence channel.
//!
//! [`SequenceView`](super::SequenceView) lists every channel and
//! [`SequenceGrid`](super::SequenceGrid) reduces them to status dots; neither
//! is what an operator wants when one channel *is* the operation. This is
//! that third shape: one channel, its state and progress, and the verbs
//! reachable in a single click rather than through the inspector.
//!
//! Every layout is built from the same pieces: the channel's state and
//! latest message, a state-chip box naming the sequence, the run and abort
//! clocks, and transport-style icon buttons. [`SequenceLayout`] only
//! arranges them for the space the control has. Picking from the box only
//! stages a choice; the chevron fused to its right edge sends the load, the
//! same stage-then-apply pair a value strip uses for a pending edit, since a
//! load replaces whatever the channel holds. The protocol has no pause, so
//! the slot a transport gives pause holds Abort (the commanded safe stop,
//! under its own glyph) rather than a hold the control system couldn't
//! honor; the play slot turns into Reset when the channel would take a
//! reset and not a start.
//!
//! Everything shown comes from the channel's components through its shared
//! [`ChannelStatusReader`], at the selected time. Which buttons act is the
//! target's call, not the panel's: each is live exactly when the channel's
//! `accepts` component says it would take that command. Scrubbed into the
//! past, the control shows the state then and takes no commands at all.
//!
//! No new command plumbing — the buttons drive the same
//! [`SequenceStore`](crate::sequences::SequenceStore) the list does, so the
//! command still travels panel DB → link → `UplinkSystem`.

use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, Div, Entity, Hsla, IntoElement, SharedString, Stateful, Subscription,
    WeakEntity, Window, div, prelude::*,
};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{SequenceAccepts, SequenceChannelState};
use serde::{Deserialize, Serialize};

use crate::icons::Icon;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{
    self, Channel, ChannelStatus, ChannelStatusReader, format_age, state_label,
};
use crate::theme::{Theme, font_family, theme, ui_px};
use crate::views::TooltipText;
use crate::views::value_strip::{
    STRIP_BOX_HEIGHT, STRIP_BOX_VALUE_SIZE, apply_chevron, box_corners, box_fill, box_padding,
};

/// Persisted shape of a [`SequenceControl`], shared by the tile and dashboard
/// surfaces.
///
/// Saved controls from before layouts carried a `compact` flag instead; it is
/// ignored, since both of its shapes load best as [`SequenceLayout::Medium`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SequenceControlConfig {
    /// Channel name — the address every published command carries.
    pub channel: String,
    pub layout: SequenceLayout,
}

/// How a control arranges itself, chosen for the space it has.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, facet::Facet)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SequenceLayout {
    /// State, sequence, clocks and buttons in one thin row; the status
    /// message lives in the state's tooltip.
    Slim,
    /// The status line and clocks over one row of sequence and buttons; a
    /// dashboard strip or a list row. `compact` is its name from before
    /// [`SequenceLayout::Slim`] existed.
    #[default]
    #[serde(alias = "compact")]
    Medium,
    /// Status, then sequence, then clocks and buttons, each a row of its
    /// own; a dashboard tile with room for a long status message. The clocks
    /// wrap onto separate lines when the tile is narrow.
    Square,
}

/// Single-channel sequence control.
#[derive(facet::Facet)]
pub struct SequenceControl {
    pub channel: SharedString,
    pub layout: SequenceLayout,
    /// Set by the first Stop click, cleared by any other action. Stop is the
    /// unsafe hard-drop, so it takes two deliberate clicks — the same guard
    /// the channel list uses.
    #[facet(skip)]
    arming_stop: bool,
    /// The sequence chosen in the picker but not yet sent. Loading replaces
    /// whatever the channel holds, so choosing only stages the name and the
    /// commit arrow sends it.
    #[facet(skip)]
    staged: Option<SharedString>,
    /// The channel's shared reader, re-acquired when `channel` is edited.
    #[facet(opaque)]
    reader: Option<(Entity<ChannelStatusReader>, Subscription)>,
}

/// What one render shows: the channel's declaration and its state at the
/// selected time.
struct Shown {
    channel: Channel,
    status: ChannelStatus,
    live: bool,
}

impl Shown {
    /// Whether the channel would take the command `bit` names right now.
    /// Nothing is commandable from the past.
    fn allows(&self, bit: u32) -> bool {
        self.live && self.status.allows(bit)
    }
}

impl SequenceControl {
    pub fn from_config(cfg: &SequenceControlConfig, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            channel: SharedString::from(cfg.channel.clone()),
            layout: cfg.layout,
            arming_stop: false,
            staged: None,
            reader: None,
        };
        this.follow_channel(cx);
        this
    }

    pub fn to_config(&self) -> SequenceControlConfig {
        SequenceControlConfig {
            channel: self.channel.to_string(),
            layout: self.layout,
        }
    }

    /// Hold the reader for `channel`, re-acquiring after an edit.
    fn follow_channel(&mut self, cx: &mut Context<Self>) {
        if self.channel.is_empty()
            || self
                .reader
                .as_ref()
                .is_some_and(|(reader, _)| *reader.read(cx).name() == self.channel)
        {
            return;
        }
        self.reader = ChannelStatusReader::acquire(self.channel.clone(), cx).map(|reader| {
            let observed = cx.observe(&reader, |_, _, cx| cx.notify());
            (reader, observed)
        });
    }

    /// The reader's channel and status, if the registry declares the channel.
    fn shown(&self, cx: &App) -> Option<Shown> {
        let reader = self.reader.as_ref()?.0.read(cx);
        Some(Shown {
            channel: reader.channel()?.clone(),
            status: reader.status().clone(),
            live: reader.live(),
        })
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

/// `T+MM:SS` (or `T+H:MM:SS` past an hour) from a count of seconds;
/// negative counts read `T-`, a countdown to the start.
fn format_clock(seconds: f64) -> String {
    let sign = if seconds < 0.0 { '-' } else { '+' };
    let total = seconds.abs().floor() as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("T{sign}{h}:{m:02}:{s:02}")
    } else {
        format!("T{sign}{m:02}:{s:02}")
    }
}

impl Render for SequenceControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        self.follow_channel(cx);

        let Some(shown) = self.shown(cx) else {
            let store = sequences::try_global(cx);
            return frame(SequenceLayout::Medium).justify_center().child(
                div()
                    .truncate()
                    .text_size(ui_px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(if store.is_none() {
                        SharedString::new_static("Sequence store unavailable")
                    } else if self.channel.is_empty() {
                        SharedString::new_static("No channel configured")
                    } else {
                        SharedString::from(format!("Unknown channel: {}", self.channel))
                    }),
            );
        };
        // An armed Stop the channel no longer takes (or that now points at
        // the past) must not survive to fire later.
        if !shown.allows(SequenceAccepts::STOP) {
            self.arming_stop = false;
        }

        let status = self.status(&shown, &theme, cx);
        let clocks = self.clocks(&shown.status, &theme, window, cx);
        let row = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(ui_px(8.0))
                .min_w_0()
        };
        match self.layout {
            SequenceLayout::Slim => {
                let load = self.load_row(&shown, SLIM_HEIGHT, &theme, cx);
                frame(self.layout).child(
                    row()
                        .child(status.flex_none())
                        .child(load.flex_1())
                        .when_some(clocks, |el, clocks| el.child(clocks))
                        .child(self.buttons(&shown, SLIM_HEIGHT, &theme, cx)),
                )
            }
            SequenceLayout::Medium => {
                let load = self.load_row(&shown, STRIP_BOX_HEIGHT, &theme, cx);
                let top = row()
                    .child(status.flex_1())
                    .when_some(clocks, |el, clocks| el.child(clocks));
                let bottom = row().child(load.flex_1()).child(self.buttons(
                    &shown,
                    STRIP_BOX_HEIGHT,
                    &theme,
                    cx,
                ));
                frame(self.layout).gap(ui_px(4.0)).child(top).child(bottom)
            }
            SequenceLayout::Square => {
                let load = self.load_row(&shown, STRIP_BOX_HEIGHT, &theme, cx);
                let bottom = row()
                    .child(div().flex_1().min_w_0().children(clocks))
                    .child(self.buttons(&shown, SQUARE_BUTTON, &theme, cx));
                frame(self.layout)
                    .justify_between()
                    .gap(ui_px(8.0))
                    .child(status)
                    .child(load)
                    .child(bottom)
            }
        }
    }
}

/// Height of a slim control's sequence box and buttons.
const SLIM_HEIGHT: f32 = 22.0;
/// Line height of the status message.
const STATUS_LINE: f32 = 18.0;
/// A square control's buttons, larger since it has the room.
const SQUARE_BUTTON: f32 = 32.0;

impl SequenceControl {
    /// The channel state and latest message, led by the state dot; a square
    /// control adds how long ago the message was raised. Armed, the confirm
    /// prompt takes the whole line: it is the one line the operator must not
    /// miss.
    fn status(&self, shown: &Shown, theme: &Theme, cx: &App) -> Stateful<Div> {
        let arming = self.arming_stop;
        let state_color = theme.run_state_color(shown.status.state);
        let square = self.layout == SequenceLayout::Square;
        let slim = self.layout == SequenceLayout::Slim;
        let dot = div()
            .flex_none()
            .size(ui_px(7.0))
            .rounded(ui_px(3.5))
            .bg(if arming {
                theme.error_accent
            } else {
                state_color
            });
        let line = div()
            .id("seq-ctl-status")
            .flex()
            .flex_row()
            .items_center()
            .gap(ui_px(6.0))
            .min_w_0()
            .text_size(ui_px(12.0))
            .line_height(ui_px(STATUS_LINE))
            .child(dot);
        if arming {
            return line.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.error_accent)
                    .child("Click stop again to hard-stop"),
            );
        }
        let message = shown.status.message.clone().filter(|m| !m.is_empty());
        let age = shown
            .status
            .message_at
            .filter(|_| square && message.is_some())
            .map(|at| SharedString::from(format_age(at, reference_time(shown.live, cx))));
        line.child(
            div()
                .flex_none()
                .text_color(state_color)
                .child(state_label(shown.status.state)),
        )
        // The channel name gives way before the message does: the message is
        // what changes, the name is on every other row of a list too.
        .when(!square && !slim, |el| {
            el.child(
                div()
                    .flex_shrink(1.0)
                    .min_w(ui_px(24.0))
                    .truncate()
                    .text_color(theme.text_tertiary)
                    .child(shown.channel.name.clone()),
            )
        })
        .when_some(message.clone().filter(|_| !slim), |el, message| {
            el.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme.text_primary)
                    .map(|el| {
                        if square {
                            el.line_clamp(2)
                        } else {
                            el.truncate()
                        }
                    })
                    .child(message),
            )
        })
        .when_some(age, |el, age| {
            el.child(
                div()
                    .flex_none()
                    .text_size(ui_px(10.0))
                    .text_color(theme.text_tertiary)
                    .child(age),
            )
        })
        // A narrow control truncates the message first; the tooltip keeps
        // it reachable.
        .when_some(message, |el, message| {
            el.tooltip(move |_window, cx| TooltipText::build(message.clone(), cx))
        })
    }

    /// The run clock, and the abort clock while a graceful stop is running.
    /// `None` when the channel names neither or neither has a reading.
    ///
    /// Each reading's small label sits on its value's baseline. Layout can't
    /// align baselines, so both share a line height and the label drops by
    /// the difference the font's metrics give between the two sizes.
    fn clocks(
        &self,
        status: &ChannelStatus,
        theme: &Theme,
        window: &Window,
        cx: &App,
    ) -> Option<Div> {
        let run = status.elapsed;
        let abort = status.abort_elapsed.filter(|s| *s > 0.0);
        if run.is_none() && abort.is_none() {
            return None;
        }
        let square = self.layout == SequenceLayout::Square;
        let size = if square { 15.0 } else { 12.0 };
        let label_size = 9.0;
        let line = (size * 1.3_f32).round();
        let text = window.text_system();
        let font = text.resolve_font(&gpui::font(font_family(cx)));
        let baseline = |s: f32| f32::from(text.baseline_offset(font, gpui::px(s), gpui::px(line)));
        let drop = (baseline(size) - baseline(label_size)).max(0.0);
        let reading = |label: &'static str, seconds: f64, color: Hsla| {
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_start()
                .gap(ui_px(4.0))
                .line_height(ui_px(line))
                .child(
                    div()
                        .pt(ui_px(drop))
                        .text_size(ui_px(label_size))
                        .text_color(theme.text_tertiary)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(ui_px(size))
                        .text_color(color)
                        .child(format_clock(seconds)),
                )
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_x(ui_px(10.0))
                .whitespace_nowrap()
                // A square tile stacks the clocks when it's too narrow for
                // both on one line; the other layouts keep them on theirs.
                .map(|el| {
                    if square {
                        el.flex_wrap().min_w_0()
                    } else {
                        el.flex_none()
                    }
                })
                .when_some(run, |el, s| el.child(reading("SEQ", s, theme.text_primary)))
                .when_some(abort, |el, s| {
                    el.child(reading("ABORT", s, theme.error_accent))
                }),
        )
    }

    /// Abort, the primary (start or reset) and the two-click Stop, each
    /// `height` unscaled pixels tall. Each acts only while the channel
    /// accepts its command.
    fn buttons(&self, shown: &Shown, height: f32, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let theme = theme.clone();
        let arming = self.arming_stop;
        let button =
            |id, icon, color, bg, tooltip| icon_button(id, icon, color, bg, tooltip, height);

        let abort_btn = if shown.allows(SequenceAccepts::ABORT) {
            pressable(
                button(
                    "seq-ctl-abort",
                    Icon::Abort,
                    theme.text_secondary,
                    theme.bg_elevated,
                    "Abort (safe stop)",
                ),
                &theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.abort(c))))
        } else {
            button(
                "seq-ctl-abort",
                Icon::Abort,
                theme.text_tertiary,
                theme.bg_elevated,
                "Abort",
            )
        };

        let primary_btn = if shown.allows(SequenceAccepts::START) {
            pressable(
                button(
                    "seq-ctl-primary",
                    Icon::Play,
                    theme.control_active,
                    theme.run_state_tint(Some(SequenceChannelState::Running)),
                    "Start",
                ),
                &theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.start(c))))
        } else if shown.allows(SequenceAccepts::RESET) {
            pressable(
                button(
                    "seq-ctl-primary",
                    Icon::Loop,
                    theme.text_secondary,
                    theme.bg_elevated,
                    "Reset",
                ),
                &theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.reset(c))))
        } else if shown.status.state == Some(SequenceChannelState::Running) {
            button(
                "seq-ctl-primary",
                Icon::Play,
                theme.control_active,
                Theme::dim(theme.control_active, 0.3),
                "Running",
            )
        } else {
            button(
                "seq-ctl-primary",
                Icon::Play,
                theme.text_tertiary,
                theme.bg_elevated,
                "Not startable",
            )
        };

        let stop_btn = if shown.allows(SequenceAccepts::STOP) {
            let armed_bg = Theme::dim(theme.error_accent, 0.25);
            button(
                "seq-ctl-stop",
                Icon::Stop,
                if arming {
                    theme.error_accent
                } else {
                    theme.text_secondary
                },
                if arming { armed_bg } else { theme.bg_elevated },
                if arming {
                    "Confirm hard stop"
                } else {
                    "Stop (hard, click twice)"
                },
            )
            // Armed, the button keeps its fill under the cursor: the hover
            // tint would cover it exactly while the pointer rests there,
            // hiding that the first click armed anything.
            .map(|el| {
                if arming {
                    el.cursor_pointer()
                        .border_1()
                        .border_color(theme.error_accent)
                } else {
                    pressable(el, &theme)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.arming_stop {
                    this.command(cx, |s, c| s.stop(c));
                } else {
                    this.arming_stop = true;
                    cx.notify();
                }
            }))
        } else {
            button(
                "seq-ctl-stop",
                Icon::Stop,
                theme.text_tertiary,
                theme.bg_elevated,
                "Stop",
            )
        };

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(ui_px(4.0))
            .child(abort_btn)
            .child(primary_btn)
            .child(stop_btn)
    }
}

/// The instant a message's age is measured from: now while live, the
/// selected time while scrubbed.
fn reference_time(live: bool, cx: &App) -> Timestamp {
    if live {
        Timestamp::now()
    } else {
        crate::temporal::view_time(cx).unwrap_or_else(Timestamp::now)
    }
}

impl SequenceControl {
    /// The sequence as a state chip's box with the commit chevron fused to
    /// it, the pair a value strip uses for a pending edit: clicking the box
    /// lists the sequences and choosing one only stages it, the chevron
    /// sends the load. A channel that loads nothing shows the system it
    /// wraps instead, read-only.
    fn load_row(&self, shown: &Shown, height: f32, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let available = shown.channel.available.clone();
        let text = self.box_text(shown);
        let chip = box_padding(box_corners(
            div()
                .id("seq-ctl-pick")
                .debug_selector(|| "seq-ctl-pick".into())
                .flex()
                .flex_1()
                .min_w_0()
                .h(ui_px(height)),
            !available.is_empty(),
        ))
        .text_size(ui_px(STRIP_BOX_VALUE_SIZE));
        if available.is_empty() {
            return div().flex().flex_row().items_center().min_w_0().child(
                chip.bg(box_fill(theme, false, false, None))
                    .text_color(theme.text_secondary)
                    .child(div().min_w_0().truncate().children(text)),
            );
        }

        let loadable = shown.allows(SequenceAccepts::LOAD);
        let pending = self.staged.is_some();
        let chip = chip
            .bg(box_fill(theme, pending, false, None))
            .text_color(if text.is_some() {
                theme.text_primary
            } else {
                theme.text_tertiary
            })
            .child(div().min_w_0().truncate().child(match text {
                Some(name) => name,
                None if loadable => SharedString::new_static("Choose sequence"),
                None => SharedString::new_static("Nothing loaded"),
            }))
            .when(loadable, |el| {
                let hover = theme.selection_bg;
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.arming_stop = false;
                        cx.notify();
                        let Some(open) = open_inspector(cx) else {
                            return;
                        };
                        open(
                            InspectorRequest {
                                rows: stage_rows(cx.weak_entity(), &available),
                                mode: InspectorMode::Anchored(event.position()),
                            },
                            window,
                            cx,
                        );
                    }))
            });

        let commit = apply_chevron("seq-ctl-commit", theme, pending && loadable)
            .h(ui_px(height))
            .debug_selector(|| "seq-ctl-commit".into())
            .when(pending && loadable, |el| {
                el.tooltip(|_window, cx| TooltipText::build("Load".into(), cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(name) = this.staged.take() {
                            this.command(cx, |s, c| s.load(c, name.to_string()));
                        }
                    }))
            });

        div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(chip)
            .child(commit)
    }
}

impl SequenceControl {
    /// What the sequence box names: the system a channel that loads nothing
    /// wraps, else the staged choice, else the loaded sequence.
    fn box_text(&self, shown: &Shown) -> Option<SharedString> {
        if shown.channel.available.is_empty() {
            return Some(shown.channel.system.clone());
        }
        self.staged
            .clone()
            .or_else(|| shown.channel.sequence(shown.status.loaded))
    }
}

/// One row per loadable sequence; choosing one stages it on the control.
fn stage_rows(
    control: WeakEntity<SequenceControl>,
    available: &[SharedString],
) -> Vec<Box<dyn InspectorRow>> {
    available
        .iter()
        .map(|name| {
            let name = name.clone();
            let control = control.clone();
            Box::new(CommandRow::new(
                name.clone(),
                Arc::new(move |_window, cx| {
                    let _ = control.update(cx, |this, cx| {
                        this.staged = Some(name.clone());
                        cx.notify();
                    });
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// The box every state of the control lays out in, so an unbound control
/// keeps the footprint of a live one. It draws no frame of its own: the
/// dashboard's widget frame or the pane is the one border around it.
fn frame(layout: SequenceLayout) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .px(ui_px(10.0))
        .when(layout == SequenceLayout::Square, |el| el.py(ui_px(10.0)))
        .overflow_hidden()
}

fn icon_button(
    id: &'static str,
    icon: Icon,
    color: Hsla,
    bg: Hsla,
    tooltip: &'static str,
    height: f32,
) -> Stateful<Div> {
    let width = height.clamp(SLIM_HEIGHT, SQUARE_BUTTON);
    div()
        .id(id)
        .debug_selector(|| id.into())
        .flex_none()
        .w(ui_px(width))
        .h(ui_px(height))
        .flex()
        .items_center()
        .justify_center()
        .rounded(ui_px(4.0))
        .bg(bg)
        .tooltip(move |_window, cx| TooltipText::build(tooltip.into(), cx))
        .child(icon.svg(
            if height > STRIP_BOX_HEIGHT {
                15.0
            } else {
                13.0
            },
            color,
        ))
}

/// The pointer and hover tint of an [`icon_button`] that acts on click.
/// Inert ones (a command the channel won't take now) keep neither, so they
/// don't read as pressable.
fn pressable(button: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.selection_bg;
    button.cursor_pointer().hover(move |s| s.bg(hover))
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui::{Modifiers, MouseButton, Point, TestAppContext, VisualTestContext};
    use metor_db::DB;
    use metor_db::disruptor::Reader;
    use metor_db::msg_log::read_msg;
    use metor_proto::types::{Msg, Timestamp};
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind, SequenceRegistry};

    use super::*;
    use crate::inspector::OpenInspectorGlobal;
    use crate::sequences::tests::{Frame, declare, install_status};
    use crate::temporal::{TimeAction, TimeExpr};
    use crate::views::dashboard::{DashboardWidget, WidgetId, WidgetKind, WidgetRect};

    fn push<T: serde::Serialize + Msg>(db: &DB, ts: i64, msg: &T) {
        db.push_msg(Timestamp(ts), T::ID, &postcard::to_allocvec(msg).unwrap())
            .unwrap();
    }

    /// `solar` running for 75 s: abortable and stoppable, nothing else.
    fn running() -> Frame {
        Frame {
            state: SequenceChannelState::Running,
            loaded: 0,
            accepts: SequenceAccepts::ABORT | SequenceAccepts::STOP,
            message: "started",
            message_at: 2,
            elapsed: 75.0,
            ..Frame::default()
        }
    }

    /// `solar` hard-stopped: the target takes a start, a reset or a load.
    fn stopped() -> Frame {
        Frame {
            state: SequenceChannelState::Stopped,
            loaded: 0,
            accepts: SequenceAccepts::START | SequenceAccepts::RESET | SequenceAccepts::LOAD,
            message: "stopped",
            message_at: 3,
            elapsed: 75.0,
            ..Frame::default()
        }
    }

    fn config() -> SequenceControlConfig {
        SequenceControlConfig {
            channel: "deploy".into(),
            ..Default::default()
        }
    }

    /// Hosts any view as a window root, so a test clicks the control
    /// through the same host a user does.
    struct Host(gpui::AnyView);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    /// A `deploy` channel with `solar` and `lunar` loadable, its state the
    /// recorded `frames`, shown in `host`, plus a tap on every command the
    /// panel publishes.
    struct Rig<'a> {
        cx: &'a mut VisualTestContext,
        commands: Reader,
        inspector: Rc<RefCell<Vec<InspectorRequest>>>,
        _temp: tempfile::TempDir,
    }

    impl<'a> Rig<'a> {
        fn new(
            cx: &'a mut TestAppContext,
            host: impl FnOnce(Arc<DB>, &mut App) -> gpui::AnyView,
        ) -> Self {
            Self::with(
                cx,
                declare("deploy", &["solar", "lunar"], "dl_slot"),
                &[(3, running())],
                host,
            )
        }

        fn with(
            cx: &'a mut TestAppContext,
            registry: SequenceRegistry,
            frames: &[(i64, Frame)],
            host: impl FnOnce(Arc<DB>, &mut App) -> gpui::AnyView,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
            let inspector = Rc::new(RefCell::new(Vec::new()));
            install_status(&db, "deploy", frames);
            cx.update(|cx| {
                crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
                cx.set_global(crate::theme::FontSettings {
                    family: "monospace".into(),
                    config: Default::default(),
                });
                crate::views::dashboard::WidgetRegistry::init(cx);
                crate::temporal::TemporalController::init(db.clone(), cx);
                crate::sequences::SequenceStore::init(db.clone(), cx);
                let inspector = inspector.clone();
                cx.set_global(OpenInspectorGlobal(Arc::new(move |request, _, _| {
                    inspector.borrow_mut().push(request);
                })));
            });
            cx.run_until_parked();
            push(&db, 1, &registry);
            cx.run_until_parked();

            let commands = db
                .with_state_mut(|s| {
                    s.get_or_insert_msg_log(SequenceCommand::ID, &db.path)
                        .cloned()
                })
                .unwrap()
                .wal_reader();
            let (_host, cx) = cx.add_window_view(|_, cx| Host(host(db.clone(), cx)));
            cx.refresh().unwrap();
            cx.run_until_parked();
            Self {
                cx,
                commands,
                inspector,
                _temp: temp,
            }
        }

        fn center(&mut self, selector: &'static str) -> Point<gpui::Pixels> {
            self.cx.refresh().unwrap();
            self.cx.debug_bounds(selector).unwrap().center()
        }

        fn click(&mut self, selector: &'static str) {
            let at = self.center(selector);
            self.cx.simulate_click(at, Modifiers::default());
            self.cx.run_until_parked();
        }

        fn sent(&mut self) -> Vec<SequenceCommandKind> {
            let mut out = Vec::new();
            while let Some(grant) = self.commands.try_next() {
                let mut buf: &[u8] = &grant;
                while let Some((rest, _, msg)) = read_msg(buf) {
                    buf = rest;
                    out.push(
                        postcard::from_bytes::<SequenceCommand>(msg)
                            .unwrap()
                            .command,
                    );
                }
            }
            out
        }

        fn seek(&mut self, at: i64) {
            self.cx.update(|_, cx| {
                crate::temporal::dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(at))), cx)
                    .unwrap()
            });
            self.cx.run_until_parked();
        }

        /// Two Stop clicks the way a hand makes them: hovered long enough
        /// for the tooltip, the second a double-click, and a status update
        /// repainting the control between its press and release.
        fn stop_twice(&mut self) -> (Vec<SequenceCommandKind>, Vec<SequenceCommandKind>) {
            let stop = self.center("seq-ctl-stop");
            let cx = &mut *self.cx;
            cx.simulate_mouse_move(stop, None, Modifiers::default());
            cx.executor()
                .advance_clock(std::time::Duration::from_secs(2));
            cx.run_until_parked();
            cx.simulate_click(stop, Modifiers::default());
            cx.run_until_parked();
            let first = self.sent();
            let cx = &mut *self.cx;
            cx.simulate_event(gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: stop,
                click_count: 2,
                ..Default::default()
            });
            cx.update(|_, cx| {
                let reader = ChannelStatusReader::acquire("deploy".into(), cx).unwrap();
                reader.update(cx, |_, cx| cx.notify());
            });
            cx.run_until_parked();
            cx.refresh().unwrap();
            cx.simulate_event(gpui::MouseUpEvent {
                button: MouseButton::Left,
                position: stop,
                click_count: 2,
                ..Default::default()
            });
            cx.run_until_parked();
            (first, self.sent())
        }
    }

    fn bare(_db: Arc<DB>, cx: &mut App) -> gpui::AnyView {
        cx.new(|cx| SequenceControl::from_config(&config(), cx))
            .into()
    }

    /// A bare control in `layout`, also handed to `slot` for inspection.
    fn laid_out(
        layout: SequenceLayout,
        slot: Rc<RefCell<Option<gpui::Entity<SequenceControl>>>>,
    ) -> impl FnOnce(Arc<DB>, &mut App) -> gpui::AnyView {
        move |_db, cx| {
            let cfg = SequenceControlConfig { layout, ..config() };
            let control = cx.new(|cx| SequenceControl::from_config(&cfg, cx));
            *slot.borrow_mut() = Some(control.clone());
            control.into()
        }
    }

    fn stop_fires_in(layout: SequenceLayout, cx: &mut TestAppContext) {
        let slot = Rc::new(RefCell::new(None));
        let (first, second) = Rig::new(cx, laid_out(layout, slot)).stop_twice();
        assert!(first.is_empty(), "{layout:?}: the first click only arms");
        assert!(
            matches!(second.as_slice(), [SequenceCommandKind::Stop]),
            "{layout:?}: {second:?}"
        );
    }

    #[gpui::test]
    fn stop_fires_on_the_second_click_in_a_slim_control(cx: &mut TestAppContext) {
        stop_fires_in(SequenceLayout::Slim, cx);
    }

    #[gpui::test]
    fn stop_fires_on_the_second_click_in_a_square_control(cx: &mut TestAppContext) {
        stop_fires_in(SequenceLayout::Square, cx);
    }

    #[gpui::test]
    fn the_clocks_read_the_components_the_registry_names(cx: &mut TestAppContext) {
        let slot = Rc::new(RefCell::new(None));
        let rig = Rig::new(cx, laid_out(SequenceLayout::Square, slot.clone()));
        let control = slot.borrow().clone().expect("built");
        rig.cx.update(|window, cx| {
            let control = control.read(cx);
            let shown = control.shown(cx).expect("declared");
            assert_eq!(shown.status.elapsed, Some(75.0));
            // Zero: no graceful stop is running, so the clock stays hidden.
            assert_eq!(shown.status.abort_elapsed, Some(0.0));
            assert!(
                control
                    .clocks(&shown.status, &crate::theme::theme(cx), window, cx)
                    .is_some()
            );
        });
    }

    #[test]
    fn clocks_read_as_mission_time() {
        assert_eq!(format_clock(0.0), "T+00:00");
        assert_eq!(format_clock(75.9), "T+01:15");
        assert_eq!(format_clock(3725.0), "T+1:02:05");
        assert_eq!(format_clock(-30.0), "T-00:30");
    }

    fn in_dashboard(db: Arc<DB>, cx: &mut App) -> gpui::AnyView {
        let widget = DashboardWidget {
            id: WidgetId(1),
            rect: WidgetRect {
                x: 20.0,
                y: 20.0,
                w: 340.0,
                h: 48.0,
            },
            kind: WidgetKind::sequence_control(),
            config: serde_json::to_string(&config()).unwrap(),
            frame: true,
        };
        let blob = serde_json::json!({ "title": "t", "widgets": [widget], "zoom": 0.7 });
        crate::views::dashboard::deserialize_dashboard(db, &blob.to_string(), cx).into()
    }

    #[gpui::test]
    fn stop_fires_on_the_second_click(cx: &mut TestAppContext) {
        let (first, second) = Rig::new(cx, bare).stop_twice();
        assert!(first.is_empty(), "the first click only arms");
        assert!(matches!(second.as_slice(), [SequenceCommandKind::Stop]));
    }

    #[gpui::test]
    fn stop_fires_on_the_second_click_in_a_dashboard(cx: &mut TestAppContext) {
        let (first, second) = Rig::new(cx, in_dashboard).stop_twice();
        assert!(first.is_empty(), "the first click only arms");
        assert!(matches!(second.as_slice(), [SequenceCommandKind::Stop]));
    }

    fn in_channel_list(_db: Arc<DB>, cx: &mut App) -> gpui::AnyView {
        cx.new(crate::views::SequenceView::new).into()
    }

    #[gpui::test]
    fn stop_fires_on_the_second_click_in_the_channel_list(cx: &mut TestAppContext) {
        let (first, second) = Rig::new(cx, in_channel_list).stop_twice();
        assert!(first.is_empty(), "the first click only arms");
        assert!(matches!(second.as_slice(), [SequenceCommandKind::Stop]));
    }

    #[gpui::test]
    fn choosing_a_sequence_stages_it_and_the_arrow_loads_it(cx: &mut TestAppContext) {
        let mut rig = Rig::with(
            cx,
            declare("deploy", &["solar", "lunar"], "dl_slot"),
            &[(3, stopped())],
            bare,
        );
        rig.click("seq-ctl-commit");
        assert!(rig.sent().is_empty(), "nothing staged, nothing to commit");

        rig.click("seq-ctl-pick");
        let mut request = rig.inspector.borrow_mut().pop().expect("picker opened");
        assert!(matches!(request.mode, InspectorMode::Anchored(_)));
        let row = request
            .rows
            .iter_mut()
            .find(|row| row.label() == "lunar")
            .expect("every loadable sequence is listed");
        rig.cx.update(|window, cx| row.activate(window, cx));
        rig.cx.run_until_parked();
        assert!(rig.sent().is_empty(), "choosing only stages");

        rig.click("seq-ctl-commit");
        let sent = rig.sent();
        assert!(
            matches!(sent.as_slice(), [SequenceCommandKind::Load { name }] if name == "lunar"),
            "{sent:?}"
        );
        rig.click("seq-ctl-commit");
        assert!(rig.sent().is_empty(), "a commit consumes the staged choice");
    }

    #[gpui::test]
    fn the_picker_stays_shut_while_the_channel_refuses_loads(cx: &mut TestAppContext) {
        let mut rig = Rig::new(cx, bare);
        rig.click("seq-ctl-pick");
        assert!(rig.inspector.borrow().is_empty());
    }

    /// After a stop the target takes a start or a reset; the primary button
    /// offers the start, and Abort is inert without the target taking it.
    #[gpui::test]
    fn the_buttons_follow_what_the_channel_accepts(cx: &mut TestAppContext) {
        let mut rig = Rig::with(
            cx,
            declare("deploy", &["solar", "lunar"], "dl_slot"),
            &[(3, stopped())],
            bare,
        );
        rig.click("seq-ctl-abort");
        assert!(rig.sent().is_empty(), "abort isn't accepted");
        rig.click("seq-ctl-stop");
        rig.click("seq-ctl-stop");
        assert!(rig.sent().is_empty(), "stop isn't accepted");
        rig.click("seq-ctl-primary");
        assert!(matches!(
            rig.sent().as_slice(),
            [SequenceCommandKind::Start]
        ));
    }

    #[gpui::test]
    fn the_primary_resets_when_only_a_reset_is_accepted(cx: &mut TestAppContext) {
        let completed = Frame {
            state: SequenceChannelState::Completed,
            accepts: SequenceAccepts::RESET,
            ..stopped()
        };
        let mut rig = Rig::with(
            cx,
            declare("deploy", &["solar"], "dl_slot"),
            &[(3, completed)],
            bare,
        );
        rig.click("seq-ctl-primary");
        assert!(matches!(
            rig.sent().as_slice(),
            [SequenceCommandKind::Reset]
        ));
        rig.click("seq-ctl-abort");
        assert!(rig.sent().is_empty());
    }

    /// Scrubbed back to when the channel ran, the control shows that state
    /// but sends nothing, even for commands accepted then.
    #[gpui::test]
    fn a_scrubbed_control_shows_the_past_and_sends_nothing(cx: &mut TestAppContext) {
        let slot = Rc::new(RefCell::new(None));
        let mut rig = Rig::with(
            cx,
            declare("deploy", &["solar", "lunar"], "dl_slot"),
            &[(3, running()), (10, stopped())],
            laid_out(SequenceLayout::Medium, slot.clone()),
        );
        rig.seek(5);
        let control = slot.borrow().clone().unwrap();
        rig.cx.update(|_, cx| {
            let shown = control.read(cx).shown(cx).unwrap();
            assert!(!shown.live);
            assert_eq!(shown.status.state, Some(SequenceChannelState::Running));
            assert!(shown.status.allows(SequenceAccepts::ABORT));
        });
        for button in [
            "seq-ctl-abort",
            "seq-ctl-primary",
            "seq-ctl-stop",
            "seq-ctl-stop",
        ] {
            rig.click(button);
        }
        rig.click("seq-ctl-pick");
        assert!(rig.sent().is_empty());
        assert!(rig.inspector.borrow().is_empty(), "no picker in the past");
    }

    /// A channel that loads nothing names the system it wraps, read-only.
    #[gpui::test]
    fn a_channel_without_sequences_shows_its_system(cx: &mut TestAppContext) {
        let slot = Rc::new(RefCell::new(None));
        let mut rig = Rig::with(
            cx,
            declare("deploy", &[], "dl_slot"),
            &[(
                3,
                Frame {
                    accepts: SequenceAccepts::LOAD | SequenceAccepts::START,
                    ..stopped()
                },
            )],
            laid_out(SequenceLayout::Medium, slot.clone()),
        );
        let control = slot.borrow().clone().unwrap();
        rig.cx.update(|_, cx| {
            let control = control.read(cx);
            let shown = control.shown(cx).unwrap();
            assert_eq!(control.box_text(&shown).as_deref(), Some("dl_slot"));
        });
        rig.click("seq-ctl-pick");
        assert!(rig.inspector.borrow().is_empty());
        assert!(rig.cx.debug_bounds("seq-ctl-commit").is_none());
    }
}
