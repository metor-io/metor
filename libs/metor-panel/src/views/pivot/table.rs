//! The pivot table: every instance of a kind of frame as one row, its
//! fields as columns.
//!
//! Where the outline pivots one branch inline, this view gathers instances
//! from anywhere through its [`Sources`] — a producer's frame tag, a path
//! pattern whose captures become key columns, a shape, hand-picked paths —
//! and lays them out on real table columns, so fields resize, reorder and
//! hide like any table's. The same view serves as a pane and a dashboard
//! widget.
//!
//! The source fields are plain inspector-editable text; the view compares
//! them against what it last resolved at render time, so an inspector edit
//! re-resolves without a setter, as the annunciator's pattern does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Pixels, Render, SharedString, Window, div,
    prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::cell::{CellFit, cell_width, field_cell, field_menu_rows};
use super::model::{Arrangement, Frame, Grid};
use super::source::{FrameIndex, RowPattern, Sources};
#[allow(unused_imports)]
use crate::inspect;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::query::Query;
use crate::theme::{theme, ui_px};
use crate::views::binding::{StreamUpdate, spawn_seeded_stream};
use crate::views::component_browser::component_tree::{
    ComponentNode, build_full_tree, natural_cmp,
};
use crate::views::component_browser::strip_cell_count;
use crate::views::gradient::{Gradient, Scale};
use crate::views::lazy_pool::VisibleEntityCache;
use crate::views::table::{Column, ColumnSort, ROW_HEIGHT, Table, TableDelegate};
use crate::views::value_strip::{ComponentValueStrip, StripStyle};
use smallvec::SmallVec;

/// Live strips kept alive; well above any visible cell count.
const CACHE_CAP: usize = 512;
/// Window for coalescing a burst of registrations into one rebuild.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// Persisted shape of a [`PivotTable`], shared by the pane and the
/// dashboard widget. Every source is optional; a table lists the union of
/// the ones set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PivotTableConfig {
    pub title: String,
    /// A frame type, gathered by the tag its producer stamps on each field.
    pub frame: String,
    /// A row pattern: a glob with `{captures}`, or a `/regex/`.
    pub pattern: String,
    /// Fields an instance must have; it may have more.
    pub shape: Vec<String>,
    /// Hand-picked instance paths.
    pub instances: Vec<String>,
    /// Keeps only instances whose path matches, in the filter bar's syntax.
    pub filter: String,
    /// Drops instances whose path matches any of these terms.
    pub exclude: String,
    /// Fields first, in this order.
    pub fields: Vec<String>,
    pub hidden: Vec<String>,
    /// Per-field gradient settings (`auto`, `off` or `lo..hi`) that replace
    /// what the field's metadata asks for.
    pub gradients: BTreeMap<String, String>,
}

/// The text of every source, as last resolved.
#[derive(Default, Clone, PartialEq)]
struct Spec {
    frame: SharedString,
    pattern: SharedString,
    filter: SharedString,
    exclude: SharedString,
    shape: Vec<SharedString>,
    instances: Vec<SharedString>,
}

/// The pivot table pane and widget.
#[derive(facet::Facet)]
pub struct PivotTable {
    /// Names the tab; empty falls back to the frame type or pattern.
    pub title: SharedString,
    #[facet(inspect::label = "Frame type")]
    pub frame: SharedString,
    #[facet(inspect::label = "Row pattern")]
    pub pattern: SharedString,
    pub filter: SharedString,
    pub exclude: SharedString,
    #[facet(opaque)]
    shape: Vec<SharedString>,
    #[facet(opaque)]
    instances: Vec<SharedString>,
    #[facet(opaque)]
    resolved: Spec,
    /// Why the pattern didn't compile, shown over the table.
    #[facet(skip)]
    error: Option<SharedString>,
    #[facet(opaque)]
    tree: Arc<ComponentNode>,
    #[facet(opaque)]
    frames: FrameIndex,
    #[facet(opaque)]
    table: Entity<Table<PivotDelegate>>,
    #[facet(opaque)]
    _watcher: gpui::Task<()>,
}

impl PivotTable {
    pub fn from_config(cfg: PivotTableConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let arrangement = Arrangement {
            order: cfg.fields.into_iter().map(SharedString::from).collect(),
            hidden: cfg.hidden.into_iter().map(SharedString::from).collect(),
            rows: Vec::new(),
        };
        let gradients = cfg
            .gradients
            .iter()
            .filter_map(|(field, text)| {
                Some((
                    SharedString::from(field.clone()),
                    Gradient::parse(text).ok()??,
                ))
            })
            .collect();
        let table = cx.new(|_| {
            let mut delegate = PivotDelegate::new(db.clone(), arrangement);
            delegate.gradients = gradients;
            Table::new(delegate)
        });
        cx.observe(&table, |_, _, cx| cx.notify()).detach();
        let strings = |v: Vec<String>| v.into_iter().map(SharedString::from).collect();
        Self {
            title: cfg.title.into(),
            frame: cfg.frame.into(),
            pattern: cfg.pattern.into(),
            filter: cfg.filter.into(),
            exclude: cfg.exclude.into(),
            shape: strings(cfg.shape),
            instances: strings(cfg.instances),
            // Never equal to a real spec, so the first render resolves.
            resolved: Spec {
                frame: "\0".into(),
                ..Default::default()
            },
            error: None,
            tree: Arc::new(ComponentNode {
                segment: SharedString::default(),
                full_name: SharedString::default(),
                component_id: None,
                children: Default::default(),
            }),
            frames: FrameIndex::default(),
            table,
            _watcher: spawn_watcher(db, cx),
        }
    }

    pub fn to_config(&self, cx: &App) -> PivotTableConfig {
        let strings = |v: &[SharedString]| v.iter().map(|s| s.to_string()).collect();
        let delegate = self.table.read(cx).delegate();
        let arrangement = &delegate.arrangement;
        let mut hidden: Vec<String> = arrangement.hidden.iter().map(|s| s.to_string()).collect();
        hidden.sort();
        PivotTableConfig {
            title: self.title.to_string(),
            frame: self.frame.to_string(),
            pattern: self.pattern.to_string(),
            shape: strings(&self.shape),
            instances: strings(&self.instances),
            filter: self.filter.to_string(),
            exclude: self.exclude.to_string(),
            fields: strings(&arrangement.order),
            hidden,
            gradients: delegate
                .gradients
                .iter()
                .map(|(field, g)| (field.to_string(), g.text()))
                .collect(),
        }
    }

    /// The tab's name: the title, else what the table gathers.
    pub fn tab_title(&self) -> SharedString {
        [&self.title, &self.frame, &self.pattern]
            .into_iter()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| SharedString::new_static("Pivot"))
    }

    fn spec(&self) -> Spec {
        Spec {
            frame: self.frame.clone(),
            pattern: self.pattern.clone(),
            filter: self.filter.clone(),
            exclude: self.exclude.clone(),
            shape: self.shape.clone(),
            instances: self.instances.clone(),
        }
    }

    /// Resolve the sources against the latest tree and hand the rows to
    /// the table.
    fn resolve(&mut self, cx: &mut Context<Self>) {
        let spec = self.spec();
        let pattern = match RowPattern::parse(&spec.pattern) {
            Ok(p) => {
                self.error = None;
                p
            }
            Err(e) => {
                self.error = Some(format!("Row pattern: {e}").into());
                None
            }
        };
        let keys = pattern
            .as_ref()
            .map(|p| p.capture_names().to_vec())
            .unwrap_or_default();
        let sources = Sources {
            frame: spec.frame.clone(),
            pattern,
            shape: spec.shape.clone(),
            instances: spec.instances.clone(),
            filter: Query::parse(&spec.filter),
            exclude: Sources::parse_exclude(&spec.exclude),
        };
        let frames = sources.resolve(&self.tree, &self.frames);
        self.resolved = spec;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_frames(frames, keys, cx);
        });
    }

    fn status(&self, cx: &App) -> SharedString {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let grid = &self.table.read(cx).delegate().grid;
        if grid.instances.is_empty() {
            let r = &self.resolved;
            let unset = r.frame.trim().is_empty()
                && r.pattern.trim().is_empty()
                && r.shape.is_empty()
                && r.instances.is_empty();
            return if unset {
                SharedString::new_static(
                    "Set a frame type or row pattern in the inspector to gather instances",
                )
            } else {
                SharedString::new_static("No instances match")
            };
        }
        format!(
            "{} × {}",
            count_label(grid.instances.len(), "instance"),
            count_label(grid.fields.len(), "field")
        )
        .into()
    }
}

/// Rebuild the tree and the frame tags whenever a component registers or
/// its metadata changes — a producer re-announcing known components with
/// new tags bumps only the metadata generation.
fn spawn_watcher(db: Arc<DB>, cx: &mut Context<PivotTable>) -> gpui::Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            let vtables = db.vtable_gen.latest();
            let metadata = db.metadata_gen.latest();
            let tree = Arc::new(build_full_tree(&db));
            let frames = FrameIndex::from_db(&db);
            let result = this.update(cx, |view, cx| {
                view.tree = tree;
                view.frames = frames;
                view.resolve(cx);
            });
            if result.is_err() {
                break;
            }
            futures_lite::future::race(
                db.vtable_gen.wait_for(|g| g != vtables),
                db.metadata_gen.wait_for(|g| g != metadata),
            )
            .await;
            cx.background_executor().timer(DEBOUNCE).await;
        }
    })
}

impl Render for PivotTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.resolved != self.spec() {
            self.resolve(cx);
        }
        let theme = theme(cx);
        let status = self.status(cx);
        let status_color = if self.error.is_some() {
            theme.error_accent
        } else {
            theme.text_tertiary
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_primary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(ui_px(24.0))
                    .px(ui_px(8.0))
                    .border_b_1()
                    .border_color(theme.border_primary)
                    .text_size(ui_px(11.0))
                    .text_color(status_color)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(status),
            )
            .child(div().flex_1().min_h_0().w_full().child(self.table.clone()))
    }
}

/// Row source for the pivot table: the resolved instances, laid out on a
/// [`Grid`] by the table's arrangement and shown in sort order.
pub struct PivotDelegate {
    db: Arc<DB>,
    frames: Vec<Frame>,
    /// Capture names, one key column each.
    keys: Vec<SharedString>,
    arrangement: Arrangement,
    grid: Arc<Grid>,
    /// Grid instance indices in display order.
    order: Vec<usize>,
    /// The sorted column by name, as the table tracks it, so the sort
    /// follows its column when captures come and go.
    sort: Option<(SharedString, ColumnSort)>,
    /// Gradient settings the user chose per field, over the metadata's.
    gradients: BTreeMap<SharedString, Gradient>,
    /// Each grid field's gradient: the user's, else what the field's
    /// metadata asks for.
    column_gradients: Vec<Option<Gradient>>,
    /// Live values of the grid fields that need them — the sorted one and
    /// any coloured on a range fitted to the column — keyed by grid index.
    watched: HashMap<usize, Watched>,
    /// A watched value changed since the order and ranges were settled.
    dirty: bool,
    strips: VisibleEntityCache<ComponentValueStrip>,
}

/// A field's elements as numbers, compared element by element.
type SortValue = SmallVec<[f64; 4]>;

/// One field's current value on every instance.
struct Watched {
    /// By grid instance; `None` where an instance lacks the field or it
    /// doesn't read as numbers.
    values: Vec<Option<SortValue>>,
    /// The range an `auto` gradient colours the column on.
    fitted: Option<Scale>,
    /// One stream per instance feeding `values`; dropped to stop.
    _streams: Vec<gpui::Task<()>>,
}

impl PivotDelegate {
    fn new(db: Arc<DB>, arrangement: Arrangement) -> Self {
        Self {
            db,
            frames: Vec::new(),
            keys: Vec::new(),
            arrangement,
            grid: Arc::new(Grid::new(SharedString::default(), Vec::new(), None, &|_| 1)),
            order: Vec::new(),
            sort: None,
            gradients: BTreeMap::new(),
            column_gradients: Vec::new(),
            watched: HashMap::new(),
            dirty: false,
            strips: VisibleEntityCache::new(CACHE_CAP),
        }
    }

    fn set_frames(
        &mut self,
        frames: Vec<Frame>,
        keys: Vec<SharedString>,
        cx: &mut Context<Table<Self>>,
    ) {
        self.frames = frames;
        self.keys = keys;
        self.relayout(cx);
    }

    fn relayout(&mut self, cx: &mut Context<Table<Self>>) {
        let db = self.db.clone();
        self.grid = Arc::new(Grid::new(
            SharedString::new_static("pivot"),
            self.frames.clone(),
            Some(&self.arrangement),
            &|id| strip_cell_count(&db, id),
        ));
        self.column_gradients = self.resolve_gradients();
        self.watch(cx);
        self.settle();
        cx.notify();
    }

    /// The user's setting for each field, else the metadata of the first
    /// instance that has it.
    fn resolve_gradients(&self) -> Vec<Option<Gradient>> {
        let grid = &self.grid;
        self.db.with_state(|state| {
            grid.fields
                .iter()
                .enumerate()
                .map(|(field_ix, field)| {
                    self.gradients.get(field).copied().or_else(|| {
                        grid.instances
                            .iter()
                            .filter_map(|i| i.ids[field_ix])
                            .find_map(|id| state.get_component_metadata(id))
                            .and_then(Gradient::from_metadata)
                    })
                })
                .collect()
        })
    }

    /// Choose a field's gradient, or with `None` go back to its metadata.
    fn set_gradient(
        &mut self,
        field: &SharedString,
        gradient: Option<Gradient>,
        cx: &mut Context<Table<Self>>,
    ) {
        match gradient {
            Some(g) => self.gradients.insert(field.clone(), g),
            None => self.gradients.remove(field),
        };
        self.relayout(cx);
    }

    /// Re-sort and refit the ranges after watched values change.
    fn settle(&mut self) {
        for watched in self.watched.values_mut() {
            watched.fitted = Scale::fit(watched.values.iter().flatten().flatten().copied());
        }
        self.apply_sort();
    }

    /// The grid index of the sorted field, when a field column is sorted.
    fn sorted_field(&self) -> Option<usize> {
        let (name, sort) = self.sort.as_ref()?;
        if *sort == ColumnSort::Default {
            return None;
        }
        self.grid.fields.iter().position(|f| f == name)
    }

    /// Follow every instance's value of the sorted field and of each field
    /// coloured on a fitted range, so the order and the colours track live
    /// data; anything else streams nothing.
    fn watch(&mut self, cx: &mut Context<Table<Self>>) {
        self.watched.clear();
        let fitted = self
            .column_gradients
            .iter()
            .enumerate()
            .filter(|(_, g)| **g == Some(Gradient::Auto))
            .map(|(ix, _)| ix);
        let mut fields: Vec<usize> = self.sorted_field().into_iter().chain(fitted).collect();
        fields.sort_unstable();
        fields.dedup();
        for field_ix in fields {
            let streams = self
                .grid
                .instances
                .iter()
                .enumerate()
                .filter_map(|(ix, instance)| Some((ix, instance.ids[field_ix]?)))
                .map(|(ix, id)| self.stream(field_ix, ix, id, cx))
                .collect();
            self.watched.insert(
                field_ix,
                Watched {
                    values: vec![None; self.grid.instances.len()],
                    fitted: None,
                    _streams: streams,
                },
            );
        }
    }

    /// Feed instance `ix`'s value of `field_ix` into what's watched.
    fn stream(
        &self,
        field_ix: usize,
        ix: usize,
        id: metor_proto::types::ComponentId,
        cx: &mut Context<Table<Self>>,
    ) -> gpui::Task<()> {
        spawn_seeded_stream(
            self.db.clone(),
            id,
            cx,
            |_, _| {
                ((), |view: metor_proto::types::ComponentView<'_>| {
                    Some(view.iter().map(|v| v.as_f64()).collect::<SortValue>())
                })
            },
            move |table: &mut Table<Self>, update, cx| {
                let value = match update {
                    StreamUpdate::Value(value) => Some(value),
                    StreamUpdate::Unavailable => None,
                    _ => return,
                };
                let delegate = table.delegate_mut();
                if let Some(slot) = delegate
                    .watched
                    .get_mut(&field_ix)
                    .and_then(|w| w.values.get_mut(ix))
                    && *slot != value
                {
                    *slot = value;
                    delegate.dirty = true;
                    cx.notify();
                }
            },
        )
    }

    /// Instance and key columns sort naturally, fields by their live value
    /// with instances lacking one last either way; unsorted is path order.
    fn apply_sort(&mut self) {
        self.dirty = false;
        let grid = &self.grid;
        let mut order: Vec<usize> = (0..grid.instances.len()).collect();
        let Some((name, sort)) = self
            .sort
            .as_ref()
            .filter(|(_, s)| *s != ColumnSort::Default)
        else {
            self.order = order;
            return;
        };
        let directed = |o: std::cmp::Ordering| {
            if *sort == ColumnSort::Descending {
                o.reverse()
            } else {
                o
            }
        };
        if let Some(watched) = self.sorted_field().and_then(|f| self.watched.get(&f)) {
            let values = &watched.values;
            order.sort_by(|&a, &b| match (&values[a], &values[b]) {
                (Some(a), Some(b)) => directed(compare_values(a, b)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        } else {
            let key = if name.as_ref() == INSTANCE_COLUMN {
                Some(None)
            } else {
                self.keys
                    .iter()
                    .position(|k| key_column(k) == *name)
                    .map(Some)
            };
            if let Some(key) = key {
                let text = |ix: usize| -> &str {
                    let inst = &grid.instances[ix];
                    match key {
                        None => &inst.label,
                        Some(k) => inst.captures.get(k).map_or("", |s| s.as_ref()),
                    }
                };
                order.sort_by(|&a, &b| directed(natural_cmp(text(a), text(b))));
            }
        }
        self.order = order;
    }

    /// First column index that shows a field.
    fn field_start(&self) -> usize {
        1 + self.keys.len()
    }

    fn set_hidden(&mut self, field: &SharedString, hidden: bool, cx: &mut Context<Table<Self>>) {
        self.arrangement.set_hidden(field, hidden);
        self.relayout(cx);
    }

    /// Colour a field by value, on a range fitted to the column or one
    /// typed in; stop; or go back to what its metadata says.
    fn gradient_rows(
        &self,
        field: &SharedString,
        cx: &mut Context<Table<Self>>,
    ) -> Vec<Box<dyn InspectorRow>> {
        let current = self
            .grid
            .fields
            .iter()
            .position(|f| f == field)
            .and_then(|ix| self.column_gradients.get(ix).copied().flatten());
        let table = cx.entity();
        let set = move |gradient: Option<Gradient>| {
            let (table, field) = (table.clone(), field.clone());
            move |cx: &mut App| {
                table.update(cx, |table, cx| {
                    table.delegate_mut().set_gradient(&field, gradient, cx);
                })
            }
        };
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        if current != Some(Gradient::Auto) {
            let apply = set(Some(Gradient::Auto));
            rows.push(Box::new(CommandRow::new(
                "Color by value",
                Arc::new(move |_, cx| apply(cx)),
            )));
        }
        let (table, field_name) = (cx.entity(), field.clone());
        rows.push(Box::new(DefaultActionRow::new(
            "Color range, e.g. 0..100",
            Arc::new(move |input, _, cx| {
                if let Ok(Some(gradient)) = Gradient::parse(&input) {
                    table.update(cx, |table, cx| {
                        table
                            .delegate_mut()
                            .set_gradient(&field_name, Some(gradient), cx);
                    });
                }
            }),
        )));
        if current.is_some_and(|g| g != Gradient::Off) {
            let apply = set(Some(Gradient::Off));
            rows.push(Box::new(CommandRow::new(
                "Stop coloring",
                Arc::new(move |_, cx| apply(cx)),
            )));
        }
        if self.gradients.contains_key(field) {
            let apply = set(None);
            rows.push(Box::new(CommandRow::new(
                "Color as its metadata says",
                Arc::new(move |_, cx| apply(cx)),
            )));
        }
        rows
    }

    fn render_text(&self, text: SharedString, primary: bool, cx: &App) -> AnyElement {
        let theme = theme(cx);
        div()
            .flex()
            .items_center()
            .h_full()
            .px(ui_px(8.0))
            .text_size(ui_px(12.0))
            .text_color(if primary {
                theme.text_primary
            } else {
                theme.text_secondary
            })
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(text)
            .into_any_element()
    }
}

impl TableDelegate for PivotDelegate {
    fn columns(&self) -> Vec<Column> {
        let mut cols = vec![
            Column::new(INSTANCE_COLUMN, 220.0)
                .min_width(80.0)
                .sortable(),
        ];
        cols.extend(
            self.keys
                .iter()
                .map(|k| Column::new(key_column(k), 80.0).sortable()),
        );
        cols.extend(
            self.grid
                .fields
                .iter()
                .zip(&self.grid.cells)
                .map(|(field, &n)| {
                    Column::new(field.clone(), cell_width(n, field))
                        .movable()
                        .sortable()
                }),
        );
        cols
    }

    fn rows_count(&self) -> usize {
        self.order.len()
    }

    fn row_height(&self) -> Pixels {
        px(ROW_HEIGHT)
    }

    fn before_render(&mut self, _cx: &mut Context<Table<Self>>) {
        if self.dirty {
            self.settle();
        }
    }

    fn frame_rendered(&mut self) {
        self.strips.prune();
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let Some(&ix) = self.order.get(row_ix) else {
            return div().into_any_element();
        };
        let grid = self.grid.clone();
        let instance = &grid.instances[ix];
        if col_ix == 0 {
            return self.render_text(instance.label.clone(), true, cx);
        }
        let start = self.field_start();
        if col_ix < start {
            let value = instance
                .captures
                .get(col_ix - 1)
                .cloned()
                .unwrap_or_default();
            return self.render_text(value, false, cx);
        }
        let field_ix = col_ix - start;
        let (Some(field), Some(&n)) = (grid.fields.get(field_ix), grid.cells.get(field_ix)) else {
            return div().into_any_element();
        };
        field_cell(
            &self.db,
            &mut self.strips,
            instance,
            field_ix,
            field,
            n,
            CellFit::Fill,
            StripStyle::boxes().with_gradient(
                self.column_gradients.get(field_ix).copied().flatten(),
                self.watched.get(&field_ix).and_then(|w| w.fitted),
            ),
            cx,
        )
    }

    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, cx: &mut Context<Table<Self>>) {
        let Some(column) = self.columns().into_iter().nth(col_ix) else {
            return;
        };
        self.sort = Some((column.name, sort));
        self.watch(cx);
        self.settle();
        cx.notify();
    }

    fn move_column(&mut self, from: usize, to: usize, cx: &mut Context<Table<Self>>) {
        let start = self.field_start();
        if from < start {
            return;
        }
        let mut fields = self.grid.fields.clone();
        let field = fields.remove(from - start);
        fields.insert(to.saturating_sub(start).min(fields.len()), field);
        self.arrangement.order = fields;
        self.relayout(cx);
    }

    /// Right-click on a header: hide a field, or bring a hidden one back.
    fn header_menu(
        &mut self,
        col_ix: usize,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) {
        let Some(open) = open_inspector(cx) else {
            return;
        };
        let field = col_ix
            .checked_sub(self.field_start())
            .and_then(|ix| self.grid.fields.get(ix));
        if field.is_none() && self.grid.hidden.is_empty() {
            return;
        }
        let table = cx.entity();
        let mut rows = field
            .map(|field| self.gradient_rows(field, cx))
            .unwrap_or_default();
        rows.extend(field_menu_rows(
            &self.grid,
            field,
            Arc::new(move |field, hidden, cx| {
                table.update(cx, |table, cx| {
                    table.delegate_mut().set_hidden(&field, hidden, cx);
                });
            }),
        ));
        open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Anchored(position),
            },
            window,
            cx,
        );
    }
}

/// The leading column, naming each instance by path.
const INSTANCE_COLUMN: &str = "Instance";

/// A capture's column name, braced so it can't collide with a field.
fn key_column(capture: &str) -> SharedString {
    format!("{{{capture}}}").into()
}

/// `"1 field"` / `"3 fields"`.
fn count_label(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

/// Element by element; NaN sorts after every number.
fn compare_values(a: &[f64], b: &[f64]) -> std::cmp::Ordering {
    let key = |v: f64| if v.is_nan() { f64::INFINITY } else { v };
    a.iter()
        .zip(b)
        .map(|(a, b)| key(*a).total_cmp(&key(*b)))
        .find(|o| o.is_ne())
        .unwrap_or_else(|| a.len().cmp(&b.len()))
}

#[cfg(test)]
mod tests {
    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;
    use metor_proto_wkt::ComponentMetadata;

    use super::*;

    /// Three PSU ports, tagged as `psu` frames except the last, plus an
    /// untagged sibling.
    fn psu_db() -> (Arc<DB>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        let fields = [
            ("dut1.psu.current", Some("dut1.psu")),
            ("dut1.psu.voltage", Some("dut1.psu")),
            ("dut2.psu.current", Some("dut2.psu")),
            ("dut2.psu.voltage", Some("dut2.psu")),
            ("dut3.psu.current", None),
            ("dut3.serial", None),
        ];
        for (name, root) in fields {
            let mut meta = ComponentMetadata::from(name);
            if let Some(root) = root {
                meta = meta.with_frame("psu", root);
            }
            let id = meta.component_id;
            db.with_state_mut(|s| {
                s.insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)?;
                s.set_component_metadata(meta, &db.path)
            })
            .unwrap();
        }
        (Arc::new(db), temp)
    }

    fn open(
        db: &Arc<DB>,
        cfg: PivotTableConfig,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<PivotTable> {
        let view = cx.update(|_, cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            crate::inspector::edits::init(cx);
            cx.new(|cx| PivotTable::from_config(cfg, db.clone(), cx))
        });
        cx.run_until_parked();
        view
    }

    fn labels(view: &Entity<PivotTable>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        cx.update(|_, cx| {
            let delegate = view.read(cx).table.read(cx).delegate();
            delegate
                .order
                .iter()
                .map(|&ix| delegate.grid.instances[ix].label.to_string())
                .collect()
        })
    }

    #[gpui::test]
    fn a_frame_type_gathers_every_tagged_instance(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        let cx = cx.add_empty_window();
        let view = open(
            &db,
            PivotTableConfig {
                frame: "psu".into(),
                ..Default::default()
            },
            cx,
        );
        assert_eq!(labels(&view, cx), ["dut1.psu", "dut2.psu"]);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    let cols = table.delegate().columns().len();
                    assert_eq!(cols, 3);
                    for row in 0..table.delegate().rows_count() {
                        for col in 0..cols {
                            table.delegate_mut().render_cell(row, col, window, cx);
                        }
                    }
                    assert!(
                        table
                            .delegate()
                            .strips
                            .contains(metor_proto::types::ComponentId::new("dut2.psu.voltage"))
                    );
                });
            })
        });
    }

    #[gpui::test]
    fn an_inspector_edit_re_resolves_and_captures_become_keys(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        let cx = cx.add_empty_window();
        let view = open(&db, PivotTableConfig::default(), cx);
        assert!(labels(&view, cx).is_empty());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                // What a reflective field write leaves behind; render notices.
                view.pattern = "dut{dut}.psu".into();
                assert!(view.resolved != view.spec());
                view.resolve(cx);
            })
        });
        assert_eq!(labels(&view, cx), ["dut1.psu", "dut2.psu", "dut3.psu"]);
        cx.update(|_, cx| {
            let table = view.read(cx).table.read(cx);
            let names: Vec<String> = table
                .delegate()
                .columns()
                .iter()
                .map(|c| c.name.to_string())
                .collect();
            assert_eq!(names, ["Instance", "{dut}", "current", "voltage"]);
            let config = view.read(cx).to_config(cx);
            assert_eq!(config.pattern, "dut{dut}.psu");
        });
    }

    #[gpui::test]
    fn hiding_and_moving_fields_persist(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        let cx = cx.add_empty_window();
        let view = open(
            &db,
            PivotTableConfig {
                frame: "psu".into(),
                ..Default::default()
            },
            cx,
        );
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    // Instance, current, voltage: drop voltage before current.
                    table.delegate_mut().move_column(2, 1, cx);
                    assert_eq!(table.delegate().grid.fields, ["voltage", "current"]);
                    table.delegate_mut().set_hidden(&"current".into(), true, cx);
                    assert_eq!(table.delegate().grid.fields, ["voltage"]);
                });
            });
            let config = view.read(cx).to_config(cx);
            assert_eq!(config.fields, ["voltage", "current"]);
            assert_eq!(config.hidden, ["current"]);
        });
    }

    #[gpui::test]
    fn a_key_sort_goes_with_its_column(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        let cx = cx.add_empty_window();
        let view = open(
            &db,
            PivotTableConfig {
                pattern: "{dut}.{part}".into(),
                ..Default::default()
            },
            cx,
        );
        let set_pattern = |pattern: &'static str, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.pattern = pattern.into();
                    view.resolve(cx);
                })
            })
        };
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    // Instance, {dut}, {part}: sort descending by {dut}.
                    table
                        .delegate_mut()
                        .sort_column(1, ColumnSort::Descending, cx);
                });
            })
        });
        assert_eq!(labels(&view, cx), ["dut3.psu", "dut2.psu", "dut1.psu"]);
        // Column 1 is now `{x}`; the sort was on `{dut}`, which is gone.
        set_pattern("{x}.{part}", cx);
        assert_eq!(labels(&view, cx), ["dut1.psu", "dut2.psu", "dut3.psu"]);
        // `{dut}` returns as the second key, and the sort reads it again.
        set_pattern("/(?<x>dut)(?<dut>\\d)\\.psu/", cx);
        assert_eq!(labels(&view, cx), ["dut3.psu", "dut2.psu", "dut1.psu"]);
    }

    #[gpui::test]
    fn a_field_sort_follows_live_values(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        let cx = cx.add_empty_window();
        let view = open(
            &db,
            PivotTableConfig {
                pattern: "dut{dut}.psu".into(),
                ..Default::default()
            },
            cx,
        );
        let deliver = |values: [Option<f64>; 3], cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.table.update(cx, |table, cx| {
                        let delegate = table.delegate_mut();
                        // `current` is grid field 0.
                        delegate.watched.get_mut(&0).unwrap().values = values
                            .iter()
                            .map(|v| v.map(|v| SortValue::from_slice(&[v])))
                            .collect();
                        delegate.dirty = true;
                        delegate.before_render(cx);
                    });
                })
            })
        };
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    // Instance, {dut}, current, voltage: sort by current.
                    table
                        .delegate_mut()
                        .sort_column(2, ColumnSort::Ascending, cx);
                    // Each dut has a current, so each streams one.
                    assert_eq!(table.delegate().watched[&0]._streams.len(), 3);
                });
            })
        });
        deliver([Some(2.0), Some(1.0), None], cx);
        assert_eq!(labels(&view, cx), ["dut2.psu", "dut1.psu", "dut3.psu"]);
        deliver([Some(0.5), Some(1.0), None], cx);
        assert_eq!(labels(&view, cx), ["dut1.psu", "dut2.psu", "dut3.psu"]);
        // Descending flips the values; one not read yet stays last.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    table
                        .delegate_mut()
                        .sort_column(2, ColumnSort::Descending, cx);
                });
            })
        });
        deliver([Some(0.5), Some(1.0), None], cx);
        assert_eq!(labels(&view, cx), ["dut2.psu", "dut1.psu", "dut3.psu"]);
    }

    #[gpui::test]
    fn a_column_gradient_follows_metadata_until_overridden(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = psu_db();
        db.with_state_mut(|s| {
            let id = metor_proto::types::ComponentId::new("dut1.psu.voltage");
            let mut meta = s.get_component_metadata(id).unwrap().clone();
            meta.metadata.insert("gradient".into(), "0..5".into());
            s.set_component_metadata(meta, &db.path)
        })
        .unwrap();
        let cx = cx.add_empty_window();
        let view = open(
            &db,
            PivotTableConfig {
                frame: "psu".into(),
                gradients: [("current".to_string(), "auto".to_string())].into(),
                ..Default::default()
            },
            cx,
        );
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.table.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    // Fields are current, voltage.
                    assert_eq!(
                        delegate.column_gradients,
                        [Some(Gradient::Auto), Some(Gradient::Range(0.0, 5.0))]
                    );
                    // Only the auto column streams, to fit its range.
                    assert_eq!(delegate.watched.keys().collect::<Vec<_>>(), [&0]);
                    delegate.watched.get_mut(&0).unwrap().values =
                        vec![Some(SortValue::from_slice(&[4.0])), None];
                    delegate.dirty = true;
                    delegate.before_render(cx);
                    let fitted = delegate.watched[&0].fitted.unwrap();
                    assert_eq!(fitted.position(2.0), 0.5);
                    delegate.set_gradient(&"voltage".into(), Some(Gradient::Off), cx);
                    delegate.set_gradient(&"current".into(), None, cx);
                    assert_eq!(
                        table.delegate().column_gradients,
                        [None, Some(Gradient::Off)]
                    );
                    assert!(table.delegate().watched.is_empty());
                });
            });
            let config = view.read(cx).to_config(cx);
            assert_eq!(
                config.gradients,
                BTreeMap::from([("voltage".to_string(), "off".to_string())])
            );
        });
    }

    #[test]
    fn values_compare_element_by_element_with_nan_last() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_values(&[1.0, 5.0], &[1.0, 2.0]), Greater);
        assert_eq!(compare_values(&[f64::NAN], &[1e300]), Greater);
        assert_eq!(compare_values(&[1.0], &[1.0, 0.0]), Less);
    }
}
