//! Temporary split layout, using the same insertion rules as the committed tree.
use std::{collections::HashMap, time::Duration};

use gpui::{App, AppContext, Bounds, Entity, EntityId, Pixels, Point, Window};

use super::{Member, Pane, SplitDirection, drag::nearest_edge};
use crate::motion::Fade;

const DURATION: Duration = Duration::from_millis(120);
pub(super) type GapWeights = HashMap<EntityId, f32>;

struct Gap {
    target: Entity<Pane>,
    direction: SplitDirection,
    placeholder: Entity<Pane>,
    transition: Fade,
}

/// The split a dragged pane would make, previewed by opening a gap in the
/// tree where it would land and closing the slot it would leave.
#[derive(Default)]
pub(super) struct SplitPreview {
    pub bounds: Bounds<Pixels>,
    pub target: Option<(Entity<Pane>, SplitDirection)>,
    /// Each pane's bounds as the drag found them. Targets are judged against
    /// these rather than the reflowing preview, so opening a gap cannot move
    /// the target out from under a still pointer.
    regions: Vec<(Entity<Pane>, Bounds<Pixels>)>,
    gaps: Vec<Gap>,
    /// The pane being dragged, which can't be its own target; `None` while
    /// a whole tab is dragged.
    source: Option<Entity<Pane>>,
}

impl SplitPreview {
    /// Follow a drag of `source` (a pane, or a whole tab when `None`) to
    /// `position` over `panes`.
    pub fn drag_move(
        &mut self,
        panes: &[Entity<Pane>],
        source: Option<Entity<Pane>>,
        position: Point<Pixels>,
        cx: &mut App,
    ) -> bool {
        // A tab opened mid-drag has panes never drawn before; measure again
        // until every region has a size.
        let unmeasured = |(_, bounds): &(Entity<Pane>, Bounds<Pixels>)| {
            bounds.size.width <= gpui::px(0.0) || bounds.size.height <= gpui::px(0.0)
        };
        if self.regions.is_empty() || self.regions.iter().any(unmeasured) {
            self.regions = panes
                .iter()
                .map(|pane| (pane.clone(), pane.read(cx).bounds()))
                .collect();
        }
        let target = self.hit(panes, source.as_ref(), position);
        self.source = source;
        self.set_target(target, cx)
    }

    fn hit(
        &self,
        panes: &[Entity<Pane>],
        dragged: Option<&Entity<Pane>>,
        position: Point<Pixels>,
    ) -> Option<(Entity<Pane>, SplitDirection)> {
        self.regions
            .iter()
            .find(|(pane, bounds)| {
                Some(pane) != dragged
                    && panes.contains(pane)
                    && bounds.contains(&position)
                    && bounds.size.width > gpui::px(0.0)
                    && bounds.size.height > gpui::px(0.0)
            })
            .map(|(pane, bounds)| (pane.clone(), nearest_edge(position, *bounds)))
    }

    fn set_target(&mut self, target: Option<(Entity<Pane>, SplitDirection)>, cx: &mut App) -> bool {
        if self.target == target {
            return false;
        }
        for gap in &mut self.gaps {
            if self
                .target
                .as_ref()
                .is_some_and(|(pane, direction)| pane == &gap.target && *direction == gap.direction)
            {
                gap.transition.exit(DURATION);
            }
        }
        if let Some((pane, direction)) = &target {
            if let Some(gap) = self
                .gaps
                .iter_mut()
                .find(|gap| &gap.target == pane && gap.direction == *direction)
            {
                gap.transition.enter(DURATION);
            } else {
                self.gaps.push(Gap {
                    target: pane.clone(),
                    direction: *direction,
                    // No items, subscriptions or pane registration: this entity
                    // exists only to reuse the real split-tree layout algorithm.
                    placeholder: cx.new(|cx| Pane::new(None, cx)),
                    transition: Fade::entrance(DURATION),
                });
            }
        }
        self.target = target;
        true
    }

    pub fn layout(
        &mut self,
        committed: &Member,
        window: &Window,
        cx: &mut App,
    ) -> (Option<Member>, GapWeights) {
        if !cx.has_active_drag() {
            self.set_target(None, cx);
            self.regions.clear();
        }
        let mut root = (!self.gaps.is_empty()).then(|| committed.clone());
        let mut weights = GapWeights::default();
        let mut openness = 0.0_f32;
        self.gaps.retain_mut(|gap| {
            let amount = gap.transition.opacity(window, cx);
            let active = self.target.as_ref().is_some_and(|(pane, direction)| {
                pane == &gap.target && *direction == gap.direction
            });
            if amount == 0.0 && !active {
                return false;
            }
            if !root.as_mut().unwrap().split(
                &gap.target,
                &Member::Pane(gap.placeholder.clone()),
                gap.direction,
            ) {
                return false;
            }
            weights.insert(gap.placeholder.entity_id(), amount);
            openness = openness.max(amount);
            true
        });
        // The source pane leaves its slot on a successful split elsewhere.
        // Shrink that slot alongside the destination gap, keeping the
        // committed tree intact until the drop.
        if openness > 0.0
            && let Some(source) = &self.source
        {
            weights.insert(source.entity_id(), 1.0 - openness);
        }
        if self.gaps.is_empty() {
            root = None;
            if !cx.has_active_drag() {
                self.source = None;
            }
        }
        (root, weights)
    }

    pub fn drop_regions(&self) -> &[(Entity<Pane>, Bounds<Pixels>)] {
        &self.regions
    }

    pub fn drop_target(&self, position: Point<Pixels>) -> Option<(Entity<Pane>, SplitDirection)> {
        let panes: Vec<_> = self.regions.iter().map(|(pane, _)| pane.clone()).collect();
        self.hit(&panes, self.source.as_ref(), position)
    }

    /// Measure the panes again on the next move, for a tree that changed
    /// under the drag (another tab opened).
    pub fn forget_regions(&mut self) {
        self.regions.clear();
        self.gaps.clear();
        self.target = None;
    }

    pub fn clear(&mut self) {
        self.target = None;
        self.regions.clear();
        self.gaps.clear();
        self.source = None;
    }
}
