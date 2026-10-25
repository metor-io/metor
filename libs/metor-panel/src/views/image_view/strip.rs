//! The histogram strip: log-count bars, black and white handles that set
//! manual levels, and a marker at the top code. It shows in the image view's
//! inspector, and under the image when the overlay is on.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Render, Subscription, WeakEntity, Window, canvas, div,
    point, prelude::*, px, size,
};

use super::histogram::Histogram;
use super::view::ImageView;
use crate::theme::{Theme, theme};

/// Logical height of the strip.
pub const STRIP_HEIGHT: f32 = 48.0;
/// How close, in logical pixels, a press must land to grab a handle.
const GRAB_PX: f32 = 6.0;
const HANDLE_PX: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Black,
    White,
}

/// The x of `value` on a strip `width` wide spanning `range`.
pub fn x_of(value: f64, range: (f64, f64), width: f32) -> f32 {
    let span = range.1 - range.0;
    if span <= 0.0 || !span.is_finite() {
        return 0.0;
    }
    (((value - range.0) / span).clamp(0.0, 1.0) * f64::from(width)) as f32
}

/// The value at `x`, clamped to `range`.
pub fn value_at(x: f32, range: (f64, f64), width: f32) -> f64 {
    let t = if width > 0.0 {
        f64::from((x / width).clamp(0.0, 1.0))
    } else {
        0.0
    };
    range.0 + t * (range.1 - range.0)
}

/// The handle within reach of `x`, the nearer when both are.
pub fn grab(x: f32, levels: (f64, f64), range: (f64, f64), width: f32) -> Option<Handle> {
    let distance = |v: f64| (x_of(v, range, width) - x).abs();
    let (black, white) = (distance(levels.0), distance(levels.1));
    match (black <= GRAB_PX, white <= GRAB_PX) {
        (true, true) if white < black => Some(Handle::White),
        (true, _) => Some(Handle::Black),
        (false, true) => Some(Handle::White),
        (false, false) => None,
    }
}

/// The levels after moving `handle` to `value`, white kept at least `gap`
/// above black.
pub fn dragged(levels: (f64, f64), handle: Handle, value: f64, gap: f64) -> (f64, f64) {
    match handle {
        Handle::Black => (value.min(levels.1 - gap), levels.1),
        Handle::White => (levels.0, value.max(levels.0 + gap)),
    }
}

/// `n` column heights in `0..=1`: the log of the summed bins under each.
pub fn columns(bins: &[u32], n: usize) -> Vec<f32> {
    if bins.is_empty() || n == 0 {
        return vec![0.0; n];
    }
    let sums: Vec<f64> = (0..n)
        .map(|c| {
            let a = c * bins.len() / n;
            let b = ((c + 1) * bins.len() / n).max(a + 1).min(bins.len());
            bins[a..b].iter().map(|&count| f64::from(count)).sum()
        })
        .collect();
    let top = sums.iter().copied().fold(0.0, f64::max).ln_1p();
    sums.iter()
        .map(|s| match top > 0.0 {
            true => (s.ln_1p() / top) as f32,
            false => 0.0,
        })
        .collect()
}

/// Paints the strip into `bounds`.
pub fn paint(
    bounds: Bounds<Pixels>,
    histogram: &Histogram,
    levels: Option<(f64, f64)>,
    top_code: Option<f64>,
    theme: &Theme,
    window: &mut Window,
) {
    window.paint_quad(gpui::fill(bounds, theme.bg_secondary));
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let n = w.max(0.0) as usize;
    if n == 0 || h <= 0.0 {
        return;
    }
    let range = histogram.range();
    let heights = columns(histogram.bins(), n);
    let origin = bounds.origin;
    let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
    let mut bars = gpui::PathBuilder::fill();
    bars.move_to(at(0.0, h));
    for (c, height) in heights.iter().enumerate() {
        let top = h - height * h;
        bars.line_to(at(c as f32, top));
        bars.line_to(at(c as f32 + 1.0, top));
    }
    bars.line_to(at(n as f32, h));
    bars.close();
    if let Ok(path) = bars.build() {
        window.paint_path(path, theme.text_tertiary);
    }
    let line = |x: f32, color, window: &mut Window| {
        let x = x.clamp(0.0, w - HANDLE_PX);
        window.paint_quad(gpui::fill(
            Bounds::new(at(x, 0.0), size(px(HANDLE_PX), px(h))),
            color,
        ));
    };
    if let Some(top) = top_code {
        line(x_of(top, range, w), theme.image_saturated(), window);
    }
    if let Some((black, white)) = levels {
        line(x_of(black, range, w), theme.text_secondary, window);
        line(x_of(white, range, w), theme.text_primary, window);
    }
}

/// A strip bound to one image view. While any is alive the view keeps a
/// histogram of every frame.
pub struct HistogramStrip {
    view: WeakEntity<ImageView>,
    bounds: Bounds<Pixels>,
    drag: Option<Handle>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl HistogramStrip {
    /// A strip for `view`, made through [`ImageView::histogram_strip`] or
    /// the view's overlay, which register it.
    pub(super) fn new(view: &Entity<ImageView>, cx: &mut Context<Self>) -> Self {
        Self {
            view: view.downgrade(),
            bounds: Bounds::default(),
            drag: None,
            focus: cx.focus_handle(),
            _observe: cx.observe(view, |_, _, cx| cx.notify()),
        }
    }

    pub fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    fn press(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((histogram, Some(levels), _)) = self.data(cx) else {
            return;
        };
        let x = f32::from(event.position.x - self.bounds.origin.x);
        let width = f32::from(self.bounds.size.width);
        self.drag = grab(x, levels, histogram.range(), width);
        cx.notify();
        cx.stop_propagation();
    }

    /// Moves the held handle to the value under `position`, keeping white
    /// at least one code above black.
    fn drag_to(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(handle), Some((histogram, Some(levels), _))) = (self.drag, self.data(cx)) else {
            return;
        };
        let x = f32::from(position.x - self.bounds.origin.x);
        let width = f32::from(self.bounds.size.width);
        let range = histogram.range();
        let code = (range.1 - range.0) / histogram.bins().len().max(1) as f64;
        let value = value_at(x, range, width);
        let levels = dragged(levels, handle, value, code);
        let _ = self.view.update(cx, |view, cx| view.set_levels(levels, cx));
    }

    fn release(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        cx.notify();
    }

    /// The view's histogram, painted levels and top code.
    fn data(&self, cx: &App) -> Option<StripData> {
        self.view.upgrade()?.read(cx).strip_data()
    }
}

/// What a strip paints: the histogram, the painted levels, the top code.
pub(super) type StripData = (std::sync::Arc<Histogram>, Option<(f64, f64)>, Option<f64>);

impl Focusable for HistogramStrip {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HistogramStrip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let data = self.data(cx);
        let theme = theme(cx);
        let dragging = self.drag.is_some();
        let this = cx.entity().downgrade();
        let painter = canvas(
            {
                let this = this.clone();
                move |bounds, _, cx| {
                    let _ = this.update(cx, |strip, _| strip.bounds = bounds);
                }
            },
            move |bounds, (), window, _| {
                match &data {
                    Some((histogram, levels, top)) => {
                        paint(bounds, histogram, *levels, *top, &theme, window)
                    }
                    None => window.paint_quad(gpui::fill(bounds, theme.bg_secondary)),
                }
                if !dragging {
                    return;
                }
                // A handle drag follows the mouse across the whole window,
                // so a release anywhere ends it.
                let mover = this.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble && event.dragging() {
                        let _ = mover.update(cx, |strip, cx| strip.drag_to(event.position, cx));
                    }
                });
                let releaser = this.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let _ = releaser.update(cx, |strip, cx| strip.release(cx));
                    }
                });
            },
        )
        .size_full();
        div()
            .id("image-histogram")
            .track_focus(&self.focus)
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|strip, event: &MouseDownEvent, _, cx| strip.press(event, cx)),
            )
            .child(painter)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn x_and_value_invert() {
        let range = (0.0, 4096.0);
        for value in [0.0, 100.0, 2048.0, 4096.0] {
            let x = x_of(value, range, 512.0);
            assert!((value_at(x, range, 512.0) - value).abs() < 1e-3);
        }
        assert_eq!(x_of(-5.0, range, 512.0), 0.0);
        assert_eq!(x_of(1e9, range, 512.0), 512.0);
        assert_eq!(x_of(1.0, (3.0, 3.0), 512.0), 0.0);
        assert_eq!(value_at(10.0, range, 0.0), 0.0);
    }

    #[test]
    fn grabbing_picks_the_nearer_handle() {
        let range = (0.0, 100.0);
        let levels = (10.0, 90.0);
        assert_eq!(
            grab(10.0 * 4.0 + 3.0, levels, range, 400.0),
            Some(Handle::Black)
        );
        assert_eq!(grab(360.0, levels, range, 400.0), Some(Handle::White));
        assert_eq!(grab(200.0, levels, range, 400.0), None);
        // Both in reach: the nearer wins.
        let close = (50.0, 51.0);
        assert_eq!(grab(204.5, close, range, 400.0), Some(Handle::White));
        assert_eq!(grab(199.0, close, range, 400.0), Some(Handle::Black));
    }

    #[test]
    fn dragging_sets_one_level() {
        assert_eq!(
            dragged((10.0, 90.0), Handle::Black, 30.0, 1.0),
            (30.0, 90.0)
        );
        assert_eq!(
            dragged((10.0, 90.0), Handle::White, 50.0, 1.0),
            (10.0, 50.0)
        );
        // The handles never cross: white stays a code above black.
        assert_eq!(dragged((10.0, 90.0), Handle::White, 5.0, 1.0), (10.0, 11.0));
        assert_eq!(
            dragged((10.0, 90.0), Handle::Black, 95.0, 1.0),
            (89.0, 90.0)
        );
    }

    #[test]
    fn columns_are_log_scaled_and_cover_every_bin() {
        let bins = [0, 1, 0, 1000];
        let cols = columns(&bins, 2);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[1], 1.0);
        assert!(cols[0] > 0.05 && cols[0] < 0.2, "{cols:?}");
        // More columns than bins repeat bins rather than dropping them.
        assert_eq!(columns(&[5], 3), [1.0, 1.0, 1.0]);
        assert_eq!(columns(&[0, 0], 2), [0.0, 0.0]);
        assert!(columns(&[], 4).iter().all(|&c| c == 0.0));
        assert!(columns(&[1], 0).is_empty());
    }
}
