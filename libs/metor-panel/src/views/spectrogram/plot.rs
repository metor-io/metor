//! GPU-backed spectrogram entity.
//!
//! Sits where [`ListLinePlot`](crate::views::list_plot::ListLinePlot) sits for
//! the list plot: it owns the source list, the inspector-reflected view knobs,
//! the render state, and one tracking task per source. What differs is the
//! frame: instead of planning trace geometry it buckets history into a
//! `[time × bin]` grid on the CPU and hands the GPU one intensity field.
//!
//! The bucketing runs in canvas prepaint, every frame, against the visible
//! window — the same place `plan_trace` walks samples for a line — so zooming
//! re-buckets rather than resampling a picture.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, IntoElement, Pixels, Render, SharedString, Window,
    canvas, div, prelude::*,
};
use metor_db::{Component, DB};
use metor_proto::types::Timestamp;

use crate::views::plot_common::reconcile_trackers;
use crate::views::time_series::time_range::{GlobalTimeRange, TimeRangeBehavior};
use crate::views::time_series::{
    Colormap, IntensityDraw, IntensityScale, Override, PlotBounds, PlotRenderState, TimeFormat,
    resolve_lod_levels,
};
use crate::wait_for_component;

use super::SpectrogramTrace;
use super::grid::{self, ELEMENT_SCAN_BUDGET, MAX_COLS, SpectrogramGrid};

/// Background state for one source, keyed by the trace's [`EntityId`].
struct SourceTracking {
    component: Option<Component>,
    /// The component's LoD companions (`metor_db::lod`), finest first.
    lod_levels: Vec<Component>,
    lod_resolved_gen: Option<u64>,
    /// True while the visible window would cost more element reads than
    /// [`ELEMENT_SCAN_BUDGET`]; the grid then comes from an LoD companion.
    over_budget: bool,
    lod_selected: Option<usize>,
    /// How to compute the history an expression's component lacks. Its ports
    /// also size the window while the component itself is still empty.
    plan: Option<crate::dynamic::ops::replay::ReplayPlan>,
}

impl SourceTracking {
    fn new() -> Self {
        Self {
            component: None,
            lod_levels: Vec::new(),
            lod_resolved_gen: None,
            over_budget: false,
            lod_selected: None,
            plan: None,
        }
    }

    fn selected_lod(&self) -> Option<&Component> {
        self.lod_levels.get(self.lod_selected?)
    }
}

/// Previous values of the reflected knobs, compared on each reconcile so an
/// inspector edit drops the interactive pan/zoom override.
#[derive(Clone, PartialEq)]
struct OverrideSnapshot {
    y_min: Option<f64>,
    y_max: Option<f64>,
    x_range: TimeRangeBehavior,
}

impl OverrideSnapshot {
    fn capture(plot: &SpectrogramPlot, cx: &gpui::App) -> Self {
        Self {
            y_min: plot.y_min_override.as_custom().copied(),
            y_max: plot.y_max_override.as_custom().copied(),
            x_range: plot.resolved_x_range(cx),
        }
    }
}

/// One frame's readout inputs, snapshotted for the chrome painter.
pub(crate) struct HoverSample {
    pub ts: i64,
    pub bin: usize,
    pub value: f32,
}

/// Self-contained spectrogram entity.
#[derive(facet::Facet)]
pub struct SpectrogramPlot {
    /// Sources. Only the first visible one renders — two overlaid fields would
    /// occlude each other — but the list shape is kept so the trace machinery
    /// (reconcile, entity-list inspector rows, config) is shared verbatim.
    pub traces: Vec<Entity<SpectrogramTrace>>,
    /// `Auto` follows the app-wide [`GlobalTimeRange`]; `Custom` pins this
    /// pane to its own window.
    pub x_range: Override<TimeRangeBehavior>,
    pub x_time_format: TimeFormat,
    /// Visible bin range, in bin indices.
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    /// Pins the colour mapping's ends in display units (dB under `Log`),
    /// replacing the per-frame automatic range.
    pub intensity_min: Override<f64>,
    pub intensity_max: Override<f64>,
    /// Sampling rate of the signal behind the spectrum, in Hz. Labels the Y
    /// axis in Hz instead of bin indices; the data is unaffected.
    pub sample_rate: Override<f64>,
    pub show_colorbar: bool,
    pub custom_title: Override<SharedString>,

    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, SourceTracking>,
    #[facet(opaque)]
    tasks: HashMap<EntityId, gpui::Task<()>>,
    #[facet(opaque)]
    inputs: crate::data_binding::InputChanges,
    #[facet(opaque)]
    view_override: Option<PlotBounds>,
    #[facet(opaque)]
    last_overrides: OverrideSnapshot,
    #[facet(opaque)]
    title_cache: SharedString,
    #[facet(opaque)]
    gpu_state: PlotRenderState,
    /// Last frame's buckets, kept so the hover readout reports exactly what
    /// is on screen rather than re-reading the DB at a different instant.
    #[facet(opaque)]
    grid: SpectrogramGrid,
}

impl SpectrogramPlot {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        cx.observe_self(Self::reconcile).detach();
        Self {
            traces: Vec::new(),
            x_range: Override::Auto,
            x_time_format: TimeFormat::default(),
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            intensity_min: Override::Auto,
            intensity_max: Override::Auto,
            sample_rate: Override::Auto,
            show_colorbar: true,
            custom_title: Override::Auto,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
            inputs: Default::default(),
            view_override: None,
            last_overrides: OverrideSnapshot {
                y_min: None,
                y_max: None,
                x_range: TimeRangeBehavior::FULL,
            },
            title_cache: "Spectrogram".into(),
            gpu_state: PlotRenderState::default(),
            grid: SpectrogramGrid::default(),
        }
    }

    pub fn bind_traces(&mut self, traces: Vec<SpectrogramTrace>, cx: &mut Context<Self>) {
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
    /// component it names now — the inspector's rebind, after the tracker
    /// latched its component at start.
    pub fn rebind_trace(&mut self, trace: &Entity<SpectrogramTrace>, cx: &mut Context<Self>) {
        let id = trace.entity_id();
        self.tracking.remove(&id);
        self.tasks.remove(&id);
        cx.notify();
    }

    pub fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>) {
        if self.view_override.map(|b| b.bits()) != view.map(|b| b.bits()) {
            self.view_override = view;
            cx.notify();
        }
    }

    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    pub fn traces(&self) -> &[Entity<SpectrogramTrace>] {
        &self.traces
    }

    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    pub fn title(&self) -> SharedString {
        self.title_cache.clone()
    }

    /// The time window this pane actually uses: its own `Custom` range, or the
    /// app-wide [`GlobalTimeRange`] when set to `Auto`.
    pub fn resolved_x_range(&self, cx: &gpui::App) -> TimeRangeBehavior {
        match self.x_range.as_custom() {
            Some(behavior) => *behavior,
            None => GlobalTimeRange::get(cx),
        }
    }

    /// The source drawn this frame: the first visible one whose component has
    /// resolved. Two fields would occlude, so only one is ever drawn.
    fn active(&self, cx: &gpui::App) -> Option<(Entity<SpectrogramTrace>, &SourceTracking)> {
        self.traces.iter().find_map(|trace| {
            if !trace.read(cx).visible {
                return None;
            }
            let tracking = self.tracking.get(&trace.entity_id())?;
            tracking.component.as_ref()?;
            Some((trace.clone(), tracking))
        })
    }

    /// Earliest sample timestamp across resolved sources — the anchor the
    /// x-axis tick generator uses so labels don't crawl during a scroll.
    pub fn data_start(&self) -> Option<f64> {
        let mut start = f64::INFINITY;
        let mut any = false;
        for tracking in self.tracking.values() {
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

    /// The view the renderer will use this frame: X the resolved time window,
    /// Y the bin range with overrides applied.
    pub fn effective_view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        if let Some(v) = self.view_override {
            return Some(v);
        }
        let (trace, tracking) = self.active(cx)?;
        let len = trace.read(cx).len.max(1);
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut any = false;
        let mut extend = |comp: &Component| {
            if let Some(s) = comp.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any = true;
            }
            if let Some(l) = comp.time_series.latest() {
                end = end.max(l.timestamp().0 as f64);
                any = true;
            }
            // Remote-only history has no resident nodes; the manifest is what
            // lets a full-range view span the whole archive.
            let manifest = comp.time_series.manifest();
            if let Some(span) = manifest.spans.first() {
                start = start.min(span.seal.start_ts.0 as f64);
                any = true;
            }
            if let Some(span) = manifest.spans.last() {
                end = end.max(span.cover_end.0 as f64);
                any = true;
            }
        };
        if let Some(comp) = &tracking.component {
            extend(comp);
        }
        // An expression's component holds only what has been computed; what
        // *could* be is bounded by its inputs, and that is the window a
        // backfill fills.
        if let Some(plan) = &tracking.plan {
            for port in &plan.ports {
                extend(port);
            }
        }
        if !any || start >= end {
            return None;
        }
        let range = self
            .resolved_x_range(cx)
            .calculate_range(Timestamp(start as i64), Timestamp(end as i64));
        let (min_x, mut max_x) = (range.start.0 as f64, range.end.0 as f64);
        if min_x >= max_x {
            max_x = min_x + 1.0;
        }
        let min_y = self.y_min_override.as_custom().copied().unwrap_or(0.0);
        let max_y = self
            .y_max_override
            .as_custom()
            .copied()
            .unwrap_or(len as f64);
        Some(PlotBounds::new(min_x, min_y, max_x, max_y).normalize())
    }

    /// Bin count of the source drawn this frame — the Y axis's extent.
    pub fn bin_count(&self, cx: &gpui::App) -> usize {
        self.active(cx)
            .map(|(trace, _)| trace.read(cx).len)
            .unwrap_or(0)
    }

    /// The intensity scale in force, which is what makes a readout dB or bare.
    pub fn scale(&self, cx: &gpui::App) -> IntensityScale {
        self.active(cx)
            .map(|(trace, _)| trace.read(cx).scale)
            .unwrap_or_default()
    }

    /// The colormap in force, so the colorbar paints the ramp actually on
    /// screen rather than the default one.
    pub fn colormap(&self, cx: &gpui::App) -> Colormap {
        self.active(cx)
            .map(|(trace, _)| trace.read(cx).colormap)
            .unwrap_or_default()
    }

    /// Colour-mapping range actually used last frame, after overrides.
    pub fn intensity_range(&self) -> (f64, f64) {
        let lo = self
            .intensity_min
            .as_custom()
            .copied()
            .unwrap_or(self.grid.lo as f64);
        let hi = self
            .intensity_max
            .as_custom()
            .copied()
            .unwrap_or(self.grid.hi as f64);
        (lo, hi)
    }

    /// Read the painted grid at a plot-relative position, both fractions in
    /// `0..1` with `frac_y` measured from the bottom.
    pub(crate) fn sample_at(
        &self,
        frac_x: f32,
        frac_y: f32,
        cx: &gpui::App,
    ) -> Option<HoverSample> {
        let view = self.effective_view(cx)?;
        if self.grid.cols == 0 || self.grid.rows == 0 {
            return None;
        }
        let col = (frac_x.clamp(0.0, 0.999) * self.grid.cols as f32) as usize;
        let bin = view.min_y + (frac_y.clamp(0.0, 0.999) as f64) * view.height();
        if bin < 0.0 {
            return None;
        }
        let row = bin as usize;
        let value = self.grid.value_at(col, row)?;
        Some(HoverSample {
            ts: self.grid.column_time(col),
            bin: row,
            value,
        })
    }

    /// Decide raw versus LoD for the current window. Hysteresis keeps the
    /// boundary from flapping while panning near the budget; over budget the
    /// finest level whose element cost fits wins, falling back to the coarsest.
    fn update_lod_state(&mut self, cx: &gpui::App) {
        const ENTER: u64 = ELEMENT_SCAN_BUDGET + ELEMENT_SCAN_BUDGET / 4;
        const EXIT: u64 = ELEMENT_SCAN_BUDGET * 4 / 5;
        let Some(view) = self.effective_view(cx) else {
            return;
        };
        let range = Timestamp(view.min_x as i64)..Timestamp(view.max_x as i64);
        let vtable_gen = self.db.vtable_gen.latest();
        for trace in &self.traces {
            let cfg = trace.read(cx);
            if !cfg.visible {
                continue;
            }
            let Some(tracking) = self.tracking.get_mut(&trace.entity_id()) else {
                continue;
            };
            let Some(component) = tracking.component.as_ref() else {
                continue;
            };
            if tracking.lod_resolved_gen != Some(vtable_gen) {
                tracking.lod_levels = resolve_lod_levels(&self.db, cfg.source.id());
                tracking.lod_resolved_gen = Some(vtable_gen);
            }
            let len = cfg.len.max(1) as u64;
            let elements = component
                .time_series
                .estimate_samples(range.clone())
                .saturating_mul(len);
            if tracking.over_budget {
                tracking.over_budget = elements >= EXIT;
            } else {
                tracking.over_budget = elements > ENTER;
            }
            if !tracking.over_budget {
                tracking.lod_selected = None;
                continue;
            }
            let mut selected = None;
            for (i, lod) in tracking.lod_levels.iter().enumerate() {
                let has_data = lod.time_series.latest().is_some()
                    || !lod.time_series.manifest().spans.is_empty();
                if !has_data {
                    continue;
                }
                selected = Some(i);
                // An LoD sample carries both halves of the bucket, so its
                // element cost is twice the bin count.
                if lod
                    .time_series
                    .estimate_samples(range.clone())
                    .saturating_mul(len * 2)
                    <= ELEMENT_SCAN_BUDGET
                {
                    break;
                }
            }
            tracking.lod_selected = selected;
        }
    }

    /// Remote-only stretches of the visible window, as `(start, width)`
    /// fractions of the plot width, asking the installed hydrator for them on
    /// the way past. Over-budget sources hydrate their LoD companion rather
    /// than pulling the raw archive for a year-wide view.
    fn gap_bands(&self, cx: &gpui::App) -> smallvec::SmallVec<[(f32, f32); 4]> {
        let mut bands: smallvec::SmallVec<[(f32, f32); 4]> = smallvec::SmallVec::new();
        let Some(view) = self.effective_view(cx) else {
            return bands;
        };
        let Some((trace, tracking)) = self.active(cx) else {
            return bands;
        };
        let Some(component) = tracking.component.as_ref() else {
            return bands;
        };
        let component_id = trace.read(cx).source.id();
        let span = view.width().max(1.0);
        let visible = Timestamp(view.min_x as i64)..Timestamp(view.max_x as i64);
        let hydrator = crate::hydration::hydrator(cx);
        let mut gaps = metor_db::manifest::GapVec::new();

        let (series, hydrate_id, hydrate) = match tracking.selected_lod() {
            Some(lod) => (&lod.time_series, lod.component_id, true),
            None if tracking.over_budget => (&component.time_series, component_id, false),
            None => (&component.time_series, component_id, true),
        };
        series.coverage(visible.clone(), &mut gaps);
        let mut band = |range: &std::ops::Range<Timestamp>| {
            let start = ((range.start.0 as f64 - view.min_x) / span).clamp(0.0, 1.0) as f32;
            let end = ((range.end.0 as f64 - view.min_x) / span).clamp(0.0, 1.0) as f32;
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
        // What an expression has never computed is a gap of the other kind:
        // nothing to fetch, something to compute.
        let history = crate::data_binding::BoundHistory {
            component: component.clone(),
            plan: tracking.plan.clone(),
        };
        for range in history.request_replay(visible.clone(), cx) {
            band(&range);
        }
        bands
    }

    fn reconcile(&mut self, cx: &mut Context<Self>) {
        for trace in &self.traces {
            trace.update(cx, |trace, cx| {
                trace.source.resolve(&self.db, cx);
                if let Some(len) = self.db.with_state(|s| {
                    s.get_component(trace.source.id())
                        .map(|c| c.schema.dim.iter().product::<usize>().max(1))
                }) {
                    trace.len = len;
                }
            });
        }

        // Point each source back at this plot so its inspector page can reach
        // the plot to rebind. Mutating without notifying cannot re-enter
        // reconcile: the plot does not observe its traces.
        let self_weak = cx.entity().downgrade();
        let self_id = cx.entity().entity_id();
        for trace in &self.traces {
            trace.update(cx, |t, _| {
                let linked = t
                    .plot
                    .as_ref()
                    .and_then(|w| w.upgrade())
                    .map(|e| e.entity_id());
                if linked != Some(self_id) {
                    t.plot = Some(self_weak.clone());
                }
            });
        }

        for id in self.inputs.changed(
            &self.traces,
            &self.db,
            |trace| vec![(trace.source.id(), trace.len)],
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
                    SourceTracking::new(),
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
            Override::Auto => derive_title(&self.traces, cx),
        };
    }

    /// Follow one source: resolve its component, then repaint whenever raw
    /// samples or LoD buckets land. All the reading happens in prepaint, so
    /// the task itself only has to wake the view.
    fn spawn_tracker(
        id: EntityId,
        trace: Entity<SpectrogramTrace>,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let component_id = match this.update(cx, |_, cx| trace.read(cx).source.id()) {
                Ok(id) => id,
                Err(_) => return,
            };
            let component = wait_for_component(&db, component_id).await;
            let installed = this.update(cx, |plot, cx| {
                let plan = crate::dynamic::expressions::replay_plan(component_id, &db, cx);
                if let Some(tracking) = plot.tracking.get_mut(&id) {
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
                let lod = this.update(cx, |plot, _| {
                    plot.tracking
                        .get(&id)
                        .and_then(|t| t.selected_lod().cloned())
                });
                let Ok(lod) = lod else { break };
                // Hydrated LoD buckets wake the same as live raw data, so a
                // static view of pure history still repaints as they land.
                match &lod {
                    Some(lod) => {
                        futures_lite::future::race(
                            component.time_series.wait(),
                            lod.time_series.wait(),
                        )
                        .await
                    }
                    None => component.time_series.wait().await,
                }
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
    }
}

impl Render for SpectrogramPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        self.update_lod_state(cx);
        let gap_bands = self.gap_bands(cx);
        let weak = cx.entity().downgrade();
        let field_canvas = canvas(
            move |bounds, window, cx| {
                let scale_factor = window.scale_factor();
                let (frame, released) = weak
                    .update(cx, |plot, cx| {
                        plot.submit_frame(bounds, scale_factor, cx);
                        (
                            plot.gpu_state.current_frame(),
                            plot.gpu_state.take_pending_release(),
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
            .child(field_canvas)
            .children(gap_bands.into_iter().map({
                let band_color = theme.plot_gap_band;
                move |(start, width)| {
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(gpui::relative(start))
                        .w(gpui::relative(width))
                        .bg(band_color)
                }
            }))
    }
}

impl SpectrogramPlot {
    /// Rebuild the grid for the visible window and hand it to the GPU.
    fn submit_frame(&mut self, bounds: Bounds<Pixels>, scale_factor: f32, cx: &mut Context<Self>) {
        let Some(view) = self.effective_view(cx) else {
            return;
        };
        let Some((trace, tracking)) = self.active(cx) else {
            return;
        };
        let cfg = trace.read(cx);
        let (len, scale, colormap, gain) = (cfg.len, cfg.scale, cfg.colormap, cfg.gain);
        let lod = tracking.selected_lod().cloned();
        let over_budget = tracking.over_budget;
        let Some(component) = tracking.component.clone() else {
            return;
        };
        // Over budget with no published level yet: bucketing the raw window
        // would cost `samples × bins` reads on the paint thread. The gap bands
        // already say the history is not resident, and the level lands soon
        // after — a stalled frame would be the worse answer.
        if over_budget && lod.is_none() {
            return;
        }

        let cols =
            ((f32::from(bounds.size.width) * scale_factor.max(1.0)) as usize).clamp(1, MAX_COLS);
        let range = Timestamp(view.min_x as i64)..Timestamp(view.max_x as i64);
        let built = match &lod {
            Some(lod) => grid::build_grid_from_lod(lod, len, range, cols, scale, &mut self.grid),
            None => grid::build_grid(&component, len, range, cols, scale, &mut self.grid),
        };
        if !built {
            return;
        }

        let (lo, hi) = self.intensity_range();
        let theme = crate::theme::theme(cx);
        let draw = IntensityDraw {
            grid: &self.grid.values,
            cols: self.grid.cols as u32,
            rows: self.grid.rows as u32,
            lo: lo as f32,
            hi: hi as f32,
            gain,
            // The tone curve is folded in while bucketing, so the shader maps
            // the already-scaled values straight through.
            scale: IntensityScale::Linear,
            colormap,
            trace_color: theme.line_color,
            row_view: (view.min_y as f32, view.max_y as f32),
        };
        if let Some(handle) =
            self.gpu_state
                .render_with_field(cx, bounds, scale_factor, view, &[], Some(&draw))
        {
            handle.spawn_and_set(cx, spectrogram_gpu_state);
        }
    }
}

fn spectrogram_gpu_state(plot: &mut SpectrogramPlot) -> &mut PlotRenderState {
    &mut plot.gpu_state
}

fn derive_title(traces: &[Entity<SpectrogramTrace>], cx: &gpui::App) -> SharedString {
    let Some(first) = traces.first() else {
        return "Spectrogram".into();
    };
    let label = first.read(cx).label.clone();
    if label.is_empty() {
        "Spectrogram".into()
    } else {
        label
    }
}
