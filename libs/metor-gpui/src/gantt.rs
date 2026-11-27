//! A Gantt chart: one lane per item, bars on a shared time axis, and a live "now" line.

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent,
    MouseMoveEvent, PathBuilder, Pixels, Point, Render, ScrollWheelEvent, SharedString, Window,
    canvas, div, fill, point, prelude::*, px, size,
};
use smallvec::SmallVec;

use crate::timeline::{Interval, Navigation, tick_step, ticks};
use crate::{StateTone, Theme, popover::readout_card, theme, ui_px, ui_scale};

/// Height of one lane at UI scale 1; it scales with [`ui_px`].
pub const LANE_HEIGHT: f32 = 24.0;
/// Height of the time ruler above the lanes at UI scale 1.
pub const RULER_HEIGHT: f32 = 24.0;
/// Width of the label gutter left of the lanes at UI scale 1.
pub const GUTTER_WIDTH: f32 = 180.0;
/// Vertical inset of a bar inside its lane.
const BAR_INSET: f32 = 5.0;
/// How strongly one pixel of wheel travel zooms the time axis.
const WHEEL_ZOOM_RATE: f64 = 0.005;

/// One span of time on a lane. Times are microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GanttBar {
    pub start: i64,
    /// `None` while running: the bar extends to now.
    pub end: Option<i64>,
    pub tone: StateTone,
    /// Drawn faintly, such as the time an item spent queued.
    pub faint: bool,
}

/// One row of the chart. Lanes draw in the order given; the app orders them, for example
/// topologically.
#[derive(Clone, Debug, PartialEq)]
pub struct GanttLane {
    pub id: u64,
    pub label: SharedString,
    pub bars: SmallVec<[GanttBar; 1]>,
}

/// The lanes a [`Gantt`] draws, and dependency links between them by lane id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GanttModel {
    pub lanes: Vec<GanttLane>,
    /// `(from, to)`: an arrow from the end of `from`'s last bar to the start of `to`'s first
    /// solid bar.
    pub links: Vec<(u64, u64)>,
}

impl GanttModel {
    /// The time range the bars cover, with running bars reaching `now`.
    pub fn range(&self, now: i64) -> Option<Interval> {
        let bars = self.lanes.iter().flat_map(|lane| &lane.bars);
        let start = bars.clone().map(|bar| bar.start).min()?;
        let end = bars.map(|bar| bar.end.unwrap_or(now)).max()?;
        Some(Interval::new(start, end.max(start)))
    }

    /// Whether any bar is still running, which keeps the chart redrawing.
    pub fn is_live(&self) -> bool {
        self.lanes
            .iter()
            .flat_map(|lane| &lane.bars)
            .any(|bar| bar.end.is_none())
    }
}

/// What a [`Gantt`] tells its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GanttEvent {
    /// A bar on the lane with this id was clicked.
    BarClicked(u64),
}

/// The x of time `t` within a plot `width` pixels wide showing `view`.
pub fn time_to_x(view: Interval, t: i64, width: f32) -> f32 {
    (view.fraction(t) * f64::from(width)) as f32
}

/// The time at `x` within a plot `width` pixels wide showing `view`.
pub fn x_to_time(view: Interval, x: f32, width: f32) -> i64 {
    view.at(f64::from(x) / f64::from(width.max(1.0)))
}

/// The lane index at `y`, measured from the top of the first lane, when lanes are
/// `lane_height` pixels tall.
pub fn lane_at(y: f32, lane_height: f32, lanes: usize) -> Option<usize> {
    if y < 0.0 || lane_height <= 0.0 {
        return None;
    }
    let index = (y / lane_height) as usize;
    (index < lanes).then_some(index)
}

/// Microseconds since the Unix epoch.
fn now() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as i64)
}

/// A Gantt chart over a [`GanttModel`].
///
/// The time axis starts fitted to the bars ([`Navigation::Fit`]). Wheel zooms around the
/// cursor, horizontal wheel and drag pan, and either switches to a manual view; [`fit`] returns
/// to fitting.
///
/// [`fit`]: Gantt::fit
pub struct Gantt {
    model: GanttModel,
    navigation: Navigation,
    show_links: bool,
    timezone: SharedString,
    /// The plot area's bounds from the last paint, in window coordinates.
    plot: Option<Bounds<Pixels>>,
    hovered: Option<(usize, usize)>,
    pointer: Point<Pixels>,
    /// The pointer x of an ongoing pan.
    pan: Option<Pixels>,
    /// [`ui_scale`] at the last render, which turns the unscaled constants into pixels.
    scale: f32,
    focus: FocusHandle,
}

impl EventEmitter<GanttEvent> for Gantt {}

impl Focusable for Gantt {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Gantt {
    /// A chart over `model`, fitted to its bars.
    pub fn new(model: GanttModel, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            navigation: Navigation::Fit,
            show_links: true,
            timezone: "local".into(),
            plot: None,
            hovered: None,
            pointer: Point::default(),
            pan: None,
            scale: 1.0,
            focus: cx.focus_handle(),
        }
    }

    /// Replaces the model, keeping the time axis as it is.
    pub fn set_model(&mut self, model: GanttModel, cx: &mut Context<Self>) {
        self.model = model;
        self.hovered = None;
        cx.notify();
    }

    /// The model being drawn.
    pub fn model(&self) -> &GanttModel {
        &self.model
    }

    /// Show or hide the dependency arrows.
    pub fn set_show_links(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_links = show;
        cx.notify();
    }

    /// The zone ruler labels and day ticks use: `"local"`, `"UTC"` or an IANA name.
    pub fn set_timezone(&mut self, timezone: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.timezone = timezone.into();
        cx.notify();
    }

    /// Fit the time axis to the bars again.
    pub fn fit(&mut self, cx: &mut Context<Self>) {
        self.navigation = Navigation::Fit;
        cx.notify();
    }

    /// The time range on screen at `now`.
    fn view(&self, now: i64) -> Interval {
        match self.navigation {
            Navigation::Manual(view) => view,
            Navigation::Follow { span } => Interval::new(now - span as i64, now),
            Navigation::Fit => self
                .model
                .range(now)
                .map_or(Interval::new(now - 60_000_000, now), Interval::padded),
        }
    }

    fn plot_width(&self) -> f32 {
        self.plot.map_or(1.0, |plot| plot.size.width.into())
    }

    fn bar_at(&self, position: Point<Pixels>, now: i64) -> Option<(usize, usize)> {
        let plot = self.plot?;
        let local = position - plot.origin;
        let lane = lane_at(
            local.y.into(),
            LANE_HEIGHT * self.scale,
            self.model.lanes.len(),
        )?;
        let t = x_to_time(self.view(now), local.x.into(), self.plot_width());
        let bars = &self.model.lanes[lane].bars;
        // Solid bars win over the faint ones they overlap.
        let mut hits = bars
            .iter()
            .enumerate()
            .filter(|(_, bar)| bar.start <= t && t <= bar.end.unwrap_or(now));
        let first = hits.next()?;
        Some(
            hits.chain(Some(first))
                .find(|(_, bar)| !bar.faint)
                .unwrap_or(first),
        )
        .map(|(bar, _)| (lane, bar))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        match self.bar_at(event.position, now()) {
            Some((lane, _)) => cx.emit(GanttEvent::BarClicked(self.model.lanes[lane].id)),
            None => self.pan = Some(event.position.x),
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pointer = event.position;
        let now = now();
        if let Some(last) = self.pan {
            if event.pressed_button != Some(MouseButton::Left) {
                self.pan = None;
                return;
            }
            let view = self.view(now);
            let dx = f64::from(f32::from(event.position.x - last));
            let delta = -(dx / f64::from(self.plot_width()) * view.span()) as i128;
            self.navigation = Navigation::Manual(view.shifted(delta));
            self.pan = Some(event.position.x);
            cx.notify();
            return;
        }
        let hovered = self.bar_at(event.position, now);
        if hovered != self.hovered || hovered.is_some() {
            self.hovered = hovered;
            cx.notify();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plot) = self.plot else {
            return;
        };
        let delta = event.delta.pixel_delta(window.line_height());
        let view = self.view(now());
        let width = self.plot_width();
        let next = if f32::from(delta.x).abs() > f32::from(delta.y).abs() {
            let shift = -(f64::from(f32::from(delta.x)) / f64::from(width) * view.span());
            view.shifted(shift as i128)
        } else {
            let fraction = f64::from(f32::from(event.position.x - plot.origin.x) / width);
            let factor = (-f64::from(f32::from(delta.y)) * WHEEL_ZOOM_RATE).exp();
            view.zoom(factor, fraction.clamp(0.0, 1.0))
        };
        self.navigation = Navigation::Manual(next);
        cx.notify();
    }

    fn ruler(&self, view: Interval, width: f32, theme: &Theme) -> impl IntoElement + use<> {
        let step = tick_step(view.span(), width);
        let labels = ticks(view, step, &self.timezone, None)
            .into_iter()
            .map(|t| {
                div()
                    .absolute()
                    .left(px(time_to_x(view, t, width) + 4.0))
                    .top(ui_px(5.0))
                    .whitespace_nowrap()
                    .child(tick_label(t, step, &self.timezone))
            });
        div()
            .relative()
            .flex_1()
            .h(ui_px(RULER_HEIGHT))
            .overflow_hidden()
            .text_size(ui_px(11.0))
            .text_color(theme.text_tertiary)
            .children(labels)
    }

    fn tooltip(&self, now: i64, theme: &Theme, cx: &App) -> Option<impl IntoElement + use<>> {
        let (lane, bar) = self.hovered?;
        let plot = self.plot?;
        let lane = &self.model.lanes[lane];
        let bar = lane.bars[bar];
        let end = bar.end.unwrap_or(now);
        let local = self.pointer - plot.origin;
        Some(
            readout_card(11.0, cx)
                .absolute()
                .left(local.x + px((GUTTER_WIDTH + 12.0) * self.scale))
                .top(local.y + px((RULER_HEIGHT + 12.0) * self.scale))
                .child(
                    div()
                        .text_color(theme.text_primary)
                        .child(lane.label.clone()),
                )
                .child(format!(
                    "{}{}",
                    duration_label(end - bar.start),
                    if bar.end.is_none() { ", running" } else { "" }
                )),
        )
    }
}

/// A tick's label: the time of day, or the date for day ticks.
fn tick_label(t: i64, step: i64, timezone: &str) -> SharedString {
    let zone = if timezone.eq_ignore_ascii_case("local") {
        jiff::tz::TimeZone::system()
    } else {
        jiff::tz::TimeZone::get(timezone).unwrap_or(jiff::tz::TimeZone::UTC)
    };
    let Ok(timestamp) = jiff::Timestamp::from_microsecond(t) else {
        return SharedString::default();
    };
    let zoned = timestamp.to_zoned(zone);
    let format = if step >= 86_400_000_000 {
        "%b %d"
    } else if step >= 60_000_000 {
        "%H:%M"
    } else {
        "%H:%M:%S"
    };
    zoned.strftime(format).to_string().into()
}

/// A duration in microseconds as `1h 2m`, `3m 4s` or `5.6s`.
pub fn duration_label(micros: i64) -> String {
    let seconds = micros.max(0) as f64 / 1e6;
    let whole = seconds as i64;
    match whole {
        0..60 => format!("{seconds:.1}s"),
        60..3600 => format!("{}m {}s", whole / 60, whole % 60),
        _ => format!("{}h {}m", whole / 3600, whole / 60 % 60),
    }
}

/// Paints the lanes' bars, the dependency links and the now line into `plot`.
struct Scene {
    view: Interval,
    now: i64,
    lanes: Vec<SmallVec<[GanttBar; 1]>>,
    links: Vec<(usize, usize)>,
    hovered: Option<(usize, usize)>,
    grid: Vec<i64>,
    /// Lane height in pixels.
    lane_height: f32,
}

impl Scene {
    fn paint(&self, plot: Bounds<Pixels>, theme: &Theme, window: &mut Window) {
        let width = f32::from(plot.size.width);
        let x = |t: i64| plot.origin.x + px(time_to_x(self.view, t, width));
        let lane_height = self.lane_height;
        let inset = BAR_INSET / LANE_HEIGHT * lane_height;
        let lane_top = |lane: usize| plot.origin.y + px(lane as f32 * lane_height);
        for &t in &self.grid {
            window.paint_quad(fill(
                Bounds::new(point(x(t), plot.origin.y), size(px(1.0), plot.size.height)),
                theme.grid_color,
            ));
        }
        for (lane, bars) in self.lanes.iter().enumerate() {
            for (index, bar) in bars.iter().enumerate() {
                let start = x(bar.start);
                let end = x(bar.end.unwrap_or(self.now)).max(start + px(2.0));
                let mut color = theme.state_color(bar.tone);
                if bar.faint {
                    color = Theme::dim(color, 0.25);
                } else if self.hovered == Some((lane, index)) {
                    color = Theme::dim(color, 0.8);
                }
                let bounds = Bounds::new(
                    point(start, lane_top(lane) + px(inset)),
                    size(end - start, px(lane_height - 2.0 * inset)),
                );
                window.paint_quad(fill(bounds, color).corner_radii(px(2.0)));
            }
        }
        for &(from, to) in &self.links {
            let Some(source) = self.lanes[from].iter().rfind(|b| !b.faint) else {
                continue;
            };
            let Some(target) = self.lanes[to].iter().find(|b| !b.faint) else {
                continue;
            };
            let start = point(
                x(source.end.unwrap_or(self.now)),
                lane_top(from) + px(lane_height / 2.0),
            );
            let end = point(x(target.start), lane_top(to) + px(lane_height / 2.0));
            let elbow = start.x.max(end.x - px(6.0));
            let mut path = PathBuilder::stroke(px(1.0));
            path.move_to(start);
            path.line_to(point(elbow, start.y));
            path.line_to(point(elbow, end.y));
            path.line_to(end);
            if let Ok(path) = path.build() {
                window.paint_path(path, theme.text_tertiary);
            }
        }
        if self.view.start <= self.now && self.now <= self.view.end {
            window.paint_quad(fill(
                Bounds::new(
                    point(x(self.now), plot.origin.y),
                    size(px(1.0), plot.size.height),
                ),
                theme.line_color,
            ));
        }
    }
}

impl Render for Gantt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let now = now();
        self.scale = ui_scale(window);
        if self.model.is_live() {
            window.request_animation_frame();
        }
        let width = self.plot_width();
        let view = self.view(now);
        let index_of = |id: u64| self.model.lanes.iter().position(|lane| lane.id == id);
        let scene = Scene {
            view,
            now,
            lanes: self
                .model
                .lanes
                .iter()
                .map(|lane| lane.bars.clone())
                .collect(),
            links: if self.show_links {
                self.model
                    .links
                    .iter()
                    .filter_map(|&(from, to)| Some((index_of(from)?, index_of(to)?)))
                    .collect()
            } else {
                Vec::new()
            },
            hovered: self.hovered,
            grid: ticks(view, tick_step(view.span(), width), &self.timezone, None),
            lane_height: LANE_HEIGHT * self.scale,
        };
        let lanes_height = ui_px(self.model.lanes.len() as f32 * LANE_HEIGHT);
        let labels = self.model.lanes.iter().map(|lane| {
            div()
                .h(ui_px(LANE_HEIGHT))
                .flex()
                .items_center()
                .px(ui_px(8.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(lane.label.clone())
        });
        let gantt = cx.entity().downgrade();
        let paint_theme = theme.clone();
        div()
            .id("gantt")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .text_size(ui_px(12.0))
            .text_color(theme.text_secondary)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .border_b_1()
                    .border_color(theme.border_primary)
                    .child(div().w(ui_px(GUTTER_WIDTH)).flex_none())
                    .child(self.ruler(view, width, &theme)),
            )
            .child(
                div().id("gantt-lanes").flex_1().overflow_y_scroll().child(
                    div()
                        .flex()
                        .flex_row()
                        .h(lanes_height)
                        .child(
                            div()
                                .w(ui_px(GUTTER_WIDTH))
                                .flex_none()
                                .border_r_1()
                                .border_color(theme.border_primary)
                                .children(labels),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .h_full()
                                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, _| this.pan = None),
                                )
                                .on_mouse_move(cx.listener(Self::on_mouse_move))
                                .on_scroll_wheel(cx.listener(Self::on_scroll))
                                .child(
                                    canvas(
                                        move |bounds, _, cx| {
                                            gantt
                                                .update(cx, |gantt, cx| {
                                                    let resized = gantt.plot.map(|b| b.size)
                                                        != Some(bounds.size);
                                                    gantt.plot = Some(bounds);
                                                    if resized {
                                                        cx.notify();
                                                    }
                                                })
                                                .ok();
                                        },
                                        move |bounds, _, window, _| {
                                            scene.paint(bounds, &paint_theme, window)
                                        },
                                    )
                                    .absolute()
                                    .size_full(),
                                ),
                        ),
                ),
            )
            .children(self.tooltip(now, &theme, cx))
    }
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::*;

    fn bar(start: i64, end: Option<i64>) -> GanttBar {
        GanttBar {
            start,
            end,
            tone: StateTone::Success,
            faint: false,
        }
    }

    #[test]
    fn time_and_x_round_trip() {
        let view = Interval::new(1_000_000, 3_000_000);
        assert_eq!(time_to_x(view, 1_000_000, 400.0), 0.0);
        assert_eq!(time_to_x(view, 2_000_000, 400.0), 200.0);
        assert_eq!(time_to_x(view, 3_000_000, 400.0), 400.0);
        assert_eq!(x_to_time(view, 100.0, 400.0), 1_500_000);
        assert_eq!(
            x_to_time(view, time_to_x(view, 2_345_678, 400.0), 400.0),
            2_345_678
        );
    }

    #[test]
    fn range_covers_the_bars_and_running_ones_reach_now() {
        let mut model = GanttModel {
            lanes: vec![
                GanttLane {
                    id: 1,
                    label: "a".into(),
                    bars: smallvec![bar(10, Some(50))],
                },
                GanttLane {
                    id: 2,
                    label: "b".into(),
                    bars: smallvec![bar(30, Some(90))],
                },
            ],
            links: vec![(1, 2)],
        };
        assert_eq!(model.range(100), Some(Interval::new(10, 90)));
        assert!(!model.is_live());
        model.lanes[1].bars.push(bar(95, None));
        assert_eq!(model.range(200), Some(Interval::new(10, 200)));
        assert!(model.is_live());
        assert_eq!(GanttModel::default().range(5), None);
    }

    #[test]
    fn lanes_are_hit_by_row() {
        assert_eq!(lane_at(-1.0, 24.0, 3), None);
        assert_eq!(lane_at(0.0, 24.0, 3), Some(0));
        assert_eq!(lane_at(36.0, 24.0, 3), Some(1));
        assert_eq!(lane_at(72.0, 24.0, 3), None);
        // At UI scale 1.5 the same y falls in an earlier lane.
        assert_eq!(lane_at(40.0, 36.0, 3), Some(1));
        assert_eq!(lane_at(40.0, 0.0, 3), None);
    }
}
