//! GPU-backed XY plot entity. Sibling to [`crate::views::time_series::LinePlot`].
//!
//! Owns the canonical trace list, view-override fields, the
//! [`PlotRenderState`] (reused as-is from the time-series module), and one
//! tracking task per trace. Each trace's tracker waits for both its X and
//! Y components, then loops scanning per-axis bounds so auto-fit reflects
//! the latest data without re-uploading on every tick.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, IntoElement, Pixels, Render, SharedString, Window,
    canvas, prelude::*,
};
use metor_db::{Component, DB};

use crate::views::plot_common::reconcile_trackers;
use crate::views::time_series::{
    AxisSource, LineDraw, NodeBoundsCache, Override, PlotBounds, PlotRenderState,
    expand_value_bounds,
};
use crate::wait_for_component;

use super::XyTrace;

/// Background state for one XY trace, keyed by the trace's [`EntityId`]
/// so reordering `traces` doesn't invalidate resolved data.
struct XyTraceTracking {
    x_component: Option<Component>,
    y_component: Option<Component>,
    x_history: Option<crate::data_binding::BoundHistory>,
    y_history: Option<crate::data_binding::BoundHistory>,
    x_bounds: Option<(f64, f64)>,
    y_bounds: Option<(f64, f64)>,
    /// Per-axis caches keyed by node identity. Reset when the cached
    /// element index for that axis changes.
    x_node_bounds: NodeBoundsCache,
    y_node_bounds: NodeBoundsCache,
    cached_x_element_index: Option<usize>,
    cached_y_element_index: Option<usize>,
}

impl XyTraceTracking {
    fn new() -> Self {
        Self {
            x_component: None,
            y_component: None,
            x_history: None,
            y_history: None,
            x_bounds: None,
            y_bounds: None,
            x_node_bounds: NodeBoundsCache::default(),
            y_node_bounds: NodeBoundsCache::default(),
            cached_x_element_index: None,
            cached_y_element_index: None,
        }
    }
}

/// Previous values of the reflected override knobs. Compared on each
/// reconcile so an inspector edit can clear the interactive view override.
#[derive(Clone, Copy, PartialEq)]
struct OverrideSnapshot {
    x_min: Option<f64>,
    x_max: Option<f64>,
    y_min: Option<f64>,
    y_max: Option<f64>,
}

impl OverrideSnapshot {
    fn capture(lp: &XyLinePlot) -> Self {
        Self {
            x_min: lp.x_min_override.as_custom().copied(),
            x_max: lp.x_max_override.as_custom().copied(),
            y_min: lp.y_min_override.as_custom().copied(),
            y_max: lp.y_max_override.as_custom().copied(),
        }
    }
}

/// Self-contained XY plot entity.
#[derive(facet::Facet)]
pub struct XyLinePlot {
    pub traces: Vec<Entity<XyTrace>>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    pub custom_title: Override<SharedString>,

    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, XyTraceTracking>,
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
}

impl XyLinePlot {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        cx.observe_self(Self::reconcile).detach();
        Self {
            traces: Vec::new(),
            x_min_override: Override::Auto,
            x_max_override: Override::Auto,
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            custom_title: Override::Auto,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
            inputs: Default::default(),
            view_override: None,
            last_overrides: OverrideSnapshot {
                x_min: None,
                x_max: None,
                y_min: None,
                y_max: None,
            },
            title_cache: "XY Plot".into(),
            gpu_state: PlotRenderState::default(),
        }
    }

    pub fn bind_traces(&mut self, traces: Vec<XyTrace>, cx: &mut Context<Self>) {
        self.traces = traces
            .into_iter()
            .map(|mut t| {
                t.x_source.resolve(&self.db, cx);
                t.y_source.resolve(&self.db, cx);
                cx.new(|_| t)
            })
            .collect();
        cx.notify();
    }

    /// Forget what `trace` was tracking, so the next reconcile follows the
    /// component it names now — the inspector's rebind, after the tracker
    /// latched its component at start.
    pub fn rebind_trace(&mut self, trace: &Entity<XyTrace>, cx: &mut Context<Self>) {
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

    fn reconcile(&mut self, cx: &mut Context<Self>) {
        for trace in &self.traces {
            trace.update(cx, |trace, cx| {
                trace.x_source.resolve(&self.db, cx);
                trace.y_source.resolve(&self.db, cx);
            });
        }

        // Point each trace back at this plot so its inspector page can reach
        // the plot to rebind. Mutating without notifying cannot re-enter
        // reconcile: the plot does not observe its traces.
        let self_weak = cx.entity().downgrade();
        let self_id = cx.entity().entity_id();
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
            });
        }

        for id in self.inputs.changed(
            &self.traces,
            &self.db,
            |trace| {
                vec![
                    (trace.x_source.id(), trace.x_element_index),
                    (trace.y_source.id(), trace.y_element_index),
                ]
            },
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
                    XyTraceTracking::new(),
                    Self::spawn_tracker(id, trace.clone(), db.clone(), cx),
                )
            },
        );

        let snapshot = OverrideSnapshot::capture(self);
        if snapshot != self.last_overrides {
            self.view_override = None;
            self.last_overrides = snapshot;
        }

        self.title_cache = match &self.custom_title {
            Override::Custom(custom) => custom.clone(),
            Override::Auto => derive_title(&self.traces, cx),
        };
    }

    /// The bounds the renderer will use this frame.
    ///
    /// Returns the interactive pan/zoom override when set; otherwise
    /// auto-fits against the visible traces, then applies any
    /// inspector-exposed bound overrides on top. Hidden traces are
    /// skipped so legend toggles re-fit the remaining trace. Returns
    /// `None` when either axis is missing data — partial bounds would
    /// paint chrome with the wrong range until the second axis settles.
    pub fn effective_view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        if let Some(v) = self.view_override {
            return Some(v);
        }
        let mut x_min = f64::INFINITY;
        let mut x_max = f64::NEG_INFINITY;
        let mut y_min = f64::INFINITY;
        let mut y_max = f64::NEG_INFINITY;
        let mut any_x = false;
        let mut any_y = false;
        for trace in &self.traces {
            if !trace.read(cx).visible {
                continue;
            }
            let Some(tracking) = self.tracking.get(&trace.entity_id()) else {
                continue;
            };
            if let Some((lo, hi)) = tracking.x_bounds {
                x_min = x_min.min(lo);
                x_max = x_max.max(hi);
                any_x = true;
            }
            if let Some((lo, hi)) = tracking.y_bounds {
                y_min = y_min.min(lo);
                y_max = y_max.max(hi);
                any_y = true;
            }
        }
        if !(any_x && any_y) {
            return None;
        }
        let min_x = self.x_min_override.as_custom().copied().unwrap_or(x_min);
        let max_x = self.x_max_override.as_custom().copied().unwrap_or(x_max);
        let min_y = self.y_min_override.as_custom().copied().unwrap_or(y_min);
        let max_y = self.y_max_override.as_custom().copied().unwrap_or(y_max);
        Some(PlotBounds::new(min_x, min_y, max_x, max_y).normalize())
    }

    pub fn traces(&self) -> &[Entity<XyTrace>] {
        &self.traces
    }

    pub fn trace_count(&self) -> usize {
        self.traces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    pub fn title(&self) -> SharedString {
        self.title_cache.clone()
    }

    fn spawn_tracker(
        id: EntityId,
        trace: Entity<XyTrace>,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let (x_id, y_id) = match this.update(cx, |_, cx| {
                let t = trace.read(cx);
                (t.x_source.id(), t.y_source.id())
            }) {
                Ok(ids) => ids,
                Err(_) => return,
            };

            // Sequential awaits are fine here: components are typically
            // already present in the DB by the time a trace is created
            // (the wizard picks from a populated list), so neither wait
            // actually blocks past the first poll.
            let x_component = wait_for_component(&db, x_id).await;
            let y_component = wait_for_component(&db, y_id).await;

            let installed = this.update(cx, |lp, cx| {
                if let Some(tracking) = lp.tracking.get_mut(&id) {
                    tracking.x_component = Some(x_component.clone());
                    tracking.y_component = Some(y_component.clone());
                    let trace = trace.read(cx);
                    tracking.x_history =
                        crate::data_binding::BoundHistory::for_binding(&trace.x_source, &db, cx);
                    tracking.y_history =
                        crate::data_binding::BoundHistory::for_binding(&trace.y_source, &db, cx);
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
                    let x_comp = tracking.x_component.clone()?;
                    let y_comp = tracking.y_component.clone()?;
                    let t = trace.read(cx);
                    let x_idx = t.x_element_index;
                    let y_idx = t.y_element_index;
                    if tracking.cached_x_element_index != Some(x_idx) {
                        tracking.x_node_bounds.clear();
                        tracking.x_bounds = None;
                        tracking.cached_x_element_index = Some(x_idx);
                    }
                    if tracking.cached_y_element_index != Some(y_idx) {
                        tracking.y_node_bounds.clear();
                        tracking.y_bounds = None;
                        tracking.cached_y_element_index = Some(y_idx);
                    }
                    let x_cache = std::mem::take(&mut tracking.x_node_bounds);
                    let y_cache = std::mem::take(&mut tracking.y_node_bounds);
                    Some((x_comp, y_comp, x_idx, y_idx, x_cache, y_cache))
                });
                let Ok(Some((x_comp, y_comp, x_idx, y_idx, mut x_cache, mut y_cache))) = inputs
                else {
                    break;
                };

                let (x_bounds, x_cache, y_bounds, y_cache) = cx
                    .background_executor()
                    .spawn(async move {
                        let xb = expand_value_bounds(&x_comp, &[x_idx], &mut x_cache);
                        let yb = expand_value_bounds(&y_comp, &[y_idx], &mut y_cache);
                        (xb, x_cache, yb, y_cache)
                    })
                    .await;

                let installed = this.update(cx, |lp, cx| {
                    let Some(tracking) = lp.tracking.get_mut(&id) else {
                        return false;
                    };
                    if tracking.cached_x_element_index == Some(x_idx) {
                        tracking.x_node_bounds = x_cache;
                        tracking.x_bounds = x_bounds;
                    }
                    if tracking.cached_y_element_index == Some(y_idx) {
                        tracking.y_node_bounds = y_cache;
                        tracking.y_bounds = y_bounds;
                    }
                    cx.notify();
                    true
                });
                if !matches!(installed, Ok(true)) {
                    break;
                }
                // Either axis can hydrate first or receive data independently.
                let mut x_wait = std::pin::pin!(x_component.time_series.wait());
                let mut y_wait = std::pin::pin!(y_component.time_series.wait());
                std::future::poll_fn(|cx| {
                    use std::future::Future;
                    if x_wait.as_mut().poll(cx).is_ready() || y_wait.as_mut().poll(cx).is_ready() {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
            }
        })
    }
}

impl Render for XyLinePlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let range = crate::temporal::snapshot(cx).and_then(|snapshot| snapshot.range);
        if let Some(range) = &range {
            for trace in &self.traces {
                if !trace.read(cx).visible {
                    continue;
                }
                if let Some(tracking) = self.tracking.get(&trace.entity_id()) {
                    for history in [&tracking.x_history, &tracking.y_history]
                        .into_iter()
                        .flatten()
                    {
                        history.request(range.clone(), cx);
                    }
                }
            }
        }
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
                                    let x_component = tracking.x_component.as_ref()?;
                                    let y_component = tracking.y_component.as_ref()?;
                                    Some(LineDraw {
                                        x: AxisSource::Element {
                                            component: x_component,
                                            element_index: config.x_element_index,
                                        },
                                        y: AxisSource::Element {
                                            component: y_component,
                                            element_index: config.y_element_index,
                                        },
                                        y_min: view.min_y,
                                        y_max: view.max_y,
                                        style: config.style,
                                        color: config.color,
                                        stroke_width: config.stroke_width,
                                        x_clip: None,
                                        time_clip: range.as_ref().map(|r| (r.start.0, r.end.0)),
                                    })
                                })
                                .collect();
                            if !draws.is_empty()
                                && let Some(handle) =
                                    lp.gpu_state.render(cx, bounds, scale_factor, view, &draws)
                            {
                                handle.spawn_and_set(cx, xy_plot_gpu_state);
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

fn xy_plot_gpu_state(lp: &mut XyLinePlot) -> &mut PlotRenderState {
    &mut lp.gpu_state
}

/// Derive a plot title from the trace list.
///
/// One trace renders as `<x_label> vs <y_label>` (using the trace's
/// `label` field if non-empty, else the component name suffix). Multiple
/// traces fall back to a generic count. Empty list yields "XY Plot".
fn derive_title(traces: &[Entity<XyTrace>], cx: &gpui::App) -> SharedString {
    if traces.is_empty() {
        return "XY Plot".into();
    }
    if traces.len() == 1 {
        let t = traces[0].read(cx);
        return if t.label.is_empty() {
            "XY Plot".into()
        } else {
            t.label.clone()
        };
    }
    SharedString::from(format!("XY Plot ({} traces)", traces.len()))
}
