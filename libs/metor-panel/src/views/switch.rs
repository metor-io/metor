//! The panel's one switch: a track with a square knob that sits right when
//! on. Inspector rows, bool value boxes, two-way choices like the sequence
//! panel's Channels / History, and the dashboard's many-position toggles all
//! draw it, at their own heights.

use std::rc::Rc;

use gpui::{
    AbsoluteLength, AnimationExt, App, Hsla, MouseButton, ParentElement, SharedString,
    SpringAnimation, SpringConfig, Styled, Window, div, prelude::*, px, relative,
};

use crate::theme::{Theme, ui_px};

/// A critically damped spring that settles in about a fifth of a second:
/// the knob slides without overshooting.
const SLIDE: SpringConfig = SpringConfig::new(900.0, 60.0, 1.0);

/// Called with the position a click on a labelled switch picked.
pub(crate) type OnPick = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// How a switch looks; the caller owns its state, and styles its own
/// element with [`Switch::apply`].
pub(crate) struct Switch {
    /// The position the knob sits at: `0` off and `1` on for a plain
    /// switch. `None` lights no position, for a value no label names.
    chosen: Option<usize>,
    height: f32,
    scaled: bool,
    slides: bool,
    /// One per position, in order. Empty for a plain on/off switch.
    labels: Vec<SharedString>,
    track: Option<Hsla>,
    knob: Option<(Hsla, Hsla)>,
    on_pick: Option<OnPick>,
}

impl Switch {
    /// A switch `height` pixels tall. Everything else follows from the
    /// height in the inspector's proportions: a knob five sevenths of it,
    /// inset by a seventh, on a track eleven sevenths wide. The knob slides
    /// between sides unless motion is turned down.
    pub(crate) fn new(on: bool, height: f32, cx: &App) -> Self {
        Self {
            chosen: Some(usize::from(on)),
            height,
            scaled: false,
            slides: crate::motion::enabled(cx),
            labels: Vec::new(),
            track: None,
            knob: None,
            on_pick: None,
        }
    }

    /// A switch between named positions, the knob under the `chosen` one.
    /// See [`Switch::labels`] for how it is laid out.
    pub(crate) fn choice(
        labels: Vec<SharedString>,
        chosen: Option<usize>,
        height: f32,
        cx: &App,
    ) -> Self {
        Self {
            chosen,
            labels,
            ..Self::new(false, height, cx)
        }
    }

    /// Size in UI units, which follow the panel's text size, rather than
    /// pixels.
    pub(crate) fn scaled(mut self) -> Self {
        self.scaled = true;
        self
    }

    /// Name both sides inside the track. Labelled positions share the track
    /// in equal columns with the knob widened under the chosen one. A choice
    /// between named things has no "on", so the track is the buttons'
    /// surface whatever is chosen, and the labels are sized like button
    /// text: 11px on a 24px switch, padded as wide as a button's.
    pub(crate) fn labels(
        mut self,
        off: impl Into<SharedString>,
        on: impl Into<SharedString>,
    ) -> Self {
        self.labels = vec![off.into(), on.into()];
        self
    }

    /// Paint the track `color` in place of its on and off colours, for
    /// states that outrank the value: an unsent edit, a dead producer.
    pub(crate) fn track(mut self, color: Hsla) -> Self {
        self.track = Some(color);
        self
    }

    /// Paint the knob `fill`, with the label over it in `text`, in place of
    /// the light knob: a position's own colour, or a stale value's.
    pub(crate) fn knob(mut self, fill: Hsla, text: Hsla) -> Self {
        self.knob = Some((fill, text));
        self
    }

    /// Let a click on a labelled position pick it.
    pub(crate) fn on_pick(mut self, pick: OnPick) -> Self {
        self.on_pick = Some(pick);
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
        let track = self
            .track
            .unwrap_or(match (self.chosen, self.labels.is_empty()) {
                (_, false) => theme.bg_elevated,
                (Some(1), true) => theme.control_active_track(),
                (_, true) => theme.text_tertiary,
            });
        let el = el
            .flex()
            .flex_none()
            .h(self.len(h))
            .p(self.len(h / 7.0))
            .rounded(self.len(h * 3.0 / 14.0))
            .bg(track);
        let el = match self.labels.is_empty() {
            true => el.w(self.len(h * 11.0 / 7.0)),
            false => el,
        };
        // Inside the inset: the knob is placed against this box, by the
        // position it is at, fractions while it slides between them.
        let lane = div().relative().flex_1().min_w_0().h_full();
        let target = self.chosen.unwrap_or(0) as f32;
        let lane = match self.slides {
            true => lane
                .with_spring(
                    "switch-knob",
                    SpringAnimation::new(SLIDE).to(target).with_epsilon(0.001),
                    {
                        let theme = theme.clone();
                        move |lane, at| self.lane(lane, at, &theme)
                    },
                )
                .into_any_element(),
            false => self.lane(lane, target, theme).into_any_element(),
        };
        el.child(lane)
    }

    /// The lane with its knob `at` a position, and the labels over it, each
    /// in the knob's colours as far as the knob covers it.
    fn lane(self, lane: gpui::Div, at: f32, theme: &Theme) -> gpui::Div {
        let (fill, on_knob) = self.knob.unwrap_or((theme.text_primary, theme.bg_primary));
        let knob = div()
            .debug_selector(|| "switch-knob".into())
            .absolute()
            .top_0()
            .bottom_0()
            .rounded(self.len(self.height / 7.0))
            .bg(fill);
        if self.labels.is_empty() {
            let knob_w = self.height * 5.0 / 7.0;
            let travel = self.height * 11.0 / 7.0 - 2.0 * self.height / 7.0 - knob_w;
            return lane.child(knob.w(self.len(knob_w)).left(self.len(travel * at)));
        }
        // Equal columns, so the knob is one column wide and slides by whole
        // columns whatever the labels say.
        let columns = self.labels.len();
        let column = 1.0 / columns as f32;
        let lane = lane
            .grid()
            .grid_cols(columns as u16)
            .items_center()
            .when(self.chosen.is_some(), |lane| {
                lane.child(knob.w(relative(column)).left(relative(at * column)))
            });
        let labels = self.labels.iter().enumerate().map(|(i, text)| {
            let covered = match self.chosen {
                Some(_) => (1.0 - (at - i as f32).abs()).clamp(0.0, 1.0),
                None => 0.0,
            };
            let pick = self.on_pick.clone().filter(|_| self.chosen != Some(i));
            div()
                .debug_selector({
                    let text = text.clone();
                    move || format!("switch-label-{text}")
                })
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .min_w_0()
                .px(self.len(self.height / 3.0))
                .text_size(self.len(self.height * 11.0 / 24.0))
                .line_height(relative(1.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(mix(theme.text_secondary, on_knob, covered))
                .when_some(pick, |label, pick| {
                    let hover = theme.text_primary;
                    label
                        .cursor_pointer()
                        .hover(move |style| style.text_color(hover))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            pick(i, window, cx);
                            cx.stop_propagation();
                        })
                })
                .child(text.clone())
        });
        lane.children(labels.collect::<Vec<_>>())
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
            div().flex().child(switch.apply(div().id("switch"), &theme))
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

    struct Choice {
        chosen: Option<usize>,
        picked: std::rc::Rc<std::cell::Cell<Option<usize>>>,
    }

    impl Render for Choice {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::theme(cx);
            let picked = self.picked.clone();
            let switch = Switch::choice(
                vec!["SAFE".into(), "ARMED".into(), "FIRE".into()],
                self.chosen,
                24.0,
                cx,
            )
            .on_pick(std::rc::Rc::new(move |i, _, _| picked.set(Some(i))));
            div().flex().child(switch.apply(div().id("choice"), &theme))
        }
    }

    #[gpui::test]
    fn any_number_of_positions_share_the_track_and_pick_on_click(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()))
        });
        let picked = std::rc::Rc::new(std::cell::Cell::new(None));
        let (choice, cx) = cx.add_window_view({
            let picked = picked.clone();
            move |_, _| Choice {
                chosen: Some(1),
                picked,
            }
        });
        let knob = knob(cx);
        let [safe, armed, fire] = [
            "switch-label-SAFE",
            "switch-label-ARMED",
            "switch-label-FIRE",
        ]
        .map(|selector| cx.debug_bounds(selector).unwrap());
        let near = |a: gpui::Pixels, b: gpui::Pixels| (a - b).abs() <= gpui::px(1.0);
        assert!(near(safe.size.width, armed.size.width) && near(armed.size.width, fire.size.width));
        assert!(fire.size.width < gpui::px(80.0), "sized to its labels");
        assert!(
            near(knob.origin.x, armed.origin.x),
            "the knob sits under the chosen one"
        );

        cx.simulate_click(fire.center(), gpui::Modifiers::default());
        assert_eq!(picked.get(), Some(2));
        cx.simulate_click(armed.center(), gpui::Modifiers::default());
        assert_eq!(picked.get(), Some(2), "the chosen position isn't a button");

        cx.update(|_, cx| {
            choice.update(cx, |choice, cx| {
                choice.chosen = None;
                cx.notify();
            })
        });
        cx.update(|window, _| window.refresh());
        assert!(
            cx.debug_bounds("switch-knob").is_none(),
            "a value no position names lights none"
        );
    }
}
