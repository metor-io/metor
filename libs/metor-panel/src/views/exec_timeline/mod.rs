//! Execution timeline: what the scheduler actually did with each cycle.
//!
//! The system graph shows topology; this shows time. The envelope leads — the
//! coordinator's own record, or a system marked
//! [`encompassing`](metor_fsw_2::ir::SystemSpec::encompassing) — as one bar
//! spanning the whole cycle, and under it one lane per system in the order the
//! coordinator steps them, each with a bar per cycle sized by that step's
//! measured `last_execute_us` and coloured by the slot's run state. The slack
//! between the stepped bars and the envelope is the host's own overhead. The
//! wiring's data flow is drawn over the lanes, so a value's path through one
//! cycle is visible as well as its cost.
//!
//! Two things make this a pane rather than a plot mode. Its X axis is shared
//! with the plots ([`TimeRangeBehavior`], pan, zoom, and a scope-style
//! [`trigger`](ExecTimeline::trigger) on the newest cycle) but its Y axis is a
//! list of instances, not a value range; and its rows come from the wiring IR
//! ([`rows`]), which is the only place step order exists. Bars are laid out by
//! prefix-summing durations across rows ([`scan::layout_cycles`]), so a row the
//! operator hid still contributes its time — hiding a lane must not move the
//! bars of the lanes below it.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, MouseButton, Pixels, Point, SharedString, Subscription, Task, Window,
    canvas, div, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::Timestamp;
use stellarator::util::AtomicCell;

mod config;
pub use config::ExecTimelineConfig;

pub mod inspector_rows;
mod paint;
pub(crate) mod rows;
pub(crate) mod scan;

#[cfg(test)]
mod tests;

use paint::{FlowPaint, GUTTER_W, LanePaint, RULER_H, RulerPaint, paint_lanes, paint_ruler};
use rows::{ExecRow, FlowEdge, RowKind, derive_edges, derive_rows};
use scan::{GanttFrame, layout_cycles, read_row, summarize};

use crate::views::table::{CELL_FONT_SIZE, CELL_PAD_X};
use crate::views::time_series::time_range::GlobalTimeRange;
use crate::views::time_series::{Override, PlotBounds, TimeFormat, TimeRangeBehavior};
use crate::wait_for_component;

/// Total bars across every lane before the timeline collapses to fixed screen
/// buckets. A cycle-rate frame over a wide window produces sub-pixel bars long
/// before it produces a slow frame; the summary keeps the worst state in each
/// bucket rather than stride-sampling one away.
const BAR_BUDGET: usize = 20_000;
/// How many buckets a summarized window is divided into — roughly two pixels
/// each on a full-width pane.
const SUMMARY_BUCKETS: i64 = 1024;

/// The two `system_status` leaves behind one lane, resolved asynchronously
/// because a panel routinely opens before the target it watches has booted.
#[derive(Default)]
struct RowTracking {
    duration: Option<Component>,
    state: Option<Component>,
}

/// The Execution Timeline pane.
pub struct ExecTimeline {
    db: Arc<DB>,
    /// Every lane the wiring declares, in step order. Filtering happens at
    /// paint; the scan always covers all of them.
    rows: Vec<ExecRow>,
    /// The wiring's data flow, one connector per producer/consumer pair,
    /// indexed against `rows`. Derived beside them and invalidated with them.
    edges: Vec<FlowEdge>,
    tracking: Vec<RowTracking>,
    resolve_tasks: Vec<Task<()>>,
    scan_task: Option<Task<()>>,
    scan_requests: Arc<AtomicCell<u64>>,
    scan_epoch: u64,
    temporal_revision: u64,
    frame: Option<Arc<GanttFrame>>,

    pub label: SharedString,
    /// `Auto` follows the app-wide [`GlobalTimeRange`].
    pub x_range: Override<TimeRangeBehavior>,
    pub x_time_format: TimeFormat,
    pub show_slots: bool,
    pub show_coordinator_row: bool,
    /// Scope-trigger mode: pin the window to the newest cycle and re-snap as
    /// each record lands, so the bars redraw in place instead of scrolling.
    /// Overrides both the app-wide range and any pan/zoom while it is on.
    pub trigger: bool,
    hidden: HashSet<SharedString>,
    /// One cycle at the target's declared rate, in microseconds; `0` until a
    /// manifest arrives. The scan's lookback and the trigger's fallback width.
    nominal_period_us: i64,

    view_override: Option<PlotBounds>,
    scanned_range: Option<(i64, i64)>,
    hover: Option<Point<Pixels>>,
    lane_area: Option<Bounds<Pixels>>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    _subscriptions: Vec<Subscription>,
}

impl ExecTimeline {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<crate::temporal::PlotSync>(|this, cx| {
            this.x_range = Override::Auto;
            this.trigger = false;
            this.set_view_override(None, cx);
            this.restart_scan(cx);
        }));
        if let Some(store) = crate::wiring::try_global(cx) {
            subscriptions.push(cx.observe(&store, |this, _, cx| this.sync_rows(cx)));
        }
        subscriptions.push(cx.observe_global::<GlobalTimeRange>(|this, cx| this.restart_scan(cx)));
        subscriptions.push(
            cx.observe_global::<crate::temporal::TemporalRevision>(|this, cx| {
                let range = this
                    .effective_view(cx)
                    .map(|v| (v.min_x as i64, v.max_x as i64));
                let revision = crate::temporal::snapshot(cx).map_or(0, |s| s.revision);
                let edited = revision != this.temporal_revision;
                this.temporal_revision = revision;
                if range != this.scanned_range {
                    if edited {
                        this.restart_scan(cx);
                    } else {
                        this.request_scan(cx);
                    }
                }
            }),
        );
        let mut this = Self {
            db,
            rows: Vec::new(),
            edges: Vec::new(),
            tracking: Vec::new(),
            resolve_tasks: Vec::new(),
            scan_task: None,
            scan_requests: Arc::new(AtomicCell::new(0)),
            scan_epoch: 0,
            temporal_revision: crate::temporal::snapshot(cx).map_or(0, |s| s.revision),
            frame: None,
            label: SharedString::new_static(""),
            x_range: Override::Auto,
            x_time_format: TimeFormat::default(),
            show_slots: true,
            show_coordinator_row: true,
            trigger: false,
            hidden: HashSet::new(),
            nominal_period_us: 0,
            view_override: None,
            scanned_range: None,
            hover: None,
            lane_area: None,
            drag_start: None,
            drag_start_view: None,
            _subscriptions: subscriptions,
        };
        this.sync_rows(cx);
        this
    }

    pub fn title(&self, _cx: &App) -> SharedString {
        if self.label.is_empty() {
            SharedString::new_static("Execution")
        } else {
            self.label.clone()
        }
    }

    /// Instance names of every derived lane, for the inspector's per-row
    /// visibility toggles.
    pub fn row_names(&self) -> Vec<SharedString> {
        self.rows.iter().map(|r| r.name.clone()).collect()
    }

    pub fn is_row_hidden(&self, name: &SharedString) -> bool {
        self.hidden.contains(name)
    }

    pub fn toggle_row(&mut self, name: SharedString, cx: &mut Context<Self>) {
        if !self.hidden.remove(&name) {
            self.hidden.insert(name);
        }
        cx.notify();
    }

    /// Choosing a time range is choosing the timebase, which is the one thing a
    /// trigger cannot survive: it releases the trigger, like turning the knob on
    /// a scope.
    pub fn set_x_range(&mut self, range: Override<TimeRangeBehavior>, cx: &mut Context<Self>) {
        self.x_range = range;
        self.view_override = None;
        self.trigger = false;
        self.restart_scan(cx);
    }

    pub fn set_trigger(&mut self, on: bool, cx: &mut Context<Self>) {
        self.trigger = on;
        self.view_override = None;
        self.restart_scan(cx);
    }

    /// Rebuild the lane list from the live wiring and restart everything that
    /// depends on it. A topology change is rare and total: the old resolve
    /// tasks are dropped (which cancels them), so nothing writes into the new
    /// tracking table.
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let store = crate::wiring::try_global(cx);
        let wiring = store.as_ref().and_then(|s| s.read(cx).state().wiring());
        let derived = wiring.map(derive_rows).unwrap_or_default();
        if derived == self.rows {
            return;
        }
        self.edges = wiring
            .map(|w| derive_edges(w, &derived))
            .unwrap_or_default();
        self.nominal_period_us = wiring
            .map(|w| w.coordinator.cycle_rate)
            .filter(|rate| *rate > 0.0)
            .map(|rate| (1.0e6 / rate) as i64)
            .unwrap_or(0);
        self.rows = derived;
        self.tracking = self.rows.iter().map(|_| RowTracking::default()).collect();
        self.resolve_tasks = (0..self.rows.len())
            .map(|i| self.spawn_resolve(i, cx))
            .collect();
        self.frame = None;
        self.restart_scan(cx);
    }

    /// Subscribe to one lane's two leaves. Both waits start before the target
    /// registers its components, so a pane opened first still binds.
    fn spawn_resolve(&self, index: usize, cx: &mut Context<Self>) -> Task<()> {
        let db = self.db.clone();
        let (duration_id, state_id) = (self.rows[index].duration_id, self.rows[index].state_id);
        cx.spawn(async move |this, cx| {
            let duration = wait_for_component(&db, duration_id).await;
            if this
                .update(cx, |timeline, cx| {
                    if let Some(tracking) = timeline.tracking.get_mut(index) {
                        tracking.duration = Some(duration);
                    }
                    timeline.restart_scan(cx);
                })
                .is_err()
            {
                return;
            }
            let state = wait_for_component(&db, state_id).await;
            let _ = this.update(cx, |timeline, cx| {
                if let Some(tracking) = timeline.tracking.get_mut(index) {
                    tracking.state = Some(state);
                }
                timeline.restart_scan(cx);
            });
        })
    }

    fn restart_scan(&mut self, cx: &mut Context<Self>) {
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        self.request_scan(cx);
    }

    fn request_scan(&mut self, cx: &mut Context<Self>) {
        self.scanned_range = self
            .effective_view(cx)
            .map(|v| (v.min_x as i64, v.max_x as i64));
        self.scan_requests
            .store(self.scan_requests.latest().wrapping_add(1));
        if self.scan_task.is_none() {
            self.scan_task = Some(Self::spawn_scan(
                self.scan_requests.clone(),
                cx,
                |input| async move { build_frame(input) },
            ));
        }
        cx.notify();
    }

    /// Keep one worker alive and coalesce requests while it scans. Dropping an
    /// async handle cannot stop a synchronous history scan already on a worker.
    fn spawn_scan<F, Fut>(
        requests: Arc<AtomicCell<u64>>,
        cx: &mut Context<Self>,
        build: F,
    ) -> Task<()>
    where
        F: Fn(ScanInput) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = GanttFrame> + Send + 'static,
    {
        cx.spawn(async move |this, cx| {
            loop {
                let request = requests.latest();
                let input = this.update(cx, |timeline, cx| {
                    let view = timeline.effective_view(cx)?;
                    let pairs: Vec<(Option<Component>, Option<Component>)> = timeline
                        .tracking
                        .iter()
                        .map(|t| (t.duration.clone(), t.state.clone()))
                        .collect();
                    Some((
                        timeline.scan_epoch,
                        ScanInput {
                            window: view.min_x as i64..view.max_x as i64,
                            lookback: timeline.lookback_us(&view),
                            data_end: timeline.data_end(),
                            nominal_period_us: timeline.nominal_period_us,
                            pairs,
                        },
                    ))
                });
                let Ok(input) = input else {
                    return;
                };
                let Some((epoch, input)) = input else {
                    requests.wait_for(|next| next != request).await;
                    continue;
                };
                let pacemaker = input
                    .pairs
                    .first()
                    .and_then(|(d, _)| d.clone())
                    .or_else(|| input.pairs.iter().find_map(|(d, _)| d.clone()));
                let head = pacemaker
                    .as_ref()
                    .and_then(|c| c.time_series.latest().map(|s| s.timestamp()));

                let build = build.clone();
                let frame = cx
                    .background_executor()
                    .spawn(async move { build(input).await })
                    .await;
                if this
                    .update(cx, |timeline, cx| {
                        // Continuous live motion can use the completed frame;
                        // a user edit or topology change invalidates its meaning.
                        if timeline.scan_epoch == epoch {
                            timeline.frame = Some(Arc::new(frame));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                match &pacemaker {
                    Some(component) => {
                        if component.time_series.latest().map(|s| s.timestamp()) != head {
                            continue;
                        }
                        futures_lite::future::race(
                            component.time_series.wait(),
                            requests.wait_for(|next| next != request),
                        )
                        .await;
                    }
                    None => requests.wait_for(|next| next != request).await,
                }
            }
        })
    }

    /// The window to draw.
    ///
    /// Trigger mode wins over everything, including the app-wide range and any
    /// pan/zoom override — that is what makes it a trigger. Otherwise the window
    /// is auto-fitted across every resolved lane exactly the way
    /// [`LinePlot::effective_view`](crate::views::LinePlot) fits its traces.
    fn effective_view(&self, cx: &App) -> Option<PlotBounds> {
        if self.trigger
            && let Some(view) = self.triggered_view()
        {
            return Some(view);
        }
        if let Some(view) = self.view_override {
            return Some(view);
        }
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut any = false;
        for tracking in &self.tracking {
            let Some(component) = tracking.duration.as_ref() else {
                continue;
            };
            if let Some(s) = component.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any = true;
            }
            if let Some(l) = component.time_series.latest() {
                end = end.max(l.timestamp().0 as f64);
                any = true;
            }
            // Remote-only history has no resident nodes; the manifest is what
            // lets a full-range window span the whole archive.
            let manifest = component.time_series.manifest();
            if let Some(span) = manifest.spans.first() {
                start = start.min(span.seal.start_ts.0 as f64);
                any = true;
            }
            if let Some(span) = manifest.spans.last() {
                end = end.max(span.cover_end.0 as f64);
                any = true;
            }
        }
        if !any || start >= end {
            return None;
        }
        let range = crate::temporal::resolve_range(
            &self.x_range,
            Timestamp(start as i64)..Timestamp(end as i64),
            cx,
        )?;
        let (min_x, mut max_x) = (range.start.0 as f64, range.end.0 as f64);
        if min_x >= max_x {
            max_x = min_x + 1.0;
        }
        Some(PlotBounds::new(min_x, 0.0, max_x, 1.0))
    }

    /// The envelope lane's resolved duration component — row 0, the pacemaker.
    fn envelope(&self) -> Option<&Component> {
        self.tracking.first()?.duration.as_ref()
    }

    /// Timestamp of the newest record the target has published, read straight
    /// off the components rather than out of the scanned window: a window
    /// parked on one historical cycle knows nothing about how far the data
    /// actually runs, and that is exactly what the stale cue must not guess.
    fn data_end(&self) -> Option<i64> {
        self.tracking
            .iter()
            .filter_map(|t| Some(t.duration.as_ref()?.time_series.latest()?.timestamp().0))
            .max()
    }

    /// The scope-trigger window: the newest cycle, from its record timestamp
    /// through its measured whole-cycle duration plus a quarter for headroom.
    ///
    /// The measured duration rather than the nominal period, because the point
    /// of the mode is to fill the pane with the cycle's *work* — on a target
    /// whose step takes 2% of its period, a nominal-width window would show a
    /// sliver of bars against a field of idle time. The nominal period stands in
    /// when no duration has been read yet.
    fn triggered_view(&self) -> Option<PlotBounds> {
        let start = self.envelope()?.time_series.latest()?.timestamp().0;
        let measured = self
            .frame
            .as_ref()
            .and_then(|f| f.cycle.last())
            .filter(|(ts, _)| *ts == start)
            .map(|(_, dur)| *dur as i64 * 5 / 4);
        let width = measured
            .filter(|w| *w > 0)
            .or(Some(self.nominal_period_us))
            .filter(|w| *w > 0)?;
        Some(PlotBounds::new(
            start as f64,
            0.0,
            (start + width) as f64,
            1.0,
        ))
    }

    /// How far left of the window the scan reads.
    ///
    /// A window zoomed inside one cycle contains no record at all — the cycle's
    /// own is to its left — so a query bounded by the window comes back empty
    /// and the lane goes blank even though its bars cover the whole view. Two
    /// nominal periods of lookback always captures the straddling record, since
    /// records are one period apart; without a nominal, the window's own width
    /// is the only scale available.
    fn lookback_us(&self, view: &PlotBounds) -> i64 {
        if self.nominal_period_us > 0 {
            self.nominal_period_us.saturating_mul(2)
        } else {
            (view.width() as i64).max(1)
        }
    }

    /// Earliest resolved sample, the anchor the X ticks align to.
    fn data_start(&self) -> f64 {
        self.tracking
            .iter()
            .filter_map(|t| t.duration.as_ref()?.time_series.start_timestamp())
            .map(|ts| ts.0 as f64)
            .fold(f64::INFINITY, f64::min)
    }

    /// Remote-only stretches of the window, as `(start, width)` fractions, and
    /// the hydration requests that fill them. Only the duration leaf is asked
    /// for: the state leaf rides the same spans.
    fn gap_bands(&self, cx: &App) -> smallvec::SmallVec<[(f32, f32); 4]> {
        let mut bands: smallvec::SmallVec<[(f32, f32); 4]> = smallvec::SmallVec::new();
        let Some(view) = self.effective_view(cx) else {
            return bands;
        };
        let span = view.width().max(1.0);
        let visible = Timestamp(view.min_x as i64)..Timestamp(view.max_x as i64);
        let hydrator = crate::hydration::hydrator(cx);
        let mut gaps = metor_db::manifest::GapVec::new();
        for (row, tracking) in self.rows.iter().zip(&self.tracking) {
            let Some(component) = tracking.duration.as_ref() else {
                continue;
            };
            gaps.clear();
            component.time_series.coverage(visible.clone(), &mut gaps);
            for gap in &gaps {
                if gap.state == metor_db::manifest::SpanState::RemoteOnly
                    && let Some(hydrator) = &hydrator
                {
                    hydrator.request(row.duration_id, gap.range.clone());
                    hydrator.request(row.state_id, gap.range.clone());
                }
                let start = ((gap.range.start.0 as f64 - view.min_x) / span).clamp(0.0, 1.0) as f32;
                let end = ((gap.range.end.0 as f64 - view.min_x) / span).clamp(0.0, 1.0) as f32;
                if end > start {
                    bands.push((start, end - start));
                }
            }
        }
        bands.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: smallvec::SmallVec<[(f32, f32); 4]> = smallvec::SmallVec::new();
        for (start, width) in bands {
            match merged.last_mut() {
                Some((last_start, last_width)) if start - (*last_start + *last_width) < 0.0005 => {
                    *last_width = (start + width - *last_start).max(*last_width);
                }
                _ => merged.push((start, width)),
            }
        }
        merged
    }

    /// Indices into `rows` for the lanes actually drawn, in row order.
    fn visible_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match row.kind {
                RowKind::Slot => self.show_slots,
                RowKind::Coordinator => self.show_coordinator_row,
                RowKind::System => true,
            })
            .filter(|(_, row)| !self.hidden.contains(&row.name))
            .map(|(i, _)| i)
            .collect()
    }

    /// The data-flow connectors whose two endpoints are both on screen,
    /// re-indexed from row positions to lane positions. An edge into a hidden
    /// lane is dropped rather than redirected — the operator asked not to see
    /// that system, and a line to nowhere is worse than no line.
    fn visible_flows(&self, visible: &[usize]) -> Vec<FlowPaint> {
        let lane_of = |row: usize| visible.iter().position(|v| *v == row);
        self.edges
            .iter()
            .filter_map(|edge| {
                let (from_lane, to_lane) = (lane_of(edge.from)?, lane_of(edge.to)?);
                Some(FlowPaint {
                    from_lane,
                    to_lane,
                    dashed: edge.delayed,
                    label: SharedString::from(format!(
                        "{} → {}",
                        self.rows[edge.from].name, self.rows[edge.to].name
                    )),
                    ports: edge.ports.clone(),
                })
            })
            .collect()
    }

    /// Pan or zoom. Touching the timebase releases the trigger, so the first
    /// drag out of trigger mode leaves the window where the trigger had it
    /// rather than snapping back to the auto fit.
    fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>) {
        let view = view.map(scan::clamp_window);

        let released = self.trigger && view.is_some();
        let changed = self.view_override.map(|v| v.bits()) != view.map(|v| v.bits());
        if changed || released {
            self.trigger &= view.is_none();
            self.view_override = view;
            self.restart_scan(cx);
        }
    }
}

/// What one scan pass needs, snapshotted on the main thread so the read itself
/// can run on the background executor.
struct ScanInput {
    /// The visible window, in microseconds.
    window: Range<i64>,
    /// How far left of it to read, so a cycle straddling the left edge is
    /// still found. See [`ExecTimeline::lookback_us`].
    lookback: i64,
    /// Newest record anywhere, for the stale cue.
    data_end: Option<i64>,
    nominal_period_us: i64,
    pairs: Vec<(Option<Component>, Option<Component>)>,
}

/// Read every lane and lay the bars out. Runs on the background executor: a
/// cycle-rate frame over a long window is a lot of samples.
fn build_frame(input: ScanInput) -> GanttFrame {
    let ScanInput {
        window,
        lookback,
        data_end,
        nominal_period_us,
        pairs,
    } = input;
    // Reading starts before the window so the cycle overlapping its left edge
    // is included; its bars are then clipped at paint rather than missing.
    let range = Timestamp(window.start.saturating_sub(lookback))..Timestamp(window.end);
    let samples: Vec<Vec<scan::CycleSample>> = pairs
        .iter()
        .map(|(duration, state)| match duration {
            Some(duration) => read_row(duration, state.as_ref(), range.clone()),
            None => Vec::new(),
        })
        .collect();
    // The envelope is row 0, and its record is the authoritative cycle set.
    let envelope = samples.first().cloned().unwrap_or_default();
    let cycle_ts: Vec<i64> = envelope.iter().map(|s| s.ts_us).collect();
    let cycle: Vec<(i64, u64)> = envelope.iter().map(|s| (s.ts_us, s.dur_us)).collect();
    let period_us = scan::measured_period(&cycle).unwrap_or(nominal_period_us);
    let mut bars = layout_cycles(&samples, &cycle_ts, Some(0));

    let total: usize = bars.iter().map(Vec::len).sum();
    let summarized = total > BAR_BUDGET;
    let bucket_us = if summarized {
        ((window.end - window.start) / SUMMARY_BUCKETS).max(1)
    } else {
        0
    };
    if summarized {
        for row in &mut bars {
            *row = summarize(row, window.clone(), bucket_us);
        }
    }

    GanttFrame {
        bars,
        // A summarized window holds thousands of cycles; the band would be a
        // solid smear, so it is dropped rather than drawn as noise.
        cycle: if summarized { Vec::new() } else { cycle },
        summarized,
        bucket_us,
        data_end,
        period_us,
    }
}

impl Render for ExecTimeline {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let visible = self.visible_rows();
        let view = self.effective_view(cx);
        let gaps = self.gap_bands(cx);
        let data_start = self.data_start();
        let fmt = self.x_time_format;

        // The gutter is the table's Name column: same header height and rule,
        // same cell padding and font, same per-row bottom border.
        let mut gutter = div()
            .flex()
            .flex_col()
            .w(px(GUTTER_W))
            .flex_none()
            .overflow_hidden()
            .child(
                div()
                    .h(px(RULER_H))
                    .flex_none()
                    .bg(theme.bg_secondary)
                    .border_b_1()
                    .border_color(theme.border_primary),
            );
        for index in &visible {
            let row = &self.rows[*index];
            let name = row.name.clone();
            let color = match row.kind {
                RowKind::Coordinator => theme.text_tertiary,
                RowKind::Slot => theme.text_secondary,
                RowKind::System => theme.text_primary,
            };
            gutter = gutter.child(
                div()
                    .flex()
                    .flex_1()
                    .max_h(px(paint::LANE_MAX))
                    .items_center()
                    .px(px(CELL_PAD_X))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .border_b_1()
                    .border_color(theme.border_primary)
                    .text_size(px(CELL_FONT_SIZE))
                    .text_color(color)
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, {
                        let name = name.clone();
                        move |event, window, cx| {
                            let proxy =
                                cx.new(|_| inspector_rows::SelectedGraphNode { id: name.clone() });
                            window.dispatch_action(
                                Box::new(crate::inspector::InspectEntity {
                                    entity: proxy.into_any(),
                                    position: event.position,
                                }),
                                cx,
                            );
                        }
                    })
                    .child(name),
            );
        }

        let ruler_paint = view.map(|view| RulerPaint {
            view,
            time_format: fmt,
            data_start,
            theme: theme.clone(),
        });
        let ruler = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds, window, cx| {
                if let Some(paint) = &ruler_paint {
                    paint_ruler(bounds, paint, window, cx);
                }
            },
        )
        .size_full();
        let ruler = div()
            .h(px(RULER_H))
            .flex_none()
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .child(ruler);

        let weak = cx.entity().downgrade();
        let lanes = canvas(
            move |bounds, _window, cx| {
                let _ = weak.update(cx, |timeline, _| timeline.lane_area = Some(bounds));
                weak.update(cx, |timeline, cx| {
                    let view = timeline.effective_view(cx)?;
                    let rows = timeline.visible_rows();
                    let labels = rows
                        .iter()
                        .map(|i| timeline.rows[*i].name.clone())
                        .collect();
                    let flows = timeline.visible_flows(&rows);
                    Some(LanePaint {
                        view,
                        frame: timeline.frame.clone(),
                        rows,
                        labels,
                        flows,
                        hover: timeline.hover,
                        time_format: timeline.x_time_format,
                        data_start: timeline.data_start(),
                        theme: crate::theme::theme(cx),
                    })
                })
                .ok()
                .flatten()
            },
            move |bounds, paint, window, cx| {
                if let Some(paint) = paint {
                    paint_lanes(bounds, &paint, window, cx);
                    crate::temporal::paint_playhead(
                        bounds,
                        (paint.view.min_x, paint.view.max_x),
                        window,
                        cx,
                    );
                }
            },
        )
        .absolute()
        .inset_0();

        let mut lane_area = div()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(v) = this.effective_view(cx) {
                        crate::temporal::picker::open_plot_actions(
                            (v.min_x, v.max_x),
                            event.position,
                            window,
                            cx,
                        );
                    }
                }),
            )
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    if event.click_count == 2 {
                        this.set_view_override(None, cx);
                    } else {
                        this.drag_start = Some(event.position);
                        this.drag_start_view = this.effective_view(cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                    this.drag_start = None;
                    this.drag_start_view = None;
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if !event.dragging() {
                        this.hover = Some(event.position);
                        cx.notify();
                        return;
                    }
                    let (Some(start), Some(start_view), Some(area)) =
                        (this.drag_start, this.drag_start_view, this.lane_area)
                    else {
                        return;
                    };
                    let (nx, _) =
                        start_view.screen_delta_to_norm(area, event.position.x - start.x, px(0.0));
                    this.set_view_override(Some(start_view.offset_x(-nx)), cx);
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let (Some(view), Some(area)) = (this.effective_view(cx), this.lane_area) else {
                        return;
                    };
                    let delta = event.delta.pixel_delta(px(20.0));
                    let zoom = f32::from(-delta.y) as f64 / 200.0;
                    let factor = (1.0 + zoom).clamp(0.5, 2.0);
                    let (ax, _) = view.screen_anchor(area, event.position);
                    this.set_view_override(Some(view.zoom_x(factor, ax)), cx);
                    cx.stop_propagation();
                }),
            )
            .child(lanes);

        for (start, width) in gaps {
            lane_area = lane_area.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui::relative(start))
                    .w(gpui::relative(width))
                    .bg(theme.plot_gap_band),
            );
        }

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(theme.bg_secondary)
            .child(gutter)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(ruler)
                    .child(lane_area),
            )
    }
}
