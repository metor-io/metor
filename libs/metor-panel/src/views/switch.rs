//! The panel's one switch: a track with a square knob that sits right when
//! on. Inspector rows, bool value boxes and two-way choices like the
//! sequence panel's Channels / History all draw it, at their own heights.

use gpui::{
    AbsoluteLength, AnimationExt, App, Hsla, ParentElement, SharedString, SpringAnimation,
    SpringConfig, Styled, div, prelude::*, px, relative,
};

use crate::theme::{Theme, ui_px};

/// A critically damped spring that settles in about a fifth of a second:
/// the knob slides without overshooting.
const SLIDE: SpringConfig = SpringConfig::new(900.0, 60.0, 1.0);

/// How a switch looks; the caller owns its state and click handling, and
/// styles its own element with [`Switch::apply`].
pub(crate) struct Switch {
    on: bool,
    height: f32,
    scaled: bool,
    slides: bool,
    labels: Option<(SharedString, SharedString)>,
    track: Option<Hsla>,
}

impl Switch {
    /// A switch `height` pixels tall. Everything else follows from the
    /// height in the inspector's proportions: a knob five sevenths of it,
    /// inset by a seventh, on a track eleven sevenths wide. The knob slides
    /// between sides unless motion is turned down.
    pub(crate) fn new(on: bool, height: f32, cx: &App) -> Self {
        Self {
            on,
            height,
            scaled: false,
            slides: crate::motion::enabled(cx),
            labels: None,
            track: None,
        }
    }

    /// Size in UI units, which follow the panel's text size, rather than
    /// pixels.
    pub(crate) fn scaled(mut self) -> Self {
        self.scaled = true;
        self
    }

    /// Name both sides inside the track, with the knob widened under
    /// whichever is chosen. A choice between two named things has no "on",
    /// so the track is the buttons' surface either way, and the labels are
    /// sized like button text: 11px on an 18px switch.
    pub(crate) fn labels(
        mut self,
        off: impl Into<SharedString>,
        on: impl Into<SharedString>,
    ) -> Self {
        self.labels = Some((off.into(), on.into()));
        self
    }

    /// Paint the track `color` in place of its on and off colours, for
    /// states that outrank the value: an unsent edit, a dead producer.
    pub(crate) fn track(mut self, color: Hsla) -> Self {
        self.track = Some(color);
        self
    }

    fn len(&self, n: f32) -> AbsoluteLength {
        match self.scaled {
            true => ui_px(n).into(),
            false => px(n).into(),
        }
    }

    /// Style `el` as this switch: its size, track and knob. The knob's
    /// slide is keyed under the nearest element id above it, so each switch
    /// needs one of its own.
    pub(crate) fn apply<E: Styled + ParentElement>(self, el: E, theme: &Theme) -> E {
        let h = self.height;
        // A labelled switch sits among buttons, so it takes their surface.
        let track = self.track.unwrap_or(match (self.on, &self.labels) {
            (_, Some(_)) => theme.bg_elevated,
            (true, None) => theme.control_active_track(),
            (false, None) => theme.text_tertiary,
        });
        let el = el
            .flex()
            .flex_none()
            .h(self.len(h))
            .p(self.len(h / 7.0))
            .rounded(self.len(h * 3.0 / 14.0))
            .bg(track);
        let el = match &self.labels {
            None => el.w(self.len(h * 11.0 / 7.0)),
            Some(_) => el,
        };
        // Inside the inset: the knob is placed against this box, as a
        // fraction of the way from the off side to the on side.
        let lane = div().relative().flex_1().h_full();
        let target = if self.on { 1.0 } else { 0.0 };
        let lane = match self.slides {
            true => lane
                .with_spring(
                    "switch-knob",
                    SpringAnimation::new(SLIDE).to(target).with_epsilon(0.001),
                    {
                        let theme = theme.clone();
                        move |lane, at| self.knob(lane, at, &theme)
                    },
                )
                .into_any_element(),
            false => self.knob(lane, target, theme).into_any_element(),
        };
        el.child(lane)
    }

    /// The lane with its knob `at` the given fraction of the way to on, and
    /// the labels over it, each in the knob's colours as it passes under.
    fn knob(self, lane: gpui::Div, at: f32, theme: &Theme) -> gpui::Div {
        let knob = div()
            .debug_selector(|| "switch-knob".into())
            .absolute()
            .top_0()
            .bottom_0()
            .rounded(self.len(self.height / 7.0))
            .bg(theme.text_primary);
        let Some((off, on)) = self.labels.clone() else {
            let knob_w = self.height * 5.0 / 7.0;
            let travel = self.height * 11.0 / 7.0 - 2.0 * self.height / 7.0 - knob_w;
            return lane.child(knob.w(self.len(knob_w)).left(self.len(travel * at)));
        };
        let label = |text: SharedString, covered: f32| {
            div()
                .debug_selector({
                    let text = text.clone();
                    move || format!("switch-label-{text}")
                })
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .px(self.len(self.height * 2.0 / 7.0))
                .text_size(self.len(self.height * 11.0 / 18.0))
                .line_height(relative(1.0))
                .text_color(mix(theme.text_secondary, theme.bg_primary, covered))
                .child(text)
        };
        // Two equal columns, so the knob is half the lane wide and slides by
        // half of it whatever the labels say.
        lane.grid()
            .grid_cols(2)
            .items_center()
            .child(knob.w(relative(0.5)).left(relative(at * 0.5)))
            .child(label(off, 1.0 - at))
            .child(label(on, at))
    }
}

/// `a` moved `t` of the way to `b`.
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a = gpui::Rgba::from(a);
    let b = gpui::Rgba::from(b);
    let t = t.clamp(0.0, 1.0);
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, Context, Pixels, Render, Window, div, prelude::*};

    use super::Switch;

    struct Rig {
        on: bool,
        labels: bool,
    }

    impl Render for Rig {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::theme(cx);
            let switch = Switch::new(self.on, 14.0, cx);
            let switch = match self.labels {
                true => switch.labels("Channels", "History"),
                false => switch,
            };
            div().child(switch.apply(div().id("switch"), &theme))
        }
    }

    fn rig(
        labels: bool,
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<Rig>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()))
        });
        cx.add_window_view(move |_, _| Rig { on: false, labels })
    }

    fn knob(cx: &mut gpui::VisualTestContext) -> Bounds<Pixels> {
        cx.update(|window, _| window.refresh());
        cx.debug_bounds("switch-knob").unwrap()
    }

    #[gpui::test]
    fn the_knob_slides_to_the_side_that_is_on(cx: &mut gpui::TestAppContext) {
        let (rig, cx) = rig(false, cx);
        let off = knob(cx);
        cx.update(|_, cx| {
            rig.update(cx, |rig, cx| {
                rig.on = true;
                cx.notify();
            })
        });
        let moving = knob(cx);
        assert!(
            moving.origin.x < off.origin.x + gpui::px(8.0),
            "it starts where it was"
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        let partway = knob(cx);
        assert!(partway.origin.x > off.origin.x, "and moves toward on");
        std::thread::sleep(std::time::Duration::from_millis(500));
        knob(cx);
        let on = knob(cx);
        assert_eq!(
            on.origin.x - off.origin.x,
            gpui::px(8.0),
            "22 wide, 10 knob, 2 inset"
        );
    }

    #[gpui::test]
    fn labelled_sides_share_the_track_evenly(cx: &mut gpui::TestAppContext) {
        let (_, cx) = rig(true, cx);
        let knob = knob(cx);
        let off = cx.debug_bounds("switch-label-Channels").unwrap();
        let on = cx.debug_bounds("switch-label-History").unwrap();
        assert_eq!(off.size.width, on.size.width);
        assert_eq!(knob.size.width, off.size.width);
        assert_eq!(knob.origin.x, off.origin.x);
    }
}
