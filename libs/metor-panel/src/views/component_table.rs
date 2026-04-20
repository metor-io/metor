use std::sync::Arc;

use super::monitor::{behavior_snapshot, edit_click};
use super::table::{Column, ColumnSort, Table, TableDelegate};
use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripClick, StripStyle};
use crate::theme::theme;
use crate::{ComponentStream, WalComponentStream};
use gpui::{
    AnyElement, App, AppContext, AsyncApp, Context, Entity, IntoElement, Pixels, SharedString,
    Window, div, prelude::*, px,
};
use metor_db::{Component, DB};

struct ComponentRow {
    db: Arc<DB>,
    name: SharedString,
    component: Component,
    sparkline: Entity<LinePlot>,
    strip: Entity<ComponentValueStrip>,
    click: StripClick,
    _task: gpui::Task<()>,
}

impl ComponentRow {
    pub fn new(
        db: Arc<DB>,
        name: impl Into<SharedString>,
        component: Component,
        cx: &mut Context<Self>,
    ) -> Self {
        let name: SharedString = name.into();
        let component_id = component.component_id;
        let line_colors = theme(cx).line_colors;
        let num_elements: usize = component.schema.dim.iter().product::<usize>().max(1);
        let traces: Vec<Trace> = (0..num_elements)
            .map(|i| {
                let mut t = Trace::new(component_id, i, line_colors[i % line_colors.len()]);
                t.stroke_width = 1.0;
                t
            })
            .collect();

        let sparkline = cx.new(|cx| LinePlot::new(db.clone(), cx));
        sparkline.update(cx, |sp, cx| sp.bind_traces(traces, cx));

        let click = edit_click(db.clone(), component_id, name.clone());
        let strip = {
            let db_for_strip = db.clone();
            let component_for_strip = component.clone();
            let click_for_strip = click.clone();
            cx.new(|cx| {
                ComponentValueStrip::new(
                    db_for_strip,
                    component_for_strip,
                    StripStyle::boxes(),
                    super::value_strip::StripBehavior {
                        on_element_click: Some(click_for_strip),
                        ..Default::default()
                    },
                    cx,
                )
            })
        };

        // Wake the row on every sample so the sort column and sparkline
        // stay current even when the strip doesn't need to redraw.
        let mut stream = WalComponentStream::new(&component);
        let task = cx.spawn(async move |this, cx| {
            loop {
                stream.next().await;
                let result = this.update(cx, |_this, cx| cx.notify());
                if result.is_err() {
                    break;
                }
            }
        });

        Self {
            db,
            name,
            component,
            sparkline,
            strip,
            click,
            _task: task,
        }
    }

    fn component_id(&self) -> metor_proto::types::ComponentId {
        self.component.component_id
    }

    fn current_value_string(&self) -> String {
        let Some(latest) = self.component.time_series.latest() else {
            return String::new();
        };
        let buf = latest.data();
        let Ok((_size, view)) = self.component.schema.parse_value(buf) else {
            return String::new();
        };
        super::format_value(view, &self.db, self.component.component_id)
    }
}

/// [`TableDelegate`] that lists every component with name, value, and
/// sparkline columns.
///
/// Rows are regenerated whenever the DB's virtual table generation
/// advances, so newly-registered components appear without a manual refresh.
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
        let prepared: Vec<(SharedString, Component)> = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter_map(|(id, meta)| {
                    let name = SharedString::from(meta.name.clone());
                    let component = state.get_component(*id)?.clone();
                    Some((name, component))
                })
                .collect()
        });

        prepared
            .into_iter()
            .filter_map(|(name, component)| {
                let db = db.clone();
                cx.new(|cx| ComponentRow::new(db, name, component, cx)).ok()
            })
            .collect()
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
        match col_ix {
            0 => {
                let name = row.read(cx).name.clone();
                div()
                    .px(px(12.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_primary)
                    .child(name)
                    .into_any_element()
            }
            1 => {
                let (strip, behavior) = {
                    let row_ref = row.read(cx);
                    let behavior = behavior_snapshot(
                        cx,
                        row_ref.db.clone(),
                        row_ref.component_id(),
                        row_ref.click.clone(),
                    );
                    (row_ref.strip.clone(), behavior)
                };
                strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(4.0))
                    .child(strip)
                    .into_any_element()
            }
            2 => {
                let row_height = self.row_height();
                let sparkline = row.read(cx).sparkline.clone();
                div()
                    .w_full()
                    .h(row_height - px(8.0))
                    .child(sparkline)
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, cx: &App) {
        match (col_ix, sort) {
            (0, ColumnSort::Ascending) => {
                self.rows
                    .sort_by(|a, b| a.read(cx).name.cmp(&b.read(cx).name));
            }
            (0, ColumnSort::Descending) => {
                self.rows
                    .sort_by(|a, b| b.read(cx).name.cmp(&a.read(cx).name));
            }
            (1, ColumnSort::Ascending) => {
                self.rows.sort_by(|a, b| {
                    a.read(cx)
                        .current_value_string()
                        .cmp(&b.read(cx).current_value_string())
                });
            }
            (1, ColumnSort::Descending) => {
                self.rows.sort_by(|a, b| {
                    b.read(cx)
                        .current_value_string()
                        .cmp(&a.read(cx).current_value_string())
                });
            }
            _ => {}
        }
    }
}

/// Table of every database component paired with [`ComponentTableDelegate`].
pub type ComponentTable = Table<ComponentTableDelegate>;

/// Construct a [`ComponentTable`] wired to `db`.
pub fn new_component_table(db: Arc<DB>, cx: &mut Context<ComponentTable>) -> ComponentTable {
    let task = ComponentTableDelegate::spawn_watcher(db.clone(), cx);
    let delegate = ComponentTableDelegate {
        rows: Vec::new(),
        _task: task,
    };
    Table::new(delegate)
}
