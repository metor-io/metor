//! Strips of panes pinned along the top and bottom of a window.
//!
//! A tab's layout comes and goes as the user switches tabs; a dock stays.
//! It is for the controls every tab wants in reach — the timeline, a
//! sequence control — and holds its own split tree of panes, owned by the
//! window's [`TileGroup`] like any tab's, so a docked pane drags, splits and
//! closes as it would in a tab. Panes and whole tabs are pinned from their
//! right-click menus.

use std::sync::Arc;

use gpui::{
    App, Context, DragMoveEvent, Empty, Entity, IntoElement, Pixels, Render, Window, div,
    prelude::*, px,
};

use super::{Member, Pane, RESIZE_HANDLE_SIZE, TabId, TileGroup};
use crate::inspector::rows::{BoolRow, CommandRow, InspectorRow};
use metor_gpui::theme;

/// The height a dock opens at when the pane pinned to it has no locked
/// size to keep.
pub(super) const DEFAULT_HEIGHT: Pixels = px(160.0);

/// The least a dock, or the tab area between the docks, can be resized to.
const MIN_HEIGHT: Pixels = px(48.0);

/// An edge of the window a dock runs along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Top,
    Bottom,
}

impl Edge {
    fn other(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Bottom => "Bottom",
        }
    }
}

/// The panes pinned along one edge: a split tree like a tab's, laid out
/// across the window's width at a height the user drags.
pub(super) struct Dock {
    pub root: Member,
    pub panes: Vec<Entity<Pane>>,
    pub height: Pixels,
}

impl Dock {
    /// Lay `member` (a pane, or a whole tab's tree) at the dock's right end.
    pub fn append(&mut self, member: Member, panes: Vec<Entity<Pane>>) {
        match &mut self.root {
            Member::Axis(axis) if axis.axis == gpui::Axis::Horizontal => {
                axis.members.push(member);
                axis.flexes.push(1.0);
            }
            root => {
                let first = root.clone();
                *root = Member::Axis(super::SplitAxis::new(
                    gpui::Axis::Horizontal,
                    vec![first, member],
                ));
            }
        }
        self.panes.extend(panes);
    }
}

/// Payload for a drag on the edge between a dock and the tabs.
#[derive(Clone)]
pub struct DockResizeDrag {
    edge: Edge,
}

impl Render for DockResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The edge between the dock on `edge` and the tabs, dragged to resize it.
pub(super) fn render_resize_handle(
    edge: Edge,
    group: &Entity<TileGroup>,
    cx: &App,
) -> impl IntoElement {
    let theme = theme(cx);
    let hover = theme.text_tertiary;
    let group = group.clone();
    div()
        .id((
            "dock-resize",
            match edge {
                Edge::Top => 0usize,
                Edge::Bottom => 1,
            },
        ))
        .flex_none()
        .w_full()
        .h(px(RESIZE_HANDLE_SIZE))
        .bg(theme.border_primary)
        .hover(move |s| s.bg(hover))
        .cursor(gpui::CursorStyle::ResizeRow)
        .on_drag(DockResizeDrag { edge }, |drag, _, _, cx| {
            cx.new(|_| drag.clone())
        })
        .on_drag_move(move |event: &DragMoveEvent<DockResizeDrag>, _, cx| {
            if event.drag(cx).edge == edge {
                let y = event.event.position.y;
                group.update(cx, |group, cx| group.resize_dock(edge, y, cx));
            }
        })
}

impl TileGroup {
    pub(super) fn dock(&self, edge: Edge) -> Option<&Dock> {
        match edge {
            Edge::Top => self.top_dock.as_ref(),
            Edge::Bottom => self.bottom_dock.as_ref(),
        }
    }

    pub(super) fn dock_mut(&mut self, edge: Edge) -> &mut Option<Dock> {
        match edge {
            Edge::Top => &mut self.top_dock,
            Edge::Bottom => &mut self.bottom_dock,
        }
    }

    /// The edge `pane` is pinned to, if it is docked here.
    pub fn docked(&self, pane: &Entity<Pane>) -> Option<Edge> {
        [Edge::Top, Edge::Bottom].into_iter().find(|edge| {
            self.dock(*edge)
                .is_some_and(|dock| dock.panes.contains(pane))
        })
    }

    /// Every docked pane, top first.
    pub(super) fn docked_panes(&self) -> impl Iterator<Item = &Entity<Pane>> {
        [Edge::Top, Edge::Bottom]
            .into_iter()
            .filter_map(|edge| self.dock(edge))
            .flat_map(|dock| dock.panes.iter())
    }

    /// Take `pane` into the dock on `edge`, from wherever it is — a tab,
    /// the other dock, or another window. A pane keeps its entity, so its
    /// item keeps its state.
    pub fn pin(&mut self, pane: Entity<Pane>, edge: Edge, cx: &mut Context<Self>) {
        if self.docked(&pane) == Some(edge) {
            return;
        }
        let height = pane
            .read(cx)
            .locked_size()
            .map(|size| px(size.height))
            .unwrap_or(DEFAULT_HEIGHT);
        if !self.detach(&pane, cx) {
            return;
        }
        self.dock_in(edge, Member::Pane(pane.clone()), vec![pane], height, cx);
    }

    /// Pin every pane of tab `id`, laid out as the tab had them, and close
    /// the tab.
    pub fn pin_tab(&mut self, id: TabId, edge: Edge, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(id) else {
            return;
        };
        let tab = self.remove_tab_at(ix, cx);
        self.dock_in(edge, tab.root, tab.panes, DEFAULT_HEIGHT, cx);
    }

    /// Put docked `pane` back in the tab on screen, beside its current pane.
    pub fn unpin(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) {
        if self.docked(&pane).is_some() {
            let id = self.active_tab();
            self.move_pane_to_tab(pane, id, cx);
        }
    }

    pub(super) fn dock_in(
        &mut self,
        edge: Edge,
        member: Member,
        panes: Vec<Entity<Pane>>,
        height: Pixels,
        cx: &mut Context<Self>,
    ) {
        for pane in &panes {
            self.adopt(pane, cx);
            pane.update(cx, |pane, cx| pane.set_docked(true, cx));
        }
        match self.dock_mut(edge) {
            Some(dock) => dock.append(member, panes),
            slot => {
                *slot = Some(Dock {
                    root: member,
                    panes,
                    height,
                })
            }
        }
        cx.notify();
    }

    /// Take docked `pane` out of its dock, closing the dock when it was the
    /// last. Returns whether it was docked here.
    pub(super) fn undock(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) -> bool {
        let Some(edge) = self.docked(pane) else {
            return false;
        };
        self.release(pane);
        pane.update(cx, |pane, cx| pane.set_docked(false, cx));
        let slot = self.dock_mut(edge);
        if let Some(dock) = slot {
            dock.root.remove(pane);
            dock.root.collapse();
            dock.panes.retain(|p| p != pane);
            if dock.panes.is_empty() {
                *slot = None;
            }
        }
        cx.notify();
        true
    }

    /// The dock on `edge` was dragged by its inner edge to `y`, in window
    /// coordinates. The tabs between the docks keep a usable height.
    pub(super) fn resize_dock(&mut self, edge: Edge, y: Pixels, cx: &mut Context<Self>) {
        let bounds = self.bounds;
        let other = self
            .dock(edge.other())
            .map_or(px(0.), |dock| dock.height + px(RESIZE_HANDLE_SIZE));
        let room = bounds.size.height - other - px(RESIZE_HANDLE_SIZE) - MIN_HEIGHT;
        let height = match edge {
            Edge::Top => y - bounds.top(),
            Edge::Bottom => bounds.bottom() - y,
        };
        if let Some(dock) = self.dock_mut(edge) {
            dock.height = height.min(room).max(MIN_HEIGHT);
            cx.notify();
        }
    }
}

/// The rows a pane's right-click menu ends with: pin it to an edge (or
/// once pinned, move or unpin it), and show or hide its header.
pub(crate) fn pane_rows(pane: &Entity<Pane>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Some(group) = pane.read(cx).owner() else {
        return Vec::new();
    };
    let docked = group.read(cx).docked(pane);
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    for edge in [Edge::Top, Edge::Bottom] {
        if docked == Some(edge) {
            continue;
        }
        let label = match docked {
            Some(_) => format!("Move to {}", edge.name()),
            None => format!("Pin to {}", edge.name()),
        };
        let (group, pane) = (group.clone(), pane.clone());
        rows.push(Box::new(CommandRow::new(
            label,
            Arc::new(move |_, cx| {
                group.update(cx, |group, cx| group.pin(pane.clone(), edge, cx));
            }),
        )));
    }
    let (read, write) = (pane.clone(), pane.clone());
    rows.push(Box::new(BoolRow::dynamic(
        "Show Header",
        Arc::new(move |cx| !read.read(cx).hide_header()),
        Arc::new(move |show, _, cx| {
            write.update(cx, |pane, cx| pane.set_hide_header(!show, cx));
        }),
    )));
    if docked.is_some() {
        let pane = pane.clone();
        rows.push(Box::new(CommandRow::new(
            "Unpin",
            Arc::new(move |_, cx| {
                group.update(cx, |group, cx| group.unpin(pane.clone(), cx));
            }),
        )));
    }
    rows
}

/// The pane rows for a menu a view opens at `position` (in window
/// coordinates) inside a pane on screen whose header is hidden. With no
/// header to right-click, the view's own menu is the way back to them.
pub(crate) fn headerless_rows_at(
    group: &Entity<TileGroup>,
    position: gpui::Point<Pixels>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let group = group.read(cx);
    let shown = group.droppable_panes();
    let pane = shown.iter().chain(group.docked_panes()).find(|pane| {
        let pane = pane.read(cx);
        pane.hide_header() && pane.bounds().contains(&position)
    });
    pane.map(|pane| pane_rows(pane, cx)).unwrap_or_default()
}

/// Right-click rows for tab `id` of `group`: pin the whole tab to an edge.
pub(crate) fn tab_rows(group: &Entity<TileGroup>, id: TabId) -> Vec<Box<dyn InspectorRow>> {
    [Edge::Top, Edge::Bottom]
        .into_iter()
        .map(|edge| {
            let group = group.clone();
            Box::new(CommandRow::new(
                format!("Pin to {}", edge.name()),
                Arc::new(move |_, cx| {
                    group.update(cx, |group, cx| group.pin_tab(id, edge, cx));
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}
