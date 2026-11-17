//! A tab's layout reduced to proportions and titles, for drawing it in
//! miniature where its live panes can't go: the preview window that rides
//! the pointer while a tab or pane is dragged out of its window.

use gpui::{App, Axis, Entity, SharedString};

use super::{Member, Pane, TabId, TileGroup};

#[derive(Clone, Debug, PartialEq)]
pub enum Sketch {
    Pane(SharedString),
    Split {
        axis: Axis,
        /// Each part with its flex weight along `axis`.
        parts: Vec<(f32, Sketch)>,
    },
}

impl Sketch {
    /// A lone pane, titled as it would title its own window.
    pub(crate) fn titled_pane(pane: &Entity<Pane>, cx: &App) -> (SharedString, Self) {
        let title = pane
            .read(cx)
            .item()
            .map(|item| item.tab_title(cx))
            .unwrap_or_else(|| SharedString::new_static("Empty"));
        (title.clone(), Self::Pane(title))
    }

    fn of_pane(pane: &Entity<Pane>, cx: &App) -> Self {
        Self::titled_pane(pane, cx).1
    }

    fn of_member(member: &Member, cx: &App) -> Self {
        match member {
            Member::Pane(pane) => Self::of_pane(pane, cx),
            Member::Axis(axis) => Self::Split {
                axis: axis.axis,
                parts: axis
                    .flexes
                    .iter()
                    .zip(&axis.members)
                    .map(|(flex, member)| (*flex, Self::of_member(member, cx)))
                    .collect(),
            },
        }
    }
}

impl TileGroup {
    /// Tab `id` as it would open in a window of its own: the zoomed pane
    /// alone when one fills it, its whole tree otherwise.
    pub fn tab_sketch(&self, id: TabId, cx: &App) -> Option<(SharedString, Sketch)> {
        let tab = &self.tabs[self.tab_index(id)?];
        let sketch = match &tab.zoomed {
            Some(pane) => Sketch::of_pane(pane, cx),
            None => Sketch::of_member(&tab.root, cx),
        };
        Some((tab.title(cx), sketch))
    }
}
