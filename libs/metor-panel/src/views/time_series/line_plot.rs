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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, IntoElement, Pixels, Render, SharedString, Window,
    canvas, prelude::*,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use super::bounds::PlotBounds;
use super::gpu::{LineDraw, PlotRenderState};
use super::override_field::Override;
use super::{NodeBoundsCache, Trace, expand_y_bounds};
use crate::views::time_series::time_range::TimeRangeBehavior;
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
#[derive(Clone, Copy, PartialEq)]
struct OverrideSnapshot {
    y_min: Option<f64>,
    y_max: Option<f64>,
    x_range: TimeRangeBehavior,
}

impl OverrideSnapshot {
    fn capture(lp: &LinePlot) -> Self {
        Self {
            y_min: lp.y_min_override.as_custom().copied(),
            y_max: lp.y_max_override.as_custom().copied(),
            x_range: lp.x_range,
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
    pub x_range: TimeRangeBehavior,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    pub custom_title: Override<SharedString>,

    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, TraceTracking>,
    #[facet(opaque)]
    tasks: HashMap<EntityId, gpui::Task<()>>,
    #[facet(opaque)]
    view_override: Option<PlotBounds>,
    #[facet(opaque)]
    last_overrides: OverrideSnapshot,
    #[facet(opaque)]
    title_cache: SharedString,
    #[facet(opaque)]
    gpu_state: PlotRenderState,
}

impl LinePlot {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        cx.observe_self(Self::reconcile).detach();
        Self {
            traces: Vec::new(),
            x_range: TimeRangeBehavior::default(),
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            custom_title: Override::Auto,
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
            view_override: None,
            last_overrides: OverrideSnapshot {
                y_min: None,
                y_max: None,
                x_range: TimeRangeBehavior::default(),
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
    /// the lack of `PartialEq` on `f64`-containing [`PlotBounds`].
    pub fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>) {
        if self.view_override.map(bounds_tuple) != view.map(bounds_tuple) {
            self.view_override = view;
            cx.notify();
        }
    }

    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// Bring tracking state and the title cache in sync with `self.traces`.
    ///
    /// Runs on every self-notify. Spawns a tracker for each new trace,
    /// drops trackers for removed traces, and resets the pan/zoom override
    /// when an inspector edit changes the reflected view knobs.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let current_ids: HashSet<EntityId> = self.traces.iter().map(|e| e.entity_id()).collect();
        let had_tracking_keys: Vec<EntityId> = self.tracking.keys().copied().collect();
        for id in had_tracking_keys {
            if !current_ids.contains(&id) {
                self.tracking.remove(&id);
                self.tasks.remove(&id);
            }
        }
        for trace in &self.traces {
            let id = trace.entity_id();
            if !self.tracking.contains_key(&id) {
                self.tracking.insert(id, TraceTracking::new());
                let task = Self::spawn_tracker(id, trace.clone(), self.db.clone(), cx);
                self.tasks.insert(id, task);
            }
        }

        let snapshot = OverrideSnapshot::capture(self);
        if snapshot != self.last_overrides {
            self.view_override = None;
            self.last_overrides = snapshot;
        }

        self.title_cache = match &self.custom_title {
            Override::Custom(custom) => custom.clone(),
            Override::Auto => derive_title(&self.traces, &self.db, cx),
        };
    }

    /// The bounds the renderer will use this frame.
    ///
    /// Returns the interactive pan/zoom override when set; otherwise
    /// auto-fits against the visible traces, then applies any
    /// inspector-exposed bound overrides on top.
    pub fn effective_view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        if let Some(v) = self.view_override {
            return Some(v);
        }
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut y_min = f64::INFINITY;
        let mut y_max = f64::NEG_INFINITY;
        let mut any_time = false;
        let mut any_y = false;
        for trace in &self.traces {
            if !trace.read(cx).visible {
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
            if let Some((lo, hi)) = tracking.y_bounds {
                y_min = y_min.min(lo);
                y_max = y_max.max(hi);
                any_y = true;
            }
        }
        if !any_time || start >= end {
            return None;
        }
        let range = self
            .x_range
            .calculate_range(Timestamp(start as i64), Timestamp(end as i64));
        let (auto_min, auto_max) = if any_y { (y_min, y_max) } else { (0.0, 1.0) };
        let min_y = self.y_min_override.as_custom().copied().unwrap_or(auto_min);
        let max_y = self.y_max_override.as_custom().copied().unwrap_or(auto_max);
        Some(PlotBounds::new(range.start.0 as f64, min_y, range.end.0 as f64, max_y).normalize())
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
                        let bounds = expand_y_bounds(&comp, &[element_index], &mut cache);
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
        let weak = cx.entity().downgrade();
        canvas(
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
                                    Some(LineDraw {
                                        component_id: config.component_id,
                                        component,
                                        element_index: config.element_index,
                                        style: config.style,
                                        color: config.color,
                                        stroke_width: config.stroke_width,
                                    })
                                })
                                .collect();
                            if !draws.is_empty()
                                && let Some(handle) =
                                    lp.gpu_state.render(cx, bounds, scale_factor, view, &draws)
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
        .size_full()
    }
}

fn line_plot_gpu_state(lp: &mut LinePlot) -> &mut PlotRenderState {
    &mut lp.gpu_state
}

/// Convert [`PlotBounds`] to a bit-pattern tuple so `PartialEq` fields can
/// compare bounds despite the `f64` contents.
fn bounds_tuple(b: PlotBounds) -> (u64, u64, u64, u64) {
    (
        b.min_x.to_bits(),
        b.min_y.to_bits(),
        b.max_x.to_bits(),
        b.max_y.to_bits(),
    )
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
