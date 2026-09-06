//! Temporary split layout, using the same insertion rules as the committed tree.
use std::{collections::HashMap, time::Duration};

use gpui::{App, AppContext, Bounds, DragMoveEvent, Entity, EntityId, Pixels, Window};

use super::{
    Member, Pane, SplitDirection,
    dock_guide::{DockGuide, DockTarget},
    drag::DraggedTab,
};
use crate::motion::Fade;

const DURATION: Duration = Duration::from_millis(120);
pub(super) type GapWeights = HashMap<EntityId, f32>;

struct Gap {
    target: Entity<Pane>,
    direction: SplitDirection,
    placeholder: Entity<Pane>,
    transition: Fade,
}

#[derive(Default)]
pub(super) struct SplitPreview {
    pub bounds: Bounds<Pixels>,
    pub target: Option<(Entity<Pane>, SplitDirection)>,
    regions: Vec<(Entity<Pane>, Bounds<Pixels>)>,
    gaps: Vec<Gap>,
    source: Option<Entity<Pane>>,
    pub guide: Option<DockGuide>,
}

impl SplitPreview {
    pub fn drag_move(
        &mut self,
        panes: &[Entity<Pane>],
        event: &DragMoveEvent<DraggedTab>,
        cx: &mut App,
    ) -> bool {
        if self.regions.is_empty() {
            self.regions = panes
                .iter()
                .map(|pane| (pane.clone(), pane.read(cx).content_bounds()))
                .collect();
        }
        let dragged = event.drag(cx);
        self.source = Some(dragged.pane.clone());
        let source = dragged.pane.read(cx);
        let candidate = self.regions.iter().find(|(pane, bounds)| {
            panes.contains(pane)
                && bounds.contains(&event.event.position)
                && bounds.size.width > gpui::px(0.0)
                && bounds.size.height > gpui::px(0.0)
                && source.index_of(dragged.item.entity_id()).is_some()
        });
        let mut changed = false;
        if let Some((pane, bounds)) = candidate {
            let allow_split = pane != &dragged.pane || source.items().len() > 1;
            if self.guide.as_ref().is_none_or(|guide| &guide.pane != pane) {
                self.guide = Some(DockGuide::new(pane.clone(), *bounds, allow_split));
                changed = true;
            }
            let guide = self.guide.as_mut().unwrap();
            changed |= guide.allow_split != allow_split;
            guide.allow_split = allow_split;
            let hovered = guide.hit(event.event.position);
            changed |= guide.hovered != hovered;
            guide.hovered = hovered;
        } else {
            changed = self.guide.take().is_some();
        }
        let target = self.guide.as_ref().and_then(|guide| match guide.hovered {
            Some(DockTarget::Split(direction)) => Some((guide.pane.clone(), direction)),
            _ => None,
        });
        self.set_target(target, cx) || changed
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
                    placeholder: cx.new(|cx| Pane::new(Vec::new(), cx)),
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
            self.guide = None;
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
            if !root
                .as_mut()
                .unwrap()
                .split(&gap.target, &gap.placeholder, gap.direction)
            {
                return false;
            }
            weights.insert(gap.placeholder.entity_id(), amount);
            openness = openness.max(amount);
            true
        });
        // A sole-tab source pane disappears on a successful split elsewhere.
        // Shrink its existing slot alongside the destination gap, keeping the
        // committed tree and item ownership intact until the drop.
        if openness > 0.0
            && let Some(source) = &self.source
            && source.read(cx).items().len() == 1
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

    pub fn drop_target(&self, position: gpui::Point<Pixels>) -> Option<(Entity<Pane>, DockTarget)> {
        let guide = self.guide.as_ref()?;
        guide
            .hit(position)
            .map(|target| (guide.pane.clone(), target))
    }

    pub fn clear(&mut self) {
        self.target = None;
        self.regions.clear();
        self.gaps.clear();
        self.source = None;
        self.guide = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{PaneItem, PaneItemHandle, TileGroup, dock_guide::GuideLayout};
    use gpui::{Context, IntoElement, MouseButton, Render, TestAppContext, div, point, px};

    struct Item;
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    struct Config;
    impl Render for Item {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
    impl PaneItem for Item {
        type Config = Config;
        fn tab_title(&self, _: &App) -> gpui::SharedString {
            "Tab".into()
        }
        fn serialization_key() -> &'static str {
            "split-preview-test"
        }
        fn to_config(&self, _: &App) -> Config {
            Config
        }
    }
    fn item(cx: &mut App) -> Box<dyn PaneItemHandle> {
        Box::new(cx.new(|_| Item))
    }

    fn lift(
        cx: &mut gpui::VisualTestContext,
        start: gpui::Point<Pixels>,
        end: gpui::Point<Pixels>,
    ) {
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            start + point(px(4.0), px(0.0)),
            MouseButton::Left,
            Default::default(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
    }

    fn settle(group: &Entity<TileGroup>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, cx| {
            group.update(cx, |group, cx| {
                for gap in &mut group.split_preview.gaps {
                    gap.transition.finish();
                }
                cx.notify();
            })
        });
        cx.refresh().unwrap();
    }

    #[gpui::test]
    fn every_split_arrow_previews_the_committed_bounds_without_mutating_the_tree(
        cx: &mut TestAppContext,
    ) {
        for direction in [
            SplitDirection::Left,
            SplitDirection::Right,
            SplitDirection::Up,
            SplitDirection::Down,
        ] {
            let (group, cx) = cx.add_window_view(|_, cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                TileGroup::new(vec![item(cx), item(cx)], cx)
            });
            cx.refresh().unwrap();
            let pane = cx.update(|_, cx| group.read(cx).panes[0].clone());
            let original = cx.update(|_, cx| pane.read(cx).content_bounds());
            let end = GuideLayout::new(original)
                .target(DockTarget::Split(direction))
                .center();
            lift(cx, point(px(8.0), px(12.0)), end);
            settle(&group, cx);
            let preview = cx.update(|_, cx| {
                let group = group.read(cx);
                assert!(
                    matches!(group.root, Member::Pane(_)),
                    "preview does not change the saved tree"
                );
                assert_eq!(group.panes.len(), 1);
                assert_eq!(group.split_preview.target.as_ref().unwrap().1, direction);
                pane.read(cx).content_bounds()
            });
            assert!(
                preview.size.width < original.size.width
                    || preview.size.height < original.size.height
            );
            // A stationary pointer remains on the same guide arrow after the pane shrinks.
            cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
            cx.update(|_, cx| {
                assert_eq!(
                    group.read(cx).split_preview.target.as_ref().unwrap().1,
                    direction
                )
            });
            cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                assert_eq!(group.read(cx).panes.len(), 2);
                assert_eq!(
                    pane.read(cx).content_bounds(),
                    preview,
                    "drop preserves previewed geometry"
                );
                assert_eq!(pane.read(cx).items().len(), 1);
            });
        }
    }

    #[gpui::test]
    fn cancelling_a_split_restores_geometry_and_ownership(cx: &mut TestAppContext) {
        let (group, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            TileGroup::new(vec![item(cx), item(cx)], cx)
        });
        cx.refresh().unwrap();
        let pane = cx.update(|_, cx| group.read(cx).panes[0].clone());
        let original = cx.update(|_, cx| pane.read(cx).content_bounds());
        lift(
            cx,
            point(px(8.0), px(12.0)),
            GuideLayout::new(original)
                .target(DockTarget::Split(SplitDirection::Left))
                .center(),
        );
        settle(&group, cx);
        cx.update(|window, cx| {
            cx.stop_active_drag(window);
        });
        cx.refresh().unwrap();
        settle(&group, cx);
        cx.update(|_, cx| {
            let group = group.read(cx);
            assert!(group.split_preview.gaps.is_empty());
            assert!(group.split_preview.target.is_none());
            assert_eq!(group.panes.len(), 1);
            assert_eq!(pane.read(cx).items().len(), 2);
            assert_eq!(pane.read(cx).content_bounds(), original);
        });
    }

    #[gpui::test]
    fn guide_stays_fixed_during_reflow_and_center_merges_tabs(cx: &mut TestAppContext) {
        for cross_pane in [false, true] {
            let (group, cx) = cx.add_window_view(|_, cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                let items = if cross_pane {
                    vec![item(cx)]
                } else {
                    vec![item(cx), item(cx)]
                };
                let mut group = TileGroup::new(items, cx);
                if cross_pane {
                    let source = group.panes[0].clone();
                    let target = cx.new(|cx| Pane::new(vec![item(cx)], cx));
                    group.split_pane(&source, target, SplitDirection::Right, cx);
                }
                group
            });
            cx.refresh().unwrap();
            let target = cx.update(|_, cx| group.read(cx).panes.last().unwrap().clone());
            let original = cx.update(|_, cx| target.read(cx).content_bounds());
            let positions = GuideLayout::new(original);
            let arrow = positions
                .target(DockTarget::Split(SplitDirection::Up))
                .center();
            lift(cx, point(px(8.0), px(12.0)), arrow);
            let guide_bounds = cx
                .debug_bounds("dock-guide")
                .expect("guide is visible while dragging");
            settle(&group, cx);
            assert_eq!(
                cx.debug_bounds("dock-guide"),
                Some(guide_bounds),
                "guide stays fixed while the panel moves"
            );
            cx.update(|_, cx| assert_ne!(target.read(cx).content_bounds(), original));
            // The center region also accepts drops away from the guide cards.
            let dock =
                original.center() + point(original.size.width * 0.2, original.size.height * 0.2);
            cx.simulate_mouse_move(dock, MouseButton::Left, Default::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                assert_eq!(
                    group.read(cx).split_preview.guide.as_ref().unwrap().hovered,
                    Some(DockTarget::Tab)
                )
            });
            assert_eq!(cx.debug_bounds("dock-guide"), Some(guide_bounds));
            cx.simulate_mouse_up(dock, MouseButton::Left, Default::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                assert_eq!(group.read(cx).panes.len(), 1);
                assert_eq!(target.read(cx).items().len(), 2);
                assert!(group.read(cx).split_preview.guide.is_none());
                assert!(group.read(cx).split_preview.gaps.is_empty());
            });
        }
    }

    #[gpui::test]
    fn pane_edges_outside_guide_cards_highlight_and_commit_splits(cx: &mut TestAppContext) {
        for direction in [
            SplitDirection::Up,
            SplitDirection::Down,
            SplitDirection::Left,
            SplitDirection::Right,
        ] {
            let (group, cx) = cx.add_window_view(|_, cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                TileGroup::new(vec![item(cx), item(cx)], cx)
            });
            cx.refresh().unwrap();
            let pane = cx.update(|_, cx| group.read(cx).panes[0].clone());
            let original = cx.update(|_, cx| pane.read(cx).content_bounds());
            let edge = match direction {
                SplitDirection::Up => point(original.center().x, original.top() + px(8.0)),
                SplitDirection::Down => point(original.center().x, original.bottom() - px(8.0)),
                SplitDirection::Left => point(original.left() + px(8.0), original.center().y),
                SplitDirection::Right => point(original.right() - px(8.0), original.center().y),
            };
            assert!(
                !GuideLayout::new(original)
                    .target(DockTarget::Split(direction))
                    .contains(&edge)
            );
            lift(cx, point(px(8.0), px(12.0)), edge);
            cx.update(|_, cx| {
                let preview = &group.read(cx).split_preview;
                assert_eq!(preview.target, Some((pane.clone(), direction)));
                assert_eq!(
                    preview.guide.as_ref().unwrap().hovered,
                    Some(DockTarget::Split(direction))
                );
            });
            settle(&group, cx);
            let preview_bounds = cx.update(|_, cx| pane.read(cx).content_bounds());
            cx.simulate_mouse_up(edge, MouseButton::Left, Default::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                let group = group.read(cx);
                assert_eq!(group.panes.len(), 2);
                assert!(
                    group
                        .panes
                        .iter()
                        .all(|pane| pane.read(cx).items().len() == 1)
                );
                assert_eq!(pane.read(cx).content_bounds(), preview_bounds);
            });
        }
    }

    #[gpui::test]
    fn lone_tab_cannot_split_into_its_own_pane(cx: &mut TestAppContext) {
        let (group, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            TileGroup::new(vec![item(cx)], cx)
        });
        cx.refresh().unwrap();
        let pane = cx.update(|_, cx| group.read(cx).panes[0].clone());
        let bounds = cx.update(|_, cx| pane.read(cx).content_bounds());
        let arrow = GuideLayout::new(bounds)
            .target(DockTarget::Split(SplitDirection::Right))
            .center();
        lift(cx, point(px(8.0), px(12.0)), arrow);
        cx.update(|_, cx| {
            let guide = group.read(cx).split_preview.guide.as_ref().unwrap();
            assert!(!guide.allow_split);
            assert!(guide.hovered.is_none());
        });
        cx.simulate_mouse_up(arrow, MouseButton::Left, Default::default());
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            assert_eq!(group.read(cx).panes.len(), 1);
            assert_eq!(pane.read(cx).items().len(), 1);
        });
    }

    #[gpui::test]
    fn switching_arrows_then_dropping_on_the_center_does_not_commit_a_split(
        cx: &mut TestAppContext,
    ) {
        let (group, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            TileGroup::new(vec![item(cx), item(cx)], cx)
        });
        cx.refresh().unwrap();
        let pane = cx.update(|_, cx| group.read(cx).panes[0].clone());
        let original = cx.update(|_, cx| pane.read(cx).content_bounds());
        let left = GuideLayout::new(original)
            .target(DockTarget::Split(SplitDirection::Left))
            .center();
        let right = GuideLayout::new(original)
            .target(DockTarget::Split(SplitDirection::Right))
            .center();
        lift(cx, point(px(8.0), px(12.0)), left);
        settle(&group, cx);
        cx.simulate_mouse_move(right, MouseButton::Left, Default::default());
        settle(&group, cx);
        cx.update(|_, cx| {
            let preview = &group.read(cx).split_preview;
            assert_eq!(preview.target.as_ref().unwrap().1, SplitDirection::Right);
            assert_eq!(
                preview.gaps.len(),
                1,
                "retired edge releases its placeholder"
            );
        });
        cx.simulate_mouse_move(
            GuideLayout::new(original).target(DockTarget::Tab).center(),
            MouseButton::Left,
            Default::default(),
        );
        // Release during the closing animation, while the actual pane is still
        // narrow. Drop classification must use the original center region.
        cx.simulate_mouse_up(
            GuideLayout::new(original).target(DockTarget::Tab).center(),
            MouseButton::Left,
            Default::default(),
        );
        cx.refresh().unwrap();
        cx.update(|_, cx| {
            assert_eq!(group.read(cx).panes.len(), 1);
            assert_eq!(pane.read(cx).items().len(), 2);
            assert_eq!(pane.read(cx).content_bounds(), original);
        });
    }

    #[gpui::test]
    fn last_tab_split_collapses_its_source_and_matches_the_final_nested_layout(
        cx: &mut TestAppContext,
    ) {
        for direction in [
            SplitDirection::Left,
            SplitDirection::Right,
            SplitDirection::Up,
            SplitDirection::Down,
        ] {
            let (group, cx) = cx.add_window_view(|_, cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                let mut group = TileGroup::new(vec![item(cx)], cx);
                let source = group.panes[0].clone();
                let target = cx.new(|cx| Pane::new(vec![item(cx)], cx));
                group.split_pane(&source, target, SplitDirection::Right, cx);
                group
            });
            cx.refresh().unwrap();
            let (source, target) = cx.update(|_, cx| {
                let panes = &group.read(cx).panes;
                (panes[0].clone(), panes[1].clone())
            });
            let original = cx.update(|_, cx| target.read(cx).content_bounds());
            let end = GuideLayout::new(original)
                .target(DockTarget::Split(direction))
                .center();
            lift(cx, point(px(8.0), px(12.0)), end);
            settle(&group, cx);
            let preview = cx.update(|_, cx| {
                assert_eq!(
                    source.read(cx).items().len(),
                    1,
                    "ownership stays in the source until release"
                );
                target.read(cx).content_bounds()
            });
            if direction.axis() == gpui::Axis::Vertical {
                assert!(
                    preview.size.width > original.size.width,
                    "source space is reclaimed during preview"
                );
            }
            cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
            cx.refresh().unwrap();
            cx.update(|_, cx| {
                let group = group.read(cx);
                assert_eq!(group.panes.len(), 2);
                assert!(!group.panes.contains(&source));
                assert_eq!(target.read(cx).content_bounds(), preview);
                assert_eq!(
                    group
                        .panes
                        .iter()
                        .map(|pane| pane.read(cx).items().len())
                        .sum::<usize>(),
                    2
                );
            });
        }
    }
}
