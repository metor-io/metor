use std::sync::Arc;
use std::time::Duration;

use super::lazy_pool::VisibleEntityCache;
use super::monitor::{behavior_snapshot, edit_click};
use super::table::{Column, ColumnSort, Table, TableDelegate};
use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripClick, StripStyle};
use crate::inspector::plot_preview::shift_hover_listener;
use crate::theme::theme;
use crate::{ComponentStream, WalComponentStream};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, IntoElement, Pixels, SharedString, Window, div,
    prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

/// Live rows kept materialized in the table. Sized well above the visible row
/// count so scrolling reuses rows, while bounding how many strip/sparkline
/// stream tasks exist at once.
const ROW_CACHE_CAP: usize = 256;

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

    fn component_id(&self) -> ComponentId {
        self.component.component_id
    }
}

/// Latest formatted value of a component, used as the sort key for the value
/// column. Reads directly from the time series so sorting never needs a live
/// row entity.
fn current_value_string(db: &DB, component: &Component) -> String {
    let Some(latest) = component.time_series.latest() else {
        return String::new();
    };
    let Ok((_size, view)) = component.schema.parse_value(latest.data()) else {
        return String::new();
    };
    super::format_value(view, db, component.component_id)
}

/// [`TableDelegate`] that lists every component with name, value, and
/// sparkline columns.
///
/// Rows are regenerated whenever the DB's virtual table generation
/// advances, so newly-registered components appear without a manual refresh.
/// Lightweight per-component metadata. Live `ComponentRow` entities (each with
/// a strip task and a sparkline) are materialized lazily for the visible range
/// in `render_cell`, so the table holds only N cheap metas plus a bounded set
/// of live rows rather than N stream subscriptions.
struct RowMeta {
    id: ComponentId,
    name: SharedString,
    component: Component,
}

pub struct ComponentTableDelegate {
    db: Arc<DB>,
    metas: Vec<RowMeta>,
    row_cache: VisibleEntityCache<ComponentRow>,
    _task: gpui::Task<()>,
}

impl ComponentTableDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Table<Self>>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let metas = build_metas(&db);
                let result = this.update(cx, |this, cx| {
                    this.delegate_mut().metas = metas;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
                // vtable_gen bumps once per registered component; debounce so a
                // startup burst rebuilds the row metas once instead of N times.
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        })
    }
}

fn build_metas(db: &Arc<DB>) -> Vec<RowMeta> {
    db.with_state(|state| {
        state
            .component_metadata_iter()
            .filter(|(_, meta)| !meta.is_hidden())
            .filter_map(|(id, meta)| {
                let name = SharedString::from(meta.name.clone());
                let component = state.get_component(*id)?.clone();
                Some(RowMeta {
                    id: *id,
                    name,
                    component,
                })
            })
            .collect()
    })
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
        self.metas.len()
    }

    fn row_height(&self) -> Pixels {
        px(50.0)
    }

    fn frame_rendered(&mut self) {
        self.row_cache.prune();
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let (id, name, component) = {
            let meta = &self.metas[row_ix];
            (meta.id, meta.name.clone(), meta.component.clone())
        };
        let db = self.db.clone();
        let row = self.row_cache.get_or_create(id, || {
            cx.new(|cx| ComponentRow::new(db, name, component, cx))
        });
        let row = &row;
        match col_ix {
            0 => {
                let name = row.read(cx).name.clone();
                let component_id = row.read(cx).component_id();
                div()
                    .id(("component-table-name", row_ix))
                    .px(px(12.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_primary)
                    .on_mouse_move(shift_hover_listener(component_id, SmallVec::new()))
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

    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, _cx: &App) {
        let db = self.db.clone();
        match (col_ix, sort) {
            (0, ColumnSort::Ascending) => {
                self.metas.sort_by(|a, b| a.name.cmp(&b.name));
            }
            (0, ColumnSort::Descending) => {
                self.metas.sort_by(|a, b| b.name.cmp(&a.name));
            }
            (1, ColumnSort::Ascending) => {
                self.metas.sort_by(|a, b| {
                    current_value_string(&db, &a.component)
                        .cmp(&current_value_string(&db, &b.component))
                });
            }
            (1, ColumnSort::Descending) => {
                self.metas.sort_by(|a, b| {
                    current_value_string(&db, &b.component)
                        .cmp(&current_value_string(&db, &a.component))
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
        db,
        metas: Vec::new(),
        row_cache: VisibleEntityCache::new(ROW_CACHE_CAP),
        _task: task,
    };
    Table::new(delegate)
}
