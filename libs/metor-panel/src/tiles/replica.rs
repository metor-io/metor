//! A tab's layout with its panes' live views, for drawing it a second time
//! somewhere its panes don't live: the preview window that rides the
//! pointer while a tab or pane is dragged out of its window.
//!
//! It holds the item views themselves, so the preview shows (and keeps
//! updating with) the real content; it doesn't hold the panes, whose
//! headers, drop targets and bounds belong to the window they live in.

use gpui::{AnyView, App, Axis, Entity, SharedString};

use super::{Member, Pane, TabId, TileGroup};

#[derive(Clone)]
pub enum Replica {
    Pane {
        title: SharedString,
        /// `None` for an empty pane.
        view: Option<AnyView>,
    },
    Split {
        axis: Axis,
        /// Each part with its flex weight along `axis`.
        parts: Vec<(f32, Replica)>,
    },
}

impl Replica {
    /// A lone pane, titled as it would title its own window.
    pub(crate) fn titled_pane(pane: &Entity<Pane>, cx: &App) -> (SharedString, Self) {
        let item = pane.read(cx).item();
        let title = item
            .map(|item| item.tab_title(cx))
            .unwrap_or_else(|| SharedString::new_static("Empty"));
        let view = item.map(|item| item.view());
        (title.clone(), Self::Pane { title, view })
    }

    fn of_member(member: &Member, cx: &App) -> Self {
        match member {
            Member::Pane(pane) => Self::titled_pane(pane, cx).1,
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
    pub fn tab_replica(&self, id: TabId, cx: &App) -> Option<(SharedString, Replica)> {
        let tab = &self.tabs[self.tab_index(id)?];
        let replica = match &tab.zoomed {
            Some(pane) => Replica::titled_pane(pane, cx).1,
            None => Replica::of_member(&tab.root, cx),
        };
        Some((tab.title(cx), replica))
    }
}
