use super::SplitPath;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Empty, Entity, Global, IntoElement, Pixels, Point,
    Render, Window, div, prelude::*, px,
};

use super::TileGroup;
use super::pane::Pane;
use super::tab_bar::TabId;
use crate::theme::theme;

/// Payload carried by gpui while a pane is dragged by its header.
///
/// The pane moves as a whole — it holds one item — so the payload is the
/// pane itself; the group that owns it is looked up at drop time, since a
/// drag can end in another window.
pub struct DraggedPane {
    pub pane: Entity<Pane>,
}

/// Payload carried by gpui while a top-level tab is dragged along its bar.
pub struct DraggedTab {
    pub group: Entity<TileGroup>,
    pub tab: TabId,
}

/// What an in-flight drag carries, mirrored app-side because gpui's own
/// payload (`App::active_drag`) is crate-private and a mouse-up outside
/// every drop target discards it without telling anyone. The ghost
/// constructor sets this at drag start; the window root takes it on
/// mouse-up — outside the window to tear the pane or tab out, inside merely
/// to clear it.
pub enum DragSource {
    Pane(Entity<Pane>),
    Tab(Entity<TileGroup>, TabId),
}

pub struct ActiveDrag {
    pub source: DragSource,
    pub source_window: AnyWindowHandle,
}

impl Global for ActiveDrag {}

pub(crate) fn set_active_drag(drag: ActiveDrag, cx: &mut App) {
    cx.set_global(drag);
}

/// Take the in-flight drag, if any. Callers still gate on
/// `cx.has_active_drag()` — a leftover mirror from a drag that ended
/// without a mouse-up reaching us must not tear anything out.
pub(crate) fn take_active_drag(cx: &mut App) -> Option<ActiveDrag> {
    cx.has_global::<ActiveDrag>()
        .then(|| cx.remove_global::<ActiveDrag>())
}

/// The floating title that follows the pointer while a pane or tab is
/// dragged.
pub(super) struct DragGhost {
    pub title: gpui::SharedString,
    pub text_style: gpui::TextStyle,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mut ghost = div();
        *ghost.text_style() = gpui::TextStyleRefinement {
            font_family: Some(self.text_style.font_family.clone()),
            font_features: Some(self.text_style.font_features.clone()),
            font_fallbacks: self.text_style.font_fallbacks.clone(),
            font_weight: Some(self.text_style.font_weight),
            font_style: Some(self.text_style.font_style),
            line_height: Some(self.text_style.line_height),
            ..Default::default()
        };
        ghost
            .debug_selector(|| "drag-ghost".into())
            .flex()
            .items_center()
            .h(px(super::tab_bar::TAB_HEIGHT))
            .max_w(px(super::tab_bar::TAB_MAX_WIDTH))
            .px(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_primary)
            .bg(theme.bg_elevated)
            .shadow_md()
            .text_size(px(12.0))
            .text_color(theme.text_primary)
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Payload identifying which split handle is being dragged.
///
/// `path` locates the axis inside the tree; `handle_ix` is the gap the user
/// grabbed, between `members[handle_ix - 1]` and `members[handle_ix]`.
#[derive(Clone)]
pub struct ResizeDrag {
    pub path: SplitPath,
    pub handle_ix: usize,
}

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Payload for a drag on the tab sidebar's resize edge.
#[derive(Clone)]
pub struct SidebarResizeDrag;

impl Render for SidebarResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Side of a pane a dragged pane is dropped against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

impl SplitDirection {
    pub fn axis(self) -> gpui::Axis {
        match self {
            Self::Up | Self::Down => gpui::Axis::Vertical,
            Self::Left | Self::Right => gpui::Axis::Horizontal,
        }
    }

    /// `true` when the new pane is inserted after the existing one along the
    /// axis (right of, or below). Used to pick insert positions.
    pub fn increasing(self) -> bool {
        matches!(self, Self::Down | Self::Right)
    }

    /// The split that suits a pane of `size`: beside it when it is wider
    /// than tall, below it otherwise, so repeated adds stay legible.
    pub fn along_longer(size: gpui::Size<Pixels>) -> Self {
        if size.width >= size.height {
            Self::Right
        } else {
            Self::Down
        }
    }
}

/// The edge of `bounds` nearest `cursor`, measured as a fraction of the
/// pane's extent so a wide pane still splits sideways near its sides.
pub fn nearest_edge(cursor: Point<Pixels>, bounds: Bounds<Pixels>) -> SplitDirection {
    let rel_x = (cursor.x - bounds.origin.x) / bounds.size.width;
    let rel_y = (cursor.y - bounds.origin.y) / bounds.size.height;
    [
        (rel_x, SplitDirection::Left),
        (1.0 - rel_x, SplitDirection::Right),
        (rel_y, SplitDirection::Up),
        (1.0 - rel_y, SplitDirection::Down),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, direction)| direction)
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    /// A 100x100 pane at the origin, so relative fractions read straight off
    /// the pixel coordinates.
    fn unit_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        }
    }

    fn edge_at(x: f32, y: f32) -> SplitDirection {
        nearest_edge(point(px(x), px(y)), unit_bounds())
    }

    #[test]
    fn every_point_splits_toward_its_nearest_edge() {
        assert_eq!(edge_at(10.0, 50.0), SplitDirection::Left);
        assert_eq!(edge_at(90.0, 50.0), SplitDirection::Right);
        assert_eq!(edge_at(50.0, 10.0), SplitDirection::Up);
        assert_eq!(edge_at(50.0, 90.0), SplitDirection::Down);
        assert_eq!(edge_at(45.0, 40.0), SplitDirection::Up);
    }

    #[test]
    fn edges_are_relative_to_the_pane_extent() {
        let wide = Bounds {
            origin: point(px(200.0), px(0.0)),
            size: size(px(1000.0), px(100.0)),
        };
        // 100px from the left but 30px from the top: the left edge is still
        // nearer as a share of the pane.
        assert_eq!(
            nearest_edge(point(px(300.0), px(30.0)), wide),
            SplitDirection::Left
        );
    }
}
