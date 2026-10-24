//! Start/stop control for a single sequence channel.
//!
//! [`SequenceView`](super::SequenceView) lists every channel and
//! [`SequenceGrid`](super::SequenceGrid) reduces them to status dots; neither
//! is what an operator wants when one channel *is* the operation. This is
//! that third shape: one channel, its state and progress, and the verbs
//! reachable in a single click rather than through the inspector.
//!
//! The shape is one strip-height row: the channel and its run state over a
//! state-chip box naming the sequence on the left, transport-style icon
//! buttons on the right, all one box height. Picking from the box only
//! stages a choice; the chevron fused to its right edge sends the load, the same stage-then-apply pair a value strip uses for a
//! pending edit, since a load replaces whatever the channel holds. The
//! protocol has no pause, so the slot a transport gives pause holds Abort
//! (the commanded safe stop, under its own glyph) rather than a hold the
//! control system couldn't honor; the play slot turns into Reset once a run
//! has ended.
//!
//! No new command plumbing — the buttons drive the same
//! [`SequenceStore`](crate::sequences::SequenceStore) the list does, so the
//! command still travels panel DB → link → `UplinkSystem`.

use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, Div, Hsla, IntoElement, SharedString, Stateful, WeakEntity, Window,
    div, prelude::*,
};
use metor_proto_wkt::SequenceRunState;
use serde::{Deserialize, Serialize};

use crate::icons::Icon;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::sequences::{self, ChannelState, is_resettable, run_state_index, run_state_label};
use crate::theme::{Theme, theme, ui_px};
use crate::views::TooltipText;
use crate::views::value_strip::{
    STRIP_BOX_HEIGHT, STRIP_BOX_VALUE_SIZE, apply_chevron, box_corners, box_fill, box_padding,
};

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
    /// The sequence chosen in the picker but not yet sent. Loading replaces
    /// whatever the channel holds, so choosing only stages the name and the
    /// commit arrow sends it.
    #[facet(skip)]
    staged: Option<SharedString>,
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
            staged: None,
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
            return strip().justify_center().child(
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

        // Armed, the confirm prompt takes the whole line: in a narrow card
        // the channel and state would otherwise squeeze it to nothing, and
        // it is the one line the operator must not miss.
        let status = (!self.compact).then(|| ch.last_message.clone()).flatten();
        let subtitle = div()
            .id("seq-ctl-status")
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
                    .bg(if arming {
                        theme.error_accent
                    } else {
                        state_color
                    }),
            )
            .map(|el| {
                if arming {
                    return el.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.error_accent)
                            .child("click stop again"),
                    );
                }
                el.child(
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
                            .text_color(theme.text_tertiary)
                            .child(status),
                    )
                })
            })
            // Refusals and progress land in the status, which a narrow card
            // truncates first; the tooltip keeps them reachable.
            .when_some(ch.last_message.clone(), |el, message| {
                el.tooltip(move |_window, cx| TooltipText::build(message.clone(), cx))
            });

        let label = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(ui_px(3.0))
            .child(subtitle)
            .child(self.load_row(&ch, &theme, cx));

        let abort_btn = pressable(
            icon_button(
                "seq-ctl-abort",
                Icon::Abort,
                theme.text_secondary,
                theme.bg_elevated,
                "Abort (safe stop)",
            ),
            &theme,
        )
        .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.abort(c))));

        let primary_btn = match ch.run_state {
            SequenceRunState::Running => icon_button(
                "seq-ctl-primary",
                Icon::Play,
                theme.control_active,
                Theme::dim(theme.control_active, 0.3),
                "Running",
            ),
            SequenceRunState::Idle => pressable(
                icon_button(
                    "seq-ctl-primary",
                    Icon::Play,
                    theme.control_active,
                    theme.run_state_tint(run_state_index(SequenceRunState::Running)),
                    "Start",
                ),
                &theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.start(c)))),
            state if is_resettable(state) => pressable(
                icon_button(
                    "seq-ctl-primary",
                    Icon::Loop,
                    theme.text_secondary,
                    theme.bg_elevated,
                    "Reset",
                ),
                &theme,
            )
            .on_click(cx.listener(|this, _, _, cx| this.command(cx, |s, c| s.reset(c)))),
            _ => icon_button(
                "seq-ctl-primary",
                Icon::Play,
                theme.text_tertiary,
                theme.bg_elevated,
                "Not startable",
            ),
        };

        let armed_bg = Theme::dim(theme.error_accent, 0.25);
        let stop_btn = icon_button(
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
        // Armed, the button keeps its fill under the cursor: the hover tint
        // would cover it exactly while the pointer rests there, hiding that
        // the first click armed anything.
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
        }));

        strip().child(label).child(
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

impl SequenceControl {
    /// The sequence as a state chip's box with the commit chevron fused to
    /// it, the pair a value strip uses for a pending edit: clicking the box
    /// lists the sequences and choosing one only stages it, the chevron
    /// sends the load.
    fn load_row(&self, ch: &ChannelState, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let pending = self.staged.is_some();
        let shown = self.staged.clone().or_else(|| ch.loaded.clone());
        let available = ch.available.clone();
        let pickable = !available.is_empty();

        let chip = box_padding(box_corners(
            div()
                .id("seq-ctl-pick")
                .debug_selector(|| "seq-ctl-pick".into())
                .flex()
                .flex_1()
                .min_w_0()
                .h(ui_px(STRIP_BOX_HEIGHT)),
            true,
        ))
        .bg(box_fill(theme, pending, false, None))
        .text_size(ui_px(STRIP_BOX_VALUE_SIZE))
        .text_color(if shown.is_some() {
            theme.text_primary
        } else {
            theme.text_tertiary
        })
        .child(div().min_w_0().truncate().child(match shown {
            Some(name) => name,
            None if pickable => SharedString::new_static("Choose sequence"),
            None => SharedString::new_static("No sequences"),
        }))
        .when(pickable, |el| {
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

        let commit = apply_chevron("seq-ctl-commit", theme, pending)
            .debug_selector(|| "seq-ctl-commit".into())
            .when(pending, |el| {
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

/// The row every state of the control lays out in, so an unbound control
/// keeps the footprint of a live one. It draws no frame of its own: the
/// dashboard's widget frame or the pane is the one border around it.
fn strip() -> Div {
    div()
        .size_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(ui_px(8.0))
        .px(ui_px(10.0))
        .overflow_hidden()
}

fn icon_button(
    id: &'static str,
    icon: Icon,
    color: Hsla,
    bg: Hsla,
    tooltip: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(|| id.into())
        .flex_none()
        .size(ui_px(STRIP_BOX_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(ui_px(4.0))
        .bg(bg)
        .tooltip(move |_window, cx| TooltipText::build(tooltip.into(), cx))
        .child(icon.svg(13.0, color))
}

/// The pointer and hover tint of an [`icon_button`] that acts on click.
/// Inert ones (Running, Not startable) keep neither, so they don't read as
/// pressable.
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
    use metor_proto_wkt::{
        SequenceChannelEvent, SequenceChannelSpec, SequenceCommand, SequenceCommandKind,
        SequenceEventKind, SequenceRegistry,
    };

    use super::*;
    use crate::inspector::OpenInspectorGlobal;
    use crate::views::dashboard::{DashboardWidget, WidgetId, WidgetKind, WidgetRect};

    fn push<T: serde::Serialize + Msg>(db: &DB, ts: i64, msg: &T) {
        db.push_msg(Timestamp(ts), T::ID, &postcard::to_allocvec(msg).unwrap())
            .unwrap();
    }

    fn event(kind: SequenceEventKind) -> SequenceChannelEvent {
        SequenceChannelEvent {
            channel: "deploy".into(),
            kind,
        }
    }

    fn config() -> SequenceControlConfig {
        SequenceControlConfig {
            channel: "deploy".into(),
            compact: false,
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

    /// A running `deploy` channel with `solar` loaded and `lunar` loadable,
    /// shown in `host`, plus a tap on every command the panel publishes.
    struct Rig<'a> {
        cx: &'a mut VisualTestContext,
        db: Arc<DB>,
        commands: Reader,
        inspector: Rc<RefCell<Vec<InspectorRequest>>>,
        _temp: tempfile::TempDir,
    }

    impl<'a> Rig<'a> {
        fn new(
            cx: &'a mut TestAppContext,
            host: impl FnOnce(Arc<DB>, &mut App) -> gpui::AnyView,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
            let inspector = Rc::new(RefCell::new(Vec::new()));
            cx.update(|cx| {
                crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
                crate::views::dashboard::WidgetRegistry::init(cx);
                crate::sequences::SequenceStore::init(db.clone(), cx);
                let inspector = inspector.clone();
                cx.set_global(OpenInspectorGlobal(Arc::new(move |request, _, _| {
                    inspector.borrow_mut().push(request);
                })));
            });
            cx.run_until_parked();
            push(
                &db,
                1,
                &SequenceRegistry {
                    channels: vec![SequenceChannelSpec {
                        name: "deploy".into(),
                        available: vec!["solar".into(), "lunar".into()],
                    }],
                },
            );
            push(
                &db,
                2,
                &event(SequenceEventKind::Loaded {
                    name: "solar".into(),
                }),
            );
            push(&db, 3, &event(SequenceEventKind::Started));
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
                db,
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

        /// Two Stop clicks the way a hand makes them: hovered long enough
        /// for the tooltip, the second a double-click, and a progress event
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
            push(
                &self.db,
                4,
                &event(SequenceEventKind::Progress {
                    detail: "phase 2".into(),
                }),
            );
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

    #[gpui::test]
    fn choosing_a_sequence_stages_it_and_the_arrow_loads_it(cx: &mut TestAppContext) {
        let mut rig = Rig::new(cx, bare);
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
}
