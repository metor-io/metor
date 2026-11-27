//! The chord menu's data model: a tree of key-addressed nodes.
//!
//! A [`ChordNode`] mirrors the palette's `InspectionItem` vocabulary — a
//! [`ChordAction::Command`] leaf or a [`ChordAction::SubMenu`] prefix — with one
//! addition: the single keystroke that selects it. Navigation is kept here as a
//! small enough that the overlay can match the selected [`ChordAction`]
//! directly without a second command representation.
use std::sync::Arc;

use gpui::{App, SharedString, Window};

/// Lazily builds a submenu's children, reflecting live state (the focused pane,
/// pending edits) at the moment the submenu is opened.
pub type SubMenuBuilder = Arc<dyn Fn(&App) -> Vec<ChordNode>>;

/// Runs when a leaf node is chosen.
pub type Command = Arc<dyn Fn(&mut Window, &mut App)>;

/// One entry in a chord menu level: the key that selects it, the label shown in
/// the popup, and what happens when it is chosen.
pub struct ChordNode {
    pub key: SharedString,
    pub label: SharedString,
    pub action: ChordAction,
}

/// What selecting a [`ChordNode`] does.
#[derive(Clone)]
pub enum ChordAction {
    SubMenu(SubMenuBuilder),
    Command(Command),
}

impl ChordNode {
    pub fn submenu(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        build: impl Fn(&App) -> Vec<ChordNode> + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            action: ChordAction::SubMenu(Arc::new(build)),
        }
    }

    pub fn command(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        callback: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            action: ChordAction::Command(Arc::new(callback)),
        }
    }
}
