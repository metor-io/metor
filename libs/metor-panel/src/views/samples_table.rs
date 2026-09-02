//! Every sample of one component as a row: its stamp, and its value as the
//! strip boxes the rest of the panel shows values in.
//!
//! Rows are not copied out of the database. The table indexes the
//! component's resident `time_series` nodes directly and decodes a sample
//! only when its row is painted, so the whole history scrolls at the cost
//! of the thirty rows on screen. Newest is at the top, which makes arriving
//! samples a prepend; to keep the rows under a reader's eye still, the index
//! is built against an anchor stamp that only follows the live head while
//! the table is scrolled to its top. Scrolled away, the anchor freezes and a
//! chip counts the samples that have landed above it.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Pixels, SharedString, Task, Window, div,
    prelude::*, px,
};
use metor_db::time_series::{TimeSeries, TimeSeriesNodeSlice};
use metor_db::{Component, ComponentSchema, DB};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};

use super::binding::component_name;
use super::format::{ValueFormatter, format_time};
use super::table::{CELL_PAD_X, Column, ColumnSort, ROW_HEIGHT, Table, TableDelegate};
use super::value_strip::{StripStyle, render_static_cells, resolve_metadata, strip_row_width};
use crate::dynamic::expressions::{self, Expression};
use crate::theme::theme;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SamplesTableConfig {
    /// A component name or the text of an `=` expression.
    pub component: String,
}

/// The samples pane.
#[derive(facet::Facet)]
pub struct SamplesTable {
    /// The component whose samples are listed. Editable: picking another
    /// rebinds on the next frame.
    pub component_id: ComponentId,
    /// Display name, or the saved text until something registers it.
    #[facet(skip)]
    component: SharedString,
    /// What the tail task is bound to, compared against `component_id`
    /// each frame.
    #[facet(opaque)]
    bound: Option<ComponentId>,
    /// The resolved component, once its producer has registered.
    #[facet(opaque)]
    series: Option<Component>,
    /// The stamp row 0 is anchored to. `None` follows the live head.
    #[facet(opaque)]
    head: Option<Timestamp>,
    /// `(anchor, newest stamp)` the index was built against.
    #[facet(opaque)]
    built: Option<(Timestamp, Timestamp)>,
    /// Samples newer than the anchor, shown while the reader is scrolled
    /// away from the top.
    #[facet(skip)]
    newer: usize,
    #[facet(opaque)]
    table: Entity<Table<SamplesDelegate>>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _expression: Option<Expression>,
    #[facet(opaque)]
    _task: Task<()>,
}

impl SamplesTable {
    pub fn from_config(cfg: &SamplesTableConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let (component_id, expression) = if cfg.component.is_empty() {
            (ComponentId(0), None)
        } else {
            match expressions::bind(&cfg.component, &db, cx) {
                Ok(bound) => (bound.id, bound.expression),
                Err(_) => (ComponentId::new(&cfg.component), None),
            }
        };
        let table = cx.new(|_| Table::new(SamplesDelegate::new()));
        let mut view = Self {
            component_id,
            component: SharedString::from(cfg.component.clone()),
            bound: None,
            series: None,
            head: None,
            built: None,
            newer: 0,
            table,
            db,
            _expression: expression,
            _task: Task::ready(()),
        };
        view.rebind(cx);
        view
    }

    pub fn to_config(&self) -> SamplesTableConfig {
        SamplesTableConfig {
            component: expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    component_name(&self.db, self.component_id).map(|name| name.to_string())
                })
                .unwrap_or_else(|| self.component.to_string()),
        }
    }

    pub fn component(&self) -> &SharedString {
        &self.component
    }

    /// Restart the tail when the inspector has re-pointed the binding.
    fn rebind(&mut self, cx: &mut Context<Self>) {
        if self.bound == Some(self.component_id) {
            return;
        }
        self.bound = Some(self.component_id);
        self.series = None;
        self.head = None;
        self.built = None;
        self.newer = 0;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().unbind();
            cx.notify();
        });
        if self.component_id == ComponentId(0) {
            self._expression = None;
            self._task = Task::ready(());
            return;
        }
        self._expression = expressions::running(self.component_id, cx);
        if let Some(name) = component_name(&self.db, self.component_id) {
            self.component = name;
        }
        self._task = self.spawn_tail(cx);
    }

    /// Wait for the producer, bind the delegate to its schema, then rebuild
    /// the index every time the series grows.
    fn spawn_tail(&self, cx: &mut Context<Self>) -> Task<()> {
        let db = self.db.clone();
        let component_id = self.component_id;
        cx.spawn(async move |this, cx| {
            let component = crate::wait_for_component(&db, component_id).await;
            let meta = resolve_metadata(&db, component_id);
            let bound = this.update(cx, |view, cx| {
                view.component = meta.component_name.clone();
                view.series = Some(component.clone());
                view.table.update(cx, |table, cx| {
                    table.delegate_mut().bind(
                        component.schema.clone(),
                        meta.formatter,
                        meta.element_names,
                    );
                    cx.notify();
                });
                view.refresh(cx);
            });
            if bound.is_err() {
                return;
            }
            loop {
                component.time_series.wait().await;
                if this.update(cx, |view, cx| view.refresh(cx)).is_err() {
                    return;
                }
            }
        })
    }

    /// Bring the index up to date with the series and the anchor.
    ///
    /// Runs on every wake and every render, and rebuilds only when the
    /// anchor or the newest stamp moved. The anchor follows the head while
    /// the table is at its top, so scrolling back up by hand is enough to
    /// return to live.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(component) = &self.series else {
            return;
        };
        let Some(latest) = component.time_series.latest().map(|l| l.timestamp()) else {
            return;
        };
        if self.head.is_none() || self.table.read(cx).at_top() {
            self.head = Some(latest);
        }
        let head = self.head.expect("anchored above");
        if self.built == Some((head, latest)) {
            return;
        }
        let index = SampleIndex::build(&component.time_series, head);
        self.newer = index.newer;
        self.built = Some((head, latest));
        self.table.update(cx, |table, cx| {
            table.delegate_mut().index = index;
            cx.notify();
        });
        cx.notify();
    }

    fn jump_to_live(&mut self, cx: &mut Context<Self>) {
        self.head = None;
        self.table.update(cx, |table, _| table.scroll_to_top());
        self.refresh(cx);
    }
}

impl Render for SamplesTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        self.refresh(cx);
        let theme = theme(cx);

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .text_color(theme.text_primary)
            .text_size(px(12.0));

        if self.component_id == ComponentId(0) {
            return root.child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_tertiary)
                    .child("No component"),
            );
        }

        if self.newer > 0 {
            root = root.child(
                div()
                    .id("samples-newer")
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(20.0))
                    .bg(theme.selection_bg)
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .child(SharedString::from(format!("{} newer ↑", self.newer)))
                    .on_click(cx.listener(|this, _, _, cx| this.jump_to_live(cx))),
            );
        }

        root.child(self.table.clone())
    }
}

/// Random access into a component's resident history, newest first.
///
/// One entry per node, with the count of samples at or before the anchor
/// stamp that the node contributes and a running total ahead of it. A row
/// finds its node by the totals and its sample by counting back from the
/// node's anchored end, since nodes store oldest first.
struct SampleIndex {
    nodes: Vec<IndexedNode>,
    /// Rows ahead of each node.
    starts: Vec<usize>,
    rows: usize,
    /// Samples newer than the anchor, across every node.
    newer: usize,
}

struct IndexedNode {
    slice: TimeSeriesNodeSlice,
    /// Samples at or before the anchor.
    used: usize,
}

impl SampleIndex {
    fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            starts: Vec::new(),
            rows: 0,
            newer: 0,
        }
    }

    fn build(series: &TimeSeries, head: Timestamp) -> Self {
        let mut index = Self::empty();
        for slice in series.iter_node_slices() {
            let stamps = slice.full_timestamps();
            let used = stamps.partition_point(|t| t.0 <= head.0);
            index.newer += stamps.len() - used;
            if used == 0 {
                continue;
            }
            index.starts.push(index.rows);
            index.rows += used;
            index.nodes.push(IndexedNode { slice, used });
        }
        index
    }

    /// The stamp and value bytes of row `row`, newest first.
    fn sample(&self, row: usize, element_size: usize) -> Option<(Timestamp, &[u8])> {
        if row >= self.rows {
            return None;
        }
        let at = self.starts.partition_point(|&start| start <= row) - 1;
        let node = &self.nodes[at];
        let local = row - self.starts[at];
        let ix = node.used - 1 - local;
        let stamp = *node.slice.full_timestamps().get(ix)?;
        let start = ix * element_size;
        let bytes = node.slice.full_data().get(start..start + element_size)?;
        Some((stamp, bytes))
    }
}

const COL_TIME: usize = 0;
const COL_VALUE: usize = 1;

/// Table delegate: the index plus what decoding a row needs. Unbound until
/// the component registers; its rows count is zero meanwhile.
pub struct SamplesDelegate {
    schema: Option<ComponentSchema>,
    formatter: ValueFormatter,
    element_names: Vec<SharedString>,
    style: StripStyle,
    index: SampleIndex,
}

impl SamplesDelegate {
    fn new() -> Self {
        Self {
            schema: None,
            formatter: ValueFormatter::default(),
            element_names: Vec::new(),
            style: StripStyle::boxes().with_intrinsic_width(),
            index: SampleIndex::empty(),
        }
    }

    fn bind(
        &mut self,
        schema: ComponentSchema,
        formatter: ValueFormatter,
        element_names: Vec<SharedString>,
    ) {
        self.schema = Some(schema);
        self.formatter = formatter;
        self.element_names = element_names;
    }

    fn unbind(&mut self) {
        self.schema = None;
        self.index = SampleIndex::empty();
    }
}

impl TableDelegate for SamplesDelegate {
    fn columns(&self) -> Vec<Column> {
        vec![
            Column::new("Time", 120.0),
            Column::new("Value", 320.0).flex().resizable(false),
        ]
    }

    fn rows_count(&self) -> usize {
        self.index.rows
    }

    fn row_height(&self) -> Pixels {
        px(ROW_HEIGHT)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let Some(schema) = &self.schema else {
            return div().into_any_element();
        };
        let Some((stamp, bytes)) = self.index.sample(row_ix, schema.size()) else {
            return div().into_any_element();
        };
        match col_ix {
            COL_TIME => div()
                .flex()
                .items_center()
                .h_full()
                .px(px(CELL_PAD_X))
                .whitespace_nowrap()
                .text_color(theme.text_tertiary)
                .child(SharedString::from(format_time(stamp.0)))
                .into_any_element(),
            COL_VALUE => {
                let Ok((_size, view)) = schema.parse_value(bytes) else {
                    return div().into_any_element();
                };
                let cells = self.formatter.format_cells(&view, &self.element_names);
                // One line per row, scrolling sideways when the strip is
                // wider than the column; axis-restricted so the wheel still
                // scrolls the table.
                let mut line = render_static_cells(row_ix, &cells, &self.style, &theme);
                if cells.len() > 1 {
                    line = line.min_w(px(strip_row_width(cells.len())));
                }
                line.style().flex_shrink = Some(0.0);
                let mut scroll = div()
                    .id(("samples-strip", row_ix))
                    .flex()
                    .items_center()
                    .h_full()
                    .px(px(4.0))
                    .overflow_x_scroll()
                    .child(line);
                scroll.style().restrict_scroll_to_axis = Some(true);
                scroll.into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &App) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use metor_proto::types::PrimType;

    use super::*;

    /// A scalar F64 component holding the stamps `1..=n`, each sample's
    /// value equal to its stamp.
    async fn series_of(n: i64) -> (tempfile::TempDir, Component) {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        let id = ComponentId::new("samples.test");
        db.with_state_mut(|state| {
            state.insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
        })
        .unwrap();
        let component = db
            .with_state(|state| state.get_component(id).cloned())
            .unwrap();
        for i in 1..=n {
            component
                .push_buf(Timestamp(i), &(i as f64).to_le_bytes())
                .unwrap();
        }
        for _ in 0..200 {
            if component.time_series.latest().map(|l| l.timestamp()) == Some(Timestamp(n)) {
                break;
            }
            stellarator::sleep(Duration::from_millis(5)).await;
        }
        (temp, component)
    }

    fn value(bytes: &[u8]) -> f64 {
        f64::from_le_bytes(bytes.try_into().unwrap())
    }

    #[stellarator::test]
    async fn rows_run_newest_first() {
        let (_temp, component) = series_of(5).await;
        let index = SampleIndex::build(&component.time_series, Timestamp(5));
        assert_eq!(index.rows, 5);
        assert_eq!(index.newer, 0);
        for row in 0..5 {
            let (stamp, bytes) = index.sample(row, 8).unwrap();
            assert_eq!(stamp, Timestamp(5 - row as i64));
            assert_eq!(value(bytes), (5 - row) as f64);
        }
        assert!(index.sample(5, 8).is_none());
    }

    #[stellarator::test]
    async fn the_anchor_hides_newer_samples_and_counts_them() {
        let (_temp, component) = series_of(5).await;
        let index = SampleIndex::build(&component.time_series, Timestamp(3));
        assert_eq!(index.rows, 3);
        assert_eq!(index.newer, 2);
        assert_eq!(index.sample(0, 8).unwrap().0, Timestamp(3));
        assert_eq!(index.sample(2, 8).unwrap().0, Timestamp(1));
    }

    #[stellarator::test]
    async fn an_anchor_before_the_first_sample_is_empty() {
        let (_temp, component) = series_of(3).await;
        let index = SampleIndex::build(&component.time_series, Timestamp(0));
        assert_eq!(index.rows, 0);
        assert_eq!(index.newer, 3);
        assert!(index.sample(0, 8).is_none());
    }

    #[test]
    fn config_round_trips() {
        let cfg = SamplesTableConfig {
            component: "=a.b * 2".into(),
        };
        let back: SamplesTableConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.component, cfg.component);
        let partial: SamplesTableConfig = serde_json::from_str("{}").unwrap();
        assert!(partial.component.is_empty());
    }
}
