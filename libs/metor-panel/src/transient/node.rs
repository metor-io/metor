//! The chord menu's data model: a tree of key-addressed nodes.
//!
//! A [`ChordNode`] mirrors the palette's `InspectionItem` vocabulary — a
//! [`ChordAction::Command`] leaf or a [`ChordAction::SubMenu`] prefix — with one
//! addition: the single keystroke that selects it. Navigation is kept here as a
//! pure [`dispatch`] function (no `cx`, no side effects) so it can be
//! unit-tested without a window; the caller invokes the returned builder.
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

/// Outcome of pressing `key` at a menu level.
pub enum Dispatch {
    /// `key` selected a submenu; `label` is the crumb and `build` produces the
    /// child level when invoked with the app context.
    Descend {
        label: SharedString,
        build: SubMenuBuilder,
    },
    /// `key` selected a command; run it and dismiss.
    Leaf(Command),
    /// No node bound `key`.
    NoMatch,
}

/// Resolve `key` against `nodes`. The first node whose key matches wins.
pub fn dispatch(nodes: &[ChordNode], key: &str) -> Dispatch {
    for node in nodes {
        if node.key.as_ref() == key {
            return match &node.action {
                ChordAction::SubMenu(build) => Dispatch::Descend {
                    label: node.label.clone(),
                    build: build.clone(),
                },
                ChordAction::Command(cb) => Dispatch::Leaf(cb.clone()),
            };
        }
    }
    Dispatch::NoMatch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Vec<ChordNode> {
        vec![
            ChordNode::submenu("w", "Window", |_cx| {
                vec![ChordNode::command("v", "Split", |_, _| {})]
            }),
            ChordNode::command("p", "Palette", |_, _| {}),
        ]
    }

    #[test]
    fn submenu_key_descends_with_label() {
        match dispatch(&menu(), "w") {
            Dispatch::Descend { label, .. } => assert_eq!(label.as_ref(), "Window"),
            _ => panic!("expected Descend"),
        }
    }

    #[test]
    fn command_key_is_leaf() {
        assert!(matches!(dispatch(&menu(), "p"), Dispatch::Leaf(_)));
    }

    #[test]
    fn unmatched_key_is_no_match() {
        assert!(matches!(dispatch(&menu(), "z"), Dispatch::NoMatch));
    }
}
