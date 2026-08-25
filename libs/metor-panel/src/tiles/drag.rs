use super::SplitPath;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Empty, Entity, Global, IntoElement, Pixels, Point,
    Render, Window, div, prelude::*, px,
};

use super::item::PaneItemHandle;
use super::pane::Pane;
use crate::theme::theme;

/// Payload carried by gpui while a tab is being dragged.
///
/// The source pane and index are retained so the drop target can remove the
/// tab from its origin without searching for it.
pub struct DraggedTab {
    pub pane: Entity<Pane>,
    pub item: Box<dyn PaneItemHandle>,
    pub ix: usize,
}

/// The tab drag in flight, mirrored app-side because gpui's own payload
/// (`App::active_drag`) is crate-private and a mouse-up outside every drop
/// target discards it without telling anyone. The ghost constructor sets
/// this at drag start; the window root takes it on mouse-up — outside the
/// window to tear the tab out, inside merely to clear it.
pub struct ActiveTabDrag {
    pub pane: Entity<Pane>,
    pub item: Box<dyn PaneItemHandle>,
    pub ix: usize,
    pub source_window: AnyWindowHandle,
}

impl Global for ActiveTabDrag {}

pub(crate) fn set_active_tab_drag(drag: ActiveTabDrag, cx: &mut App) {
    cx.set_global(drag);
}

/// Take the in-flight tab drag, if any. Callers still gate on
/// `cx.has_active_drag()` — a leftover mirror from a drag that ended
/// without a mouse-up reaching us must not tear anything out.
pub(crate) fn take_active_tab_drag(cx: &mut App) -> Option<ActiveTabDrag> {
    cx.has_global::<ActiveTabDrag>()
        .then(|| cx.remove_global::<ActiveTabDrag>())
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let title = self.item.tab_title(cx);
        div()
            .bg(theme.bg_primary)
            .border_1()
            .border_color(theme.border_primary)
            .px(px(8.0))
            .py(px(4.0))
            .text_color(theme.text_primary)
            .text_size(px(12.0))
            .child(title)
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

/// Side of a pane a tab was dropped against.
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
}

/// Classify a drop position inside a pane's content area.
///
/// The pane is divided into four edge strips (25% deep) plus a central
/// region. A drop on an edge splits the pane; a drop in the center yields
/// `None`, signalling the caller to insert the tab instead.
pub fn detect_split_zone(cursor: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<SplitDirection> {
    let margin = 0.25;
    let rel_x = (cursor.x - bounds.origin.x) / bounds.size.width;
    let rel_y = (cursor.y - bounds.origin.y) / bounds.size.height;

    if rel_x < margin {
        Some(SplitDirection::Left)
    } else if rel_x > 1.0 - margin {
        Some(SplitDirection::Right)
    } else if rel_y < margin {
        Some(SplitDirection::Up)
    } else if rel_y > 1.0 - margin {
        Some(SplitDirection::Down)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    /// A 100x100 pane at the origin, so relative fractions read straight off
    /// the pixel coordinates.
    fn unit_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        }
    }

    fn zone_at(x: f32, y: f32) -> Option<SplitDirection> {
        detect_split_zone(point(px(x), px(y)), unit_bounds())
    }

    #[test]
    fn center_yields_no_split() {
        assert_eq!(zone_at(50.0, 50.0), None);
    }

    #[test]
    fn edges_map_to_their_direction() {
        assert_eq!(zone_at(10.0, 50.0), Some(SplitDirection::Left));
        assert_eq!(zone_at(90.0, 50.0), Some(SplitDirection::Right));
        assert_eq!(zone_at(50.0, 10.0), Some(SplitDirection::Up));
        assert_eq!(zone_at(50.0, 90.0), Some(SplitDirection::Down));
    }

    #[test]
    fn horizontal_edges_win_over_vertical_in_corners() {
        // A corner sits in both an x- and a y-strip; the x-check runs first, so
        // Left/Right take precedence.
        assert_eq!(zone_at(10.0, 10.0), Some(SplitDirection::Left));
        assert_eq!(zone_at(90.0, 90.0), Some(SplitDirection::Right));
    }

    #[test]
    fn bounds_origin_is_honored() {
        let bounds = Bounds {
            origin: point(px(200.0), px(200.0)),
            size: size(px(100.0), px(100.0)),
        };
        // Absolute (250,250) is the center of an origin-shifted pane.
        assert_eq!(detect_split_zone(point(px(250.0), px(250.0)), bounds), None);
        assert_eq!(
            detect_split_zone(point(px(210.0), px(250.0)), bounds),
            Some(SplitDirection::Left)
        );
    }
}
