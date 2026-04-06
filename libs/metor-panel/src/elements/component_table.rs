use std::sync::Arc;

use super::table::{Column, ColumnSort, Table, TableDelegate};
use super::time_series::{PlotBounds, expand_y_bounds, paint_data_line};
use crate::{ComponentStream, WalComponentStream, theme::theme};
use super::ElementIndexes;
use gpui::{
    AnyElement, App, AppContext, AsyncApp, Context, Entity, IntoElement, Pixels, SharedString,
    Window, canvas, div, prelude::*, px,
};
use metor_db::{Component, DB};

struct ComponentRow {
    db: Arc<DB>,
    name: SharedString,
    component: Component,
    indexes: ElementIndexes,
    y_bounds: Option<(f64, f64)>,
    last_scan_ts: Option<metor_proto::types::Timestamp>,
    _task: gpui::Task<()>,
}

impl ComponentRow {
    pub fn new(db: Arc<DB>, name: impl Into<SharedString>, component: Component, cx: &mut Context<Self>) -> Self {
        let num_elements: usize = component.schema.dim.iter().product();
        let indexes: ElementIndexes = (0..num_elements.max(1)).collect();
        let idx_clone = indexes.clone();
        let mut stream = WalComponentStream::new(&component);
        let task = cx.spawn(async move |this, cx| {
            loop {
                let _ = stream.next().await;
                let result = this.update(cx, |this, cx| {
                    let latest_ts = this.component.time_series.latest().map(|n| n.timestamp());
                    this.y_bounds = expand_y_bounds(
                        &this.component,
                        &idx_clone,
                        this.y_bounds,
                        this.last_scan_ts,
                    );
                    this.last_scan_ts = latest_ts;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }
        });
        Self {
            db,
            name: name.into(),
            component,
            indexes,
            y_bounds: None,
            last_scan_ts: None,
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
        super::format_value(view, &self.db, self.component.component_id)
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

/// Table delegate that displays all components in the database with name, value, and sparkline columns.
pub struct ComponentTableDelegate {
    rows: Vec<Entity<ComponentRow>>,
    _task: gpui::Task<()>,
}

impl ComponentTableDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Table<Self>>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let rows = Self::build_rows(&db, cx);
                let result = this.update(cx, |this, cx| {
                    this.delegate_mut().rows = rows;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn build_rows(db: &Arc<DB>, cx: &mut AsyncApp) -> Vec<Entity<ComponentRow>> {
        db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter_map(|(id, meta)| {
                    let name = meta.name.clone();
                    let component = state.get_component(*id)?.clone();
                    let db = db.clone();
                    Some(cx.new(|cx| ComponentRow::new(db, name, component, cx)).ok()?)
                })
                .collect()
        })
    }
}

impl TableDelegate for ComponentTableDelegate {
    fn columns(&self) -> Vec<Column> {
        vec![
            Column::new("Name", 280.0).sortable(),
            Column::new("Value", 320.0).sortable(),
            Column::new("Sparkline", 200.0).flex().resizable(false),
        ]
    }

    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn row_height(&self) -> Pixels {
        px(50.0)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let row = &self.rows[row_ix];
        let row_ref = row.read(cx);
        match col_ix {
            0 => div()
                .px(px(12.0))
                .text_size(px(13.0))
                .text_color(theme.text_primary)
                .child(row_ref.name.clone())
                .into_any_element(),
            1 => {
                let value = row_ref.current_value();
                div()
                    .px(px(8.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::from(value))
                    .into_any_element()
            }
            2 => {
                let component = row_ref.component.clone();
                let plot_bounds = row_ref.sparkline_bounds();
                let indexes = row_ref.indexes.clone();
                let row_height = self.row_height();
                let line_colors = theme.line_colors;
                canvas(
                    move |bounds, _window, _cx| (bounds, component, plot_bounds, indexes),
                    move |_, (bounds, component, _, indexes), window, _cx| {
                        if let Some(view) = plot_bounds {
                            for (i, &idx) in indexes.iter().enumerate() {
                                let color = line_colors[i % line_colors.len()];
                                paint_data_line(bounds, &component, &view, color, px(1.0), idx, window);
                            }
                        }
                    },
                )
                .w_full()
                .h(row_height - px(8.0))
                .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, cx: &App) {
        match (col_ix, sort) {
            (0, ColumnSort::Ascending) => {
                self.rows.sort_by(|a, b| a.read(cx).name.cmp(&b.read(cx).name));
            }
            (0, ColumnSort::Descending) => {
                self.rows.sort_by(|a, b| b.read(cx).name.cmp(&a.read(cx).name));
            }
            (1, ColumnSort::Ascending) => {
                self.rows.sort_by(|a, b| a.read(cx).current_value().cmp(&b.read(cx).current_value()));
            }
            (1, ColumnSort::Descending) => {
                self.rows.sort_by(|a, b| b.read(cx).current_value().cmp(&a.read(cx).current_value()));
            }
            _ => {}
        }
    }
}

/// A table showing all database components with sortable columns and sparklines.
pub type ComponentTable = Table<ComponentTableDelegate>;

pub fn new_component_table(db: Arc<DB>, cx: &mut Context<ComponentTable>) -> ComponentTable {
    let task = ComponentTableDelegate::spawn_watcher(db.clone(), cx);
    let delegate = ComponentTableDelegate {
        rows: Vec::new(),
        _task: task,
    };
    Table::new(delegate)
}
