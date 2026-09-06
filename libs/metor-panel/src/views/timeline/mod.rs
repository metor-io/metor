//! A shared time-navigation surface for tiles, dashboards, and time editor previews.
mod commands;
mod config;
mod interaction;
pub mod model;
mod paint;
#[cfg(test)]
mod tests;
mod tooltip;

pub use crate::plot_events::index::TimelineEvent;
use crate::{
    plot_events::{EventSource, EventSourceRegistry, index::EventIndex},
    temporal::{self, TimeAction, TimeExpr, TimeRangeSpec},
};
pub use config::TimelineConfig;
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, MouseButton, Pixels, Point, Subscription,
    Window, canvas, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::Timestamp;
use model::{Interval, Navigation, clamp};
use std::{rc::Rc, sync::Arc, time::Instant};

/// Hosts receive edits once per frame and decide how to apply or stage them.
pub type TimeEdit = Arc<dyn Fn(TimeAction, &mut Window, &mut App)>;

/// Restricts a draft widget to the field being edited by its host.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    View,
    Range,
    Start,
    End,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragKind {
    Seek,
    Brush,
    Start,
    End,
    Move,
    Pan,
    Overview,
    OverviewStart,
    OverviewEnd,
}
struct Drag {
    kind: DragKind,
    position: Point<Pixels>,
    last_position: Point<Pixels>,
    viewport: Interval,
    overview: Interval,
    range: Option<Interval>,
    spec: TimeRangeSpec,
    time: i64,
    time_expr: TimeExpr,
    edge_delta: f64,
    edge_since: Option<Instant>,
    moved: bool,
}

struct Lane {
    key: String,
    name: gpui::SharedString,
    source: Option<Rc<dyn EventSource>>,
    index: Arc<EventIndex>,
}

/// One reusable timeline; only navigation and presentation are local to the widget.
pub struct Timeline {
    db: Arc<DB>,
    config: TimelineConfig,
    viewport: Option<Interval>,
    fit: Option<Interval>,
    context_key: String,
    history: Vec<Navigation>,
    area: Option<Bounds<Pixels>>,
    hover: Option<Point<Pixels>>,
    drag: Option<Drag>,
    draft: Option<TimeRangeSpec>,
    published_range: Option<TimeRangeSpec>,
    preview: Option<TimeAction>,
    preview_error: Option<String>,
    edit: Option<(EditTarget, TimeEdit)>,
    pending_seek: Option<i64>,
    click: Option<(i64, Instant)>,
    lanes: Vec<Lane>,
    lane_scroll: usize,
    event_hits: Vec<paint::EventHit>,
    selected: Option<Arc<TimelineEvent>>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    frame_pending: bool,
    last_frame: Instant,
}

impl Timeline {
    #[cfg(test)]
    pub(crate) fn content_bounds(&self) -> Option<Bounds<Pixels>> {
        self.area
    }
    pub fn from_config(config: TimelineConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            db,
            config,
            viewport: None,
            fit: None,
            context_key: String::new(),
            history: Vec::new(),
            area: None,
            hover: None,
            drag: None,
            draft: None,
            published_range: None,
            preview: None,
            preview_error: None,
            edit: None,
            pending_seek: None,
            click: None,
            lanes: Vec::new(),
            lane_scroll: 0,
            event_hits: Vec::new(),
            selected: None,
            focus: cx.focus_handle(),
            _subscriptions: Vec::new(),
            frame_pending: false,
            last_frame: Instant::now(),
        };
        this._subscriptions
            .push(cx.observe_global::<temporal::TemporalRevision>(|_, cx| cx.notify()));
        this._subscriptions
            .push(cx.observe_global::<crate::theme::ActiveTheme>(|_, cx| cx.notify()));
        this.sync_sources(cx);
        this
    }
    pub fn to_config(&self, _: &App) -> TimelineConfig {
        self.config.clone()
    }
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
    /// Supply caller-owned point/span events without creating an ingestion service.
    pub fn set_events(
        &mut self,
        id: &str,
        name: gpui::SharedString,
        events: Vec<TimelineEvent>,
        cx: &mut Context<Self>,
    ) {
        let key = format!("custom:{id}");
        let generation = self
            .lanes
            .iter()
            .find(|l| l.key == key)
            .map_or(0, |l| l.index.generation + 1);
        let index = Arc::new(EventIndex::new(
            events.into_iter().map(Arc::new).collect(),
            generation,
        ));
        if let Some(lane) = self.lanes.iter_mut().find(|l| l.key == key) {
            lane.name = name;
            lane.index = index;
        } else {
            self.lanes.push(Lane {
                key,
                name,
                source: None,
                index,
            });
        }
        cx.notify();
    }

    pub fn preview(
        db: Arc<DB>,
        target: EditTarget,
        edit: TimeEdit,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::from_config(TimelineConfig::default(), db, cx);
        this.edit = Some((target, edit));
        this
    }
    pub fn set_preview(&mut self, preview: Result<TimeAction, String>, cx: &mut Context<Self>) {
        match preview {
            Ok(value) => {
                self.preview = Some(value);
                self.preview_error = None;
            }
            Err(error) => self.preview_error = Some(error),
        }
        cx.notify();
    }
    fn sync_sources(&mut self, cx: &mut Context<Self>) {
        self.lanes.clear();
        for key in self
            .config
            .sources
            .iter()
            .filter_map(|s| crate::plot_events::kind_key_from_string(s))
        {
            if let Some(source) = EventSourceRegistry::source_for(key, &self.db, cx) {
                if let Some(target) = source.observe_target(cx) {
                    if let Some(subscription) = crate::plot_events::observe_target(target, cx) {
                        self._subscriptions.push(subscription);
                    }
                }
                let index = crate::plot_events::index::snapshot(source.as_ref(), cx);
                self.lanes.push(Lane {
                    key: crate::plot_events::kind_key_to_string(key),
                    name: source.name(cx),
                    source: Some(source),
                    index,
                });
            }
        }
    }
    fn range_spec(&self, cx: &App) -> TimeRangeSpec {
        self.draft
            .or_else(|| match &self.preview {
                Some(TimeAction::Range(range)) => Some(*range),
                _ => None,
            })
            .unwrap_or_else(|| temporal::config(cx).range)
    }
    fn range(&self, cx: &App) -> Option<Interval> {
        let context = temporal::snapshot(cx)?.context;
        let r = self.range_spec(cx).resolve(&context).ok()?;
        Some(Interval::new(r.start.0, r.end.0))
    }
    fn time(&self, cx: &App) -> Option<i64> {
        if let Some(t) = self.pending_seek {
            return Some(t);
        }
        if let Some((time, _)) = self.click {
            return Some(time);
        }
        let context = temporal::snapshot(cx)?.context;
        if let Some(TimeAction::Seek(expr)) = self.preview {
            expr.resolve(&context).ok().map(|t| t.0)
        } else {
            context.view.map(|t| t.0)
        }
    }
    fn context(&self, cx: &App) -> Option<Interval> {
        let snapshot = temporal::snapshot(cx)?;
        let mut context = snapshot
            .context
            .extent
            .map(|r| Interval::new(r.start.0, r.end.0));
        for range in [
            self.range(cx),
            self.time(cx).map(|t| Interval::new(t, t.saturating_add(1))),
        ]
        .into_iter()
        .flatten()
        {
            context = Some(context.map_or(range, |r| r.hull(range)));
        }
        context
    }
    fn navigate(&mut self, navigation: Navigation, cx: &mut Context<Self>) {
        self.history.push(self.config.navigation);
        if self.history.len() > 32 {
            self.history.remove(0);
        }
        self.config.navigation = navigation;
        if navigation == Navigation::Fit {
            self.fit = None;
            self.viewport = None;
        }
        cx.notify();
    }
    fn update_view(&mut self, elapsed: f64, cx: &App) -> bool {
        let config = temporal::config(cx);
        let key = format!(
            "{}:{:?}:{}",
            config.scope_prefix, config.source_clock, config.wall_clock
        );
        if key != self.context_key {
            self.context_key = key;
            self.fit = None;
            if self.config.navigation == Navigation::Fit {
                self.viewport = None;
            }
        }
        let Some(context) = self.context(cx) else {
            return false;
        };
        let fit = context.padded();
        let previous_fit = self.fit.unwrap_or(fit);
        self.fit = Some(previous_fit.hull(fit));
        if self.drag.is_some() {
            return false;
        }
        let target = match self.config.navigation {
            Navigation::Fit => self.fit.unwrap(),
            Navigation::Manual(view) => Interval::new(view.start, view.end),
            Navigation::Follow { span } => {
                let t = self.time(cx).unwrap_or(context.end);
                let span = if span.is_finite() {
                    span.max(1_000.0)
                } else {
                    context.span()
                };
                Interval::new(
                    clamp(i128::from(t) - (span * 0.8) as i128),
                    clamp(i128::from(t) + (span * 0.2) as i128),
                )
            }
        };
        let Some(old) = self.viewport else {
            self.viewport = Some(target);
            return false;
        };
        if matches!(self.config.navigation, Navigation::Manual(_)) {
            self.viewport = Some(target);
            return false;
        }
        let error = (i128::from(target.start) - i128::from(old.start))
            .abs()
            .max((i128::from(target.end) - i128::from(old.end)).abs()) as f64;
        if error > old.span() || error < (target.span() / 2000.0).max(1.0) {
            self.viewport = Some(target);
            return false;
        }
        let fraction = (1.0 - (-elapsed / 0.07).exp()).clamp(0.01, 1.0);
        self.viewport = Some(Interval::new(
            clamp(
                i128::from(old.start)
                    + ((i128::from(target.start) - i128::from(old.start)) as f64 * fraction).round()
                        as i128,
            ),
            clamp(
                i128::from(old.end)
                    + ((i128::from(target.end) - i128::from(old.end)) as f64 * fraction).round()
                        as i128,
            ),
        ));
        true
    }
}

impl Focusable for Timeline {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Timeline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for lane in &mut self.lanes {
            if let Some(source) = &lane.source {
                lane.index = crate::plot_events::index::snapshot(source.as_ref(), cx);
            }
        }
        let elapsed = self.last_frame.elapsed().as_secs_f64().min(0.05);
        let moving = self.update_view(elapsed, cx);
        if (moving || self.drag.is_some() || self.pending_seek.is_some() || self.click.is_some())
            && !self.frame_pending
        {
            self.frame_pending = true;
            let weak = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.frame_pending = false;
                    this.animate(window, cx);
                    cx.notify();
                });
            });
        }
        let entity = cx.entity();
        let painter = canvas(
            move |bounds, window, cx| {
                entity.update(cx, |this, cx| {
                    this.area = Some(bounds);
                    this.show_readout(window, cx);
                });
            },
            {
                let entity = cx.entity();
                move |bounds, _, window, cx| {
                    let weak = entity.downgrade();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Bubble && !bounds.contains(&event.position)
                        {
                            let _ = weak.update(cx, |this, cx| {
                                if this.drag.is_some() {
                                    this.pointer_move(event, cx);
                                } else {
                                    this.clear_hover(cx);
                                }
                            });
                        }
                    });
                    entity.update(cx, |this, cx| this.paint(bounds, window, cx));
                }
            },
        )
        .size_full();
        div()
            .id("timeline")
            .key_context("Timeline")
            .track_focus(&self.focus)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.clear_hover(cx);
                }
            }))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .rounded(px(3.0))
            .when(self.edit.is_none(), |surface| {
                surface
                    .border_1()
                    .border_color(crate::theme::theme(cx).border_primary)
            })
            .bg(crate::theme::theme(cx).bg_primary)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::pointer_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::pointer_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_menu(event.position, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, event, _, cx| this.pointer_move(event, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| this.release(window, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| this.release(window, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| this.release(window, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| this.release(window, cx)),
            )
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.scroll(event, cx)))
            .on_key_down(cx.listener(Self::key_down))
            .child(painter)
    }
}
