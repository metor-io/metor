//! Tab positions animate after layout, so text layout and pointer hitboxes stay coherent.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, Size, Window,
};

const DURATION: Duration = Duration::from_millis(120);

pub(super) type TabSizes = Rc<RefCell<HashMap<gpui::EntityId, Size<Pixels>>>>;

#[derive(Default)]
pub(super) struct Rail {
    pub bounds: Cell<Bounds<Pixels>>,
    sizes: TabSizes,
    requested_frame: Cell<bool>,
}

impl Rail {
    pub(super) fn new(sizes: TabSizes) -> Self {
        Self {
            sizes,
            ..Self::default()
        }
    }
}

struct Position {
    from: Point<Pixels>,
    target: Point<Pixels>,
    started: Instant,
}

impl Position {
    fn new(target: Point<Pixels>, now: Instant) -> Self {
        Self {
            from: target,
            target,
            started: now,
        }
    }

    fn sample(&self, now: Instant) -> Point<Pixels> {
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / DURATION.as_secs_f32())
        .min(1.0);
        if progress >= 1.0 {
            return self.target;
        }
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.from + (self.target - self.from) * eased
    }

    fn update(&mut self, target: Point<Pixels>, animate: bool, now: Instant) -> Point<Pixels> {
        if !animate {
            *self = Self::new(target, now);
        } else if self.target != target {
            self.from = self.sample(now);
            self.target = target;
            self.started = now;
        }
        self.sample(now)
    }
}

struct TabPosition {
    position: Position,
    rail_size: Size<Pixels>,
    viewport: Size<Pixels>,
    vertical: bool,
}

/// Shares the child's layout node, then offsets its prepaint so paint and input
/// both follow the displayed position. State belongs to the tab's stable ID.
pub(super) struct MovingTab {
    id: ElementId,
    item_id: gpui::EntityId,
    child: AnyElement,
    rail: Rc<Rail>,
    animate: bool,
    vertical: bool,
}

impl MovingTab {
    pub(super) fn new(
        id: gpui::EntityId,
        child: impl IntoElement,
        rail: Rc<Rail>,
        animate: bool,
        vertical: bool,
    ) -> Self {
        Self {
            id: ("tab-position", id).into(),
            item_id: id,
            child: child.into_any_element(),
            rail,
            animate,
            vertical,
        }
    }
}

impl IntoElement for MovingTab {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for MovingTab {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.rail
            .sizes
            .borrow_mut()
            .insert(self.item_id, bounds.size);
        let rail = self.rail.bounds.get();
        // Both origins include the same scroll offset. Subtracting them keeps
        // scrolling and movement of the pane itself out of the animation.
        let target = bounds.origin - rail.origin;
        let now = Instant::now();
        let viewport = window.viewport_size();
        let animate = self.animate && crate::motion::enabled(cx);
        window.with_element_state(id.unwrap(), |state: Option<TabPosition>, window| {
            let mut state = state.unwrap_or_else(|| TabPosition {
                position: Position::new(target, now),
                rail_size: rail.size,
                viewport,
                vertical: self.vertical,
            });
            let animate = animate
                && state.rail_size == rail.size
                && state.viewport == viewport
                && state.vertical == self.vertical;
            let displayed = state.position.update(target, animate, now);
            state.rail_size = rail.size;
            state.viewport = viewport;
            state.vertical = self.vertical;
            if displayed != target && !self.rail.requested_frame.replace(true) {
                window.request_animation_frame();
            }
            window.with_element_offset(displayed - target, |window| {
                self.child.prepaint(window, cx);
            });
            ((), state)
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, div, point, prelude::*, px};

    #[test]
    fn reordering_retargets_from_the_displayed_position() {
        let start = Instant::now();
        let origin = point(px(0.0), px(0.0));
        let target = point(px(100.0), px(28.0));
        let mut position = Position::new(origin, start);
        assert_eq!(position.update(target, true, start), origin);
        let halfway = start + DURATION / 2;
        let displayed = position.sample(halfway);
        assert!(displayed.x > origin.x && displayed.x < target.x);
        assert_eq!(position.update(origin, true, halfway), displayed);
        assert_eq!(position.sample(halfway + DURATION), origin);
    }

    #[test]
    fn direct_manipulation_and_reduced_motion_snap_and_clear_travel() {
        let start = Instant::now();
        let mut position = Position::new(point(px(0.0), px(0.0)), start);
        let target = point(px(100.0), px(0.0));
        assert_eq!(position.update(target, false, start), target);
        assert_eq!(position.update(target, true, start + DURATION), target);
    }

    struct TestRail {
        first: bool,
        tab_id: gpui::EntityId,
        displayed: Rc<Cell<Bounds<Pixels>>>,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for TestRail {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rail = Rc::new(Rail::default());
            let tracker = rail.clone();
            let displayed = self.displayed.clone();
            let clicks = self.clicks.clone();
            div()
                .relative()
                .flex()
                .w_full()
                .h(px(28.0))
                .child(
                    gpui::canvas(
                        move |bounds, _, _| tracker.bounds.set(bounds),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .when(self.first, |bar| {
                    bar.child(div().w(px(100.0)).h_full().flex_shrink_0())
                })
                .child(MovingTab::new(
                    self.tab_id,
                    div()
                        .w(px(100.0))
                        .h_full()
                        .flex_shrink_0()
                        .on_mouse_down(gpui::MouseButton::Left, move |_, _, _| {
                            clicks.set(clicks.get() + 1)
                        })
                        .child(
                            gpui::canvas(
                                move |bounds, _, _| displayed.set(bounds),
                                |_, _, _, _| {},
                            )
                            .size_full(),
                        ),
                    rail,
                    true,
                    false,
                ))
        }
    }

    #[gpui::test]
    fn moving_tab_hitbox_follows_its_displayed_bounds(cx: &mut gpui::TestAppContext) {
        let displayed = Rc::new(Cell::new(Bounds::default()));
        let clicks = Rc::new(Cell::new(0));
        let (host, cx) = cx.add_window_view(|_, cx| TestRail {
            first: true,
            tab_id: cx.entity_id(),
            displayed: displayed.clone(),
            clicks: clicks.clone(),
        });
        cx.refresh().unwrap();
        assert_eq!(displayed.get().origin.x, px(100.0));
        cx.update(|_, cx| {
            host.update(cx, |host, cx| {
                host.first = false;
                cx.notify();
            })
        });
        cx.refresh().unwrap();
        assert!(
            displayed.get().origin.x > px(50.0),
            "tab starts moving from its old location"
        );
        let position = displayed.get().origin + point(px(10.0), px(10.0));
        cx.simulate_click(position, gpui::Modifiers::default());
        assert_eq!(
            clicks.get(),
            1,
            "click is routed to the displayed tab, not its target bounds"
        );
    }
}
