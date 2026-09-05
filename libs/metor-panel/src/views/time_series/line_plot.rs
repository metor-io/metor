//! GPU-backed line plot entity shared by every plot site in the panel.
//!
//! [`LinePlot`] owns the canonical trace list, the inspector-exposed view
//! overrides, the [`PlotRenderState`], and one tracking task per trace
//! keyed by [`gpui::EntityId`]. Parents embed a `LinePlot` entity and
//! leave rendering, bounds tracking, and GPU management to it.
//!
//! Every self-notify runs [`LinePlot::reconcile`], which spawns or drops
//! trackers for added or removed traces, invalidates the view override
//! when a reflected knob changes, and refreshes the cached title.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, Hsla, IntoElement, Pixels, Render, SharedString,
    Window, canvas, div, prelude::*,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use super::bounds::PlotView;
use super::gpu::{AxisSource, LineDraw, PlotRenderState};
use super::override_field::Override;
use super::{NodeBoundsCache, TimeFormat, Trace, YAxis, expand_value_bounds};
use crate::views::plot_common::reconcile_trackers;
use crate::views::time_series::time_range::{GlobalTimeRange, TimeRangeBehavior};
use crate::wait_for_component;

/// Background state for one trace, keyed by the trace's [`EntityId`] so
/// reordering `traces` doesn't invalidate resolved data.
struct TraceTracking {
    component: Option<Component>,
    y_bounds: Option<(f64, f64)>,
    /// Moved in and out of the background scan task on each tick; cleared
    /// when the trace's element index changes.
    node_bounds: NodeBoundsCache,
    cached_element_index: Option<usize>,
    /// The component's LoD companions (`metor_db::lod`), finest first.
    /// Re-resolved when the DB's component set changes.
    lod_levels: Vec<Component>,
    lod_resolved_gen: Option<u64>,
    /// True while the visible window holds more raw samples than
    /// [`RAW_SAMPLE_BUDGET`]; the trace then renders min/max buckets
    /// instead of raw data.
    over_budget: bool,
    /// Index into `lod_levels` while over budget; `None` when no level
    /// has been published yet (the trace shows gap bands instead).
    lod_selected: Option<usize>,
    /// Bounds of the selected LoD component, scanned by the tracker task.
    /// Keyed so a level switch invalidates the cache.
    lod_y_bounds: Option<(f64, f64)>,
    lod_node_bounds: NodeBoundsCache,
    lod_cache_key: Option<ComponentId>,
    /// How to compute the history this component lacks, when it is an
    /// expression's. Its ports also size the window while the component
    /// itself is still empty.
    plan: Option<crate::dynamic::ops::replay::ReplayPlan>,
}

impl TraceTracking {
    fn new() -> Self {
        Self {
            component: None,
            y_bounds: None,
            node_bounds: NodeBoundsCache::default(),
            cached_element_index: None,
            lod_levels: Vec::new(),
            lod_resolved_gen: None,
            over_budget: false,
            lod_selected: None,
            lod_y_bounds: None,
            lod_node_bounds: NodeBoundsCache::default(),
            lod_cache_key: None,
            plan: None,
        }
    }

    fn selected_lod(&self) -> Option<&Component> {
        self.lod_levels.get(self.lod_selected?)
    }
}

/// Per-trace sample budget for raw rendering: a quarter of the GPU value
/// capacity, leaving room for several traces per plot. Windows estimated
/// above it render from a min/max LoD component.
const RAW_SAMPLE_BUDGET: u64 = 1_000_000;
/// Per-trace bucket budget when picking an LoD level; each bucket renders
/// two points, so this matches the raw budget in GPU terms.
const LOD_BUCKET_BUDGET: u64 = RAW_SAMPLE_BUDGET / 2;

/// Previous values of the reflected override knobs. Compared on each
/// reconcile so an inspector edit can clear the interactive view override.
#[derive(Clone, PartialEq)]
struct OverrideSnapshot {
    /// Per-axis `(min, max)` overrides, index-aligned with `LinePlot::axes`.
    axes: smallvec::SmallVec<[(Option<f64>, Option<f64>); 2]>,
    /// The *resolved* range, so a global-range edit clears the pan/zoom
    /// override exactly like a per-plot edit does.
    x_range: TimeRangeBehavior,
}

impl OverrideSnapshot {
    fn capture(lp: &LinePlot, cx: &gpui::App) -> Self {
        let axes = lp
            .axes
            .iter()
            .map(|axis| {
                let a = axis.read(cx);
                (
                    a.y_min_override.as_custom().copied(),
                    a.y_max_override.as_custom().copied(),
                )
            })
            .collect();
        Self {
            axes,
            x_range: lp.resolved_x_range(cx),
        }
    }
}

/// Self-contained line plot entity.
///
/// Owns the render state, traces, per-trace trackers, and the
/// inspector-reflected view-override fields. Parents `.child(entity.clone())`
/// into their render trees and mutate this entity's fields directly.
#[derive(facet::Facet)]
pub struct LinePlot {
    pub traces: Vec<Entity<Trace>>,
    /// Y axes, stacked left-to-right outward from the plot. Always holds at
    /// least one (axis 0, the primary). Each trace names its axis by index.
    pub axes: Vec<Entity<YAxis>>,
    /// Event-flag overlays drawn in the plot's top gutter. Each names an
    /// event source by key; a visible overlay is queried and flagged per frame.
    pub event_overlays: Vec<Entity<super::EventOverlay>>,
    /// `Auto` follows the app-wide [`GlobalTimeRange`]; `Custom` pins this
    /// plot to its own window.
    pub x_range: Override<TimeRangeBehavior>,
    pub x_time_format: TimeFormat,
    pub custom_title: Override<SharedString>,
    /// Draw the control system's alarm limit lines for traces on this plot.
    pub show_alarm_limits: bool,
    /// Tint the plot background while a trace has an active alarm.
    pub show_alarm_color: bool,

    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, TraceTracking>,
    #[facet(opaque)]
    tasks: HashMap<EntityId, gpui::Task<()>>,
    #[facet(opaque)]
    inputs: crate::data_binding::InputChanges,
    #[facet(opaque)]
    view_override: Option<PlotView>,
    #[facet(opaque)]
    last_overrides: OverrideSnapshot,
    #[facet(opaque)]
    title_cache: SharedString,
    #[facet(opaque)]
    gpu_state: PlotRenderState,
}

impl LinePlot {
    /// Upper bound on Y axes. Each axis reserves `Y_LABEL_WIDTH` of left
    /// chrome; beyond this the plot area collapses on a typical panel.
    pub const MAX_AXES: usize = 4;

    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        cx.observe_self(Self::reconcile).detach();
        let primary_axis = cx.new(|_| YAxis::new("Y"));
        Self {
            traces: Vec::new(),
            axes: vec![primary_axis],
            event_overlays: Vec::new(),
            x_range: Override::Auto,
            x_time_format: TimeFormat::default(),
            custom_title: Override::Auto,
            show_alarm_limits: true,
            show_alarm_color: true,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
            inputs: Default::default(),
            view_override: None,
            last_overrides: OverrideSnapshot {
                axes: smallvec::SmallVec::new(),
                x_range: TimeRangeBehavior::FULL,
            },
            title_cache: "Plot".into(),
            gpu_state: PlotRenderState::default(),
        }
    }

    /// Replace the trace list from raw values.
    ///
    /// Convenience for callers (sparklines, table rows) that don't need a
    /// persistent `Entity<Trace>` handle for each series.
    pub fn bind_traces(&mut self, traces: Vec<Trace>, cx: &mut Context<Self>) {
        self.traces = traces
            .into_iter()
            .map(|mut t| {
                t.source.resolve(&self.db, cx);
                cx.new(|_| t)
            })
            .collect();
        cx.notify();
    }

    /// Forget what `trace` was tracking, so the next reconcile follows the
    /// component it names now. The inspector calls this after rewriting a
    /// trace's source; the tracker latched its component when it started
    /// and would otherwise keep reading the old one.
    pub fn rebind_trace(&mut self, trace: &Entity<Trace>, cx: &mut Context<Self>) {
        let id = trace.entity_id();
        self.tracking.remove(&id);
        self.tasks.remove(&id);
        cx.notify();
    }

    /// Pin the view to `view`, or clear the override with `None`.
    ///
    /// Called by pan/zoom handlers. Uses a bit-pattern compare to dodge
    /// the lack of `PartialEq` on `f64`-containing [`PlotView`].
    pub fn set_view_override(&mut self, view: Option<PlotView>, cx: &mut Context<Self>) {
        let changed =
            self.view_override.as_ref().map(PlotView::bits) != view.as_ref().map(PlotView::bits);
        if changed {
            self.view_override = view;
            cx.notify();
        }
    }

    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// The time window this plot actually uses: its own `Custom` range, or
    /// the app-wide [`GlobalTimeRange`] when set to `Auto`.
    pub fn resolved_x_range(&self, cx: &gpui::App) -> TimeRangeBehavior {
        match self.x_range.as_custom() {
            Some(behavior) => *behavior,
            None => GlobalTimeRange::get(cx),
        }
    }

    /// Decide raw versus LoD per trace for the current window. Hysteresis
    /// keeps the boundary from flapping while panning near the budget;
    /// over budget the finest level whose visible bucket count fits
    /// [`LOD_BUCKET_BUDGET`] wins, falling back to the coarsest.
    fn update_lod_state(&mut self, cx: &gpui::App) {
        const ENTER: u64 = RAW_SAMPLE_BUDGET + RAW_SAMPLE_BUDGET / 4;
        const EXIT: u64 = RAW_SAMPLE_BUDGET * 4 / 5;
        let Some(view) = self.effective_view(cx) else {
            return;
        };
        let (min_x, max_x) = view.x;
        let range = Timestamp(min_x as i64)..Timestamp(max_x as i64);
        let vtable_gen = self.db.vtable_gen.latest();
        for trace in &self.traces {
            let cfg = trace.read(cx);
            let Some(tracking) = self.tracking.get_mut(&trace.entity_id()) else {
                continue;
            };
            let Some(component) = tracking.component.as_ref() else {
                continue;
            };
            if !cfg.visible {
                continue;
            }
            if tracking.lod_resolved_gen != Some(vtable_gen) {
                tracking.lod_levels = resolve_lod_levels(&self.db, cfg.source.id());
                tracking.lod_resolved_gen = Some(vtable_gen);
            }
            let estimate = component.time_series.estimate_samples(range.clone());
            if tracking.over_budget {
                tracking.over_budget = estimate >= EXIT;
            } else {
                tracking.over_budget = estimate > ENTER;
            }
            if !tracking.over_budget {
                tracking.lod_selected = None;
                continue;
            }
            // Finest level fitting the budget wins; coarser fallback. A
            // level with no data at all (still backfilling, or its first
            // bucket hasn't completed) can't render and is skipped.
            let mut selected = None;
            for (i, lod) in tracking.lod_levels.iter().enumerate() {
                let has_data = lod.time_series.latest().is_some()
                    || !lod.time_series.manifest().spans.is_empty();
                if !has_data {
                    continue;
                }
                selected = Some(i);
                if lod.time_series.estimate_samples(range.clone()) <= LOD_BUCKET_BUDGET {
                    break;
                }
            }
            tracking.lod_selected = selected;
        }
    }

    /// Remote-only stretches of the visible window, as `(start, width)`
    /// fractions of the plot width. This also asks the installed hydrator
    /// (if any) to fetch them — bounded, deduplicated, cheap to call per
    /// frame. Over-budget traces never hydrate raw nodes (a year-wide view
    /// must not pull the archive): gaps come from, and hydration targets,
    /// the selected LoD companion, which is ~`2/ratio` the size.
    fn gap_bands(&self, cx: &gpui::App) -> smallvec::SmallVec<[(f32, f32); 4]> {
        let mut bands: smallvec::SmallVec<[(f32, f32); 4]> = smallvec::SmallVec::new();
        let Some(view) = self.effective_view(cx) else {
            return bands;
        };
        let (min_x, max_x) = view.x;
        let span = (max_x - min_x).max(1.0);
        let visible = Timestamp(min_x as i64)..Timestamp(max_x as i64);
        let hydrator = crate::hydration::hydrator(cx);
        let mut gaps = metor_db::manifest::GapVec::new();
        for trace in &self.traces {
            let cfg = trace.read(cx);
            if !cfg.visible {
                continue;
            }
            let Some(tracking) = self.tracking.get(&trace.entity_id()) else {
                continue;
            };
            let Some(component) = tracking.component.as_ref() else {
                continue;
            };
            let (series, hydrate_id, hydrate) = match tracking.selected_lod() {
                Some(lod) => (&lod.time_series, lod.component_id, true),
                // Over budget with no published level yet: show raw gaps
                // but never pull the raw archive for a wide view.
                None if tracking.over_budget => (&component.time_series, cfg.source.id(), false),
                None => (&component.time_series, cfg.source.id(), true),
            };
            gaps.clear();
            series.coverage(visible.clone(), &mut gaps);
            let mut band = |range: &std::ops::Range<Timestamp>| {
                let start = ((range.start.0 as f64 - min_x) / span).clamp(0.0, 1.0) as f32;
                let end = ((range.end.0 as f64 - min_x) / span).clamp(0.0, 1.0) as f32;
                if end > start {
                    bands.push((start, end - start));
                }
            };
            for gap in &gaps {
                if hydrate
                    && gap.state == metor_db::manifest::SpanState::RemoteOnly
                    && let Some(hydrator) = &hydrator
                {
                    hydrator.request(hydrate_id, gap.range.clone());
                }
                band(&gap.range);
            }
            // What an expression has never computed is a gap of the other
            // kind: nothing to fetch, something to compute. Always against
            // the raw series — a LoD level is derived from it afterwards.
            let history = crate::data_binding::BoundHistory {
                component: component.clone(),
                plan: tracking.plan.clone(),
            };
            for range in history.request_replay(visible.clone(), cx) {
                band(&range);
            }
        }
        // A year-wide view over a sparse archive yields hundreds of
        // sub-pixel gaps; merge anything closer than ~half a pixel so the
        // overlay stays a handful of divs.
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

    /// Bring tracking state and the title cache in sync with `self.traces`.
    ///
    /// Runs on every self-notify. Spawns a tracker for each new trace,
    /// drops trackers for removed traces, and resets the pan/zoom override
    /// when an inspector edit changes the reflected view knobs.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        for trace in &self.traces {
            trace.update(cx, |trace, cx| {
                trace.source.resolve(&self.db, cx);
            });
        }

        // Always keep at least the primary axis so traces have a home and
        // the chrome has something to render, and never exceed the chrome
        // budget (the generic "Add" doesn't gate, so cap here).
        if self.axes.is_empty() {
            let axis = cx.new(|_| YAxis::new("Y"));
            self.axes.push(axis);
        }
        self.axes.truncate(Self::MAX_AXES);
        // Point each trace back at this plot (so the axis picker can list
        // sibling axes) and clamp stale axis indices. Mutating the trace
        // entity without notifying is safe: the plot doesn't observe its
        // traces, so this can't re-enter reconcile.
        let self_weak = cx.entity().downgrade();
        let self_id = cx.entity().entity_id();
        let axis_count = self.axes.len();
        for trace in &self.traces {
            trace.update(cx, |t, _| {
                let linked = t
                    .line_plot
                    .as_ref()
                    .and_then(|w| w.upgrade())
                    .map(|e| e.entity_id());
                if linked != Some(self_id) {
                    t.line_plot = Some(self_weak.clone());
                }
                if t.axis_index >= axis_count {
                    t.axis_index = 0;
                }
            });
        }

        for id in self.inputs.changed(
            &self.traces,
            &self.db,
            |trace| vec![(trace.source.id(), trace.element_index)],
            cx,
        ) {
            self.tracking.remove(&id);
            self.tasks.remove(&id);
        }

        let db = self.db.clone();
        reconcile_trackers(
            &self.traces,
            &mut self.tracking,
            &mut self.tasks,
            |id, trace| {
                (
                    TraceTracking::new(),
                    Self::spawn_tracker(id, trace.clone(), db.clone(), cx),
                )
            },
        );

        let snapshot = OverrideSnapshot::capture(self, cx);
        if snapshot != self.last_overrides {
            self.view_override = None;
            self.last_overrides = snapshot;
        }

        self.title_cache = match &self.custom_title {
            Override::Custom(custom) => custom.clone(),
            Override::Auto => derive_title(&self.traces, &self.db, cx),
        };
    }

    /// The view the renderer will use this frame.
    ///
    /// Returns the interactive pan/zoom override when set; otherwise
    /// auto-fits the shared X range against every visible trace and each
    /// axis's Y range against just the traces assigned to it, then applies
    /// per-axis inspector overrides on top.
    pub fn effective_view(&self, cx: &gpui::App) -> Option<PlotView> {
        if let Some(v) = &self.view_override {
            return Some(v.clone());
        }
        let axis_count = self.axes.len().max(1);
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut any_time = false;
        let mut y_min = vec![f64::INFINITY; axis_count];
        let mut y_max = vec![f64::NEG_INFINITY; axis_count];
        let mut any_y = vec![false; axis_count];
        for trace in &self.traces {
            let cfg = trace.read(cx);
            if !cfg.visible {
                continue;
            }
            let Some(tracking) = self.tracking.get(&trace.entity_id()) else {
                continue;
            };
            let Some(comp) = &tracking.component else {
                continue;
            };
            let mut extend = |comp: &Component| {
                if let Some(s) = comp.time_series.start_timestamp() {
                    start = start.min(s.0 as f64);
                    any_time = true;
                }
                if let Some(l) = comp.time_series.latest() {
                    end = end.max(l.timestamp().0 as f64);
                    any_time = true;
                }
                // Remote-only history has no resident nodes; the manifest is
                // what lets a full-range view span the whole archive.
                let manifest = comp.time_series.manifest();
                if let Some(span) = manifest.spans.first() {
                    start = start.min(span.seal.start_ts.0 as f64);
                    any_time = true;
                }
                if let Some(span) = manifest.spans.last() {
                    end = end.max(span.cover_end.0 as f64);
                    any_time = true;
                }
            };
            extend(comp);
            // An expression's component holds only what has been computed;
            // what *could* be is bounded by its inputs, and that is the
            // window a backfill fills.
            if let Some(plan) = &tracking.plan {
                for port in &plan.ports {
                    extend(port);
                }
            }
            let ai = cfg.axis_index.min(axis_count - 1);
            if let Some((lo, hi)) = tracking.y_bounds {
                y_min[ai] = y_min[ai].min(lo);
                y_max[ai] = y_max[ai].max(hi);
                any_y[ai] = true;
            }
            // Resident raw bounds only cover hydrated data; the LoD
            // component is the Y authority for summarized history.
            if tracking.lod_selected.is_some()
                && let Some((lo, hi)) = tracking.lod_y_bounds
            {
                y_min[ai] = y_min[ai].min(lo);
                y_max[ai] = y_max[ai].max(hi);
                any_y[ai] = true;
            }
        }
        if !any_time || start >= end {
            return None;
        }
        let range = self
            .resolved_x_range(cx)
            .calculate_range(Timestamp(start as i64), Timestamp(end as i64));
        let (min_x, mut max_x) = (range.start.0 as f64, range.end.0 as f64);
        if min_x >= max_x {
            max_x = min_x + 1.0;
        }
        let mut axes: smallvec::SmallVec<[(f64, f64); 2]> = smallvec::SmallVec::new();
        for (i, axis) in self.axes.iter().enumerate() {
            let a = axis.read(cx);
            let (auto_min, auto_max) = if any_y[i] {
                (y_min[i], y_max[i])
            } else {
                (0.0, 1.0)
            };
            let lo_override = a.y_min_override.as_custom().copied();
            let hi_override = a.y_max_override.as_custom().copied();
            let lo = lo_override.unwrap_or(auto_min);
            let hi = hi_override.unwrap_or(auto_max);
            // Auto-fit ends get edge padding so a trace riding its extreme
            // isn't hidden under the axis rule; explicit overrides are the
            // user's exact numbers and pass through unpadded.
            axes.push(super::bounds::pad_auto_range(
                lo,
                hi,
                lo_override.is_none(),
                hi_override.is_none(),
            ));
        }
        Some(PlotView {
            x: (min_x, max_x),
            axes,
        })
    }

    /// Resolved chrome color per axis, index-aligned with `axes`.
    ///
    /// `Some` for an explicit `Custom` color; `None` for `Auto`, where the
    /// caller falls back to the neutral theme color. Axes are neutral by
    /// default — color is opt-in.
    pub fn axis_colors(&self, cx: &gpui::App) -> smallvec::SmallVec<[Option<Hsla>; 2]> {
        self.axes
            .iter()
            .map(|axis| match &axis.read(cx).color {
                Override::Custom(c) => Some(*c),
                Override::Auto => None,
            })
            .collect()
    }

    /// Earliest sample timestamp across resolved traces.
    ///
    /// Used by the x-axis tick generator to anchor labels against a
    /// stable origin even as the plot scrolls.
    pub fn data_start(&self) -> Option<f64> {
        let mut start = f64::INFINITY;
        let mut any = false;
        for trace in &self.traces {
            let Some(tracking) = self.tracking.get(&trace.entity_id()) else {
                continue;
            };
            let Some(comp) = &tracking.component else {
                continue;
            };
            if let Some(s) = comp.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any = true;
            }
        }
        any.then_some(start)
    }

    pub fn traces(&self) -> &[Entity<Trace>] {
        &self.traces
    }

    /// The resolved [`Component`] backing `trace`, if its tracker has caught
    /// up to the DB. Returned by clone because trackers keep their own
    /// `Component` and the borrow can't escape `self`.
    pub fn component_for_trace(&self, trace: &Entity<Trace>, _cx: &gpui::App) -> Option<Component> {
        self.tracking
            .get(&trace.entity_id())?
            .component
            .as_ref()
            .cloned()
    }

    /// Value plotted by `trace` at the sample whose timestamp is closest to
    /// `ts` across every live node.
    ///
    /// Used by measurement-cursor rendering (dot marker per trace) and by
    /// the Δy / Mean readouts that need a value at the cursor endpoints.
    pub fn trace_value_at(&self, trace_id: EntityId, ts: Timestamp, cx: &gpui::App) -> Option<f64> {
        self.trace_sample_at(trace_id, ts, cx).map(|(_, v)| v)
    }

    /// The sample of `trace` whose timestamp is closest to `ts` across every
    /// live node: its actual timestamp plus the plotted value. Returns `None`
    /// when the trace's component hasn't resolved yet, when its element index
    /// falls outside the schema, or when the component has no samples at all.
    ///
    /// The hover readout and its on-plot sample markers both consume this,
    /// so the highlighted point is always the sample the readout reports.
    pub fn trace_sample_at(
        &self,
        trace_id: EntityId,
        ts: Timestamp,
        cx: &gpui::App,
    ) -> Option<(Timestamp, f64)> {
        let trace = self.traces.iter().find(|t| t.entity_id() == trace_id)?;
        let cfg = trace.read(cx);
        let component = self.tracking.get(&trace_id)?.component.as_ref()?;
        let elem_size = component.schema.size();
        if elem_size == 0 {
            return None;
        }
        let total_elements: usize = component.schema.dim.iter().product::<usize>().max(1);
        if cfg.element_index >= total_elements {
            return None;
        }
        // Nodes iterate newest-first, but a historical cursor endpoint can
        // land in any of them — keep the global best, not the first hit.
        let mut best: Option<(i64, _, usize)> = None;
        for node in component.time_series.list.iter() {
            let Some((idx, dist)) = super::cursor::nearest_in_node(node.timestamps(), ts) else {
                continue;
            };
            if best.as_ref().map(|(d, _, _)| dist < *d).unwrap_or(true) {
                best = Some((dist, node, idx));
            }
        }
        let (_, node, sample_idx) = best?;
        let sample_ts = *node.timestamps().get(sample_idx)?;
        let data = node.data.data();
        let base = sample_idx * elem_size;
        let buf = data.get(base..base + elem_size)?;
        let value =
            crate::dynamic::tensor::read_f64_at(buf, component.schema.prim_type, cfg.element_index);
        Some((sample_ts, value))
    }

    pub fn trace(&self, idx: usize) -> Option<&Entity<Trace>> {
        self.traces.get(idx)
    }

    pub fn trace_count(&self) -> usize {
        self.traces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Title shown in the host panel. Cached by [`Self::reconcile`].
    pub fn title(&self) -> SharedString {
        self.title_cache.clone()
    }

    fn spawn_tracker(
        id: EntityId,
        trace: Entity<Trace>,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let trace_id = match this.update(cx, |_, cx| trace.read(cx).source.id()) {
                Ok(id) => id,
                Err(_) => return,
            };
            let component = wait_for_component(&db, trace_id).await;
            let installed = this.update(cx, |lp, cx| {
                let plan = crate::dynamic::expressions::replay_plan(trace_id, &db, cx);
                if let Some(tracking) = lp.tracking.get_mut(&id) {
                    tracking.component = Some(component.clone());
                    tracking.plan = plan;
                    cx.notify();
                    true
                } else {
                    false
                }
            });
            if !matches!(installed, Ok(true)) {
                return;
            }

            loop {
                let inputs = this.update(cx, |lp, cx| {
                    let tracking = lp.tracking.get_mut(&id)?;
                    let comp = tracking.component.clone()?;
                    let element_index = trace.read(cx).element_index;
                    if tracking.cached_element_index != Some(element_index) {
                        tracking.node_bounds.clear();
                        tracking.y_bounds = None;
                        tracking.lod_node_bounds.clear();
                        tracking.lod_y_bounds = None;
                        tracking.cached_element_index = Some(element_index);
                    }
                    // The selected LoD component's bounds are scanned
                    // alongside the raw ones; switching levels invalidates
                    // its cache.
                    let lod = tracking.selected_lod().cloned();
                    let lod_key = lod.as_ref().map(|l| l.component_id);
                    if tracking.lod_cache_key != lod_key {
                        tracking.lod_node_bounds.clear();
                        tracking.lod_y_bounds = None;
                        tracking.lod_cache_key = lod_key;
                    }
                    let cache = std::mem::take(&mut tracking.node_bounds);
                    let lod_cache = std::mem::take(&mut tracking.lod_node_bounds);
                    Some((comp, element_index, cache, lod, lod_cache))
                });
                let Ok(Some((comp, element_index, mut cache, lod, mut lod_cache))) = inputs else {
                    break;
                };

                let lod_for_wait = lod.clone();
                let (bounds, cache, lod_bounds, lod_cache) = cx
                    .background_executor()
                    .spawn(async move {
                        let bounds = expand_value_bounds(&comp, &[element_index], &mut cache);
                        // LoD schemas are [2, ..shape]: the element's min
                        // and its max live `n_elements` apart.
                        let lod_bounds = lod.as_ref().and_then(|lod| {
                            let n = lod.schema.dim.iter().skip(1).product::<usize>().max(1);
                            expand_value_bounds(
                                lod,
                                &[element_index, n + element_index],
                                &mut lod_cache,
                            )
                        });
                        (bounds, cache, lod_bounds, lod_cache)
                    })
                    .await;

                let installed = this.update(cx, |lp, cx| {
                    let Some(tracking) = lp.tracking.get_mut(&id) else {
                        return false;
                    };
                    // Drop the result if the trace's element index was
                    // changed concurrently; the next iteration rescans.
                    if tracking.cached_element_index == Some(element_index) {
                        tracking.node_bounds = cache;
                        tracking.y_bounds = bounds;
                        if tracking.lod_cache_key == lod_for_wait.as_ref().map(|l| l.component_id) {
                            tracking.lod_node_bounds = lod_cache;
                            tracking.lod_y_bounds = lod_bounds;
                        }
                    }
                    cx.notify();
                    true
                });
                if !matches!(installed, Ok(true)) {
                    break;
                }
                // Hydrated LoD nodes wake the same as live raw data, so a
                // static plot of pure history still repaints as its
                // buckets land.
                match &lod_for_wait {
                    Some(lod) => {
                        futures_lite::future::race(
                            component.time_series.wait(),
                            lod.time_series.wait(),
                        )
                        .await
                    }
                    None => component.time_series.wait().await,
                }
            }
        })
    }
}

impl Render for LinePlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let envelope_alpha = theme.plot_envelope_alpha;
        self.update_lod_state(cx);
        let degraded = self.gpu_state.degraded();
        let gap_bands = self.gap_bands(cx);
        let weak = cx.entity().downgrade();
        let plot_canvas = canvas(
            move |bounds, window, cx| {
                let scale_factor = window.scale_factor();
                let (frame, released) = weak
                    .update(cx, |lp, cx| {
                        if let Some(view) = lp.effective_view(cx) {
                            let mut draws: Vec<LineDraw<'_>> = Vec::new();
                            for trace in &lp.traces {
                                let config = trace.read(cx);
                                if !config.visible {
                                    continue;
                                }
                                let Some(tracking) = lp.tracking.get(&trace.entity_id()) else {
                                    continue;
                                };
                                let Some(component) = tracking.component.as_ref() else {
                                    continue;
                                };
                                let axis = view.axis_bounds(config.axis_index);
                                // Summarized history draws as dimmed
                                // min/max buckets; raw samples cover only
                                // the live tail past the last bucket.
                                let raw_clip = if let Some(lod) = tracking.selected_lod() {
                                    let mut color = config.color;
                                    color.a *= envelope_alpha;
                                    draws.push(LineDraw {
                                        x: AxisSource::Timestamps,
                                        y: AxisSource::MinMax {
                                            component: lod,
                                            element_index: config.element_index,
                                        },
                                        y_min: axis.min_y,
                                        y_max: axis.max_y,
                                        style: super::PlotStyle::Line,
                                        color,
                                        stroke_width: config.stroke_width,
                                        x_clip: None,
                                    });
                                    let tail = lod
                                        .time_series
                                        .latest()
                                        .map(|l| l.timestamp().0 as f64)
                                        .unwrap_or(f64::NEG_INFINITY);
                                    Some((tail, f64::INFINITY))
                                } else {
                                    None
                                };
                                draws.push(LineDraw {
                                    x: AxisSource::Timestamps,
                                    y: AxisSource::Element {
                                        component,
                                        element_index: config.element_index,
                                    },
                                    y_min: axis.min_y,
                                    y_max: axis.max_y,
                                    style: config.style,
                                    color: config.color,
                                    stroke_width: config.stroke_width,
                                    x_clip: raw_clip,
                                });
                            }
                            // X data + a 0..1 Y placeholder; each trace's Y is
                            // normalized per axis in the materializer.
                            let gpu_view = view.x_bounds();
                            if !draws.is_empty()
                                && let Some(handle) =
                                    lp.gpu_state
                                        .render(cx, bounds, scale_factor, gpu_view, &draws)
                            {
                                handle.spawn_and_set(cx, line_plot_gpu_state);
                            }
                        }
                        (
                            lp.gpu_state.current_frame(),
                            lp.gpu_state.take_pending_release(),
                        )
                    })
                    .unwrap_or((None, None));
                if let Some(img) = released {
                    let _ = window.drop_image(img);
                }
                (bounds, frame)
            },
            |_, (bounds, frame): (Bounds<Pixels>, Option<Arc<gpui::RenderImage>>), window, _cx| {
                if let Some(img) = frame {
                    let _ = window.paint_image(bounds, Corners::default(), img, 0, false);
                }
            },
        )
        .size_full();
        div()
            .size_full()
            .relative()
            .child(plot_canvas)
            .children(gap_bands.into_iter().map({
                let band_color = theme.plot_gap_band;
                move |(start, width)| {
                    // History that lives in a store but not on disk yet; the
                    // hydrator has been asked for it and the band disappears
                    // when the data lands and wakes the trackers.
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(gpui::relative(start))
                        .w(gpui::relative(width))
                        .bg(band_color)
                }
            }))
            .when(degraded, |el| {
                // The last frame clamped its uploads: more samples are in
                // view than the GPU buffers hold, so the trace is shown
                // decimated beyond the usual per-pixel budget.
                el.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_1()
                        .px_1()
                        .text_xs()
                        .text_color(theme.text_tertiary)
                        .child("decimated"),
                )
            })
    }
}

fn line_plot_gpu_state(lp: &mut LinePlot) -> &mut PlotRenderState {
    &mut lp.gpu_state
}

/// The LoD companions published for `source`, finest first. Resolved by
/// the `lod_source_id` metadata key rather than name construction so a
/// renamed source keeps its levels.
pub(crate) fn resolve_lod_levels(db: &DB, source: ComponentId) -> Vec<Component> {
    let source_key = source.0.to_string();
    db.with_state(|state| {
        let mut levels: Vec<(u64, Component)> = state
            .component_metadata_iter()
            .filter_map(|(id, meta)| {
                if meta.metadata.get(metor_db::lod::LOD_SOURCE_ID_KEY)? != &source_key {
                    return None;
                }
                let ratio: u64 = meta
                    .metadata
                    .get(metor_db::lod::LOD_RATIO_KEY)?
                    .parse()
                    .ok()?;
                Some((ratio, state.get_component(*id)?.clone()))
            })
            .collect();
        levels.sort_unstable_by_key(|(ratio, _)| *ratio);
        levels.into_iter().map(|(_, component)| component).collect()
    })
}

/// Derive a plot title from the trace list.
///
/// A component contributes just its name when every element is plotted;
/// partial coverage gets an `[x,y]`-style subset. Multiple components are
/// joined with `", "`.
fn derive_title(traces: &[Entity<Trace>], db: &Arc<DB>, cx: &gpui::App) -> SharedString {
    if traces.is_empty() {
        return "Plot".into();
    }

    let mut groups: HashMap<ComponentId, Vec<usize>> = HashMap::new();
    let mut order: Vec<ComponentId> = Vec::new();
    for trace in traces {
        let t = trace.read(cx);
        let id = t.source.id();
        groups
            .entry(id)
            .or_insert_with(|| {
                order.push(id);
                Vec::new()
            })
            .push(t.element_index);
    }

    let parts: Vec<String> = order
        .iter()
        .map(|comp_id| {
            let indexes = &groups[comp_id];
            let all_elements =
                crate::inspector::trace_picker::element_names_for_component(db, *comp_id);
            let comp_name = db
                .with_state(|s| s.get_component_metadata(*comp_id).map(|m| m.name.clone()))
                .unwrap_or_default();

            if indexes.len() == all_elements.len() {
                comp_name
            } else {
                let names: Vec<&str> = indexes
                    .iter()
                    .filter_map(|&i| all_elements.get(i).map(|s| s.as_str()))
                    .collect();
                format!("{}[{}]", comp_name, names.join(","))
            }
        })
        .collect();

    SharedString::from(parts.join(", "))
}

#[cfg(test)]
mod rebind_tests {
    use super::*;
    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;

    /// Rewriting a trace's source is only half of a rebind: the plot's
    /// tracker latched the old component when it started, so the plot has
    /// to be told to follow the new one.
    #[gpui::test]
    fn a_rebound_trace_is_tracked_on_its_new_component(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        for name in ["adcs.rate", "wheels.rpm"] {
            db.with_state_mut(|s| {
                s.insert_component(
                    ComponentId::new(name),
                    ComponentSchema::new(PrimType::F64, &[][..]),
                    &db.path,
                )
            })
            .unwrap();
        }
        let plot = cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            cx.new(|cx| {
                let mut lp = LinePlot::new(db.clone(), cx);
                let trace = Trace::new(
                    ComponentId::new("adcs.rate"),
                    0,
                    crate::theme::DARK.line_colors[0],
                );
                lp.bind_traces(vec![trace], cx);
                lp
            })
        });
        cx.run_until_parked();
        let trace = cx.update(|cx| plot.read(cx).traces()[0].clone());
        let tracked = cx.update(|cx| {
            plot.read(cx)
                .component_for_trace(&trace, cx)
                .map(|c| c.component_id)
        });
        assert_eq!(tracked, Some(ComponentId::new("adcs.rate")));

        cx.update(|cx| {
            trace.update(cx, |t, cx| {
                t.source = crate::data_binding::Binding::from(ComponentId::new("wheels.rpm"));
                cx.notify();
            });
        });
        cx.run_until_parked();
        let tracked = cx.update(|cx| {
            plot.read(cx)
                .component_for_trace(&trace, cx)
                .map(|c| c.component_id)
        });
        assert_eq!(tracked, Some(ComponentId::new("wheels.rpm")));
    }
}
