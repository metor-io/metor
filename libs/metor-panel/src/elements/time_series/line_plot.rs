//! Reusable line-plot element.
//!
//! [`LinePlot`] is the single gpui entity every plot site in metor-panel
//! embeds to actually draw lines. It owns:
//!
//! - the canonical list of trace entities and their reflected
//!   view-override knobs (`x_range`, y-axis overrides, `custom_title`);
//! - a keyed map of per-trace tracking state (resolved [`Component`],
//!   incrementally scanned y-bounds, last-scan watermark);
//! - the [`PlotRenderState`] that drives the GPU renderer;
//! - one tracking task per trace keyed by [`gpui::EntityId`], spawned
//!   and dropped as the `traces` Vec changes.
//!
//! Parents (`Monitor`, `ComponentRow`, `TimeSeriesPlot`) construct one
//! `Entity<LinePlot>`, push traces via [`bind_traces`], and embed
//! `self.sparkline.clone()` as a child in their render tree. They do
//! not touch canvas closures, GPU state, or y-bounds math directly.
//!
//! Mutations to the reflected fields (including traces) go through
//! `cx.notify`, which triggers [`Self::reconcile`] — this is where new
//! trace ids get trackers spawned, removed ids get trackers dropped,
//! and scalar changes invalidate the interactive view override.

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
use crate::offset_parse::TimeRangeBehavior;
use crate::wait_for_component;

/// Per-trace tracking state keyed by the trace entity's [`EntityId`].
/// Independent of the position in `LinePlot::traces`, so reordering or
/// inserting traces does not invalidate already-resolved components or
/// y-bounds watermarks.
struct TraceTracking {
    component: Option<Component>,
    y_bounds: Option<(f64, f64)>,
    /// Per-node (min, max) cache owned by the tracker. Taken out on each
    /// background scan and put back with the result; invalidated when
    /// `cached_element_index` changes.
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

/// Snapshot of the scalar view-override fields used to detect edits
/// between reconciles (so inspector writes reset `view_override`).
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

/// A self-contained GPU-accelerated line plot. Holds its own render
/// state, trace list, per-trace y-bounds trackers, and view-override
/// knobs. Implements [`Render`] so parents just `.child(entity.clone())`
/// it into their own trees.
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

    /// Replace all traces from raw [`Trace`] values. Used by `Monitor`
    /// and `ComponentRow` which don't need entity ownership.
    pub fn bind_traces(&mut self, traces: Vec<Trace>, cx: &mut Context<Self>) {
        self.traces = traces.into_iter().map(|t| cx.new(|_| t)).collect();
        cx.notify();
    }

    pub fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>) {
        if self.view_override.map(bounds_tuple) != view.map(bounds_tuple) {
            self.view_override = view;
            cx.notify();
        }
    }

    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// Reconcile `tracking` + `tasks` with the current `traces` Vec,
    /// invalidate the title cache, and clear the interactive view
    /// override if a scalar knob changed. Runs on every self-notify.
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

    /// The view currently used for rendering: the interactive override
    /// if set, otherwise auto-fit from trace data + inspector overrides.
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

    /// The earliest sample timestamp across resolved traces. Used by
    /// `TimeSeriesPlot`'s chrome canvases to offset x-axis labels.
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

    /// The display title: custom title if set, otherwise derived from
    /// the current trace list. Recomputed only when traces or
    /// `custom_title` change (via [`Self::reconcile`]).
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
                    // Guard against element_index churn during the scan.
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

/// `PlotBounds` doesn't implement `PartialEq` (it holds `f64`s), so the
/// setters compare via a tuple conversion for change detection.
fn bounds_tuple(b: PlotBounds) -> (u64, u64, u64, u64) {
    (
        b.min_x.to_bits(),
        b.min_y.to_bits(),
        b.max_x.to_bits(),
        b.max_y.to_bits(),
    )
}

/// Group traces by component and format as `"comp"` (all elements) or
/// `"comp[x,y]"` (subset). Multiple components join with `", "`.
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
            let all_elements = crate::trace_picker::element_names_for_component(db, *comp_id);
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
