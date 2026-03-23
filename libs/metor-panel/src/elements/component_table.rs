use std::fmt::Write;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AppContext, AsyncApp, Bounds, Context, Entity, IntoElement, Pixels, SharedString, Window,
    canvas, div, point, prelude::*, px, uniform_list,
};
use metor_db::{Component, DB};
use metor_proto::types::Timestamp;

use super::time_series::{PlotBounds, compute_y_bounds, paint_data_line};
use crate::{ComponentStream, WalComponentStream, theme::DARK};

const ROW_HEIGHT: f32 = 60.0;
const NAME_WIDTH: f32 = 280.0;
const VALUE_WIDTH: f32 = 320.0;

struct ComponentRow {
    name: String,
    component: Component,
    y_bounds: Option<(f64, f64)>,
    _task: gpui::Task<()>,
}

impl ComponentRow {
    pub fn new(name: String, component: Component, cx: &mut Context<Self>) -> Self {
        let mut stream = WalComponentStream::new(&component);
        let task = cx.spawn(async move |this, cx| {
            loop {
                let _ = stream.next().await;
                let result = this.update(cx, |this, cx| {
                    this.y_bounds = compute_y_bounds(&this.component);
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }
        });
        Self {
            name,
            component,
            y_bounds: None,
            _task: task,
        }
    }

    fn current_value(&self) -> String {
        let Some(latest) = self.component.time_series.latest() else {
            return String::new();
        };
        let buf = latest.data();
        let Ok((_size, view)) = self.component.schema.parse_value(buf) else {
            return String::new();
        };
        let mut s = String::new();
        let _ = write!(s, "{:.4}", view);
        s
    }

    fn sparkline_bounds(&self) -> Option<PlotBounds> {
        let ts = &self.component.time_series;
        let start = ts.start_timestamp()?.0 as f64;
        let end = ts.latest()?.timestamp().0 as f64;
        if start == end {
            return None;
        }
        let (min_y, max_y) = self.y_bounds?;
        Some(PlotBounds::new(start, min_y, end, max_y).normalize())
    }
}

pub struct ComponentTable {
    db: Arc<DB>,
    rows: Vec<Entity<ComponentRow>>,
    _task: gpui::Task<()>,
}

impl ComponentTable {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let task = Self::spawn_watcher(db.clone(), cx);
        Self {
            db,
            rows: Vec::new(),
            _task: task,
        }
    }

    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let rows = Self::build_rows(&db, cx);
                let result = this.update(cx, |this, cx| {
                    this.rows = rows;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn build_rows(db: &DB, cx: &mut AsyncApp) -> Vec<Entity<ComponentRow>> {
        db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter_map(|(id, meta)| {
                    let name = meta.name.clone();
                    let component = state.get_component(*id)?.clone();
                    Some(cx.new(|cx| ComponentRow::new(name, component, cx)).ok()?)
                })
                .collect()
        })
    }
}

impl Render for ComponentRow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let component = self.component.clone();
        let plot_bounds = self.sparkline_bounds();
        let value = self.current_value();
        let color = DARK.line_color;
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(ROW_HEIGHT))
            .border_b_1()
            .border_color(DARK.border_primary)
            .child(
                div()
                    .w(px(NAME_WIDTH))
                    .px(px(12.0))
                    .text_size(px(13.0))
                    .text_color(DARK.text_primary)
                    .child(SharedString::from(self.name.clone())),
            )
            .child(
                div()
                    .w(px(VALUE_WIDTH))
                    .px(px(8.0))
                    .text_size(px(13.0))
                    .text_color(DARK.text_primary)
                    .child(SharedString::from(value)),
            )
            .child(
                canvas(
                    move |bounds, _window, _cx| (bounds, component, plot_bounds),
                    move |_, (bounds, component, _), window, _cx| {
                        if let Some(view) = plot_bounds {
                            paint_data_line(bounds, &component, &view, color, px(1.0), window);
                        }
                    },
                )
                .flex_1()
                .h(px(ROW_HEIGHT - 8.0)),
            )
    }
}

impl Render for ComponentTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.rows.len();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(DARK.bg_primary)
            .child(
                // Header row
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(32.0))
                    .border_b_1()
                    .border_color(DARK.border_primary)
                    .child(
                        div()
                            .w(px(NAME_WIDTH))
                            .px(px(12.0))
                            .text_size(px(12.0))
                            .text_color(DARK.text_tertiary)
                            .child("Name"),
                    )
                    .child(
                        div()
                            .w(px(VALUE_WIDTH))
                            .px(px(8.0))
                            .text_size(px(12.0))
                            .text_color(DARK.text_tertiary)
                            .child("Value"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px(px(8.0))
                            .text_size(px(12.0))
                            .text_color(DARK.text_tertiary)
                            .child("Sparkline"),
                    ),
            )
            .child(
                uniform_list(
                    "component-rows",
                    row_count,
                    cx.processor(
                        |this: &mut Self, range: Range<usize>, _window, _cx| {
                            this.rows[range]
                                .iter()
                                .cloned()
                                .map(|row: Entity<ComponentRow>| row.into_any_element())
                                .collect()
                        },
                    ),
                )
                .flex_1(),
            )
    }
}
