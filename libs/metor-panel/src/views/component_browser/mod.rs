use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, IntoElement, MouseButton, Pixels, Point,
    SharedString, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::{SmallVec, smallvec};

pub mod component_tree;

use super::column_browser::{ColumnBrowser, ColumnBrowserDelegate};
use super::monitor::{behavior_snapshot, edit_click};
use super::time_series::Override;
use super::value_strip::{ComponentValueStrip, StripBehavior, StripClick, StripStyle};
use crate::icons::Icon;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::theme::{Theme, theme};
use crate::tiles::PlotComponentAction;
use component_tree::{ComponentNode, build_tree, compress_subtree, resolve_path};

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

/// Cached live preview for a component in the detail column.
struct ComponentPreview {
    component_id: ComponentId,
    full_name: SharedString,
    strip: Entity<ComponentValueStrip>,
}

/// User-defined view into the namespace.
///
/// `synth` is a pruned mirror of the real tree kept only to branches whose
/// full names match `regex`; it's refreshed on filter add and on every
/// real-tree rebuild so lookups are O(1) cache reads instead of recursing
/// the whole namespace per render.
struct FilterEntry {
    label: SharedString,
    regex: regex::Regex,
    synth: Arc<ComponentNode>,
}

/// Which tree the current selection is navigating.
///
/// `Filter(label)` means the user drilled into the synthetic filter row at
/// the real tree root; `Real` means straight real-tree navigation (with or
/// without a reroot).
#[derive(Clone)]
enum SelectionRoot {
    Real,
    Filter(SharedString),
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

/// Translate a glob-like pattern (`*`, `?`) into an anchored regex.
///
/// Everything that isn't a wildcard is passed through `regex::escape`, so
/// dots in component names match literally. `*` maps to `.*` and `?` to
/// `.` — both traverse segment boundaries, which matches the user's
/// expectation that `*.health` picks up `cube_sat.imu.health`.
pub(crate) fn glob_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::with_capacity(pattern.len() + 2);
    out.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out)
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
    previews: BTreeMap<SharedString, ComponentPreview>,
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
                    delegate.reconcile_previews(cx);
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn refresh_filter_synths(&mut self) {
        let tree = self.tree.clone();
        for filter in &mut self.filters {
            filter.synth = build_filter_synth(&filter.regex, filter.label.clone(), &tree);
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

    fn override_depth(&self) -> usize {
        self.selection
            .root_override
            .as_ref()
            .map(|p| p.len())
            .unwrap_or(0)
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
        }
    }

    /// Append a new filter with `label` and `pattern`, compiling the glob.
    /// Returns the filter label on success; on failure (invalid glob or a
    /// label collision with another filter or a top-level real segment),
    /// returns an error message suitable for inspector surface.
    fn add_filter(
        &mut self,
        label: SharedString,
        pattern: SharedString,
        cx: &mut Context<ComponentBrowser>,
    ) -> Result<(), SharedString> {
        if label.is_empty() {
            return Err(SharedString::new_static("filter label cannot be empty"));
        }
        if self.tree.children.contains_key(&label) {
            return Err(SharedString::new_static(
                "filter label collides with a top-level prefix",
            ));
        }
        if self.filters.iter().any(|f| f.label == label) {
            return Err(SharedString::new_static("filter label already in use"));
        }
        let regex = glob_to_regex(pattern.as_ref())
            .map_err(|_| SharedString::new_static("invalid glob pattern"))?;
        let synth = build_filter_synth(&regex, label.clone(), &self.tree);
        self.filters.push(FilterEntry {
            label,
            regex,
            synth,
        });
        self.reconcile_previews(cx);
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
            self.reconcile_previews(cx);
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
                    vec![Box::new(DefaultActionRow {
                        label: SharedString::new_static("label = pattern (e.g. *.health)"),
                        callback: Arc::new(move |input, _window, cx| {
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
                    }) as Box<dyn InspectorRow>]
                }),
            )));
        }

        if let Some(node) = item {
            // A column-0 row whose segment matches a filter label (only
            // possible when no reroot is active, since rerooted columns
            // don't surface filter siblings) is the synthetic filter
            // root — offer Remove instead of the real-component actions.
            let filter_label = if column_ix == 0 && self.selection.root_override.is_none() {
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
                rows.push(Box::new(CommandRow::new(
                    SharedString::new_static("Plot component"),
                    Arc::new(move |window, cx| {
                        let count = element_count(&db, component_id);
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
        self.resolved_chain()
            .last()
            .cloned()
            .unwrap_or_else(|| self.real_root())
    }

    /// Align `previews` with the components currently under the detail
    /// target: start streams for new entries, drop ones that fell out,
    /// leave shared entries untouched so existing tasks keep running.
    fn reconcile_previews(&mut self, cx: &mut Context<ComponentBrowser>) {
        let target = self.detail_target();
        let mut target_nodes: Vec<Arc<ComponentNode>> = Vec::new();
        collect_component_nodes(&target, &mut target_nodes);

        let desired: BTreeSet<SharedString> =
            target_nodes.iter().map(|n| n.full_name.clone()).collect();
        self.previews.retain(|name, _| desired.contains(name));

        for node in target_nodes {
            if self.previews.contains_key(&node.full_name) {
                continue;
            }
            let Some(id) = node.component_id else {
                continue;
            };
            let full_name = node.full_name.clone();
            let click = edit_click(self.db.clone(), id, full_name.clone());
            let right_click = right_click_plot(self.db.clone(), id);
            let strip = {
                let db = self.db.clone();
                cx.new(|cx| {
                    ComponentValueStrip::new(
                        db,
                        id,
                        StripStyle::boxes(),
                        StripBehavior {
                            on_element_click: Some(click),
                            on_element_right_click: Some(right_click),
                            ..Default::default()
                        },
                        cx,
                    )
                })
            };
            self.previews.insert(
                full_name.clone(),
                ComponentPreview {
                    component_id: id,
                    full_name,
                    strip,
                },
            );
        }
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
            SelectionRoot::Real => match &self.selection.root_override {
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
    pub fn set_root_path(
        &mut self,
        segs: &[SharedString],
        cx: &mut Context<ComponentBrowser>,
    ) {
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
        let segments: SmallVec<[SharedString; 8]> = chain.iter().map(|n| n.segment.clone()).collect();
        if !self.selection.path.starts_with(&segments[..]) {
            self.selection.path = segments.clone();
        }
        self.selection.root_override = Some(segments);
        self.pending_root_path = None;
        self.reconcile_previews(cx);
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

/// Build the right-click callback for a strip cell: opens an anchored
/// inspector with a single "Plot this element" command.
fn right_click_plot(db: Arc<DB>, component_id: ComponentId) -> StripClick {
    Arc::new(move |element_index, position, window, cx| {
        let _ = &db;
        let Some(open) = open_inspector(cx) else {
            return;
        };
        let rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(CommandRow::new(
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

impl ColumnBrowserDelegate for ComponentBrowserDelegate {
    type Item = Arc<ComponentNode>;

    fn resolve_selection(&self, _cx: &App) -> SmallVec<[Self::Item; 8]> {
        self.resolved_chain()
    }

    fn root_items(&self, _cx: &App) -> Vec<Self::Item> {
        // Rerooted columns show only the rerooted node's real children.
        // At the true root (no override), filter synth roots appear as
        // virtual siblings of the real top-level prefixes — the user sees
        // and enters them from column 0 just like a real prefix.
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
            None => match &self.selection.root_override {
                Some(segments) if !segments.is_empty() => SharedString::from(
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
        // Column-0 clicks reset which tree the selection anchors on:
        // filter siblings switch into `Filter` mode, real prefixes
        // switch back to `Real`. Reroots short-circuit this since their
        // column 0 never surfaces filter siblings.
        if column_ix == 0 && self.selection.root_override.is_none() {
            self.selection.root = match self.find_filter(&item.segment) {
                Some(filter) => SelectionRoot::Filter(filter.label.clone()),
                None => SelectionRoot::Real,
            };
            self.selection.path.clear();
            self.selection.path.push(item.segment.clone());
            self.reconcile_previews(cx);
            return;
        }

        // Deeper clicks truncate-and-push. Node segments can be compound
        // (path-compressed prefixes, filter children's full names) so
        // splitting `full_name` on `.` would miss the real child-map keys.
        let keep = match &self.selection.root {
            SelectionRoot::Real => column_ix + self.override_depth(),
            SelectionRoot::Filter(_) => column_ix,
        };
        self.selection.path.truncate(keep);
        self.selection.path.push(item.segment.clone());
        self.reconcile_previews(cx);
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
        self.reconcile_previews(cx);
    }

    fn clear_root_override(&mut self, cx: &mut Context<ComponentBrowser>) {
        // Drop any pending restore first, otherwise the watcher would
        // re-apply it on its next tick and clobber the user's clear.
        self.pending_root_path = None;
        if self.selection.root_override.is_some() {
            self.selection.root_override = None;
            self.reconcile_previews(cx);
        }
    }

    fn render_detail(
        &mut self,
        _tail: Option<&Self::Item>,
        _window: &mut Window,
        cx: &mut Context<ComponentBrowser>,
    ) -> Option<AnyElement> {
        let theme = theme(cx);

        if self.previews.is_empty() {
            return Some(
                div()
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No values"))
                    .into_any_element(),
            );
        }

        // Push the latest pending-edit snapshot into each strip before
        // the list repaints. Click/right-click callbacks are cheap Arc
        // captures (`db.clone()` + id) so rebuilding them per frame
        // avoids caching state that would otherwise need invalidation.
        let updates: Vec<(Entity<ComponentValueStrip>, StripBehavior)> = self
            .previews
            .values()
            .map(|p| {
                let click = edit_click(self.db.clone(), p.component_id, p.full_name.clone());
                let right_click = right_click_plot(self.db.clone(), p.component_id);
                let mut behavior = behavior_snapshot(cx, self.db.clone(), p.component_id, click);
                behavior.on_element_right_click = Some(right_click);
                (p.strip.clone(), behavior)
            })
            .collect();
        for (strip, behavior) in updates {
            strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
        }

        let preview_data: Vec<PreviewRow> = self
            .previews
            .values()
            .map(|p| PreviewRow {
                component_id: p.component_id,
                full_name: p.full_name.clone(),
                strip: p.strip.clone(),
                db: self.db.clone(),
            })
            .collect();
        let count = preview_data.len();
        let scroll_handle = self.detail_scroll_handle.clone();

        let list_view = uniform_list("component-detail-list", count, move |range, _window, cx| {
            let theme = crate::theme::theme(cx);
            range
                .map(|ix| render_preview_entry(&preview_data[ix], &theme))
                .collect()
        })
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
            let seg = tail.children.keys().next().cloned().expect("len == 1");
            self.selection.path.push(seg);
            changed = true;
        }
        if changed {
            self.reconcile_previews(cx);
        }
    }

    fn on_context_menu(
        &mut self,
        column_ix: usize,
        item: Option<&Self::Item>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<ComponentBrowser>,
    ) {
        let Some(open) = open_inspector(cx) else {
            return;
        };
        let browser = cx.entity().downgrade();
        let rows = self.build_context_rows(column_ix, item, browser);
        if rows.is_empty() {
            return;
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
            children: BTreeMap::new(),
        }),
        selection: Selection::empty(),
        filters: Vec::new(),
        previews: BTreeMap::new(),
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
    regex: &regex::Regex,
    label: SharedString,
    tree: &Arc<ComponentNode>,
) -> Arc<ComponentNode> {
    let pruned: Vec<Arc<ComponentNode>> = tree
        .children
        .values()
        .filter_map(|child| prune_to_matches(child, regex))
        .map(compress_subtree)
        .collect();

    let children: BTreeMap<SharedString, Arc<ComponentNode>> = if pruned.len() == 1
        && pruned[0].component_id.is_none()
        && !pruned[0].children.is_empty()
    {
        pruned[0].children.clone()
    } else {
        pruned.into_iter().map(|c| (c.segment.clone(), c)).collect()
    };

    Arc::new(ComponentNode {
        segment: label.clone(),
        full_name: label,
        component_id: None,
        children,
    })
}

/// Return the original `node` when it matches `regex`, otherwise a new
/// node with only its matching descendants kept. Matching nodes
/// short-circuit with their full subtree so the user can explore
/// siblings of the thing they were looking for.
fn prune_to_matches(node: &Arc<ComponentNode>, regex: &regex::Regex) -> Option<Arc<ComponentNode>> {
    if regex.is_match(node.full_name.as_ref()) {
        return Some(node.clone());
    }
    let children: BTreeMap<SharedString, Arc<ComponentNode>> = node
        .children
        .values()
        .filter_map(|child| prune_to_matches(child, regex))
        .map(|c| (c.segment.clone(), c))
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

struct PreviewRow {
    component_id: ComponentId,
    full_name: SharedString,
    strip: Entity<ComponentValueStrip>,
    db: Arc<DB>,
}

fn render_preview_entry(row: &PreviewRow, theme: &Arc<Theme>) -> AnyElement {
    let db = row.db.clone();
    let component_id = row.component_id;
    let icon_id = row.component_id.0 as usize;
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
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .on_mouse_move(crate::inspector::plot_preview::shift_hover_listener(
                            component_id,
                            SmallVec::new(),
                        ))
                        .child(row.full_name.clone()),
                )
                .child(plot_icon),
        )
        .child(row.strip.clone())
        .into_any_element()
}
