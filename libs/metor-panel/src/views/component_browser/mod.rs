use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, IntoElement, MouseButton, SharedString,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::{SmallVec, smallvec};

pub mod component_tree;

use super::column_browser::{ColumnBrowser, ColumnBrowserDelegate};
use super::copy::{copy_name_row, copy_rows};
use super::lazy_pool::VisibleEntityCache;
use super::monitor::{behavior_snapshot, edit_click};
use super::time_series::Override;
use super::value_strip::{
    ComponentValueStrip, StripBehavior, StripClick, StripStyle, strip_row_width,
};
use crate::icons::Icon;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::query::Query;
use crate::theme::{Theme, theme};
use crate::tiles::PlotComponentAction;
use component_tree::{Children, ComponentNode, build_tree, compress_subtree, resolve_path};

/// [`ColumnBrowser`] specialized for the DB component namespace.
pub type ComponentBrowser = ColumnBrowser<ComponentBrowserDelegate>;

/// Event raised on Enter / double-click of a leaf component.
///
/// Consumers decide how to act on an activation. The bundled `BrowserPanel`
/// ignores it; other hosts can use it to wire up navigation or plotting.
#[derive(Clone, Copy)]
pub enum BrowserEvent {
    Activated(ComponentId),
}

impl EventEmitter<BrowserEvent> for ColumnBrowser<ComponentBrowserDelegate> {}

/// A view into the namespace — one the user saved, or the one the filter
/// bar is typing.
///
/// `synth` is a pruned mirror of the real tree kept only to branches whose
/// full names match `query`; it's refreshed on filter add and on every
/// real-tree rebuild so lookups are O(1) cache reads instead of recursing
/// the whole namespace per render.
struct FilterEntry {
    label: SharedString,
    query: Query,
    synth: Arc<ComponentNode>,
}

/// Which tree the current selection is navigating.
///
/// `Filter(label)` means the user drilled into the synthetic filter row at
/// the real tree root; `Live` means the filter bar has a query and the
/// columns start inside its pruned tree; `Real` means straight real-tree
/// navigation (with or without a reroot).
#[derive(Clone)]
enum SelectionRoot {
    Real,
    Filter(SharedString),
    Live,
}

/// Absolute selection path plus the root context it's interpreted under.
///
/// Under `Real`, `path[0]` is a real top-level segment and the leading
/// `root_override.len()` entries (when set) must match the reroot prefix —
/// mirroring the pre-refactor absolute-path model.
///
/// Under `Filter`, `path[0]` is the filter label and `path[1..]` are
/// segments inside the filter's pruned synth tree.
struct Selection {
    root: SelectionRoot,
    path: SmallVec<[SharedString; 8]>,
    root_override: Option<SmallVec<[SharedString; 8]>>,
}

impl Selection {
    fn empty() -> Self {
        Self {
            root: SelectionRoot::Real,
            path: SmallVec::new(),
            root_override: None,
        }
    }
}

/// Browses the dot-delimited component namespace.
///
/// Selection is stored as absolute path segments, so a DB rebuild (or a
/// reroot via `set_root_override`) can't orphan it. The detail column is
/// always rendered and shows the union of component values at or below the
/// current tail, or the effective root when selection is empty.
pub struct ComponentBrowserDelegate {
    db: Arc<DB>,
    tree: Arc<ComponentNode>,
    selection: Selection,
    filters: Vec<FilterEntry>,
    /// The filter bar's query, pruned to a tree the columns navigate while
    /// it is non-empty. Built against the rerooted tree, so filtering
    /// inside a reroot narrows what the reroot shows.
    live: Option<FilterEntry>,
    /// Components in the whole namespace, for the bar's `shown / total`.
    component_total: usize,
    /// Flat, name-sorted list of every component under the current detail
    /// target. Cheap to rebuild on selection change; live strips are
    /// materialized lazily for the visible range via `strip_cache`.
    detail_components: Vec<(ComponentId, SharedString)>,
    strip_cache: VisibleEntityCache<ComponentValueStrip>,
    detail_scroll_handle: UniformListScrollHandle,
    custom_title: Override<SharedString>,
    /// Re-root path waiting for the watcher to publish a tree it can
    /// resolve against. Set by [`Self::set_pending_root_path`] (typically
    /// from a config restore); cleared by a successful [`Self::set_root_path`]
    /// or by any user-driven `clear_root_override` / `set_root_override`
    /// so the user's intent wins over a pending restore.
    pending_root_path: Option<SmallVec<[SharedString; 8]>>,
    _watcher: gpui::Task<()>,
}

impl ComponentBrowserDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<ComponentBrowser>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let tree = build_tree(&db);
                let result = this.update(cx, |browser, cx| {
                    let delegate = browser.delegate_mut();
                    delegate.tree = tree;
                    delegate.refresh_filter_synths();
                    if let Some(segs) = delegate.pending_root_path.clone() {
                        delegate.set_root_path(&segs, cx);
                    }
                    delegate.rebuild_detail_list(cx);
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
                // vtable_gen bumps once per registered component, so a startup
                // burst would rebuild the whole tree N times. Debounce so a
                // burst collapses into one rebuild (build_tree reads the latest
                // full state regardless).
                cx.background_executor().timer(VTABLE_DEBOUNCE).await;
            }
        })
    }

    fn refresh_filter_synths(&mut self) {
        let tree = self.tree.clone();
        for filter in &mut self.filters {
            filter.synth = build_filter_synth(&filter.query, filter.label.clone(), &tree);
        }
        self.component_total = component_count(&tree);
        let root = self.real_root();
        if let Some(live) = &mut self.live {
            live.synth = build_filter_synth(&live.query, live.label.clone(), &root);
        }
    }

    fn find_filter(&self, label: &SharedString) -> Option<&FilterEntry> {
        self.filters.iter().find(|f| f.label == *label)
    }

    /// Real-tree node anchoring non-filter navigation (after any reroot).
    fn real_root(&self) -> Arc<ComponentNode> {
        match &self.selection.root_override {
            Some(p) => resolve_path(&self.tree, p)
                .last()
                .cloned()
                .unwrap_or_else(|| self.tree.clone()),
            None => self.tree.clone(),
        }
    }

    /// How many leading `selection.path` entries the reroot accounts for.
    /// Only real-tree paths carry the reroot prefix; filter and live paths
    /// start at their synth root.
    fn override_depth(&self) -> usize {
        match self.selection.root {
            SelectionRoot::Real => self
                .selection
                .root_override
                .as_ref()
                .map(|p| p.len())
                .unwrap_or(0),
            SelectionRoot::Filter(_) | SelectionRoot::Live => 0,
        }
    }

    fn resolved_chain(&self) -> SmallVec<[Arc<ComponentNode>; 8]> {
        match &self.selection.root {
            SelectionRoot::Real => {
                let mut out = SmallVec::new();
                let mut current = self.real_root();
                for seg in self.selection.path.iter().skip(self.override_depth()) {
                    let Some(next) = current.children.get(seg).cloned() else {
                        break;
                    };
                    out.push(next.clone());
                    current = next;
                }
                out
            }
            SelectionRoot::Filter(label) => {
                let Some(filter) = self.find_filter(label) else {
                    return SmallVec::new();
                };
                // `path[0]` is the filter label — emit the synth root as
                // the column-0 selection so column layout matches real
                // navigation one-for-one.
                let mut out = SmallVec::new();
                out.push(filter.synth.clone());
                let mut current = filter.synth.clone();
                for seg in self.selection.path.iter().skip(1) {
                    let Some(next) = current.children.get(seg).cloned() else {
                        break;
                    };
                    out.push(next.clone());
                    current = next;
                }
                out
            }
            SelectionRoot::Live => {
                let Some(live) = &self.live else {
                    return SmallVec::new();
                };
                let mut out = SmallVec::new();
                let mut current = live.synth.clone();
                for seg in &self.selection.path {
                    let Some(next) = current.children.get(seg).cloned() else {
                        break;
                    };
                    out.push(next.clone());
                    current = next;
                }
                out
            }
        }
    }

    /// Append a new filter with `label` and `pattern`, read as a
    /// [`Query`]. On failure (an empty pattern or a label collision with
    /// another filter or a top-level real segment), returns an error
    /// message suitable for inspector surface.
    fn add_filter(
        &mut self,
        label: SharedString,
        pattern: SharedString,
        cx: &mut Context<ComponentBrowser>,
    ) -> Result<(), SharedString> {
        if label.is_empty() {
            return Err(SharedString::new_static("filter label cannot be empty"));
        }
        if self.tree.children.get(&label).is_some() {
            return Err(SharedString::new_static(
                "filter label collides with a top-level prefix",
            ));
        }
        if self.filters.iter().any(|f| f.label == label) {
            return Err(SharedString::new_static("filter label already in use"));
        }
        let query = Query::parse(pattern.as_ref());
        if !query.has_terms() {
            return Err(SharedString::new_static("filter pattern cannot be empty"));
        }
        let synth = build_filter_synth(&query, label.clone(), &self.tree);
        self.filters.push(FilterEntry {
            label,
            query,
            synth,
        });
        self.rebuild_detail_list(cx);
        cx.notify();
        Ok(())
    }

    fn remove_filter(&mut self, label: &SharedString, cx: &mut Context<ComponentBrowser>) {
        let before = self.filters.len();
        self.filters.retain(|f| f.label != *label);
        if self.filters.len() != before {
            if matches!(&self.selection.root, SelectionRoot::Filter(l) if l == label) {
                self.selection = Selection::empty();
            }
            self.rebuild_detail_list(cx);
            cx.notify();
        }
    }

    /// Assemble the rows shown by the right-click inspector.
    ///
    /// `browser` is a weak handle to `self`'s owning view so the row
    /// callbacks can schedule back into the delegate without capturing a
    /// strong cycle.
    fn build_context_rows(
        &self,
        column_ix: usize,
        item: Option<&Arc<ComponentNode>>,
        browser: gpui::WeakEntity<ComponentBrowser>,
    ) -> Vec<Box<dyn InspectorRow>> {
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        let live = self.live.is_some();

        // "Add filter…": always available. Cascades into a page whose
        // only row is a `DefaultActionRow` that consumes the search
        // field text directly — the user types the pattern into the
        // search bar and presses Enter. Commit parses `name=pattern`
        // (falling back to pattern-only) and installs the filter.
        {
            let browser = browser.clone();
            rows.push(Box::new(NavRow::new(
                SharedString::new_static("Add filter…"),
                SharedString::new_static(""),
                Box::new(move |_cx| {
                    let browser = browser.clone();
                    vec![Box::new(DefaultActionRow::new(
                        SharedString::new_static("label = pattern (e.g. *.health)"),
                        Arc::new(move |input, _window, cx| {
                            let trimmed = input.trim();
                            if trimmed.is_empty() {
                                return;
                            }
                            let (label, pattern) = match trimmed.split_once('=') {
                                Some((lhs, rhs)) => {
                                    (lhs.trim().to_string(), rhs.trim().to_string())
                                }
                                None => (trimmed.to_string(), trimmed.to_string()),
                            };
                            if let Some(browser) = browser.upgrade() {
                                browser.update(cx, |browser, cx| {
                                    let _ = browser.delegate_mut().add_filter(
                                        SharedString::from(label),
                                        SharedString::from(pattern),
                                        cx,
                                    );
                                });
                            }
                        }),
                    )) as Box<dyn InspectorRow>]
                }),
            )));
        }

        if let Some(node) = item {
            // A column-0 row whose segment matches a filter label (only
            // possible when no reroot or live query is active, since neither
            // surfaces filter siblings) is the synthetic filter root — offer
            // Remove instead of the real-component actions.
            let filter_label = if column_ix == 0 && self.selection.root_override.is_none() && !live
            {
                self.find_filter(&node.segment).map(|f| f.label.clone())
            } else {
                None
            };
            if let Some(label) = filter_label {
                let browser_remove = browser.clone();
                rows.push(Box::new(CommandRow::new(
                    SharedString::from(format!("Remove filter \"{}\"", label)),
                    Arc::new(move |_window, cx| {
                        if let Some(browser) = browser_remove.upgrade() {
                            let label = label.clone();
                            browser.update(cx, |browser, cx| {
                                browser.delegate_mut().remove_filter(&label, cx);
                            });
                        }
                    }),
                )));
            } else if let Some(component_id) = node.component_id
                && node.children.is_empty()
            {
                let db = self.db.clone();
                let db_for_plot = db.clone();
                rows.push(Box::new(CommandRow::new(
                    SharedString::new_static("Plot component"),
                    Arc::new(move |window, cx| {
                        let count = element_count(&db_for_plot, component_id);
                        let indices: SmallVec<[usize; 4]> = (0..count).collect();
                        window.dispatch_action(
                            Box::new(PlotComponentAction {
                                component_id,
                                indices,
                            }),
                            cx,
                        );
                    }),
                )));
                rows.extend(copy_rows(db, component_id, node.full_name.clone(), None));
            } else {
                let reroot_column = column_ix;
                let browser_reroot = browser.clone();
                let label = node.full_name.clone();
                rows.push(Box::new(CommandRow::new(
                    SharedString::from(format!("Reroot to \"{}\"", label)),
                    Arc::new(move |_window, cx| {
                        if let Some(browser) = browser_reroot.upgrade() {
                            browser.update(cx, |browser, cx| {
                                browser.apply_root_override(reroot_column + 1, cx);
                            });
                        }
                    }),
                )));
                match node.component_id {
                    Some(id) => {
                        rows.extend(copy_rows(self.db.clone(), id, node.full_name.clone(), None))
                    }
                    None => rows.push(copy_name_row(node.full_name.clone())),
                }
            }
        }

        if self.selection.root_override.is_some() {
            let browser_clear = browser.clone();
            rows.push(Box::new(CommandRow::new(
                SharedString::new_static("Clear reroot"),
                Arc::new(move |_window, cx| {
                    if let Some(browser) = browser_clear.upgrade() {
                        browser.update(cx, |browser, cx| {
                            browser.apply_root_override(0, cx);
                        });
                    }
                }),
            )));
        }

        rows
    }

    fn detail_target(&self) -> Arc<ComponentNode> {
        self.resolved_chain().last().cloned().unwrap_or_else(|| {
            self.live
                .as_ref()
                .map(|live| live.synth.clone())
                .unwrap_or_else(|| self.real_root())
        })
    }

    /// Rebuild the flat list of components under the current detail target.
    ///
    /// Only collects ids and names — no entities or streams. The detail view
    /// materializes a `ComponentValueStrip` lazily for each *visible* row, so
    /// selecting a node with thousands of descendants (e.g. the root) is cheap
    /// and revisiting a node reuses its cached, still-live strip.
    fn rebuild_detail_list(&mut self, cx: &mut Context<ComponentBrowser>) {
        let target = self.detail_target();
        let mut nodes: Vec<Arc<ComponentNode>> = Vec::new();
        collect_component_nodes(&target, &mut nodes);
        let mut components: Vec<(ComponentId, SharedString)> = nodes
            .into_iter()
            .filter_map(|n| n.component_id.map(|id| (id, n.full_name.clone())))
            .collect();
        // Match the prior name-sorted display order (`previews` was keyed by
        // full name); DFS order is otherwise close but not identical.
        components.sort_by(|a, b| a.1.cmp(&b.1));
        self.detail_components = components;
        cx.notify();
    }

    /// Effective title for the host panel. Custom override wins; otherwise
    /// the auto title reflects the current root.
    pub fn title(&self) -> SharedString {
        match &self.custom_title {
            Override::Custom(t) => t.clone(),
            Override::Auto => self.derive_title(),
        }
    }

    fn derive_title(&self) -> SharedString {
        match &self.selection.root {
            SelectionRoot::Filter(label) => label.clone(),
            SelectionRoot::Real | SelectionRoot::Live => match &self.selection.root_override {
                Some(segs) if !segs.is_empty() => SharedString::from(
                    segs.iter()
                        .map(|s| s.as_ref())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => SharedString::new_static("Components"),
            },
        }
    }

    pub fn custom_title(&self) -> &Override<SharedString> {
        &self.custom_title
    }

    pub fn set_custom_title(
        &mut self,
        title: Override<SharedString>,
        cx: &mut Context<ComponentBrowser>,
    ) {
        self.custom_title = title;
        cx.notify();
    }

    pub fn root_override(&self) -> Option<&[SharedString]> {
        self.selection.root_override.as_deref()
    }

    /// Restore a re-root from persisted segments. Resolves the chain
    /// against the real tree; if the tree hasn't populated yet or the
    /// segments don't match, the call is a no-op. Pair with
    /// [`Self::set_pending_root_path`] so the watcher retries the apply
    /// each time it publishes a fresh tree.
    pub fn set_root_path(&mut self, segs: &[SharedString], cx: &mut Context<ComponentBrowser>) {
        if segs.is_empty() {
            return;
        }
        let chain = resolve_path(&self.tree, segs);
        if chain.len() != segs.len() {
            return;
        }
        if !matches!(self.selection.root, SelectionRoot::Real) {
            return;
        }
        let segments: SmallVec<[SharedString; 8]> =
            chain.iter().map(|n| n.segment.clone()).collect();
        if !self.selection.path.starts_with(&segments[..]) {
            self.selection.path = segments.clone();
        }
        self.selection.root_override = Some(segments);
        self.pending_root_path = None;
        self.rebuild_detail_list(cx);
        cx.notify();
    }

    /// Stash a re-root path to apply once the watcher's next tree refresh
    /// includes the requested segments. Used by config restore: the tree
    /// may be empty when the panel is first constructed, so a one-shot
    /// `set_root_path` would silently fail.
    pub fn set_pending_root_path(&mut self, segs: Option<SmallVec<[SharedString; 8]>>) {
        self.pending_root_path = segs;
    }
}

impl ComponentBrowser {
    pub fn title(&self) -> SharedString {
        self.delegate().title()
    }
}

/// The right-click menu of a strip cell: plot the element, or copy it or
/// the component's name.
pub(crate) fn strip_menu(db: Arc<DB>, component_id: ComponentId) -> StripClick {
    Arc::new(move |element_index, position, window, cx| {
        let Some(open) = open_inspector(cx) else {
            return;
        };
        let name = component_name(&db, component_id);
        let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(CommandRow::new(
            SharedString::from(format!("Plot element [{}]", element_index)),
            Arc::new(move |window, cx| {
                window.dispatch_action(
                    Box::new(PlotComponentAction {
                        component_id,
                        indices: smallvec![element_index],
                    }),
                    cx,
                );
            }),
        ))];
        rows.extend(copy_rows(
            db.clone(),
            component_id,
            name,
            Some(element_index),
        ));
        open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Anchored(position),
            },
            window,
            cx,
        );
    })
}

/// A component's registered name, or empty until it registers.
fn component_name(db: &DB, component_id: ComponentId) -> SharedString {
    db.with_state(|state| {
        state
            .get_component_metadata(component_id)
            .map(|m| SharedString::from(m.name.clone()))
            .unwrap_or_default()
    })
}

/// Lookup the total element count (`dim.iter().product()`) for a
/// component, defaulting to 1 when the component isn't registered.
fn element_count(db: &DB, component_id: ComponentId) -> usize {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| c.schema.dim.iter().product::<usize>().max(1))
            .unwrap_or(1)
    })
}

/// Number of boxes the detail strip renders for a component: strings and
/// enums collapse into a single cell, everything else gets one per element.
/// Mirrors `format_cells` so [`strip_row_width`] sizing stays in step.
pub(crate) fn strip_cell_count(db: &DB, component_id: ComponentId) -> usize {
    let collapses = db.with_state(|state| {
        state
            .get_component_metadata(component_id)
            .map(|m| m.is_string() || m.enum_variants().is_some())
            .unwrap_or(false)
    });
    if collapses {
        1
    } else {
        element_count(db, component_id)
    }
}

impl ColumnBrowserDelegate for ComponentBrowserDelegate {
    type Item = Arc<ComponentNode>;

    fn resolve_selection(&self, _cx: &App) -> SmallVec<[Self::Item; 8]> {
        self.resolved_chain()
    }

    fn root_items(&self, _cx: &App) -> Vec<Self::Item> {
        // A live query replaces column 0 with its matches. Otherwise
        // rerooted columns show only the rerooted node's real children,
        // and at the true root (no override) filter synth roots appear as
        // virtual siblings of the real top-level prefixes — the user sees
        // and enters them from column 0 just like a real prefix.
        if let Some(live) = &self.live {
            return live.synth.children.values().cloned().collect();
        }
        if self.selection.root_override.is_some() {
            return self.real_root().children.values().cloned().collect();
        }
        let mut items: Vec<Self::Item> = self.tree.children.values().cloned().collect();
        for filter in &self.filters {
            items.push(filter.synth.clone());
        }
        items
    }

    fn children(&self, item: &Self::Item, _cx: &App) -> Vec<Self::Item> {
        item.children.values().cloned().collect()
    }

    fn is_leaf(&self, item: &Self::Item) -> bool {
        item.component_id.is_some() && item.children.is_empty()
    }

    fn item_label(&self, item: &Self::Item) -> SharedString {
        item.segment.clone()
    }

    fn shift_hover_action(
        &self,
        item: &Self::Item,
        anchor: gpui::Point<gpui::Pixels>,
        _cx: &App,
    ) -> Option<Box<dyn gpui::Action>> {
        let component_id = item.component_id?;
        Some(Box::new(crate::tiles::PreviewPlotAction {
            component_id,
            indices: SmallVec::new(),
            anchor,
        }))
    }

    fn column_label(&self, parent: Option<&Self::Item>) -> SharedString {
        match parent {
            Some(node) => node.full_name.clone(),
            None => match (&self.live, &self.selection.root_override) {
                (Some(live), _) => live.label.clone(),
                (None, Some(segments)) if !segments.is_empty() => SharedString::from(
                    segments
                        .iter()
                        .map(|s| s.as_ref())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => SharedString::new_static("Components"),
            },
        }
    }

    fn items_equal(&self, a: &Self::Item, b: &Self::Item) -> bool {
        a.full_name == b.full_name
    }

    fn set_selection(
        &mut self,
        column_ix: usize,
        item: &Self::Item,
        cx: &mut Context<ComponentBrowser>,
    ) {
        // Column-0 clicks reset which tree the selection anchors on: a
        // live query anchors on its synth, filter siblings switch into
        // `Filter` mode, real prefixes switch back to `Real`. Reroots
        // short-circuit this since their column 0 never surfaces filter
        // siblings.
        if column_ix == 0 && (self.live.is_some() || self.selection.root_override.is_none()) {
            self.selection.root = if self.live.is_some() {
                SelectionRoot::Live
            } else {
                match self.find_filter(&item.segment) {
                    Some(filter) => SelectionRoot::Filter(filter.label.clone()),
                    None => SelectionRoot::Real,
                }
            };
            self.selection.path.clear();
            self.selection.path.push(item.segment.clone());
            self.rebuild_detail_list(cx);
            return;
        }

        // Deeper clicks truncate-and-push. Node segments can be compound
        // (path-compressed prefixes, filter children's full names) so
        // splitting `full_name` on `.` would miss the real child-map keys.
        let keep = column_ix + self.override_depth();
        self.selection.path.truncate(keep);
        self.selection.path.push(item.segment.clone());
        self.rebuild_detail_list(cx);
    }

    fn set_root_override(
        &mut self,
        ancestors: SmallVec<[Self::Item; 8]>,
        cx: &mut Context<ComponentBrowser>,
    ) {
        // Filter syntheses have no real subtree, so rerooting from
        // inside a filter would dead-end the browser.
        if !matches!(self.selection.root, SelectionRoot::Real) {
            return;
        }
        if ancestors.last().is_none() {
            return;
        }
        // Compressed trees can have compound segments (e.g.
        // `cube_sat.sim`), so dot-splitting `full_name` would miss the
        // real child-map keys. Read the compressed segments straight off
        // the ancestor chain.
        //
        // Under an existing override, `ancestors` comes from
        // `resolved_chain()`, which already walks past `override_depth`
        // — i.e. it carries only the visible suffix. Prepend the active
        // override so the new override is a full path from the real tree
        // root; otherwise `real_root()` would fall back to the full tree
        // and the reroot would silently no-op while still showing its
        // (now stale) title.
        let mut segments: SmallVec<[SharedString; 8]> = self
            .selection
            .root_override
            .as_ref()
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default();
        segments.extend(ancestors.iter().map(|a| a.segment.clone()));
        if !self.selection.path.starts_with(&segments[..]) {
            self.selection.path = segments.clone();
        }
        self.selection.root_override = Some(segments);
        // User-driven reroot supersedes any restore-from-config still in flight.
        self.pending_root_path = None;
        self.rebuild_detail_list(cx);
    }

    fn clear_root_override(&mut self, cx: &mut Context<ComponentBrowser>) {
        // Drop any pending restore first, otherwise the watcher would
        // re-apply it on its next tick and clobber the user's clear.
        self.pending_root_path = None;
        if self.selection.root_override.is_some() {
            self.selection.root_override = None;
            self.rebuild_detail_list(cx);
        }
    }

    fn render_detail(
        &mut self,
        _tail: Option<&Self::Item>,
        _window: &mut Window,
        cx: &mut Context<ComponentBrowser>,
    ) -> Option<AnyElement> {
        let theme = theme(cx);

        if self.detail_components.is_empty() {
            return Some(
                div()
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No values"))
                    .into_any_element(),
            );
        }

        let count = self.detail_components.len();
        let scroll_handle = self.detail_scroll_handle.clone();

        // Materialize a live strip only for the visible range. The cache keeps
        // strips alive across renders and selection changes, so scrolling and
        // revisiting a node reuse existing tasks instead of re-subscribing.
        let list_view = uniform_list(
            "component-detail-list",
            count,
            cx.processor(
                move |this: &mut ComponentBrowser,
                      range: Range<usize>,
                      _window: &mut Window,
                      cx: &mut Context<ComponentBrowser>| {
                    let theme = crate::theme::theme(cx);
                    let delegate = this.delegate_mut();
                    let db = delegate.db.clone();
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        let (id, full_name) = delegate.detail_components[ix].clone();
                        let strip = delegate.strip_cache.get_or_create(id, || {
                            let db = db.clone();
                            cx.new(|cx| {
                                ComponentValueStrip::new(
                                    db,
                                    id,
                                    StripStyle::boxes(),
                                    StripBehavior::default(),
                                    cx,
                                )
                            })
                        });
                        // Refresh the pending-edit snapshot each frame; click
                        // callbacks are cheap Arc captures (`db.clone()` + id).
                        let click = edit_click(db.clone(), id, full_name.clone());
                        let right_click = strip_menu(db.clone(), id);
                        let mut behavior = behavior_snapshot(cx, db.clone(), id, click);
                        behavior.on_element_right_click = Some(right_click);
                        strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
                        let row = PreviewRow {
                            component_id: id,
                            full_name,
                            strip,
                            db: db.clone(),
                        };
                        items.push(render_preview_entry(&row, &theme));
                    }
                    delegate.strip_cache.prune();
                    items
                },
            ),
        )
        .track_scroll(scroll_handle)
        .h_full();

        Some(
            div()
                .id("component-browser-detail")
                .size_full()
                .p(px(8.0))
                .child(list_view)
                .into_any_element(),
        )
    }

    fn detail_label(&self, tail: Option<&Self::Item>) -> SharedString {
        match tail {
            Some(node) => node.full_name.clone(),
            None if self.live.is_some() => SharedString::new_static("Matches"),
            None => match &self.selection.root_override {
                Some(segments) if !segments.is_empty() => SharedString::from(
                    segments
                        .iter()
                        .map(|s| s.as_ref())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => SharedString::new_static("All components"),
            },
        }
    }

    fn on_activate(
        &mut self,
        item: &Self::Item,
        _window: &mut Window,
        cx: &mut Context<ComponentBrowser>,
    ) {
        if let Some(id) = item.component_id {
            cx.emit(BrowserEvent::Activated(id));
        }
    }

    /// Skip past any column that would only hold a single row by
    /// pushing that row's segment onto `selection.path` so the next
    /// render starts one column deeper. Walks from the tail of the
    /// currently-resolved selection downward.
    ///
    /// Already-compressed chains don't repeat because the compressed
    /// node carries the full subtree; this loop only triggers at
    /// branch boundaries (root, filter entry points, post-reroot).
    fn auto_extend_selection(&mut self, cx: &mut Context<ComponentBrowser>) {
        let mut changed = false;
        // If `selection.path` carries stale segments (e.g., a re-root was
        // just cleared and the path's prefix no longer resolves against
        // the full tree), `resolved_chain` returns short of the path and
        // `detail_target` falls back to `real_root()` — which would let
        // the loop push from the wrong subtree forever. Truncate the
        // path to its resolvable prefix first so the extension below
        // walks from a node that actually matches.
        let resolved_len = self.resolved_chain().len();
        let expected_len = self
            .selection
            .path
            .len()
            .saturating_sub(self.override_depth());
        if resolved_len < expected_len {
            self.selection
                .path
                .truncate(self.override_depth() + resolved_len);
            changed = true;
        }
        loop {
            let tail = self.detail_target();
            if tail.children.len() != 1 {
                break;
            }
            let seg = tail.children.first().expect("len == 1").segment.clone();
            self.selection.path.push(seg);
            changed = true;
        }
        if changed {
            self.rebuild_detail_list(cx);
        }
    }

    fn context_rows(
        &mut self,
        column_ix: usize,
        item: Option<&Self::Item>,
        cx: &mut Context<ComponentBrowser>,
    ) -> Vec<Box<dyn InspectorRow>> {
        self.build_context_rows(column_ix, item, cx.entity().downgrade())
    }

    fn filter_placeholder(&self) -> Option<SharedString> {
        Some(SharedString::new_static("Filter components…"))
    }

    fn apply_filter(&mut self, query: &Query, cx: &mut Context<ComponentBrowser>) {
        if query.has_terms() {
            let label = SharedString::from(query.text().trim().to_string());
            let synth = build_filter_synth(query, label.clone(), &self.real_root());
            self.live = Some(FilterEntry {
                label,
                query: query.clone(),
                synth,
            });
            self.selection.root = SelectionRoot::Live;
            self.selection.path.clear();
        } else {
            self.live = None;
            if matches!(self.selection.root, SelectionRoot::Live) {
                self.selection.root = SelectionRoot::Real;
                self.selection.path.clear();
                if let Some(prefix) = &self.selection.root_override {
                    self.selection.path = prefix.clone();
                }
            }
        }
        self.rebuild_detail_list(cx);
    }

    /// Enter keeps the query: it becomes a saved filter named after itself,
    /// and the columns move into it so nothing changes on screen.
    fn submit_filter(&mut self, query: &Query, cx: &mut Context<ComponentBrowser>) -> bool {
        let label = SharedString::from(query.text().trim().to_string());
        if self.add_filter(label.clone(), label.clone(), cx).is_err() {
            return false;
        }
        self.live = None;
        self.selection.root = SelectionRoot::Filter(label.clone());
        self.selection.path.clear();
        self.selection.path.push(label);
        self.rebuild_detail_list(cx);
        true
    }

    fn filter_status(&self) -> Option<SharedString> {
        let live = self.live.as_ref()?;
        Some(SharedString::from(format!(
            "{} / {}",
            component_count(&live.synth),
            self.component_total
        )))
    }

    fn filter_hint(&self) -> Option<SharedString> {
        let live = self.live.as_ref()?;
        let taken = self.tree.children.get(&live.label).is_some()
            || self.filters.iter().any(|f| f.label == live.label);
        (!taken).then(|| SharedString::new_static("↵ save filter"))
    }
}

/// Build a [`ComponentBrowser`] rooted at `db`'s component namespace.
pub fn new_component_browser(db: Arc<DB>, cx: &mut Context<ComponentBrowser>) -> ComponentBrowser {
    let watcher = ComponentBrowserDelegate::spawn_watcher(db.clone(), cx);
    let delegate = ComponentBrowserDelegate {
        db,
        tree: Arc::new(ComponentNode {
            segment: SharedString::new_static(""),
            full_name: SharedString::new_static(""),
            component_id: None,
            children: Children::default(),
        }),
        selection: Selection::empty(),
        filters: Vec::new(),
        live: None,
        component_total: 0,
        detail_components: Vec::new(),
        strip_cache: VisibleEntityCache::new(DETAIL_STRIP_CACHE_CAP),
        detail_scroll_handle: UniformListScrollHandle::new(),
        custom_title: Override::Auto,
        pending_root_path: None,
        _watcher: watcher,
    };
    ColumnBrowser::new(delegate, cx)
}

fn collect_component_nodes(node: &Arc<ComponentNode>, out: &mut Vec<Arc<ComponentNode>>) {
    if node.component_id.is_some() {
        out.push(node.clone());
    }
    for child in node.children.values() {
        collect_component_nodes(child, out);
    }
}

fn component_count(node: &Arc<ComponentNode>) -> usize {
    usize::from(node.component_id.is_some())
        + node.children.values().map(component_count).sum::<usize>()
}

/// Build the synthetic filter root: a pruned real tree run through
/// `compress_subtree` so the same single-child collapse rules apply to
/// filter navigation as to plain navigation.
///
/// When compression leaves exactly one non-component top-level child,
/// its children are hoisted into the synth root so the user lands at
/// the first real branch point. Without this, `*.tick_time` (all
/// matches under `cube_sat`) would render an extra single-row
/// `cube_sat` column before the branches. A component-at-branch child
/// is kept so it stays clickable.
fn build_filter_synth(
    query: &Query,
    label: SharedString,
    tree: &Arc<ComponentNode>,
) -> Arc<ComponentNode> {
    let pruned: Vec<Arc<ComponentNode>> = tree
        .children
        .values()
        .filter_map(|child| prune_to_matches(child, query))
        .map(compress_subtree)
        .collect();

    let children: Children = if pruned.len() == 1
        && pruned[0].component_id.is_none()
        && !pruned[0].children.is_empty()
    {
        pruned[0].children.clone()
    } else {
        pruned.into_iter().collect()
    };

    Arc::new(ComponentNode {
        segment: label.clone(),
        full_name: label,
        component_id: None,
        children,
    })
}

/// Return the original `node` when it matches `query`, otherwise a new
/// node with only its matching descendants kept. Matching nodes
/// short-circuit with their full subtree so the user can explore
/// siblings of the thing they were looking for.
pub(crate) fn prune_to_matches(
    node: &Arc<ComponentNode>,
    query: &Query,
) -> Option<Arc<ComponentNode>> {
    if query.matches_name(node.full_name.as_ref()) {
        return Some(node.clone());
    }
    let children: Children = node
        .children
        .values()
        .filter_map(|child| prune_to_matches(child, query))
        .collect();
    if children.is_empty() {
        return None;
    }
    Some(Arc::new(ComponentNode {
        segment: node.segment.clone(),
        full_name: node.full_name.clone(),
        component_id: node.component_id,
        children,
    }))
}

const PREVIEW_ROW_HEIGHT: f32 = 56.0;

/// Live strips kept alive in the detail column. Well above any plausible
/// visible-row count so scrolling and short navigations reuse strips, while
/// still bounding how many stream tasks/WAL readers exist at once.
const DETAIL_STRIP_CACHE_CAP: usize = 256;

/// Window for coalescing a burst of vtable-generation bumps into one rebuild.
const VTABLE_DEBOUNCE: Duration = Duration::from_millis(50);

struct PreviewRow {
    component_id: ComponentId,
    full_name: SharedString,
    strip: Entity<ComponentValueStrip>,
    db: Arc<DB>,
}

/// The right-click menu of a detail row's name: plot every element, or
/// copy the value or the name.
fn open_detail_menu(
    db: &Arc<DB>,
    component_id: ComponentId,
    name: &SharedString,
    position: gpui::Point<gpui::Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(open) = open_inspector(cx) else {
        return;
    };
    let count = element_count(db, component_id);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(CommandRow::new(
        "Plot component",
        Arc::new(move |window, cx| {
            let indices: SmallVec<[usize; 4]> = (0..count).collect();
            window.dispatch_action(
                Box::new(PlotComponentAction {
                    component_id,
                    indices,
                }),
                cx,
            );
        }),
    ))];
    rows.extend(copy_rows(db.clone(), component_id, name.clone(), None));
    open(
        InspectorRequest {
            rows,
            mode: InspectorMode::Anchored(position),
        },
        window,
        cx,
    );
}

fn render_preview_entry(row: &PreviewRow, theme: &Arc<Theme>) -> AnyElement {
    let db = row.db.clone();
    let component_id = row.component_id;
    let icon_id = row.component_id.0 as usize;

    // The strip lays its element boxes out with `flex_wrap`, and this row is
    // a fixed-height `uniform_list` item — wrapped lines would be clipped
    // invisibly. Hold the strip at its single-line width (`flex_shrink = 0`
    // keeps the auto basis at max-content; `min_w` backstops it with the
    // computed box-row width) so overflow scrolls sideways instead of
    // wrapping out of view. Axis-restricted so vertical wheel deltas still
    // scroll the detail list underneath.
    let n_cells = strip_cell_count(&row.db, component_id);
    let mut strip_line = div().child(row.strip.clone());
    if n_cells > 1 {
        strip_line = strip_line.min_w(px(strip_row_width(n_cells)));
    }
    strip_line.style().flex_shrink = Some(0.0);
    let mut strip_scroll = div()
        .id(("detail-strip", icon_id))
        .w_full()
        .overflow_x_scroll()
        .child(strip_line);
    strip_scroll.style().restrict_scroll_to_axis = Some(true);

    let plot_icon = div()
        .id(("detail-plot", icon_id))
        .flex()
        .items_center()
        .justify_center()
        .w(px(16.0))
        .h(px(16.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.selection_bg))
        .on_mouse_down(
            MouseButton::Left,
            move |_event: &gpui::MouseDownEvent, window, cx| {
                let count = element_count(&db, component_id);
                let indices: SmallVec<[usize; 4]> = (0..count).collect();
                window.dispatch_action(
                    Box::new(PlotComponentAction {
                        component_id,
                        indices,
                    }),
                    cx,
                );
                cx.stop_propagation();
            },
        )
        .child(Icon::Plot.svg_color(11.0, theme.text_secondary));

    div()
        .h(px(PREVIEW_ROW_HEIGHT))
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .child(
                    div()
                        .id(("detail-name", icon_id))
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .on_mouse_move(crate::inspector::plot_preview::shift_hover_listener(
                            component_id,
                            SmallVec::new(),
                        ))
                        .on_mouse_down(MouseButton::Right, {
                            let db = row.db.clone();
                            let name = row.full_name.clone();
                            move |event: &gpui::MouseDownEvent, window, cx| {
                                open_detail_menu(
                                    &db,
                                    component_id,
                                    &name,
                                    event.position,
                                    window,
                                    cx,
                                );
                                cx.stop_propagation();
                            }
                        })
                        .child(row.full_name.clone()),
                )
                .child(plot_icon),
        )
        .child(strip_scroll)
        .into_any_element()
}
