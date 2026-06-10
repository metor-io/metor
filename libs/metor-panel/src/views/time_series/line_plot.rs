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
}

impl TraceTracking {
    fn new() -> Self {
        Self {
            component: None,
            y_bounds: None,
            node_bounds: NodeBoundsCache::default(),
            cached_element_index: None,
        }
    }
}

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
    tracking: HashMap<EntityId, TraceTracking>,
    #[facet(opaque)]
    tasks: HashMap<EntityId, gpui::Task<()>>,
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
            x_range: Override::Auto,
            x_time_format: TimeFormat::default(),
            custom_title: Override::Auto,
            show_alarm_limits: true,
            show_alarm_color: true,
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
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
        self.traces = traces.into_iter().map(|t| cx.new(|_| t)).collect();
        cx.notify();
    }

    /// Pin the view to `view`, or clear the override with `None`.
    ///
    /// Called by pan/zoom handlers. Uses a bit-pattern compare to dodge
    /// the lack of `PartialEq` on `f64`-containing [`PlotView`].
    pub fn set_view_override(&mut self, view: Option<PlotView>, cx: &mut Context<Self>) {
        let changed = self.view_override.as_ref().map(PlotView::bits)
            != view.as_ref().map(PlotView::bits);
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

    /// Bring tracking state and the title cache in sync with `self.traces`.
    ///
    /// Runs on every self-notify. Spawns a tracker for each new trace,
    /// drops trackers for removed traces, and resets the pan/zoom override
    /// when an inspector edit changes the reflected view knobs.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
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
            if let Some(s) = comp.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any_time = true;
            }
            if let Some(l) = comp.time_series.latest() {
                end = end.max(l.timestamp().0 as f64);
                any_time = true;
            }
            let ai = cfg.axis_index.min(axis_count - 1);
            if let Some((lo, hi)) = tracking.y_bounds {
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
            let lo = a.y_min_override.as_custom().copied().unwrap_or(auto_min);
            let mut hi = a.y_max_override.as_custom().copied().unwrap_or(auto_max);
            if lo >= hi {
                hi = lo + 1.0;
            }
            axes.push((lo, hi));
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
    /// `ts`. Returns `None` when the trace's component hasn't resolved yet,
    /// when its element index falls outside the schema, or when the
    /// component has no samples bracketing `ts`.
    ///
    /// Used by measurement-cursor rendering (dot marker per trace) and by
    /// the Δy / Mean readouts that need a value at the cursor endpoints.
    pub fn trace_value_at(&self, trace_id: EntityId, ts: Timestamp, cx: &gpui::App) -> Option<f64> {
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
        for node in component.time_series.list.iter() {
            let timestamps = node.timestamps();
            if timestamps.is_empty() {
                continue;
            }
            let idx = match timestamps.binary_search_by_key(&ts.0, |t| t.0) {
                Ok(i) => i,
                Err(i) => i,
            };
            let mut best: Option<(i64, usize)> = None;
            for cand_idx in [idx.saturating_sub(1), idx, idx + 1] {
                let Some(cand) = timestamps.get(cand_idx) else {
                    continue;
                };
                let d = (cand.0 - ts.0).abs();
                if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, cand_idx));
                }
            }
            let Some((_, sample_idx)) = best else {
                continue;
            };
            let data = node.data.data();
            let base = sample_idx * elem_size;
            let Some(buf) = data.get(base..base + elem_size) else {
                continue;
            };
            return Some(crate::dynamic::tensor::read_f64_at(
                buf,
                component.schema.prim_type,
                cfg.element_index,
            ));
        }
        None
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
            let trace_id = match this.update(cx, |_, cx| trace.read(cx).component_id) {
                Ok(id) => id,
                Err(_) => return,
            };
            let component = wait_for_component(&db, trace_id).await;
            let installed = this.update(cx, |lp, cx| {
                if let Some(tracking) = lp.tracking.get_mut(&id) {
                    tracking.component = Some(component.clone());
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
                        tracking.cached_element_index = Some(element_index);
                    }
                    let cache = std::mem::take(&mut tracking.node_bounds);
                    Some((comp, element_index, cache))
                });
                let Ok(Some((comp, element_index, mut cache))) = inputs else {
                    break;
                };

                let (bounds, cache) = cx
                    .background_executor()
                    .spawn(async move {
                        let bounds = expand_value_bounds(&comp, &[element_index], &mut cache);
                        (bounds, cache)
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
                    }
                    cx.notify();
                    true
                });
                if !matches!(installed, Ok(true)) {
                    break;
                }
                component.time_series.wait().await;
            }
        })
    }
}

impl Render for LinePlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let degraded = self.gpu_state.degraded();
        let weak = cx.entity().downgrade();
        let plot_canvas = canvas(
            move |bounds, window, cx| {
                let scale_factor = window.scale_factor();
                let (frame, released) = weak
                    .update(cx, |lp, cx| {
                        if let Some(view) = lp.effective_view(cx) {
                            let draws: Vec<LineDraw<'_>> = lp
                                .traces
                                .iter()
                                .filter_map(|trace| {
                                    let config = trace.read(cx);
                                    if !config.visible {
                                        return None;
                                    }
                                    let tracking = lp.tracking.get(&trace.entity_id())?;
                                    let component = tracking.component.as_ref()?;
                                    let axis = view.axis_bounds(config.axis_index);
                                    Some(LineDraw {
                                        x: AxisSource::Timestamps,
                                        y: AxisSource::Element {
                                            component_id: config.component_id,
                                            component,
                                            element_index: config.element_index,
                                        },
                                        y_min: axis.min_y,
                                        y_max: axis.max_y,
                                        style: config.style,
                                        color: config.color,
                                        stroke_width: config.stroke_width,
                                    })
                                })
                                .collect();
                            // X data + a 0..1 Y placeholder; each trace's Y is
                            // normalized per axis in the materializer.
                            let gpu_view = view.x_bounds();
                            if !draws.is_empty()
                                && let Some(handle) =
                                    lp.gpu_state.render(cx, bounds, scale_factor, gpu_view, &draws)
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
                        .text_color(crate::theme::DARK.text_tertiary)
                        .child("decimated"),
                )
            })
    }
}

fn line_plot_gpu_state(lp: &mut LinePlot) -> &mut PlotRenderState {
    &mut lp.gpu_state
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
        let id = t.component_id;
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
