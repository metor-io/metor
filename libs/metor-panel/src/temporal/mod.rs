//! Shared display time, independent of ingestion and the live expression runtime.
pub(crate) mod display;
pub mod model;
pub(crate) mod picker;
pub(crate) mod samples;
#[cfg(test)]
mod tests;

pub use display::TimeDisplay;
use gpui::{App, AppContext, Context, Entity, Global};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};
pub use model::{Anchor, TimeContext, TimeExpr, TimeRangeSpec};
use serde::{Deserialize, Serialize};
use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

/// Persisted temporal intent; playback always restores paused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemporalConfig {
    pub version: u32,
    pub view: TimeExpr,
    pub range: TimeRangeSpec,
    pub timezone: String,
    pub display: TimeDisplay,
    pub t0: Option<i64>,
    pub source_clock: Option<ComponentId>,
    pub wall_clock: bool,
    pub scope_prefix: String,
    pub rate: f64,
    pub step_micros: i64,
}
impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            version: 1,
            view: TimeExpr::LIVE,
            range: TimeRangeSpec::FULL,
            timezone: "UTC".into(),
            display: TimeDisplay::Timestamp,
            t0: None,
            source_clock: None,
            wall_clock: false,
            scope_prefix: String::new(),
            rate: 1.0,
            step_micros: 1_000_000,
        }
    }
}

/// All consumers resolve against this same clock snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemporalSnapshot {
    pub context: TimeContext,
    pub range: Option<Range<Timestamp>>,
    pub live: bool,
    pub playing: bool,
    pub revision: u64,
    pub error: Option<String>,
}

struct Playback {
    base: Timestamp,
    started: Instant,
    bounds: Range<Timestamp>,
}

/// One application clock drives replay and all following views.
pub struct TemporalController {
    db: Arc<DB>,
    pub config: TemporalConfig,
    pub snapshot: TemporalSnapshot,
    published: Option<TemporalSnapshot>,
    playback: Option<Playback>,
    resume_live: bool,
    components: Vec<Component>,
    last_bounds: Instant,
    bounds_busy: bool,
    _bounds: gpui::Task<()>,
    _tick: gpui::Task<()>,
}
struct TemporalGlobal(Entity<TemporalController>);
impl Global for TemporalGlobal {}

/// Temporal changes for consumers which need a global observation.
#[derive(Default)]
pub(crate) struct TemporalRevision(pub u64);
impl Global for TemporalRevision {}

#[derive(Default)]
pub(crate) struct PlotSync(pub u64);
impl Global for PlotSync {}

impl TemporalController {
    pub fn init(db: Arc<DB>, cx: &mut App) -> Entity<Self> {
        if let Some(g) = cx.try_global::<TemporalGlobal>() {
            return g.0.clone();
        }
        let controller = cx.new(|cx: &mut Context<Self>| {
            let tick = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(33))
                        .await;
                    if this
                        .update(cx, |this, cx| this.tick(Instant::now(), cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let now = Timestamp::now();
            Self {
                db,
                config: TemporalConfig::default(),
                snapshot: TemporalSnapshot {
                    context: TimeContext {
                        live: Some(now),
                        view: Some(now),
                        extent: None,
                    },
                    range: None,
                    live: true,
                    playing: false,
                    revision: 0,
                    error: None,
                },
                playback: None,
                published: None,
                resume_live: false,
                components: vec![],
                last_bounds: Instant::now() - Duration::from_secs(1),
                bounds_busy: false,
                _bounds: gpui::Task::ready(()),
                _tick: tick,
            }
        });
        cx.set_global(TemporalGlobal(controller.clone()));
        controller.update(cx, |this, cx| this.tick(Instant::now(), cx));
        controller
    }

    fn tick(&mut self, now: Instant, cx: &mut Context<Self>) {
        if !self.bounds_busy
            && now.saturating_duration_since(self.last_bounds) >= Duration::from_millis(250)
        {
            self.last_bounds = now;
            self.bounds_busy = true;
            let db = self.db.clone();
            let prefix = self.config.scope_prefix.clone();
            self._bounds = cx.spawn(async move |this, cx| {
                let scope = prefix.clone();
                let (components, extent) = cx
                    .background_executor()
                    .spawn(async move {
                        let components: Vec<_> = db.with_state(|state| {
                            state
                                .component_metadata_iter()
                                .filter(|(_, m)| {
                                    m.name.starts_with(&scope)
                                        && !m.metadata.contains_key("lod_source_id")
                                        && !m.metadata.contains_key("expression")
                                        && !m.name.starts_with("__")
                                })
                                .filter_map(|(id, _)| state.get_component(*id).cloned())
                                .collect()
                        });
                        let mut extent: Option<Range<Timestamp>> = None;
                        for c in &components {
                            if let Some(mut r) = crate::data_binding::component_extent(c) {
                                r.end = Timestamp(r.end.0.saturating_sub(1));
                                if let Some(e) = &mut extent {
                                    e.start = e.start.min(r.start);
                                    e.end = e.end.max(r.end);
                                } else {
                                    extent = Some(r);
                                }
                            }
                        }
                        (components, extent)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.bounds_busy = false;
                    if this.config.scope_prefix == prefix {
                        this.components = components;
                        this.snapshot.context.extent = extent;
                    } else {
                        this.last_bounds = Instant::now() - Duration::from_secs(1);
                    }
                    this.tick(Instant::now(), cx);
                });
            });
        }
        let data_head = self
            .components
            .iter()
            .filter_map(|c| c.time_series.latest().map(|s| s.timestamp()))
            .max();
        // Historical discovery is throttled; the data-end anchor follows the
        // resident head on every tick, including when a scan returns older bounds.
        if let (Some(extent), Some(head)) = (&mut self.snapshot.context.extent, data_head) {
            extent.end = extent.end.max(head);
        }
        self.snapshot.context.live = match self.config.source_clock {
            Some(id) => self.db.with_state(|s| {
                crate::data_binding::component_extent(s.get_component(id)?)
                    .map(|extent| Timestamp(extent.end.0.saturating_sub(1)))
            }),
            None if self.config.wall_clock => Some(Timestamp::now()),
            None => data_head
                .max(self.snapshot.context.extent.as_ref().map(|e| e.end))
                .or_else(|| Some(Timestamp::now())),
        };
        let view = if let Some(playback) = &self.playback {
            let elapsed =
                now.duration_since(playback.started).as_secs_f64() * 1e6 * self.config.rate;
            let t = Timestamp(
                playback
                    .base
                    .0
                    .saturating_add(elapsed.min(i64::MAX as f64) as i64)
                    .min(playback.bounds.end.0),
            );
            if t >= playback.bounds.end {
                self.config.view = TimeExpr::fixed(t);
                self.playback = None;
            }
            Ok(t)
        } else {
            self.config.view.resolve(&self.snapshot.context)
        };
        self.snapshot.context.view = view.as_ref().ok().copied();
        let range = self.config.range.resolve(&self.snapshot.context);
        self.snapshot.range = range.as_ref().ok().cloned();
        self.snapshot.error = view.err().or_else(|| range.err());
        self.snapshot.live = self.config.view == TimeExpr::LIVE && self.playback.is_none();
        self.snapshot.playing = self.playback.is_some();
        if self.published.as_ref() != Some(&self.snapshot) {
            self.published = Some(self.snapshot.clone());
            let revision = cx
                .try_global::<TemporalRevision>()
                .map_or(1, |r| r.0.wrapping_add(1));
            cx.set_global(TemporalRevision(revision));
            cx.refresh_windows();
        }
        // Readers still age live freshness when the source clock has stopped.
        cx.notify();
    }

    pub fn apply(&mut self, action: TimeAction, cx: &mut Context<Self>) -> Result<(), String> {
        match action {
            TimeAction::Seek(expr) => {
                if expr.anchor == Anchor::View {
                    return Err("View time cannot reference itself".into());
                }
                expr.resolve(&self.snapshot.context)?;
                self.config.view = expr;
                self.playback = None;
                self.resume_live = false;
            }
            TimeAction::Range(range) => {
                let mut bounds = range.resolve(&self.snapshot.context)?;
                if let Some(extent) = &self.snapshot.context.extent {
                    bounds.start = bounds.start.max(extent.start);
                    bounds.end = bounds.end.min(Timestamp(extent.end.0.saturating_add(1)));
                }
                if self.playback.is_some() {
                    let base = self.snapshot.context.view.ok_or("No selected time")?;
                    self.config.view = TimeExpr::fixed(base);
                    self.playback = if bounds.contains(&base) {
                        Some(Playback {
                            base,
                            started: Instant::now(),
                            bounds,
                        })
                    } else {
                        None
                    };
                }
                self.config.range = range;
            }
            TimeAction::Pause => {
                self.resume_live = self.snapshot.live;
                self.config.view =
                    TimeExpr::fixed(self.snapshot.context.view.ok_or("No selected time")?);
                self.playback = None;
            }
            TimeAction::Live => {
                self.config.view = TimeExpr::LIVE;
                self.playback = None;
                self.resume_live = false;
            }
            TimeAction::Play { from_start } => {
                if !from_start && self.resume_live {
                    self.config.view = TimeExpr::LIVE;
                    self.playback = None;
                    self.resume_live = false;
                } else {
                    let mut bounds = self.snapshot.range.clone().ok_or("No playback interval")?;
                    if let Some(extent) = &self.snapshot.context.extent {
                        bounds.start = bounds.start.max(extent.start);
                        bounds.end = bounds.end.min(Timestamp(extent.end.0.saturating_add(1)));
                    }
                    if bounds.start >= bounds.end {
                        return Err("No recorded data in this range".into());
                    }
                    let base = if from_start {
                        bounds.start
                    } else {
                        self.snapshot.context.view.ok_or("No selected time")?
                    };
                    if base < bounds.start || base >= bounds.end {
                        return Err("Choose Play from range start".into());
                    }
                    self.resume_live = false;
                    self.config.view = TimeExpr::fixed(base);
                    self.playback = Some(Playback {
                        base,
                        started: Instant::now(),
                        bounds,
                    });
                }
            }
            TimeAction::Step(direction) => {
                self.resume_live = false;
                let step = self
                    .config
                    .step_micros
                    .checked_mul(direction as i64)
                    .ok_or("Step overflow")?;
                let t = self
                    .snapshot
                    .context
                    .view
                    .ok_or("No selected time")?
                    .0
                    .checked_add(step)
                    .ok_or("Timestamp overflow")?;
                self.config.view = TimeExpr::fixed(Timestamp(t));
                self.playback = None;
            }
            TimeAction::Pin => {
                self.config.range =
                    TimeRangeSpec::fixed(self.snapshot.range.clone().ok_or("No range to pin")?);
            }
            TimeAction::Rate(rate) => {
                if !rate.is_finite() || rate <= 0.0 || rate > 100.0 {
                    return Err("Rate must be greater than 0 and at most 100".into());
                }
                if let Some(p) = &mut self.playback {
                    p.base = self.snapshot.context.view.ok_or("No selected time")?;
                    p.started = Instant::now();
                }
                self.config.rate = rate;
            }
            TimeAction::StepSize(step) => {
                if step <= 0 {
                    return Err("Step must be positive".into());
                }
                self.config.step_micros = step;
            }
            TimeAction::Timezone(zone) => {
                model::ParseContext::new(&zone, Timestamp::now(), Timestamp::now())?;
                self.config.timezone = zone;
            }
            TimeAction::Display(display) => self.config.display = display,
            TimeAction::T0(t0) => self.config.t0 = t0,
            TimeAction::Scope(prefix) => {
                self.config.scope_prefix = prefix;
                self.snapshot.context.extent = None;
                self.pause_if_playing();
                self.last_bounds = Instant::now() - Duration::from_secs(1);
            }
            TimeAction::SyncPlots => {
                let revision = cx
                    .try_global::<PlotSync>()
                    .map_or(1, |v| v.0.wrapping_add(1));
                cx.set_global(PlotSync(revision));
            }
            TimeAction::WallClock => {
                self.config.wall_clock = true;
                self.config.source_clock = None;
                self.pause_if_playing();
            }
            TimeAction::Clock(source) => {
                self.config.wall_clock = false;
                self.config.source_clock = source;
                self.pause_if_playing();
            }
        }
        self.snapshot.revision = self.snapshot.revision.wrapping_add(1);
        self.tick(Instant::now(), cx);
        Ok(())
    }
    fn pause_if_playing(&mut self) {
        if self.playback.take().is_some()
            && let Some(t) = self.snapshot.context.view
        {
            self.config.view = TimeExpr::fixed(t);
        }
    }
    pub(crate) fn clock_source(&self, query: &str) -> Result<Option<ComponentId>, String> {
        if query == "session" {
            return Ok(None);
        }
        if let Some(id) = query.strip_prefix("id:") {
            let id = ComponentId(id.parse().map_err(|_| "Invalid component id")?);
            return self
                .db
                .with_state(|s| s.get_component(id).map(|_| Some(id)))
                .ok_or_else(|| "Clock source is not registered".into());
        }
        self.db
            .with_state(|s| {
                s.component_metadata_iter()
                    .find(|(_, m)| m.name == query)
                    .map(|(id, _)| Some(*id))
            })
            .ok_or_else(|| "Choose session, wall, or a registered clock source".into())
    }
    pub(crate) fn source_names(&self, query: &str) -> Vec<(ComponentId, String)> {
        self.db.with_state(|s| {
            s.component_metadata_iter()
                .filter(|(_, m)| {
                    m.name.starts_with(query) && !m.metadata.contains_key("expression")
                })
                .take(30)
                .map(|(id, m)| (*id, m.name.clone()))
                .collect()
        })
    }
    pub fn saved(&self) -> TemporalConfig {
        let mut config = self.config.clone();
        if self.playback.is_some()
            && let Some(t) = self.snapshot.context.view
        {
            config.view = TimeExpr::fixed(t);
        }
        config
    }
    pub fn restore(&mut self, config: TemporalConfig, cx: &mut Context<Self>) {
        self.playback = None;
        self.resume_live = false;
        self.config = config;
        if self.config.version != 1 || self.config.view.anchor == Anchor::View {
            self.config = TemporalConfig::default();
        }
        if !self.config.rate.is_finite() || self.config.rate <= 0.0 || self.config.rate > 100.0 {
            self.config.rate = 1.0;
        }
        if self.config.step_micros <= 0 {
            self.config.step_micros = 1_000_000;
        }
        if jiff::tz::TimeZone::get(&self.config.timezone).is_err()
            && !self.config.timezone.eq_ignore_ascii_case("local")
        {
            self.config.timezone = "UTC".into();
        }
        self.snapshot.revision = self.snapshot.revision.wrapping_add(1);
        self.last_bounds = Instant::now() - Duration::from_secs(1);
        self.tick(Instant::now(), cx);
    }
}

/// Commands shared by inspector, palette, toolbar and plot interactions.
#[derive(Clone, Debug)]
pub enum TimeAction {
    Seek(TimeExpr),
    Range(TimeRangeSpec),
    Pause,
    Live,
    Play { from_start: bool },
    Step(i8),
    Pin,
    Rate(f64),
    StepSize(i64),
    Timezone(String),
    Display(TimeDisplay),
    T0(Option<i64>),
    Scope(String),
    Clock(Option<ComponentId>),
    WallClock,
    SyncPlots,
}
pub fn controller(cx: &App) -> Option<Entity<TemporalController>> {
    cx.try_global::<TemporalGlobal>().map(|g| g.0.clone())
}
pub fn dispatch(action: TimeAction, cx: &mut App) -> Result<(), String> {
    controller(cx)
        .ok_or("Time controller unavailable")?
        .update(cx, |c, cx| c.apply(action, cx))
}
pub fn snapshot(cx: &App) -> Option<TemporalSnapshot> {
    controller(cx).map(|c| c.read(cx).snapshot.clone())
}
pub fn view_time(cx: &App) -> Option<Timestamp> {
    snapshot(cx).and_then(|s| s.context.view)
}
pub fn is_live(cx: &App) -> bool {
    snapshot(cx).is_none_or(|s| s.live)
}
pub fn config(cx: &App) -> TemporalConfig {
    controller(cx)
        .map(|c| c.read(cx).config.clone())
        .unwrap_or_default()
}

pub(crate) fn paint_playhead(
    area: gpui::Bounds<gpui::Pixels>,
    x: (f64, f64),
    window: &mut gpui::Window,
    cx: &App,
) {
    if is_live(cx) {
        return;
    }
    let Some(t) = view_time(cx) else {
        return;
    };
    let fraction = (t.0 as f64 - x.0) / (x.1 - x.0);
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return;
    }
    window.paint_quad(gpui::fill(
        gpui::Bounds {
            origin: gpui::point(
                area.origin.x + area.size.width * fraction as f32,
                area.origin.y,
            ),
            size: gpui::size(gpui::px(1.0), area.size.height),
        },
        crate::theme::theme(cx).control_active,
    ));
}

pub(crate) fn save_layout(cx: &App) -> metor_proto_wkt::TemporalLayout {
    let c = controller(cx)
        .map(|c| c.read(cx).saved())
        .unwrap_or_default();
    metor_proto_wkt::TemporalLayout {
        version: 1,
        view_time: c.view.to_string(),
        range_start: c.range.start.to_string(),
        range_end: c.range.end.to_string(),
        timezone: c.timezone,
        elapsed_display: c.display == TimeDisplay::Elapsed,
        t0: c.t0,
        source_clock: c.source_clock.map(|id| id.0),
        wall_clock: c.wall_clock,
        scope_prefix: c.scope_prefix,
        rate: c.rate,
        step_micros: c.step_micros,
    }
}

pub(crate) fn restore_layout(saved: &metor_proto_wkt::TemporalLayout, cx: &mut App) {
    let parse = || -> Result<TemporalConfig, String> {
        if saved.version != 1 {
            return Err("Unsupported temporal version".into());
        }
        let context =
            model::ParseContext::new(&saved.timezone, Timestamp::now(), Timestamp::now())?;
        Ok(TemporalConfig {
            version: saved.version,
            view: model::parse_instant(&saved.view_time, &context, false)?,
            range: TimeRangeSpec {
                start: model::parse_instant(&saved.range_start, &context, true)?,
                end: model::parse_instant(&saved.range_end, &context, true)?,
            },
            timezone: saved.timezone.clone(),
            display: if saved.elapsed_display {
                TimeDisplay::Elapsed
            } else {
                TimeDisplay::Timestamp
            },
            t0: saved.t0,
            source_clock: saved.source_clock.map(ComponentId),
            wall_clock: saved.wall_clock,
            scope_prefix: saved.scope_prefix.clone(),
            rate: saved.rate,
            step_micros: saved.step_micros,
        })
    };
    if let Ok(config) = parse()
        && let Some(c) = controller(cx)
    {
        c.update(cx, |c, cx| c.restore(config, cx));
    }
}

pub(crate) fn legacy_range(cx: &App) -> String {
    use crate::views::time_series::time_range::{Offset, TimeRangeBehavior};
    let range = config(cx).range;
    let old = |expr: TimeExpr| match (expr.anchor, expr.offset) {
        (Anchor::DataStart, n) if n >= 0 => Some(Offset::Earliest(Duration::from_micros(n as u64))),
        (Anchor::DataEnd, n) if n <= 0 => {
            Some(Offset::Latest(Duration::from_micros(n.unsigned_abs())))
        }
        (Anchor::Timestamp(t), n) => t.checked_add(n).map(|t| Offset::Fixed(Timestamp(t))),
        _ => None,
    };
    if let (Some(start), Some(end)) = (old(range.start), old(range.end)) {
        TimeRangeBehavior { start, end }.to_string()
    } else if let Some(r) = snapshot(cx).and_then(|s| s.range) {
        TimeRangeBehavior {
            start: Offset::Fixed(r.start),
            end: Offset::Fixed(r.end),
        }
        .to_string()
    } else {
        TimeRangeBehavior::FULL.to_string()
    }
}

pub(crate) fn resolve_range(
    override_: &crate::views::time_series::Override<crate::views::time_series::TimeRangeBehavior>,
    extent: Range<Timestamp>,
    cx: &App,
) -> Option<Range<Timestamp>> {
    if let Some(s) = snapshot(cx) {
        match override_ {
            crate::views::time_series::Override::Auto => s.range,
            crate::views::time_series::Override::Custom(range) => {
                let mut context = s.context;
                context.extent = Some(extent);
                TimeRangeSpec::from(*range).resolve(&context).ok()
            }
        }
    } else {
        Some(
            override_
                .as_custom()
                .copied()
                .unwrap_or_default()
                .calculate_range(extent.start, extent.end),
        )
    }
}

#[cfg(test)]
mod live_head_tests {
    use super::*;

    #[gpui::test]
    fn hydrating_old_data_preserves_live_and_live_relative_ranges(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let id = ComponentId::new("archived.value");
        db.with_state_mut(|state| {
            state.insert_component(
                id,
                metor_db::ComponentSchema::new(metor_proto::types::PrimType::F64, &[][..]),
                &db.path,
            )
        })
        .unwrap();
        let component = db
            .with_state(|state| state.get_component(id).cloned())
            .unwrap();
        cx.update(|cx| {
            let controller = TemporalController::init(db, cx);
            controller.update(cx, |controller, cx| {
                // Model bounds discovered from the archive before any data is resident.
                controller.bounds_busy = true;
                controller.components = vec![component.clone()];
                controller.snapshot.context.extent = Some(Timestamp(10)..Timestamp(100));
                controller.config.range = TimeRangeSpec {
                    start: TimeExpr {
                        anchor: Anchor::Live,
                        offset: -20,
                    },
                    end: TimeExpr::LIVE,
                };
                controller.tick(Instant::now(), cx);
                assert_eq!(controller.snapshot.context.live, Some(Timestamp(100)));
                component
                    .time_series
                    .install_samples(
                        8,
                        [(Timestamp(10), 42f64.to_le_bytes().as_slice())],
                        metor_db::manifest::SpanSource::RemoteFetch,
                    )
                    .unwrap();
                controller.tick(Instant::now(), cx);
                assert_eq!(controller.snapshot.context.live, Some(Timestamp(100)));
                assert_eq!(controller.snapshot.context.view, Some(Timestamp(100)));
                assert_eq!(
                    controller.snapshot.range,
                    Some(Timestamp(80)..Timestamp(100))
                );
            });
        });
    }
}
