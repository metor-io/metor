//! What the inspector pane is looking at.
//!
//! The popup inspector is handed its rows by whoever opened it; the pane
//! instead follows a selection. Clicking a pane publishes it here, and a
//! pane item that holds children of its own (a dashboard's widgets) answers
//! through [`InspectHost`], so the pane can list those children and show the
//! selected one's rows without knowing the host's type.
use gpui::{App, Entity, Global, SharedString, Subscription, WeakEntity};

use crate::inspector::rows::InspectorRow;
use crate::tiles::Pane;

/// Serialization key of the inspector pane. Selection skips panes showing
/// it, so focusing the inspector to edit never retargets it to itself.
pub const INSPECTOR_PANE_KEY: &str = "inspector";

/// The pane the user last clicked, other than an inspector.
///
/// Held weakly: a closed pane simply stops resolving. The inspector pane
/// observes this global and reads the pane's active item when it renders,
/// so switching tabs inside the selected pane retargets it too.
#[derive(Default)]
pub struct InspectionSelection {
    pane: Option<WeakEntity<Pane>>,
}

impl Global for InspectionSelection {}

/// Publish `pane` as the inspection target, unless it is showing an
/// inspector or is already the target.
pub fn select_pane(pane: &Entity<Pane>, cx: &mut App) {
    if pane
        .read(cx)
        .active_item()
        .is_some_and(|item| item.serialization_key() == INSPECTOR_PANE_KEY)
    {
        return;
    }
    if selected_pane(cx).as_ref() == Some(pane) {
        return;
    }
    cx.set_global(InspectionSelection {
        pane: Some(pane.downgrade()),
    });
}

pub fn selected_pane(cx: &App) -> Option<Entity<Pane>> {
    cx.try_global::<InspectionSelection>()?
        .pane
        .as_ref()?
        .upgrade()
}

/// A child a host lists in the inspector pane, keyed by an id that is stable
/// for the host's lifetime.
#[derive(Clone, PartialEq)]
pub struct InspectChild {
    pub id: u64,
    pub label: SharedString,
    pub kind: SharedString,
}

/// A pane item whose children are inspectable one at a time.
///
/// The host owns the selection, so picking a child in the inspector pane
/// and clicking it on the host's own surface are the same edit.
pub trait InspectHost {
    fn children(&self, cx: &App) -> Vec<InspectChild>;
    fn selected_child(&self, cx: &App) -> Option<u64>;
    fn select_child(&self, child: Option<u64>, cx: &mut App);
    /// Highlight `child` on the host while its row is under the pointer.
    /// Leaving one row and entering the next can arrive in either order,
    /// so un-hovering clears only the child it names.
    fn hover_child(&self, child: u64, hovered: bool, cx: &mut App);
    /// `None` once the child is gone.
    fn child_rows(&self, child: u64, cx: &App) -> Option<Vec<Box<dyn InspectorRow>>>;
    /// Rows for the host itself, shown after its children.
    fn host_rows(&self, cx: &App) -> Vec<Box<dyn InspectorRow>>;
    /// Call `changed` whenever the host notifies: its children, selection
    /// or own rows may have moved.
    fn observe(&self, changed: Box<dyn Fn(&mut App)>, cx: &mut App) -> Subscription;
}
