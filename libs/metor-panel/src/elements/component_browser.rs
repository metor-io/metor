use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, IntoElement, SharedString,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use super::column_browser::{ColumnBrowser, ColumnBrowserDelegate};
use super::monitor::{behavior_snapshot, edit_click};
use super::value_strip::{ComponentValueStrip, StripBehavior, StripClick, StripStyle};
use crate::component_tree::{ComponentNode, build_tree, resolve_path};
use crate::theme::{Theme, theme};

/// Specialization of [`ColumnBrowser`] that browses the DB's component namespace.
pub type ComponentBrowser = ColumnBrowser<ComponentBrowserDelegate>;

/// Emitted by [`ComponentBrowser`] when the user activates a leaf component
/// (Enter / double-click). Owners decide the follow-up action — v1's
/// `BrowserPanel` wrapper ignores these.
#[derive(Clone, Copy)]
pub enum BrowserEvent {
    Activated(ComponentId),
}

impl EventEmitter<BrowserEvent> for ColumnBrowser<ComponentBrowserDelegate> {}

/// Streaming preview for a single component.
struct ComponentPreview {
    component_id: ComponentId,
    full_name: SharedString,
    strip: Entity<ComponentValueStrip>,
    click: StripClick,
}

/// Delegate that browses the dot-delimited namespace of DB components.
///
/// Selection is stored as an absolute path of `SharedString` segments so
/// external tree rebuilds don't invalidate it. The detail column is always
/// rendered; it shows the union of values for every component at or below
/// the current selection tail (or the effective root when nothing is
/// selected).
pub struct ComponentBrowserDelegate {
    db: Arc<DB>,
    tree: Arc<ComponentNode>,
    selection_path: SmallVec<[SharedString; 8]>,
    root_override_path: Option<SmallVec<[SharedString; 8]>>,
    previews: BTreeMap<SharedString, ComponentPreview>,
    detail_scroll_handle: UniformListScrollHandle,
    _watcher: gpui::Task<()>,
}

impl ComponentBrowserDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<ComponentBrowser>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let tree = build_tree(&db);
                let result = this.update(cx, |browser, cx| {
                    browser.delegate_mut().tree = tree;
                    browser.delegate_mut().reconcile_previews(cx);
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn effective_root(&self) -> Arc<ComponentNode> {
        match &self.root_override_path {
            Some(p) => resolve_path(&self.tree, p)
                .last()
                .cloned()
                .unwrap_or_else(|| self.tree.clone()),
            None => self.tree.clone(),
        }
    }

    fn override_depth(&self) -> usize {
        self.root_override_path
            .as_ref()
            .map(|p| p.len())
            .unwrap_or(0)
    }

    fn relative_selection(&self) -> SmallVec<[Arc<ComponentNode>; 8]> {
        let start = self.effective_root();
        let skip = self.override_depth();
        let mut out = SmallVec::new();
        let mut current = start;
        for seg in self.selection_path.iter().skip(skip) {
            let Some(next) = current.children.get(seg).cloned() else {
                break;
            };
            out.push(next.clone());
            current = next;
        }
        out
    }

    fn segments_of(name: &SharedString) -> SmallVec<[SharedString; 8]> {
        name.split('.')
            .filter(|s| !s.is_empty())
            .map(|s| SharedString::from(s.to_string()))
            .collect()
    }

    fn detail_target(&self) -> Arc<ComponentNode> {
        self.relative_selection()
            .last()
            .cloned()
            .unwrap_or_else(|| self.effective_root())
    }

    /// Bring `self.previews` into sync with the components under the current
    /// detail target: spawn streams for new entries, drop tasks for entries
    /// that left the target set, leave already-streaming entries alone.
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
            let strip = {
                let db = self.db.clone();
                let click = click.clone();
                cx.new(|cx| {
                    ComponentValueStrip::new(
                        db,
                        id,
                        StripStyle::boxes(),
                        StripBehavior {
                            on_element_click: Some(click),
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
                    click,
                },
            );
        }
    }
}

impl ColumnBrowserDelegate for ComponentBrowserDelegate {
    type Item = Arc<ComponentNode>;

    fn resolve_selection(&self, _cx: &App) -> SmallVec<[Self::Item; 8]> {
        self.relative_selection()
    }

    fn root_items(&self, _cx: &App) -> Vec<Self::Item> {
        self.effective_root().children.values().cloned().collect()
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

    fn column_label(&self, parent: Option<&Self::Item>) -> SharedString {
        match parent {
            Some(node) => node.full_name.clone(),
            None => match &self.root_override_path {
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
        _column_ix: usize,
        item: &Self::Item,
        cx: &mut Context<ComponentBrowser>,
    ) {
        self.selection_path = Self::segments_of(&item.full_name);
        self.reconcile_previews(cx);
    }

    fn set_root_override(
        &mut self,
        ancestors: SmallVec<[Self::Item; 8]>,
        cx: &mut Context<ComponentBrowser>,
    ) {
        let Some(new_root) = ancestors.last() else {
            return;
        };
        let segments = Self::segments_of(&new_root.full_name);
        if !self.selection_path.starts_with(&segments[..]) {
            self.selection_path = segments.clone();
        }
        self.root_override_path = Some(segments);
        self.reconcile_previews(cx);
    }

    fn clear_root_override(&mut self, cx: &mut Context<ComponentBrowser>) {
        self.root_override_path = None;
        self.reconcile_previews(cx);
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

        // Refresh each strip's interactive snapshot against the current
        // pending-edit state before the list re-renders.
        let updates: Vec<(Entity<ComponentValueStrip>, StripBehavior)> = self
            .previews
            .values()
            .map(|p| {
                (
                    p.strip.clone(),
                    behavior_snapshot(cx, p.component_id, p.click.clone()),
                )
            })
            .collect();
        for (strip, behavior) in updates {
            strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
        }

        let preview_data: Vec<(SharedString, Entity<ComponentValueStrip>)> = self
            .previews
            .values()
            .map(|p| (p.full_name.clone(), p.strip.clone()))
            .collect();
        let count = preview_data.len();
        let scroll_handle = self.detail_scroll_handle.clone();

        let list_view = uniform_list("component-detail-list", count, move |range, _window, cx| {
            let theme = crate::theme::theme(cx);
            range
                .map(|ix| {
                    let (name, strip) = &preview_data[ix];
                    render_preview_entry(name.clone(), strip.clone(), &theme)
                })
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
            None => match &self.root_override_path {
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
}

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
        selection_path: SmallVec::new(),
        root_override_path: None,
        previews: BTreeMap::new(),
        detail_scroll_handle: UniformListScrollHandle::new(),
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

const PREVIEW_ROW_HEIGHT: f32 = 56.0;

fn render_preview_entry(
    full_name: SharedString,
    strip: Entity<ComponentValueStrip>,
    theme: &Arc<Theme>,
) -> AnyElement {
    div()
        .h(px(PREVIEW_ROW_HEIGHT))
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_secondary)
                .child(full_name),
        )
        .child(strip)
        .into_any_element()
}
