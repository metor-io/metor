use gpui::{Bounds, Entity, IntoElement, Pixels, Point, Render, Window, Context, Empty, div, prelude::*, px};
use super::SplitPath;
use serde::{Deserialize, Serialize};

use crate::theme::theme;
use super::item::PaneItemHandle;
use super::pane::Pane;

/// Data carried during a tab drag operation.
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

/// Data carried during a split resize drag.
#[derive(Clone)]
pub struct ResizeDrag {
    /// Path of member indices to the SplitAxis being resized.
    pub path: SplitPath,
    /// Index of the handle within that axis (between member[ix-1] and member[ix]).
    pub handle_ix: usize,
}

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Direction for splitting a pane.
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

    /// Whether the new pane goes after (true) or before (false) the existing one.
    pub fn increasing(self) -> bool {
        matches!(self, Self::Down | Self::Right)
    }
}

/// Given cursor position within bounds, determine split direction.
/// Returns None if cursor is in the center (meaning "add as tab").
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
