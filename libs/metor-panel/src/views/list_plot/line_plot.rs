//! GPU-backed list-plot entity. Mirrors [`crate::views::xy_plot::XyLinePlot`]
//! but the per-trace tracker is much simpler: each tick just rescans the
//! latest sample, so there's no node-bounds cache and no per-element
//! invalidation.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, IntoElement, Pixels, Render, SharedString, Window,
    canvas, prelude::*,
};
use metor_db::{Component, DB};

use crate::views::plot_common::reconcile_trackers;
use crate::views::time_series::{
    AxisSource, LineDraw, Override, PlotBounds, PlotRenderState, expand_latest_sample_bounds,
};
use crate::wait_for_component;

use super::ListTrace;

/// Background state for one list trace, keyed by [`EntityId`].
struct ListTraceTracking {
    component: Option<Component>,
    y_bounds: Option<(f64, f64)>,
}

impl ListTraceTracking {
    fn new() -> Self {
        Self {
            component: None,
            y_bounds: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct OverrideSnapshot {
    x_min: Option<f64>,
    x_max: Option<f64>,
    y_min: Option<f64>,
    y_max: Option<f64>,
}

impl OverrideSnapshot {
    fn capture(lp: &ListLinePlot) -> Self {
        Self {
            x_min: lp.x_min_override.as_custom().copied(),
            x_max: lp.x_max_override.as_custom().copied(),
            y_min: lp.y_min_override.as_custom().copied(),
            y_max: lp.y_max_override.as_custom().copied(),
        }
    }
}

/// Self-contained list plot entity.
#[derive(facet::Facet)]
pub struct ListLinePlot {
    pub traces: Vec<Entity<ListTrace>>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    pub custom_title: Override<SharedString>,

    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, ListTraceTracking>,
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

impl ListLinePlot {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        cx.observe_self(Self::reconcile).detach();
        Self {
            traces: Vec::new(),
            x_min_override: Override::Auto,
            x_max_override: Override::Auto,
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            custom_title: Override::Auto,
            db,
            tracking: HashMap::new(),
            tasks: HashMap::new(),
            view_override: None,
            last_overrides: OverrideSnapshot {
                x_min: None,
                x_max: None,
                y_min: None,
                y_max: None,
            },
            title_cache: "List Plot".into(),
            gpu_state: PlotRenderState::default(),
        }
    }

    pub fn bind_traces(&mut self, traces: Vec<ListTrace>, cx: &mut Context<Self>) {
        self.traces = traces.into_iter().map(|t| cx.new(|_| t)).collect();
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
        let db = self.db.clone();
        reconcile_trackers(
            &self.traces,
            &mut self.tracking,
            &mut self.tasks,
            |id, trace| {
                (
                    ListTraceTracking::new(),
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

    /// Auto-fits across visible traces. X is `[0, max_len - 1]`; Y comes
    /// from the latest-sample scan. Returns `None` until at least one
    /// trace has tracked data.
    pub fn effective_view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        if let Some(v) = self.view_override {
            return Some(v);
        }
        let mut max_len: usize = 0;
        let mut y_min = f64::INFINITY;
        let mut y_max = f64::NEG_INFINITY;
        let mut any = false;
        for trace in &self.traces {
            let t = trace.read(cx);
            if !t.visible {
                continue;
            }
            max_len = max_len.max(t.len);
            let Some(tracking) = self.tracking.get(&trace.entity_id()) else {
                continue;
            };
            if let Some((lo, hi)) = tracking.y_bounds {
                y_min = y_min.min(lo);
                y_max = y_max.max(hi);
                any = true;
            }
        }
        if !any || max_len == 0 {
            return None;
        }
        let x_min = self.x_min_override.as_custom().copied().unwrap_or(0.0);
        let x_max = self
            .x_max_override
            .as_custom()
            .copied()
            .unwrap_or((max_len.saturating_sub(1)) as f64);
        let min_y = self.y_min_override.as_custom().copied().unwrap_or(y_min);
        let max_y = self.y_max_override.as_custom().copied().unwrap_or(y_max);
        Some(PlotBounds::new(x_min, min_y, x_max, max_y).normalize())
    }

    pub fn traces(&self) -> &[Entity<ListTrace>] {
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
        trace: Entity<ListTrace>,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let component_id = match this.update(cx, |_, cx| trace.read(cx).component_id) {
                Ok(id) => id,
                Err(_) => return,
            };
            let component = wait_for_component(&db, component_id).await;
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
                    let len = trace.read(cx).len;
                    Some((comp, len))
                });
                let Ok(Some((comp, len))) = inputs else {
                    break;
                };

                let bounds = cx
                    .background_executor()
                    .spawn(async move { expand_latest_sample_bounds(&comp, len) })
                    .await;

                let installed = this.update(cx, |lp, cx| {
                    let Some(tracking) = lp.tracking.get_mut(&id) else {
                        return false;
                    };
                    tracking.y_bounds = bounds;
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

impl Render for ListLinePlot {
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
                                        x: AxisSource::LatestSampleIndex { len: config.len },
                                        y: AxisSource::LatestSampleElements {
                                            component,
                                            len: config.len,
                                        },
                                        y_min: view.min_y,
                                        y_max: view.max_y,
                                        style: config.style,
                                        color: config.color,
                                        stroke_width: config.stroke_width,
                                        x_clip: None,
                                    })
                                })
                                .collect();
                            if !draws.is_empty()
                                && let Some(handle) =
                                    lp.gpu_state.render(cx, bounds, scale_factor, view, &draws)
                            {
                                handle.spawn_and_set(cx, list_plot_gpu_state);
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

fn list_plot_gpu_state(lp: &mut ListLinePlot) -> &mut PlotRenderState {
    &mut lp.gpu_state
}

fn derive_title(traces: &[Entity<ListTrace>], cx: &gpui::App) -> SharedString {
    if traces.is_empty() {
        return "List Plot".into();
    }
    if traces.len() == 1 {
        let t = traces[0].read(cx);
        return if t.label.is_empty() {
            "List Plot".into()
        } else {
            t.label.clone()
        };
    }
    SharedString::from(format!("List Plot ({} traces)", traces.len()))
}
