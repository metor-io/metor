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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Render, ScrollHandle, SharedString, Window, div, prelude::*, px,
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
use crate::inspector::rows::{BoolRow, CommandRow, InspectorRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::query::Query;
use crate::theme::theme;
use model::{
    Disclosure, FrameType, Layout, OutlineRow, Pivot, RowKind, alike, common_suffix,
    component_count, flatten, signature, type_key,
};

/// Indent per tree depth, in pixels.
const INDENT: f32 = 14.0;
const ROW_HEIGHT: f32 = 30.0;
/// Row height once sparklines are on — enough for a readable trace.
const SPARKLINE_ROW_HEIGHT: f32 = 44.0;
/// Horizontal padding inside one pivot cell.
const PIVOT_CELL_PAD: f32 = 8.0;
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

    pub fn columns(&self, cx: &App) -> OutlineColumns {
        self.table.read(cx).delegate().columns
    }

    pub fn set_columns(&mut self, columns: OutlineColumns, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_columns(columns, cx);
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

    /// Branches shown as instances × fields, for persistence.
    pub fn pivoted_paths(&self, cx: &App) -> Vec<String> {
        let mut out: Vec<String> = self
            .table
            .read(cx)
            .delegate()
            .pivoted
            .iter()
            .map(|p| p.to_string())
            .collect();
        out.sort();
        out
    }

    pub fn set_pivoted_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.pivoted = paths.into_iter().map(SharedString::from).collect();
            delegate.reflatten(cx);
        });
    }

    /// Frame types as `(label, fields)`, for persistence.
    pub fn types(&self, cx: &App) -> Vec<(String, Vec<String>)> {
        self.table
            .read(cx)
            .delegate()
            .types
            .iter()
            .map(|t| {
                (
                    t.label.to_string(),
                    t.fields.iter().map(|f| f.to_string()).collect(),
                )
            })
            .collect()
    }

    pub fn set_types(&mut self, types: Vec<(String, Vec<String>)>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.types = types
                .into_iter()
                .map(|(label, fields)| FrameType {
                    label: SharedString::from(label),
                    fields: fields.into_iter().map(SharedString::from).collect(),
                })
                .collect();
            delegate.reflatten(cx);
        });
    }

    /// The type label the outline is focused on, if any.
    pub fn focus(&self, cx: &App) -> Option<String> {
        self.table
            .read(cx)
            .delegate()
            .focus
            .as_ref()
            .map(|f| f.to_string())
    }

    pub fn set_focus(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table
                .delegate_mut()
                .set_focus(label.map(SharedString::from), cx);
        });
        cx.notify();
    }

    /// The strip shown while focused on a type: what's showing, and the
    /// way back.
    fn render_focus_bar(&self, label: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(24.0))
            .px(px(8.0))
            .gap(px(6.0))
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(11.0))
            .text_color(theme.text_secondary)
            .child(SharedString::new_static("Focused on"))
            .child(div().text_color(theme.text_primary).child(label))
            .child(div().flex_1())
            .child(
                div()
                    .id("outline-unfocus")
                    .cursor_pointer()
                    .text_color(theme.text_tertiary)
                    .hover(|s| s.text_color(theme.text_primary))
                    .child(SharedString::new_static("Show all"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_focus(None, cx))),
            )
            .into_any_element()
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
        let focus = self.table.read(cx).delegate().focus.clone();
        let focus_bar = focus.map(|label| self.render_focus_bar(label, cx));

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
            .children(focus_bar)
            .child(div().flex_1().min_h_0().w_full().child(self.table.clone()))
    }
}

/// The inspector's column toggles for an outline pane, one per optional
/// column.
pub fn column_rows(outline: Entity<ComponentOutline>) -> Vec<BoolRow> {
    let toggles: [(&str, fn(&mut OutlineColumns) -> &mut bool); 3] = [
        ("Unit column", |c| &mut c.unit),
        ("Type column", |c| &mut c.ty),
        ("Sparklines", |c| &mut c.sparkline),
    ];
    toggles
        .into_iter()
        .map(|(label, field)| {
            let read = outline.clone();
            let write = outline.clone();
            BoolRow::dynamic(
                label,
                Arc::new(move |cx| *field(&mut read.read(cx).columns(cx))),
                Arc::new(move |on, _window, cx| {
                    write.update(cx, |outline, cx| {
                        let mut columns = outline.columns(cx);
                        *field(&mut columns) = on;
                        outline.set_columns(columns, cx);
                    });
                }),
            )
        })
        .collect()
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
#[derive(Clone, Copy)]
enum Col {
    Name,
    Unit,
    Type,
    Sparkline,
    Value,
}

/// Which optional columns the outline shows. Name and Value always do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OutlineColumns {
    pub unit: bool,
    pub ty: bool,
    pub sparkline: bool,
}

impl Default for OutlineColumns {
    fn default() -> Self {
        Self {
            unit: true,
            ty: true,
            sparkline: false,
        }
    }
}

impl OutlineColumns {
    fn order(self) -> SmallVec<[Col; 5]> {
        let mut cols: SmallVec<[Col; 5]> = SmallVec::new();
        cols.push(Col::Name);
        if self.unit {
            cols.push(Col::Unit);
        }
        if self.ty {
            cols.push(Col::Type);
        }
        if self.sparkline {
            cols.push(Col::Sparkline);
        }
        cols.push(Col::Value);
        cols
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
    /// Branches rotated into a grid; a pivot only shows while its branch
    /// is open, so folding one keeps the choice for when it reopens.
    pivoted: HashSet<SharedString>,
    /// One scroll offset per pivot, shared by its header and instance rows
    /// so the grid scrolls sideways as a unit.
    pivot_scrolls: HashMap<SharedString, ScrollHandle>,
    /// Frame types the user has collected, shown above the tree.
    types: Vec<FrameType>,
    /// A type label shown alone.
    focus: Option<SharedString>,
    rows: Vec<OutlineRow>,
    columns: OutlineColumns,
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
            pivoted: HashSet::new(),
            pivot_scrolls: HashMap::new(),
            types: Vec::new(),
            focus: None,
            rows: Vec::new(),
            columns: OutlineColumns::default(),
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
        let db = self.db.clone();
        self.rows = flatten(
            &self.tree,
            &Layout {
                disclosure: &self.disclosure,
                pivoted: &self.pivoted,
                types: &self.types,
                focus: self.focus.as_ref(),
                query: &self.query,
                cell_count: &|id| strip_cell_count(&db, id),
            },
        );
        cx.notify();
    }

    /// Pivoting opens the branch too — a pivot behind a fold would look
    /// like nothing happened.
    fn set_pivoted(&mut self, row: &OutlineRow, on: bool, cx: &mut Context<Table<Self>>) {
        let path = row.node.full_name.clone();
        if on {
            self.pivoted.insert(path.clone());
            self.disclosure.set_expanded(&path, row.depth, true);
        } else {
            self.pivoted.remove(&path);
            self.pivot_scrolls.remove(&path);
        }
        self.reflatten(cx);
    }

    /// Collect every frame shaped like `exemplar` into a type named after
    /// it. A second type with the same name gets a numbered label so both
    /// keep distinct keys.
    fn add_type(&mut self, exemplar: &Arc<ComponentNode>, cx: &mut Context<Table<Self>>) {
        let fields = signature(exemplar);
        if let Some(existing) = self.types.iter().find(|t| t.fields == fields) {
            let key = type_key(&existing.label);
            self.disclosure.set_expanded(&key, 0, true);
            self.reflatten(cx);
            return;
        }
        // Name the type by what its instances share, not by the exemplar:
        // a compressed chain like `alarms.health` would otherwise label a
        // type whose other instances are `nav.health`, `ctrl.health`, …
        let instances = alike(&self.tree, &fields);
        let mut base = common_suffix(instances.iter().map(|n| n.full_name.as_ref()));
        if base.is_empty() {
            base = exemplar
                .full_name
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string();
        }
        let mut label = base.clone();
        let mut n = 2;
        while self.types.iter().any(|t| t.label.as_ref() == label) {
            label = format!("{base} ({n})");
            n += 1;
        }
        let label = SharedString::from(label);
        self.disclosure.set_expanded(&type_key(&label), 0, true);
        self.types.push(FrameType { label, fields });
        self.reflatten(cx);
    }

    fn remove_type(&mut self, label: &SharedString, cx: &mut Context<Table<Self>>) {
        self.types.retain(|t| &t.label != label);
        self.pivot_scrolls.remove(&type_key(label));
        if self.focus.as_ref() == Some(label) {
            self.focus = None;
        }
        self.reflatten(cx);
    }

    fn set_focus(&mut self, label: Option<SharedString>, cx: &mut Context<Table<Self>>) {
        self.focus = label;
        self.reflatten(cx);
    }

    /// The type a synthetic branch row stands for.
    fn type_label(&self, row: &OutlineRow) -> Option<SharedString> {
        self.types
            .iter()
            .find(|t| type_key(&t.label) == row.node.full_name)
            .map(|t| t.label.clone())
    }

    fn pivot_scroll(&mut self, path: &SharedString) -> ScrollHandle {
        self.pivot_scrolls.entry(path.clone()).or_default().clone()
    }

    /// Right-click on a branch: pivot it, or open and fold its subtree.
    fn open_branch_menu(
        &mut self,
        row: &OutlineRow,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) {
        let Some(open) = open_inspector(cx) else {
            return;
        };
        let table = cx.entity();
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        if let Some(label) = self.type_label(row) {
            let focused = self.focus.as_ref() == Some(&label);
            let (focus_label, next) = if focused {
                ("Show all", None)
            } else {
                ("Focus", Some(label.clone()))
            };
            let focus_table = table.clone();
            rows.push(Box::new(CommandRow::new(
                focus_label,
                Arc::new(move |_window, cx| {
                    focus_table.update(cx, |table, cx| {
                        table.delegate_mut().set_focus(next.clone(), cx);
                    });
                }),
            )));
            rows.push(Box::new(CommandRow::new(
                "Remove type",
                Arc::new(move |_window, cx| {
                    table.update(cx, |table, cx| {
                        table.delegate_mut().remove_type(&label, cx);
                    });
                }),
            )));
            open(
                InspectorRequest {
                    rows,
                    mode: InspectorMode::Anchored(position),
                },
                window,
                cx,
            );
            return;
        }
        let pivoted = row.is_pivoted();
        let can_pivot = row.node.children.values().any(|c| !c.children.is_empty());
        if can_pivot {
            let label = if pivoted { "Unpivot" } else { "Pivot" };
            let target = row.clone();
            let table = table.clone();
            rows.push(Box::new(CommandRow::new(
                label,
                Arc::new(move |_window, cx| {
                    table.update(cx, |table, cx| {
                        table.delegate_mut().set_pivoted(&target, !pivoted, cx);
                    });
                }),
            )));
        }
        // Any branch with components has a shape worth collecting.
        if row.component_count > usize::from(row.node.component_id.is_some()) {
            let exemplar = row.node.clone();
            let table = table.clone();
            rows.push(Box::new(CommandRow::new(
                "Pivot alike frames",
                Arc::new(move |_window, cx| {
                    table.update(cx, |table, cx| {
                        table.delegate_mut().add_type(&exemplar, cx);
                    });
                }),
            )));
        }
        for (label, expanded) in [("Expand all", true), ("Collapse all", false)] {
            let target = row.clone();
            let table = table.clone();
            rows.push(Box::new(CommandRow::new(
                label,
                Arc::new(move |_window, cx| {
                    table.update(cx, |table, cx| {
                        let delegate = table.delegate_mut();
                        delegate
                            .disclosure
                            .set_subtree(&target.node, target.depth, expanded);
                        delegate.reflatten(cx);
                    });
                }),
            )));
        }
        open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Anchored(position),
            },
            window,
            cx,
        );
    }

    fn set_query(&mut self, query: Query, cx: &mut Context<Table<Self>>) {
        self.query = query;
        self.reflatten(cx);
    }

    fn set_columns(&mut self, columns: OutlineColumns, cx: &mut Context<Table<Self>>) {
        self.columns = columns;
        if !columns.sparkline {
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
        // Instance rows are flat — they don't disclose — but still indent
        // under their branch.
        let (is_branch, label, color) = match &row.kind {
            RowKind::PivotInstance { pivot, ix } => (
                false,
                pivot.instances[*ix].label.clone(),
                theme.text_primary,
            ),
            _ => (
                row.is_branch(),
                row.node.segment.clone(),
                theme.text_primary,
            ),
        };

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
            .text_color(color)
            .child(slot)
            .child(
                div()
                    .flex_shrink_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(label),
            );
        // The name keeps its width; a tight column truncates the count.
        if let RowKind::PivotBranch(pivot) = &row.kind {
            cell = cell.child(
                div()
                    .ml(px(4.0))
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(format!(
                        "{} × {}",
                        count_label(pivot.instances.len(), "instance"),
                        count_label(pivot.fields.len(), "field")
                    ))),
            );
        }

        if is_branch {
            let menu_row = row.clone();
            cell = cell
                .cursor_pointer()
                .on_click(cx.listener(move |table, event: &ClickEvent, _, cx| {
                    let subtree = event.modifiers().alt;
                    table.delegate_mut().toggle_row(row_ix, subtree, cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |table, event: &MouseDownEvent, window, cx| {
                        table.delegate_mut().open_branch_menu(
                            &menu_row,
                            event.position,
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }),
                );
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
        match &row.kind {
            // An open pivot's branch row doubles as its header; a folded
            // one (a type kept closed) has nothing to align to.
            RowKind::PivotBranch(pivot) if row.expanded => {
                return self.render_pivot_header(row_ix, pivot, cx);
            }
            RowKind::PivotBranch(_) => return div().into_any_element(),
            RowKind::PivotInstance { pivot, ix } => {
                let pivot = pivot.clone();
                return self.render_pivot_cells(row_ix, &pivot, *ix, cx);
            }
            _ => {}
        }
        let Some(id) = row.node.component_id else {
            let label = count_label(row.component_count, "component");
            return div()
                .flex()
                .items_center()
                .h_full()
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

    /// The field labels of a pivot, laid out on the same cell grid as the
    /// instance rows beneath and sharing their scroll offset.
    fn render_pivot_header(
        &mut self,
        row_ix: usize,
        pivot: &Pivot,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let scroll = self.pivot_scroll(&pivot.key);
        let cells = pivot.fields.iter().zip(&pivot.cells).map(|(field, &n)| {
            div()
                .w(px(pivot_cell_width(n, field)))
                .h_full()
                .flex()
                .items_center()
                .flex_shrink_0()
                .border_r_1()
                .border_color(theme.border_primary)
                .px(px(PIVOT_CELL_PAD / 2.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .text_color(theme.text_tertiary)
                .child(field.clone())
        });
        pivot_scroller(row_ix, scroll)
            .children(cells)
            .into_any_element()
    }

    /// One instance's strips, one fixed-width cell per field.
    fn render_pivot_cells(
        &mut self,
        row_ix: usize,
        pivot: &Pivot,
        ix: usize,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let scroll = self.pivot_scroll(&pivot.key);
        let instance = &pivot.instances[ix];
        let db = self.db.clone();
        let mut cells: Vec<AnyElement> = Vec::with_capacity(pivot.fields.len());
        for (field_ix, (field, &n)) in pivot.fields.iter().zip(&pivot.cells).enumerate() {
            let cell = div()
                .w(px(pivot_cell_width(n, field)))
                .h_full()
                .flex_shrink_0()
                .border_r_1()
                .border_color(theme.border_primary)
                .px(px(PIVOT_CELL_PAD / 2.0))
                .flex()
                .items_center();
            let Some(id) = instance.ids[field_ix] else {
                cells.push(
                    cell.text_size(px(12.0))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::new_static("—"))
                        .into_any_element(),
                );
                continue;
            };
            let strip = self.strip(id, cx);
            let name = SharedString::from(format!("{}.{}", instance.node.full_name, field));
            let click = edit_click(db.clone(), id, name);
            let mut behavior = behavior_snapshot(cx, db.clone(), id, click);
            behavior.on_element_right_click = Some(right_click_plot(db.clone(), id));
            strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
            // Hold the strip to one line: the cell is sized for it, and
            // an element that still doesn't fit should clip, not wrap.
            let mut line = div().min_w(px(strip_row_width(n))).child(strip);
            line.style().flex_shrink = Some(0.0);
            cells.push(cell.child(line).into_any_element());
        }
        pivot_scroller(row_ix, scroll)
            .children(cells)
            .into_any_element()
    }

    fn render_text(&self, text: Option<String>, cx: &mut Context<Table<Self>>) -> AnyElement {
        let theme = theme(cx);
        div()
            .flex()
            .items_center()
            .h_full()
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
        self.columns
            .order()
            .into_iter()
            .map(|col| match col {
                Col::Name => Column::new("Name", 280.0).min_width(140.0),
                Col::Unit => Column::new("Unit", 64.0),
                Col::Type => Column::new("Type", 88.0),
                Col::Sparkline => Column::new("Sparkline", 200.0).min_width(120.0),
                Col::Value => Column::new("Value", 320.0).flex().resizable(false),
            })
            .collect()
    }

    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn row_height(&self) -> Pixels {
        if self.columns.sparkline {
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
        let Some(col) = self.columns.order().get(col_ix).copied() else {
            return div().into_any_element();
        };
        match col {
            Col::Name => self.render_name(row_ix, &row, cx),
            Col::Value => self.render_value(row_ix, &row, cx),
            Col::Unit | Col::Type | Col::Sparkline
                if matches!(row.kind, RowKind::PivotInstance { .. }) =>
            {
                div().into_any_element()
            }
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

/// Approximate advance of one character of the 11px label face; the cell
/// only needs to be roomy enough that a label isn't clipped.
const LABEL_CHAR_WIDTH: f32 = 6.8;

/// Width of a pivot cell: the wider of its `n`-element strip and its label,
/// plus padding and the divider — a pixel short and the strip wraps.
fn pivot_cell_width(n: usize, label: &str) -> f32 {
    let strip = strip_row_width(n.max(1));
    let text = label.chars().count() as f32 * LABEL_CHAR_WIDTH;
    strip.max(text) + PIVOT_CELL_PAD + 1.0
}

/// The sideways-scrolling row container every pivot row shares the offset
/// of. Axis-restricted so the wheel still scrolls the table vertically.
fn pivot_scroller(row_ix: usize, scroll: ScrollHandle) -> gpui::Stateful<gpui::Div> {
    let mut scroller = div()
        .id(("outline-pivot", row_ix))
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .overflow_x_scroll()
        .track_scroll(&scroll);
    scroller.style().restrict_scroll_to_axis = Some(true);
    scroller
}

/// `"1 field"` / `"3 fields"`.
fn count_label(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
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
