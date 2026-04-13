//! Reusable line-plot element.
//!
//! [`LinePlot`] is the single gpui entity every plot site in metor-panel
//! embeds to actually draw lines. It owns:
//!
//! - the per-trace config, resolved [`Component`], and incrementally
//!   scanned y-bounds;
//! - the user-interactive view overrides ([`set_view_override`] for
//!   drag/pan, per-axis y-min/y-max overrides, [`TimeRangeBehavior`]);
//! - the [`PlotRenderState`] that drives the GPU renderer;
//! - one tracking task per trace that waits for the trace's component
//!   to appear in the DB and then grows its y-bounds incrementally as
//!   new samples arrive.
//!
//! Parents (`Monitor`, `ComponentRow`, `TimeSeriesPlot`) construct one
//! `Entity<LinePlot>`, push traces via [`bind_traces`], and embed
//! `self.sparkline.clone()` as a child in their render tree. They do
//! not touch canvas closures, GPU state, or y-bounds math directly.

use std::sync::Arc;

use gpui::{Bounds, Context, Corners, IntoElement, Pixels, Render, Window, canvas, prelude::*};
use metor_db::{Component, DB};
use metor_proto::types::Timestamp;

use super::bounds::PlotBounds;
use super::gpu::{LineDraw, PlotRenderState};
use super::{Trace, expand_y_bounds};
use crate::offset_parse::TimeRangeBehavior;
use crate::wait_for_component;

/// Per-trace state inside a [`LinePlot`]. `component` is `None` until
/// the tracker task's [`wait_for_component`] resolves.
struct Entry {
    config: gpui::Entity<Trace>,
    component: Option<Component>,
    y_bounds: Option<(f64, f64)>,
    last_scan_ts: Option<Timestamp>,
}

impl Entry {
    /// Build a borrowing [`LineDraw`] for the GPU submit, or `None` if
    /// the component hasn't resolved yet or the trace is hidden.
    fn as_line_draw(&self, cx: &gpui::App) -> Option<LineDraw<'_>> {
        let config = self.config.read(cx);
        if !config.visible {
            return None;
        }
        let component = self.component.as_ref()?;
        Some(LineDraw {
            component_id: config.component_id,
            component,
            element_index: config.element_index,
            style: config.style,
            color: config.color,
            stroke_width: config.stroke_width,
        })
    }
}

/// A self-contained GPU-accelerated line plot. Holds its own render
/// state, trace list, per-trace y-bounds trackers, and view-override
/// knobs. Implements [`Render`] so parents just `.child(entity.clone())`
/// it into their own trees.
pub struct LinePlot {
    entries: Vec<Entry>,
    view_override: Option<PlotBounds>,
    y_min_override: Option<f64>,
    y_max_override: Option<f64>,
    x_range: TimeRangeBehavior,
    gpu_state: PlotRenderState,
    /// One per trace, dropped and respawned on every [`bind_traces`].
    _tasks: Vec<gpui::Task<()>>,
}

impl LinePlot {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            view_override: None,
            y_min_override: None,
            y_max_override: None,
            x_range: TimeRangeBehavior::default(),
            gpu_state: PlotRenderState::default(),
            _tasks: Vec::new(),
        }
    }

    /// Replace all traces from raw Trace values. Creates Entity handles internally.
    /// Used by Monitor's sparkline which doesn't need entity ownership.
    pub fn bind_traces(&mut self, db: Arc<DB>, traces: Vec<Trace>, cx: &mut Context<Self>) {
        let entities: Vec<gpui::Entity<Trace>> =
            traces.into_iter().map(|t| cx.new(|_| t)).collect();
        self.bind_trace_entities(db, entities, cx);
    }

    /// Replace all traces from pre-existing Entity<Trace> handles and restart tracking tasks.
    pub fn bind_trace_entities(
        &mut self,
        db: Arc<DB>,
        trace_entities: Vec<gpui::Entity<Trace>>,
        cx: &mut Context<Self>,
    ) {
        self.entries = trace_entities
            .into_iter()
            .map(|config| Entry {
                config,
                component: None,
                y_bounds: None,
                last_scan_ts: None,
            })
            .collect();
        // Clear any interactive view override so the freshly-rebound
        // plot reverts to auto-fit until the user drags again.
        self.view_override = None;
        self._tasks = (0..self.entries.len())
            .map(|idx| Self::spawn_tracker(db.clone(), idx, cx))
            .collect();
        cx.notify();
    }

    /// Mutate one trace's config in place (style / color / visible /
    /// stroke_width / label). Does not touch the tracker task.
    pub fn update_trace<F>(&mut self, index: usize, f: F, cx: &mut Context<Self>)
    where
        F: FnOnce(&mut Trace),
    {
        if let Some(entry) = self.entries.get(index) {
            entry.config.update(cx, |config, _| f(config));
            cx.notify();
        }
    }

    pub fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>) {
        if self.view_override.map(bounds_tuple) != view.map(bounds_tuple) {
            self.view_override = view;
            cx.notify();
        }
    }

    pub fn set_y_min_override(&mut self, value: Option<f64>, cx: &mut Context<Self>) {
        if self.y_min_override != value {
            self.y_min_override = value;
            self.view_override = None;
            cx.notify();
        }
    }

    pub fn set_y_max_override(&mut self, value: Option<f64>, cx: &mut Context<Self>) {
        if self.y_max_override != value {
            self.y_max_override = value;
            self.view_override = None;
            cx.notify();
        }
    }

    pub fn set_x_range(&mut self, behavior: TimeRangeBehavior, cx: &mut Context<Self>) {
        if self.x_range != behavior {
            self.x_range = behavior;
            self.view_override = None;
            cx.notify();
        }
    }

    pub fn x_range(&self) -> TimeRangeBehavior {
        self.x_range
    }

    pub fn y_min_override(&self) -> Option<f64> {
        self.y_min_override
    }

    pub fn y_max_override(&self) -> Option<f64> {
        self.y_max_override
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
        for entry in &self.entries {
            if !entry.config.read(cx).visible {
                continue;
            }
            let Some(comp) = &entry.component else {
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
            if let Some((lo, hi)) = entry.y_bounds {
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
        let min_y = self.y_min_override.unwrap_or(auto_min);
        let max_y = self.y_max_override.unwrap_or(auto_max);
        Some(PlotBounds::new(range.start.0 as f64, min_y, range.end.0 as f64, max_y).normalize())
    }

    /// The earliest sample timestamp across resolved traces. Used by
    /// `TimeSeriesPlot`'s chrome canvases to offset x-axis labels.
    pub fn data_start(&self) -> Option<f64> {
        let mut start = f64::INFINITY;
        let mut any = false;
        for entry in &self.entries {
            let Some(comp) = &entry.component else {
                continue;
            };
            if let Some(s) = comp.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any = true;
            }
        }
        any.then_some(start)
    }

    /// Iterate over all trace entity handles, resolved or not.
    pub fn traces(&self) -> impl Iterator<Item = &gpui::Entity<Trace>> {
        self.entries.iter().map(|e| &e.config)
    }

    pub fn trace(&self, idx: usize) -> Option<&gpui::Entity<Trace>> {
        self.entries.get(idx).map(|e| &e.config)
    }

    pub fn trace_count(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn spawn_tracker(db: Arc<DB>, idx: usize, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let trace_id = match this.update(cx, |lp, cx| {
                lp.entries.get(idx).map(|e| e.config.read(cx).component_id)
            }) {
                Ok(Some(id)) => id,
                _ => return,
            };
            let component = wait_for_component(&db, trace_id).await;
            let installed = this.update(cx, |lp, cx| {
                if let Some(entry) = lp.entries.get_mut(idx) {
                    entry.component = Some(component.clone());
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
                let result = this.update(cx, |lp, cx| {
                    let Some(entry) = lp.entries.get_mut(idx) else {
                        return false;
                    };
                    let Some(comp) = entry.component.as_ref() else {
                        return false;
                    };
                    let latest_ts = comp.time_series.latest().map(|n| n.timestamp());
                    let element_index = entry.config.read(cx).element_index;
                    entry.y_bounds =
                        expand_y_bounds(comp, &[element_index], entry.y_bounds, entry.last_scan_ts);
                    entry.last_scan_ts = latest_ts;
                    cx.notify();
                    true
                });
                if !matches!(result, Ok(true)) {
                    break;
                }
                component.time_series.wait().await;
            }
        })
    }
}

impl Default for LinePlot {
    fn default() -> Self {
        Self::new()
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
                                .entries
                                .iter()
                                .filter_map(|e| e.as_line_draw(cx))
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
