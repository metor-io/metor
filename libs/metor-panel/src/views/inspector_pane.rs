//! The inspector docked as a pane: the right-click popup's rows, kept open
//! and following the selection.
//!
//! The pane resolves its target on every render, from the pane last clicked
//! ([`target::selected_pane`]) or a pinned one, so it needs no event plumbing
//! from the panes it inspects. A host item (a dashboard) adds its children as
//! an Elements section and shows the selected child's rows above its own.
//! The rows are one ordinary inline [`Inspector`] page, so search, drilling
//! in and inline editing all work as they do in the popup.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EntityId, Focusable, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use crate::icons::Icon;
use crate::inspector::rows::{
    HeaderRow, InspectorRow, RowAction, TextRow, row_base, row_label, tag_pill,
};
use crate::inspector::target::{self, InspectChild, InspectHost};
use crate::inspector::{Inspector, InspectorMode};
use crate::theme::theme;
use crate::tiles::{Pane, PaneItemHandle};

/// Nothing persists: the target is whatever is selected when the layout
/// opens, and a pin names a live pane that a reload would not bring back.
#[derive(Default, Serialize, Deserialize)]
pub struct InspectorPaneConfig {}

/// An item in some pane, located without holding it alive.
#[derive(Clone)]
struct Target {
    pane: WeakEntity<Pane>,
    item: EntityId,
}

impl Target {
    fn active_in(pane: &Entity<Pane>, cx: &App) -> Option<Self> {
        Some(Self {
            pane: pane.downgrade(),
            item: pane.read(cx).active_item()?.entity_id(),
        })
    }

    fn resolve(&self, cx: &App) -> Option<(Entity<Pane>, Box<dyn PaneItemHandle>)> {
        let pane = self.pane.upgrade()?;
        let item = pane
            .read(cx)
            .items()
            .iter()
            .find(|item| item.entity_id() == self.item)?
            .clone_handle();
        Some((pane, item))
    }
}

/// What the inspector is built for: a new item, or a new child selected
/// within it, starts it over on a fresh page stack.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    item: EntityId,
    child: Option<u64>,
}

pub struct InspectorPane {
    db: Arc<DB>,
    pinned: Option<Target>,
    shown: Option<Shown>,
    inspector: Option<Entity<Inspector>>,
    /// Set when the host notifies; the inspector's live source rebuilds its
    /// root page on the next render. Field rows read their values live, so
    /// only a host's structure (its children, its own rows) needs this.
    dirty: Rc<Cell<bool>>,
    /// Watches the inspected pane (tab switches) and its host item.
    watching: Vec<Subscription>,
    _selection: Subscription,
}

impl InspectorPane {
    pub fn from_config(_cfg: InspectorPaneConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self {
            db,
            pinned: None,
            shown: None,
            inspector: None,
            dirty: Rc::new(Cell::new(false)),
            watching: Vec::new(),
            _selection: cx.observe_global::<target::InspectionSelection>(|_, cx| cx.notify()),
        }
    }

    pub fn to_config(&self, _cx: &App) -> InspectorPaneConfig {
        InspectorPaneConfig {}
    }

    fn target(&mut self, cx: &App) -> Option<Target> {
        if let Some(pinned) = &self.pinned {
            if pinned.resolve(cx).is_some() {
                return Some(pinned.clone());
            }
            self.pinned = None;
        }
        Target::active_in(&target::selected_pane(cx)?, cx)
    }

    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        self.pinned = match self.pinned {
            Some(_) => None,
            None => self.target(cx),
        };
        cx.notify();
    }

    /// Rebuild the inspector when the target or its selected child moved.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resolved = self
            .target(cx)
            .and_then(|t| Some((t.clone(), t.resolve(cx)?)));
        let shown = resolved.as_ref().map(|(_, (_, item))| Shown {
            item: item.entity_id(),
            child: item.inspect_host().and_then(|host| host.selected_child(cx)),
        });
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        self.dirty.set(false);
        self.watching.clear();
        let had_focus = self
            .inspector
            .as_ref()
            .is_some_and(|i| i.focus_handle(cx).contains_focused(window, cx));
        let Some((target, (pane, item))) = resolved else {
            self.inspector = None;
            return;
        };

        self.watching
            .push(cx.observe(&pane, |_, _, cx| cx.notify()));
        if let Some(host) = item.inspect_host() {
            let this = cx.entity().downgrade();
            let dirty = self.dirty.clone();
            self.watching.push(host.observe(
                Box::new(move |cx| {
                    dirty.set(true);
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |_, cx| cx.notify());
                    }
                }),
                cx,
            ));
        }

        let rows = item_rows(&*item, &self.db, cx);
        let dirty = self.dirty.clone();
        let db = self.db.clone();
        let inspector = cx.new(|cx| {
            let mut inspector = Inspector::new(rows, InspectorMode::Inline, cx);
            inspector.set_persistent();
            inspector.follow(Box::new(move |cx| {
                if !dirty.replace(false) {
                    return None;
                }
                let (_, item) = target.resolve(cx)?;
                Some(item_rows(&*item, &db, cx))
            }));
            inspector
        });
        if had_focus {
            inspector.focus_handle(cx).focus(window, cx);
        }
        self.inspector = Some(inspector);
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let mut crumbs: Vec<SharedString> = Vec::new();
        if let Some((_, item)) = self.target(cx).and_then(|t| t.resolve(cx)) {
            crumbs.push(item.tab_title(cx));
            if let Some(host) = item.inspect_host()
                && let Some(selected) = host.selected_child(cx)
                && let Some(child) = host.children(cx).into_iter().find(|c| c.id == selected)
            {
                crumbs.push(child.label);
            }
        }
        let pinned = self.pinned.is_some();
        let mut trail = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .overflow_hidden()
            .text_size(px(12.0));
        let last = crumbs.len().saturating_sub(1);
        for (ix, crumb) in crumbs.into_iter().enumerate() {
            if ix > 0 {
                trail = trail.child(Icon::ChevronRight.svg(8.0, theme.text_tertiary));
            }
            trail = trail.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(if ix == last {
                        theme.text_primary
                    } else {
                        theme.text_secondary
                    })
                    .child(crumb),
            );
        }
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .gap(px(6.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .child(trail)
            .child(
                div()
                    .id("inspector-pane-pin")
                    .flex_none()
                    .p(px(3.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.selection_bg))
                    .child(if pinned { Icon::Lock } else { Icon::LockOpen }.svg(
                        12.0,
                        if pinned {
                            theme.text_primary
                        } else {
                            theme.text_tertiary
                        },
                    ))
                    .tooltip(move |_, cx| {
                        crate::views::TooltipText::build(
                            if pinned {
                                "Pinned: click to follow the selection again".into()
                            } else {
                                "Following the selection: click to pin this target".into()
                            },
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_pin(cx))),
            )
            .into_any_element()
    }
}

impl Render for InspectorPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(window, cx);
        let theme = theme(cx);
        let header = self.render_header(cx);
        let body: AnyElement = match &self.inspector {
            Some(inspector) => {
                let focus = inspector.focus_handle(cx);
                div()
                    .flex_1()
                    .min_h_0()
                    // Clicking anywhere in the list gives it the keyboard,
                    // which it otherwise only takes while focused.
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        focus.focus(window, cx);
                    })
                    .child(inspector.clone())
                    .into_any_element()
            }
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.0))
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child("Select a pane or element to inspect")
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_elevated)
            .child(header)
            .child(body)
    }
}

/// The root page for `item`: a host's children, the selected child's rows
/// and the host's own; otherwise the item's reflected rows.
fn item_rows(item: &dyn PaneItemHandle, db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    let title = item.tab_title(cx);
    let Some(host) = item.inspect_host() else {
        rows.push(Box::new(HeaderRow::new(title)));
        match crate::inspector::reflect::rows_for_any_entity(&item.entity_any(cx), db, cx) {
            Some(own) if !own.is_empty() => rows.extend(own),
            _ => rows.push(Box::new(TextRow::readonly(
                "Nothing to inspect".into(),
                SharedString::default(),
            ))),
        }
        return rows;
    };

    let children = host.children(cx);
    let selected = host.selected_child(cx);
    if !children.is_empty() {
        rows.push(Box::new(HeaderRow::new(format!(
            "Elements ({})",
            children.len()
        ))));
        for child in &children {
            rows.push(Box::new(ChildRow {
                child: child.clone(),
                selected: selected == Some(child.id),
                host: host.clone(),
            }));
        }
    }
    if let Some(id) = selected
        && let Some(child) = children.iter().find(|c| c.id == id)
        && let Some(child_rows) = host.child_rows(id, cx)
    {
        rows.push(Box::new(HeaderRow::new(child.label.clone())));
        rows.extend(child_rows);
    }
    rows.push(Box::new(HeaderRow::new(title)));
    rows.extend(host.host_rows(cx));
    rows
}

/// One of a host's children in the Elements section. Activating it selects
/// the child on the host; hovering it highlights the child there.
struct ChildRow {
    child: InspectChild,
    selected: bool,
    host: Rc<dyn InspectHost>,
}

impl InspectorRow for ChildRow {
    fn label(&self) -> &str {
        &self.child.label
    }

    fn identity(&self) -> SharedString {
        format!("child:{}", self.child.id).into()
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let host = self.host.clone();
        let id = self.child.id;
        row_base(row_ix, selected, cx)
            .gap(px(6.0))
            .child(div().w(px(8.0)).flex_none().when(self.selected, |slot| {
                slot.child(Icon::Dot.svg(8.0, theme.selection_ring()))
            }))
            .child(row_label(
                self.child.label.clone(),
                if self.selected {
                    theme.text_primary
                } else {
                    theme.text_secondary
                },
            ))
            .child(div().flex_1())
            .child(tag_pill(self.child.kind.clone(), cx))
            .on_hover(move |hovered, _window, cx| host.hover_child(id, *hovered, cx))
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        self.host.select_child(Some(self.child.id), cx);
        RowAction::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::dashboard::{DashboardPanel, WidgetKind, WidgetRegistry, test_support};

    struct Fixture {
        _temp: tempfile::TempDir,
        db: Arc<DB>,
        dashboard: Entity<DashboardPanel>,
        dashboard_pane: Entity<Pane>,
    }

    fn open(
        cx: &mut gpui::TestAppContext,
    ) -> (Fixture, Entity<InspectorPane>, &mut gpui::VisualTestContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let mut fixture = None;
        let (pane, cx) = cx.add_window_view(|_, cx| {
            test_support::install(&db, cx);
            let dashboard = test_support::dashboard(&db, cx);
            let dashboard_pane = cx.new(|cx| Pane::new(vec![Box::new(dashboard.clone())], cx));
            target::select_pane(&dashboard_pane, cx);
            fixture = Some((dashboard, dashboard_pane));
            InspectorPane::from_config(InspectorPaneConfig {}, db.clone(), cx)
        });
        let (dashboard, dashboard_pane) = fixture.unwrap();
        cx.run_until_parked();
        let fixture = Fixture {
            _temp: temp,
            db,
            dashboard,
            dashboard_pane,
        };
        (fixture, pane, cx)
    }

    fn labels(pane: &Entity<InspectorPane>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        cx.run_until_parked();
        cx.update(|_, cx| {
            pane.read(cx)
                .inspector
                .as_ref()
                .map(|i| i.read(cx).visible_labels())
                .unwrap_or_default()
        })
    }

    fn activate(
        pane: &Entity<InspectorPane>,
        label: &str,
        nth: usize,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, cx| {
            let inspector = pane.read(cx).inspector.clone().unwrap();
            inspector.update(cx, |i, cx| i.activate_label(label, nth, window, cx));
        });
    }

    #[gpui::test]
    fn picking_an_element_selects_it_on_the_dashboard(cx: &mut gpui::TestAppContext) {
        let (f, pane, cx) = open(cx);
        let rows = labels(&pane, cx);
        assert_eq!(rows[..3], ["Elements (2)", "Blank", "Blank"]);
        assert!(rows.contains(&"Add Widget".to_string()), "host rows follow");
        assert!(!rows.contains(&"Frame".to_string()), "nothing selected yet");

        activate(&pane, "Blank", 1, cx);
        let selected = cx.update(|_, cx| f.dashboard.selected_child(cx));
        assert_eq!(selected, Some(2));
        let rows = labels(&pane, cx);
        assert!(rows.contains(&"Frame".to_string()), "the widget's own rows");
        assert!(rows.contains(&"Bring to Front".to_string()));
    }

    #[gpui::test]
    fn commands_return_to_the_root_page_instead_of_closing(cx: &mut gpui::TestAppContext) {
        let (_f, pane, cx) = open(cx);
        activate(&pane, "Zoom", 0, cx);
        assert!(labels(&pane, cx).contains(&"Fit All".to_string()));
        activate(&pane, "Fit All", 0, cx);
        let dismissed = cx.update(|_, cx| {
            let inspector = pane.read(cx).inspector.clone().unwrap();
            inspector.read(cx).dismissed
        });
        assert!(!dismissed);
        assert_eq!(labels(&pane, cx)[0], "Elements (2)");
    }

    #[gpui::test]
    fn an_inspector_pane_is_never_its_own_target(cx: &mut gpui::TestAppContext) {
        let (f, _pane, cx) = open(cx);
        cx.update(|_, cx| {
            let spec = cx
                .global::<WidgetRegistry>()
                .spec(&WidgetKind::new("inspector"))
                .unwrap();
            let mut registry = crate::tiles::ItemRegistry::default();
            registry.register_view(spec, f.db.clone());
            let inspector_pane = cx.new(|cx| {
                let mut pane = Pane::new(Vec::new(), cx);
                let item = registry
                    .deserialize(target::INSPECTOR_PANE_KEY, "{}", cx)
                    .unwrap();
                pane.add_item(item, cx);
                pane
            });
            target::select_pane(&inspector_pane, cx);
            assert_eq!(target::selected_pane(cx), Some(f.dashboard_pane.clone()));
        });
    }

    #[gpui::test]
    fn a_pinned_pane_ignores_new_selections(cx: &mut gpui::TestAppContext) {
        let (f, pane, cx) = open(cx);
        pane.update(cx, |p, cx| p.toggle_pin(cx));
        let _other_pane = cx.update(|_, cx| {
            let other = test_support::dashboard(&f.db, cx);
            let other_pane = cx.new(|cx| Pane::new(vec![Box::new(other)], cx));
            target::select_pane(&other_pane, cx);
            other_pane
        });
        cx.run_until_parked();
        let shown = pane.read_with(cx, |p, _| p.shown.map(|s| s.item));
        assert_eq!(shown, Some(f.dashboard.entity_id()));

        pane.update(cx, |p, cx| p.toggle_pin(cx));
        cx.run_until_parked();
        let shown = pane.read_with(cx, |p, _| p.shown.map(|s| s.item));
        assert_ne!(
            shown,
            Some(f.dashboard.entity_id()),
            "unpinned follows again"
        );
    }
}
