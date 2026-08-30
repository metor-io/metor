//! The component outline: the namespace as a collapsible tree-table.
//!
//! Every dot-delimited component name is a path, so the whole namespace is
//! a tree, and this pane draws that tree the way an outliner does — one row
//! per node, indented, with a disclosure triangle on each branch. Folded
//! branches summarize what they hide; components show a live value strip
//! plus unit and type, and optionally a sparkline. It replaces the data
//! table's guess at "groups" with the structure the names already carry.
//!
//! The rows are flattened by [`model`] and painted through [`Table`], so
//! the pane scales to the whole namespace: only visible rows hold live
//! strips, kept in a [`VisibleEntityCache`].

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels,
    Render, SharedString, Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

pub(crate) mod model;

use super::column_browser::ToggleFilterBar;
use super::component_browser::component_tree::{ComponentNode, build_tree};
use super::component_browser::{right_click_plot, strip_cell_count};
use super::filter_bar::{FilterBar, FilterBarEvent};
use super::lazy_pool::VisibleEntityCache;
use super::monitor::{behavior_snapshot, edit_click};
use super::table::{Column, ColumnSort, Table, TableDelegate};
use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripBehavior, StripStyle, strip_row_width};
use crate::icons::Icon;
use crate::inspector::plot_preview::shift_hover_listener;
use crate::inspector::rows::BoolRow;
use crate::query::Query;
use crate::theme::theme;
use model::{Disclosure, OutlineRow, component_count, flatten};

/// Indent per tree depth, in pixels.
const INDENT: f32 = 14.0;
const ROW_HEIGHT: f32 = 30.0;
/// Row height once sparklines are on — enough for a readable trace.
const SPARKLINE_ROW_HEIGHT: f32 = 44.0;
/// Live strips and sparklines kept alive; well above any visible row count
/// so scrolling reuses entities while bounding stream tasks.
const CACHE_CAP: usize = 256;
/// Window for coalescing a burst of vtable-generation bumps into one rebuild.
const VTABLE_DEBOUNCE: Duration = Duration::from_millis(50);

/// The pane: a filter bar over the outline table.
pub struct ComponentOutline {
    table: Entity<Table<OutlineDelegate>>,
    filter: Entity<FilterBar>,
    /// Hidden until asked for (Cmd-F, the inspector); hiding clears the
    /// query so nothing filters invisibly.
    filter_visible: bool,
    focus_handle: FocusHandle,
}

impl ComponentOutline {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let table = cx.new(|cx| Table::new(OutlineDelegate::new(db, cx)));
        let filter = cx.new(|cx| FilterBar::new("Filter components…", focus_handle.clone(), cx));
        cx.subscribe(&filter, |this, bar, event, cx| {
            if matches!(event, FilterBarEvent::Changed) {
                let query = bar.read(cx).query().clone();
                this.table.update(cx, |table, cx| {
                    table.delegate_mut().set_query(query, cx);
                });
            }
            cx.notify();
        })
        .detach();
        Self {
            table,
            filter,
            filter_visible: false,
            focus_handle,
        }
    }

    pub fn sparklines(&self, cx: &App) -> bool {
        self.table.read(cx).delegate().sparklines
    }

    pub fn set_sparklines(&mut self, on: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_sparklines(on, cx);
        });
        cx.notify();
    }

    pub fn filter_visible(&self) -> bool {
        self.filter_visible
    }

    /// Show or hide the bar without moving focus; hiding clears the query.
    pub fn set_filter_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.filter_visible = visible;
        if !visible {
            self.filter.update(cx, |bar, cx| bar.clear(cx));
        }
        cx.notify();
    }

    /// The Cmd-F gesture: a hidden bar appears with the caret in it; a
    /// visible one goes away.
    pub fn toggle_filter_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = !self.filter_visible;
        self.set_filter_visible(visible, cx);
        if visible {
            self.filter.read(cx).focus(window);
        }
    }

    pub fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).text().to_string()
    }

    /// Restore a persisted query: the bar shows whenever there is one.
    pub fn set_filter_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.filter_visible = true;
        }
        self.filter.update(cx, |bar, cx| bar.set_text(text, cx));
        cx.notify();
    }

    /// Branch paths flipped away from the default disclosure, for persistence.
    pub fn toggled_paths(&self, cx: &App) -> Vec<String> {
        self.table.read(cx).delegate().disclosure.toggled_paths()
    }

    pub fn set_toggled_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.disclosure = Disclosure::from_paths(paths);
            delegate.reflatten(cx);
        });
    }
}

impl Focusable for ComponentOutline {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComponentOutline {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.filter_visible.then(|| {
            let status = self.table.read(cx).delegate().filter_status();
            self.filter.update(cx, |bar, _| bar.status = status);
            self.filter.clone()
        });

        div()
            .key_context("ComponentOutline")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleFilterBar, window, cx| {
                this.toggle_filter_bar(window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .children(toolbar)
            .child(div().flex_1().min_h_0().w_full().child(self.table.clone()))
    }
}

/// The inspector's sparkline toggle for an outline pane.
pub fn sparklines_row(outline: Entity<ComponentOutline>) -> BoolRow {
    let read = outline.clone();
    BoolRow::dynamic(
        "Sparklines",
        Arc::new(move |cx| read.read(cx).sparklines(cx)),
        Arc::new(move |on, _window, cx| {
            outline.update(cx, |outline, cx| outline.set_sparklines(on, cx));
        }),
    )
}

/// The inspector's filter-bar toggle for an outline pane.
pub fn filter_bar_row(outline: Entity<ComponentOutline>) -> BoolRow {
    let read = outline.clone();
    BoolRow::dynamic(
        "Filter bar",
        Arc::new(move |cx| read.read(cx).filter_visible()),
        Arc::new(move |checked, window, cx| {
            outline.update(cx, |outline, cx| {
                if checked != outline.filter_visible() {
                    outline.toggle_filter_bar(window, cx);
                }
            });
        }),
    )
}

/// Column order. Value comes last so it can absorb the remaining width —
/// wide tensors need it most — and the sparkline column slots in before it
/// only while enabled.
enum Col {
    Name,
    Unit,
    Type,
    Sparkline,
    Value,
}

impl Col {
    fn at(col_ix: usize, sparklines: bool) -> Self {
        match (col_ix, sparklines) {
            (0, _) => Col::Name,
            (1, _) => Col::Unit,
            (2, _) => Col::Type,
            (3, true) => Col::Sparkline,
            _ => Col::Value,
        }
    }
}

/// Row source for the outline table.
///
/// Holds the tree snapshot, the disclosure state, and the flattened rows
/// they produce; the flat list is rebuilt on every change rather than per
/// frame, since a render only indexes into it.
pub struct OutlineDelegate {
    db: Arc<DB>,
    tree: Arc<ComponentNode>,
    disclosure: Disclosure,
    query: Query,
    rows: Vec<OutlineRow>,
    sparklines: bool,
    strips: VisibleEntityCache<ComponentValueStrip>,
    sparks: VisibleEntityCache<LinePlot>,
    _watcher: gpui::Task<()>,
}

impl OutlineDelegate {
    fn new(db: Arc<DB>, cx: &mut Context<Table<Self>>) -> Self {
        let watcher = Self::spawn_watcher(db.clone(), cx);
        Self {
            db,
            tree: build_tree_root(),
            disclosure: Disclosure::default(),
            query: Query::default(),
            rows: Vec::new(),
            sparklines: false,
            strips: VisibleEntityCache::new(CACHE_CAP),
            sparks: VisibleEntityCache::new(CACHE_CAP),
            _watcher: watcher,
        }
    }

    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Table<Self>>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let tree = build_tree(&db);
                let result = this.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    delegate.tree = tree;
                    delegate.reflatten(cx);
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
                cx.background_executor().timer(VTABLE_DEBOUNCE).await;
            }
        })
    }

    fn reflatten(&mut self, cx: &mut Context<Table<Self>>) {
        self.rows = flatten(&self.tree, &self.disclosure, &self.query);
        cx.notify();
    }

    fn set_query(&mut self, query: Query, cx: &mut Context<Table<Self>>) {
        self.query = query;
        self.reflatten(cx);
    }

    fn set_sparklines(&mut self, on: bool, cx: &mut Context<Table<Self>>) {
        self.sparklines = on;
        if !on {
            self.sparks = VisibleEntityCache::new(CACHE_CAP);
        }
        cx.notify();
    }

    /// Click on a branch row: toggle it, or with alt held, its whole subtree.
    fn toggle_row(&mut self, row_ix: usize, subtree: bool, cx: &mut Context<Table<Self>>) {
        let Some(row) = self.rows.get(row_ix).cloned() else {
            return;
        };
        if subtree {
            self.disclosure
                .set_subtree(&row.node, row.depth, !row.expanded);
        } else {
            self.disclosure.toggle(&row.node.full_name);
        }
        self.reflatten(cx);
    }

    /// `shown / total` components while a query is narrowing the tree.
    fn filter_status(&self) -> Option<SharedString> {
        if self.query.is_empty() {
            return None;
        }
        let shown = self
            .rows
            .iter()
            .filter(|r| r.node.component_id.is_some())
            .count();
        let total = component_count(&self.tree);
        Some(SharedString::from(format!("{shown} / {total}")))
    }

    fn strip(
        &mut self,
        id: ComponentId,
        cx: &mut Context<Table<Self>>,
    ) -> Entity<ComponentValueStrip> {
        let db = self.db.clone();
        self.strips.get_or_create(id, || {
            cx.new(|cx| {
                ComponentValueStrip::new(db, id, StripStyle::boxes(), StripBehavior::default(), cx)
            })
        })
    }

    fn sparkline(&mut self, id: ComponentId, cx: &mut Context<Table<Self>>) -> Entity<LinePlot> {
        let db = self.db.clone();
        let line_colors = theme(cx).line_colors;
        let elements = element_count(&db, id);
        self.sparks.get_or_create(id, || {
            let traces: Vec<Trace> = (0..elements)
                .map(|i| {
                    let mut t = Trace::new(id, i, line_colors[i % line_colors.len()]);
                    t.stroke_width = 1.0;
                    t
                })
                .collect();
            let plot = cx.new(|cx| LinePlot::new(db.clone(), cx));
            plot.update(cx, |plot, cx| plot.bind_traces(traces, cx));
            plot
        })
    }

    fn render_name(
        &mut self,
        row_ix: usize,
        row: &OutlineRow,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let is_branch = row.is_branch();

        let mut slot = div().w(px(12.0)).flex_shrink_0();
        if is_branch {
            let icon = if row.expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            slot = slot.child(icon.svg_color(10.0, theme.text_secondary));
        }

        let mut cell = div()
            .id(("outline-name", row_ix))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .h_full()
            .pl(px(8.0 + row.depth as f32 * INDENT))
            .pr(px(8.0))
            .text_size(px(13.0))
            .text_color(theme.text_primary)
            .child(slot)
            .child(
                div()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(row.node.segment.clone()),
            );

        if is_branch {
            cell = cell.cursor_pointer().on_click(cx.listener(
                move |table, event: &ClickEvent, _, cx| {
                    let subtree = event.modifiers().alt;
                    table.delegate_mut().toggle_row(row_ix, subtree, cx);
                },
            ));
        }
        if let Some(id) = row.node.component_id {
            cell = cell.on_mouse_move(shift_hover_listener(id, SmallVec::new()));
        }
        cell.into_any_element()
    }

    fn render_value(
        &mut self,
        row_ix: usize,
        row: &OutlineRow,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let Some(id) = row.node.component_id else {
            let n = row.component_count;
            let label = if n == 1 {
                "1 component".to_string()
            } else {
                format!("{n} components")
            };
            return div()
                .px(px(8.0))
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(label))
                .into_any_element();
        };

        let db = self.db.clone();
        let strip = self.strip(id, cx);
        let click = edit_click(db.clone(), id, row.node.full_name.clone());
        let mut behavior = behavior_snapshot(cx, db.clone(), id, click);
        behavior.on_element_right_click = Some(right_click_plot(db.clone(), id));
        strip.update(cx, |s, cx| s.set_behavior(behavior, cx));

        // The strip wraps its element boxes; inside a fixed-height row a
        // wrapped line would clip invisibly, so hold it to one line and let
        // overflow scroll sideways (axis-restricted so the wheel still
        // scrolls the table).
        let n_cells = strip_cell_count(&db, id);
        let mut line = div().child(strip);
        if n_cells > 1 {
            line = line.min_w(px(strip_row_width(n_cells)));
        }
        line.style().flex_shrink = Some(0.0);
        let mut scroll = div()
            .id(("outline-strip", row_ix))
            .flex()
            .items_center()
            .h_full()
            .px(px(4.0))
            .overflow_x_scroll()
            .child(line);
        scroll.style().restrict_scroll_to_axis = Some(true);
        scroll.into_any_element()
    }

    fn render_text(&self, text: Option<String>, cx: &mut Context<Table<Self>>) -> AnyElement {
        let theme = theme(cx);
        div()
            .px(px(8.0))
            .text_size(px(12.0))
            .text_color(theme.text_secondary)
            .whitespace_nowrap()
            .children(text.map(SharedString::from))
            .into_any_element()
    }
}

impl TableDelegate for OutlineDelegate {
    fn columns(&self) -> Vec<Column> {
        let mut cols = vec![
            Column::new("Name", 280.0).min_width(140.0),
            Column::new("Unit", 64.0),
            Column::new("Type", 88.0),
        ];
        if self.sparklines {
            cols.push(Column::new("Sparkline", 200.0).min_width(120.0));
        }
        cols.push(Column::new("Value", 320.0).flex().resizable(false));
        cols
    }

    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn row_height(&self) -> Pixels {
        if self.sparklines {
            px(SPARKLINE_ROW_HEIGHT)
        } else {
            px(ROW_HEIGHT)
        }
    }

    fn frame_rendered(&mut self) {
        self.strips.prune();
        self.sparks.prune();
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(row_ix).cloned() else {
            return div().into_any_element();
        };
        match Col::at(col_ix, self.sparklines) {
            Col::Name => self.render_name(row_ix, &row, cx),
            Col::Value => self.render_value(row_ix, &row, cx),
            Col::Unit => {
                let unit = row.node.component_id.and_then(|id| unit(&self.db, id));
                self.render_text(unit, cx)
            }
            Col::Type => {
                let ty = row
                    .node
                    .component_id
                    .and_then(|id| type_label(&self.db, id));
                self.render_text(ty, cx)
            }
            Col::Sparkline => {
                let Some(id) = row.node.component_id else {
                    return div().into_any_element();
                };
                let plot = self.sparkline(id, cx);
                div()
                    .w_full()
                    .h(self.row_height() - px(8.0))
                    .child(plot)
                    .into_any_element()
            }
        }
    }

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &App) {}
}

/// An empty tree to hold until the watcher publishes the first real one.
fn build_tree_root() -> Arc<ComponentNode> {
    Arc::new(ComponentNode {
        segment: SharedString::default(),
        full_name: SharedString::default(),
        component_id: None,
        children: Default::default(),
    })
}

fn element_count(db: &DB, id: ComponentId) -> usize {
    db.with_state(|state| {
        state
            .get_component(id)
            .map(|c| c.schema.dim.iter().product::<usize>().max(1))
            .unwrap_or(1)
    })
}

fn unit(db: &DB, id: ComponentId) -> Option<String> {
    db.with_state(|state| {
        state
            .get_component_metadata(id)
            .and_then(|m| m.metadata.get("unit").cloned())
    })
}

/// `f64` for a scalar, `f32[3]` for a vector, `u8[4×4]` for a matrix.
fn type_label(db: &DB, id: ComponentId) -> Option<String> {
    db.with_state(|state| {
        let schema = &state.get_component(id)?.schema;
        let mut label = schema.prim_type.as_str().to_string();
        if !schema.dim.is_empty() {
            let dims: Vec<String> = schema.dim.iter().map(|d| d.to_string()).collect();
            label.push('[');
            label.push_str(&dims.join("×"));
            label.push(']');
        }
        Some(label)
    })
}
