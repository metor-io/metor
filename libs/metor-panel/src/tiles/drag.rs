use gpui::{Bounds, Entity, IntoElement, Pixels, Point, Render, Window, Context, Empty, div, prelude::*, px};
use super::SplitPath;
use serde::{Deserialize, Serialize};

use crate::theme::theme;
use super::item::PaneItemHandle;
use super::pane::Pane;

/// Payload carried by gpui while a tab is being dragged.
///
/// The source pane and index are retained so the drop target can remove the
/// tab from its origin without searching for it.
pub struct DraggedTab {
    pub pane: Entity<Pane>,
    pub item: Box<dyn PaneItemHandle>,
    pub ix: usize,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
pub fn detect_split_zone(
    cursor: Point<Pixels>,
    bounds: Bounds<Pixels>,
) -> Option<SplitDirection> {
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
