use super::*;
use crate::inspector::{
    InspectorMode, InspectorRequest,
    rows::{BoolRow, CommandRow, HeaderRow, InspectorRow, NavRow},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    FitContext,
    FitRange,
    FollowTime,
    PreviousZoom,
    ZoomIn,
    ZoomOut,
    UseVisibleRange,
    PreviousEvent,
    NextEvent,
}

impl Timeline {
    pub(crate) fn register(cx: &mut App) {
        cx.global_mut::<crate::inspector::registry::InspectorRegistry>()
            .register_type_builder::<Self>(Arc::new(|entity, _, cx| {
                entity
                    .downcast::<Self>()
                    .map(|e| Self::rows(e, cx))
                    .unwrap_or_default()
            }));
    }
    pub(crate) fn rows(entity: Entity<Self>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
        entity.read(cx).inspector_rows(entity.clone())
    }
    // Mouse listeners already hold the entity's update lease. Build from that
    // state directly; reading the entity again would panic in GPUI.
    fn inspector_rows(&self, entity: Entity<Self>) -> Vec<Box<dyn InspectorRow>> {
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        for (label, command) in [
            ("Fit context", Command::FitContext),
            ("Fit selected range", Command::FitRange),
            ("Follow view time", Command::FollowTime),
            ("Previous zoom", Command::PreviousZoom),
            ("Zoom in", Command::ZoomIn),
            ("Zoom out", Command::ZoomOut),
            (
                "Use visible timeline as global range",
                Command::UseVisibleRange,
            ),
            ("Previous event", Command::PreviousEvent),
            ("Next event", Command::NextEvent),
        ] {
            let entity = entity.clone();
            rows.push(Box::new(CommandRow::new(
                label,
                Arc::new(move |window, cx| {
                    entity.update(cx, |this, cx| this.command(command, window, cx));
                }),
            )));
        }
        let snap = self.config.snap;
        let target = entity.clone();
        rows.push(Box::new(BoolRow::new(
            "Snap to events and anchors",
            snap,
            Arc::new(move |value, _, cx| {
                target.update(cx, |this, cx| {
                    this.config.snap = value;
                    cx.notify();
                });
            }),
        )));
        for lane in &self.lanes {
            let key = lane.key.clone();
            let visible = !self.config.collapsed.contains(&key);
            let entity = entity.clone();
            rows.push(Box::new(BoolRow::new(
                format!("Show {}", lane.name),
                visible,
                Arc::new(move |value, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.config.collapsed.retain(|s| s != &key);
                        if !value {
                            this.config.collapsed.push(key.clone());
                        }
                        cx.notify();
                    });
                }),
            )));
        }
        rows.push(Box::new(HeaderRow::new(
            "Event lanes show retained history; source filters are independent of telemetry scope.",
        )));
        rows.push(Box::new(NavRow::new(
            "Global time controls",
            "",
            Box::new(temporal::picker::rows),
        )));
        rows
    }
    pub(super) fn open_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_hover(cx);
        if self.edit.is_some() {
            return;
        }
        if let Some(open) = crate::inspector::open_inspector(cx) {
            let mut rows = self.inspector_rows(cx.entity());
            let events = self.nearby_events(position);
            if !events.is_empty() {
                rows.insert(
                    0,
                    Box::new(NavRow::new(
                        format!("{} events near cursor", events.len()),
                        "",
                        Box::new(move |cx| event_list_rows(&events, 0, cx)),
                    )),
                );
            }

            open(
                InspectorRequest {
                    rows,
                    mode: InspectorMode::Anchored(position),
                },
                window,
                cx,
            );
        }
        cx.stop_propagation();
    }
    fn nearby_events(&self, position: Point<Pixels>) -> Vec<Arc<TimelineEvent>> {
        if let Some((events, _)) = self.hit_events(position, usize::MAX) {
            return events;
        }
        let Some(view) = self.viewport else {
            return Vec::new();
        };
        let time = view.at(self.fraction(position.x));
        let tolerance = (view.span() * 6.0
            / self
                .time_area()
                .map_or(500.0, |a| f64::from(f32::from(a.size.width).max(1.0))))
        .ceil() as i128;
        let start = clamp(i128::from(time) - tolerance);
        let end = clamp(i128::from(time) + tolerance + 1);
        let mut events: Vec<_> = self
            .lanes
            .iter()
            .filter(|l| !self.config.collapsed.contains(&l.key))
            .flat_map(|lane| {
                lane.index.events[lane.index.bounds(start, end)]
                    .iter()
                    .cloned()
            })
            .collect();
        for lane in &self.lanes {
            if !self.config.collapsed.contains(&lane.key) {
                events.extend(
                    lane.index
                        .spans_in(start, end, 256)
                        .into_iter()
                        .filter(|e| e.event.ts.0 < start)
                        .cloned(),
                );
            }
        }
        events.sort_by_key(|e| (e.event.ts, e.id));
        events
    }
    pub(super) fn inspect_events(
        &self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.edit.is_some() {
            return;
        }
        let events = self.nearby_events(position);
        if !events.is_empty()
            && let Some(open) = crate::inspector::open_inspector(cx)
        {
            open(
                InspectorRequest {
                    rows: event_list_rows(&events, 0, cx),
                    mode: InspectorMode::Anchored(position),
                },
                window,
                cx,
            );
        }
    }
    fn command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::FitContext => self.navigate(Navigation::Fit, cx),
            Command::FitRange => {
                if let Some(range) = self.range(cx) {
                    self.navigate(Navigation::Manual(range.padded()), cx);
                }
            }
            Command::FollowTime => {
                if let Some(view) = self.viewport {
                    self.navigate(Navigation::Follow { span: view.span() }, cx);
                }
            }
            Command::PreviousZoom => {
                if let Some(previous) = self.history.pop() {
                    self.config.navigation = previous;
                    if previous == Navigation::Fit {
                        self.fit = None;
                    }
                    cx.notify();
                }
            }
            Command::ZoomIn | Command::ZoomOut => {
                let fraction = self
                    .viewport
                    .zip(self.time(cx))
                    .map_or(0.5, |(v, t)| v.fraction(t).clamp(0.0, 1.0));
                self.zoom(
                    if command == Command::ZoomIn { 0.5 } else { 2.0 },
                    fraction,
                    cx,
                );
            }
            Command::UseVisibleRange => {
                if let Some(view) = self.viewport {
                    self.emit(
                        TimeAction::Range(TimeRangeSpec::fixed(
                            Timestamp(view.start)..Timestamp(view.end),
                        )),
                        window,
                        cx,
                    );
                }
            }
            Command::PreviousEvent | Command::NextEvent => {
                let time = self.time(cx).unwrap_or(0);
                let candidate = self
                    .lanes
                    .iter()
                    .filter_map(|l| {
                        l.index.nearest(
                            time,
                            if command == Command::PreviousEvent {
                                -1
                            } else {
                                1
                            },
                        )
                    })
                    .min_by_key(|e| (i128::from(e.event.ts.0) - i128::from(time)).abs())
                    .cloned();
                if let Some(e) = candidate {
                    self.selected = Some(e.clone());
                    self.emit(TimeAction::Seek(TimeExpr::fixed(e.event.ts)), window, cx);
                }
            }
        }
    }
    pub(super) fn key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" if self.drag.is_some() => {
                self.drag = None;
                self.draft = None;
                self.pending_seek = None;
                cx.notify();
            }
            "+" | "=" => self.command(Command::ZoomIn, window, cx),
            "-" => self.command(Command::ZoomOut, window, cx),
            "f" => self.command(Command::FitContext, window, cx),
            "z" => self.command(Command::FitRange, window, cx),
            "x" => self.command(Command::PreviousZoom, window, cx),
            "left" | "right" => {
                let direction = if event.keystroke.key == "left" { -1 } else { 1 };
                if let Some(t) = self.time(cx) {
                    let step = temporal::config(cx).step_micros;
                    self.emit(
                        TimeAction::Seek(TimeExpr::fixed(Timestamp(clamp(
                            i128::from(t) + i128::from(step) * direction,
                        )))),
                        window,
                        cx,
                    );
                }
            }
            "i" | "o" => {
                if let Some(t) = self.time(cx) {
                    let mut range = self.range_spec(cx);
                    if event.keystroke.key == "i" {
                        range.start = TimeExpr::fixed(Timestamp(t));
                    } else {
                        range.end = TimeExpr::fixed(Timestamp(t));
                    }
                    self.emit(TimeAction::Range(range), window, cx);
                }
            }
            "space" if self.edit.is_none() => {
                let playing = temporal::snapshot(cx).is_some_and(|s| s.live || s.playing);
                self.emit(
                    if playing {
                        TimeAction::Pause
                    } else {
                        TimeAction::Play { from_start: false }
                    },
                    window,
                    cx,
                );
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

fn event_list_rows(
    events: &[Arc<TimelineEvent>],
    offset: usize,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    for event in events.iter().skip(offset).take(100) {
        let event = event.clone();
        rows.push(Box::new(NavRow::new(
            format!(
                "{} · {}",
                temporal::display::label(event.event.ts, cx),
                event.event.label
            ),
            "",
            Box::new(move |cx| event_rows(event.clone(), cx)),
        )));
    }
    if offset + 100 < events.len() {
        let events = events.to_vec();
        rows.push(Box::new(NavRow::new(
            "More events",
            "",
            Box::new(move |cx| event_list_rows(&events, offset + 100, cx)),
        )));
    }
    rows
}

fn event_rows(event: Arc<TimelineEvent>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    use crate::inspector::rows::{PreviewSpec, RowAction, TextRow};
    let time = event.event.ts;
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![
        Box::new(HeaderRow::new(event.event.label.clone())),
        Box::new(TextRow::readonly(
            "Time".into(),
            temporal::display::label(time, cx).into(),
        )),
        Box::new(CommandRow::new(
            "Go to event time",
            Arc::new(move |_, cx| {
                let _ = temporal::dispatch(TimeAction::Seek(TimeExpr::fixed(time)), cx);
            }),
        )),
    ];
    if let Some(end) = event.end {
        rows.push(Box::new(CommandRow::new(
            "Set range to event",
            Arc::new(move |_, cx| {
                let _ = temporal::dispatch(TimeAction::Range(TimeRangeSpec::fixed(time..end)), cx);
            }),
        )));
    }
    for (label, value) in crate::plot_events::details::fields(&event.event.detail) {
        rows.push(Box::new(TextRow::readonly(label.into(), value.into())));
    }
    if let crate::plot_events::EventDetail::Json(value) = &event.event.detail {
        let value = value.clone();
        rows.push(Box::new(CommandRow::action(
            "Inspect payload",
            Arc::new(move |_, cx| {
                let tree = cx.new(|cx| crate::views::JsonTree::new(value.clone(), cx));
                RowAction::CascadeView(PreviewSpec {
                    view: tree.into(),
                    size: gpui::size(px(480.0), px(300.0)),
                    label: "Event payload".into(),
                })
            }),
        )));
    }
    rows
}
