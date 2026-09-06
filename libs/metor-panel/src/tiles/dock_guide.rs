//! Stable docking targets, independent of the layout being previewed.
use gpui::{
    App, Bounds, Div, Entity, Pixels, Point, Stateful, Window, div, point, prelude::*, px, size,
};

use super::{Pane, SplitDirection, drag::detect_split_zone};
use crate::{
    motion::{Fade, MENU_ENTER},
    theme::{Theme, theme},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DockTarget {
    Tab,
    Split(SplitDirection),
}

impl DockTarget {
    const ALL: [Self; 5] = [
        Self::Split(SplitDirection::Up),
        Self::Split(SplitDirection::Left),
        Self::Tab,
        Self::Split(SplitDirection::Right),
        Self::Split(SplitDirection::Down),
    ];

    fn miniature(self, scale: f32, selected: bool, theme: &Theme) -> Div {
        let vertical = matches!(self, Self::Split(SplitDirection::Up | SplitDirection::Down));
        let leading = matches!(
            self,
            Self::Tab | Self::Split(SplitDirection::Up | SplitDirection::Left)
        );
        let cell = |filled| {
            div()
                .flex_1()
                .size_full()
                .rounded(px(3.0 * scale))
                .border_1()
                .border_color(if selected && filled {
                    theme.control_active
                } else {
                    theme.border_primary
                })
                .when(filled, |cell| {
                    cell.bg(if selected {
                        Theme::dim(theme.control_active, 0.11)
                    } else {
                        theme.bg_secondary
                    })
                })
        };
        div()
            .size_full()
            .flex()
            .when(vertical, |layout| layout.flex_col())
            .gap(px(4.0 * scale))
            .child(cell(leading))
            .when(self != Self::Tab, |layout| layout.child(cell(!leading)))
    }
}

#[derive(Clone, Copy)]
pub(super) struct GuideLayout {
    region: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    scale: f32,
}

impl GuideLayout {
    pub fn new(region: Bounds<Pixels>) -> Self {
        let scale = (f32::from(region.size.width) / 184.0)
            .min(f32::from(region.size.height) / 184.0)
            .clamp(0.0, 1.0);
        let extent = size(px(184.0 * scale), px(184.0 * scale));
        Self {
            region,
            bounds: Bounds::new(
                region.center() - point(extent.width / 2.0, extent.height / 2.0),
                extent,
            ),
            scale,
        }
    }

    pub fn target(self, target: DockTarget) -> Bounds<Pixels> {
        let (column, row) = match target {
            DockTarget::Tab => (1.0, 1.0),
            DockTarget::Split(SplitDirection::Up) => (1.0, 0.0),
            DockTarget::Split(SplitDirection::Down) => (1.0, 2.0),
            DockTarget::Split(SplitDirection::Left) => (0.0, 1.0),
            DockTarget::Split(SplitDirection::Right) => (2.0, 1.0),
        };
        Bounds::new(
            self.bounds.origin + point(px(column * 68.0 * self.scale), px(row * 68.0 * self.scale)),
            size(px(48.0 * self.scale), px(48.0 * self.scale)),
        )
    }
}

pub(super) struct DockGuide {
    pub pane: Entity<Pane>,
    pub layout: GuideLayout,
    pub hovered: Option<DockTarget>,
    pub allow_split: bool,
    fade: Fade,
}

impl DockGuide {
    pub fn new(pane: Entity<Pane>, bounds: Bounds<Pixels>, allow_split: bool) -> Self {
        Self {
            pane,
            layout: GuideLayout::new(bounds),
            hovered: None,
            allow_split,
            fade: Fade::entrance(MENU_ENTER),
        }
    }

    pub fn hit(&self, position: Point<Pixels>) -> Option<DockTarget> {
        if !self.layout.region.contains(&position) {
            return None;
        }
        // Cards are explicit targets; everywhere else uses the pane's original
        // edge/center zones, so preview reflow cannot move the target away.
        let target = DockTarget::ALL
            .into_iter()
            .find(|target| self.layout.target(*target).contains(&position))
            .unwrap_or_else(|| {
                detect_split_zone(position, self.layout.region)
                    .map(DockTarget::Split)
                    .unwrap_or(DockTarget::Tab)
            });
        (target == DockTarget::Tab || self.allow_split).then_some(target)
    }

    pub fn render(&mut self, origin: Point<Pixels>, window: &Window, cx: &App) -> Stateful<Div> {
        let theme = theme(cx);
        let layout = self.layout;
        let position = layout.bounds.origin - origin;
        let mut guide = div()
            .id(("dock-guide", self.pane.entity_id()))
            .debug_selector(|| "dock-guide".into())
            .absolute()
            .left(position.x)
            .top(position.y)
            .w(layout.bounds.size.width)
            .h(layout.bounds.size.height)
            .occlude()
            .opacity(self.fade.opacity(window, cx));
        for (index, target) in DockTarget::ALL.into_iter().enumerate() {
            let bounds = layout.target(target);
            let selected = self.hovered == Some(target);
            let enabled = target == DockTarget::Tab || self.allow_split;
            let position = bounds.origin - layout.bounds.origin;
            guide = guide.child(
                div()
                    .id(index)
                    .absolute()
                    .left(position.x)
                    .top(position.y)
                    .w(bounds.size.width)
                    .h(bounds.size.height)
                    .flex()
                    .p(px(5.0 * layout.scale))
                    .rounded(px(4.0 * layout.scale))
                    .border_1()
                    .border_color(if selected {
                        theme.control_active
                    } else {
                        theme.border_primary
                    })
                    .bg(theme.bg_elevated)
                    .opacity(if enabled { 1.0 } else { 0.3 })
                    .child(target.miniature(layout.scale, selected, &theme)),
            );
        }
        guide
    }
}
