use gpui::{Bounds, Pixels, Point, point, px};
use smallvec::SmallVec;

/// The visible window of a multi-axis plot: one shared X range plus one Y
/// range per [`YAxis`](super::YAxis), index-aligned with the plot's `axes`.
///
/// Downstream rendering normalizes each trace's Y into `[0,1]` against its
/// axis, so the GPU and cursor markers only ever need [`Self::x_bounds`]
/// (X data + a `0..1` Y); the real per-axis range is consumed solely by the
/// Y tick labels via [`Self::axis_bounds`].
#[derive(Clone, Debug)]
pub struct PlotView {
    pub x: (f64, f64),
    pub axes: SmallVec<[(f64, f64); 2]>,
}

impl PlotView {
    /// X data range with a placeholder `0..1` Y — fed to the GPU and to every
    /// X-only screen calculation.
    pub fn x_bounds(&self) -> PlotBounds {
        PlotBounds::new(self.x.0, 0.0, self.x.1, 1.0)
    }

    /// X range paired with axis `i`'s Y range (falling back to the first axis
    /// then `0..1`). Used for that axis's tick labels and for hit-testing a
    /// trace assigned to it.
    pub fn axis_bounds(&self, i: usize) -> PlotBounds {
        let (min_y, max_y) = self
            .axes
            .get(i)
            .copied()
            .or_else(|| self.axes.first().copied())
            .unwrap_or((0.0, 1.0));
        PlotBounds::new(self.x.0, min_y, self.x.1, max_y)
    }

    pub fn axis_count(&self) -> usize {
        self.axes.len()
    }

    /// Pan the X range by `frac` of its current width.
    pub fn offset_x(mut self, frac: f64) -> Self {
        let dx = frac * (self.x.1 - self.x.0);
        self.x = (self.x.0 + dx, self.x.1 + dx);
        self
    }

    /// Pan every axis's Y range by `frac` of that axis's own height.
    pub fn offset_y_all(mut self, frac: f64) -> Self {
        for (lo, hi) in &mut self.axes {
            let dy = frac * (*hi - *lo);
            *lo += dy;
            *hi += dy;
        }
        self
    }

    /// Zoom the X range by `factor` about `anchor` (0 = left edge, 1 = right).
    pub fn zoom_x(mut self, factor: f64, anchor: f64) -> Self {
        let b = PlotBounds::new(self.x.0, 0.0, self.x.1, 1.0).zoom_x(factor, anchor);
        self.x = (b.min_x, b.max_x);
        self
    }

    /// Zoom every axis's Y range by `factor` about `anchor` (0 = bottom).
    pub fn zoom_y_all(mut self, factor: f64, anchor: f64) -> Self {
        for (lo, hi) in &mut self.axes {
            let b = PlotBounds::new(0.0, *lo, 0.0, *hi).zoom_y(factor, anchor);
            *lo = b.min_y;
            *hi = b.max_y;
        }
        self
    }

    /// Zoom only axis `i`'s Y range about `anchor` (0 = bottom).
    pub fn zoom_axis_y(mut self, i: usize, factor: f64, anchor: f64) -> Self {
        if let Some((lo, hi)) = self.axes.get_mut(i) {
            let b = PlotBounds::new(0.0, *lo, 0.0, *hi).zoom_y(factor, anchor);
            *lo = b.min_y;
            *hi = b.max_y;
        }
        self
    }

    /// Pan only axis `i`'s Y range by `frac` of its height.
    pub fn offset_axis_y(mut self, i: usize, frac: f64) -> Self {
        if let Some((lo, hi)) = self.axes.get_mut(i) {
            let dy = frac * (*hi - *lo);
            *lo += dy;
            *hi += dy;
        }
        self
    }

    /// Bit-pattern key for change detection, dodging `f64`'s lack of `Eq`.
    pub fn bits(&self) -> SmallVec<[u64; 6]> {
        let mut out: SmallVec<[u64; 6]> = SmallVec::new();
        out.push(self.x.0.to_bits());
        out.push(self.x.1.to_bits());
        for (lo, hi) in &self.axes {
            out.push(lo.to_bits());
            out.push(hi.to_bits());
        }
        out
    }
}

/// Data-space rectangle for a plot view.
///
/// Doubles as the coordinate transform: pan/zoom methods mutate the bounds
/// and [`PlotBounds::to_screen`] maps data points into pixels.
#[derive(Clone, Copy, Debug)]
pub struct PlotBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl PlotBounds {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Bit-pattern key for change detection, dodging `f64`'s lack of `Eq`.
    pub fn bits(&self) -> (u64, u64, u64, u64) {
        (
            self.min_x.to_bits(),
            self.min_y.to_bits(),
            self.max_x.to_bits(),
            self.max_y.to_bits(),
        )
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn offset(mut self, dx: f64, dy: f64) -> Self {
        self.min_x += dx;
        self.max_x += dx;
        self.min_y += dy;
        self.max_y += dy;
        self
    }

    pub fn offset_by_norm(self, nx: f64, ny: f64) -> Self {
        self.offset(nx * self.width(), ny * self.height())
    }

    pub fn zoom_at(self, factor: f64, anchor_x: f64, anchor_y: f64) -> Self {
        let dw = self.width() * (factor - 1.0);
        let dh = self.height() * (factor - 1.0);
        Self {
            min_x: self.min_x - dw * anchor_x,
            max_x: self.max_x + dw * (1.0 - anchor_x),
            min_y: self.min_y - dh * anchor_y,
            max_y: self.max_y + dh * (1.0 - anchor_y),
        }
    }

    pub fn zoom_x(self, factor: f64, anchor_x: f64) -> Self {
        let dw = self.width() * (factor - 1.0);
        Self {
            min_x: self.min_x - dw * anchor_x,
            max_x: self.max_x + dw * (1.0 - anchor_x),
            ..self
        }
    }

    pub fn zoom_y(self, factor: f64, anchor_y: f64) -> Self {
        let dh = self.height() * (factor - 1.0);
        Self {
            min_y: self.min_y - dh * anchor_y,
            max_y: self.max_y + dh * (1.0 - anchor_y),
            ..self
        }
    }

    pub fn offset_x(self, nx: f64) -> Self {
        let dx = nx * self.width();
        Self {
            min_x: self.min_x + dx,
            max_x: self.max_x + dx,
            ..self
        }
    }

    pub fn offset_y(self, ny: f64) -> Self {
        let dy = ny * self.height();
        Self {
            min_y: self.min_y + dy,
            max_y: self.max_y + dy,
            ..self
        }
    }

    pub fn normalize(mut self) -> Self {
        if self.min_x >= self.max_x {
            self.min_x = self.max_x.min(self.min_x);
            self.max_x = self.min_x + 1.0;
        }
        if self.min_y >= self.max_y {
            self.min_y = self.max_y.min(self.min_y);
            self.max_y = self.min_y + 1.0;
        }
        self
    }

    pub fn screen_delta_to_norm(
        &self,
        screen_bounds: Bounds<Pixels>,
        dx: Pixels,
        dy: Pixels,
    ) -> (f64, f64) {
        let nx = f32::from(dx) as f64 / f32::from(screen_bounds.size.width) as f64;
        let ny = f32::from(dy) as f64 / f32::from(screen_bounds.size.height) as f64;
        (nx, ny)
    }

    pub fn screen_anchor(&self, screen_bounds: Bounds<Pixels>, pos: Point<Pixels>) -> (f64, f64) {
        let nx = (f32::from(pos.x - screen_bounds.origin.x) / f32::from(screen_bounds.size.width))
            as f64;
        let ny = (f32::from(pos.y - screen_bounds.origin.y) / f32::from(screen_bounds.size.height))
            as f64;
        (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
    }

    pub fn to_screen(
        &self,
        screen_bounds: Bounds<Pixels>,
        data_x: f64,
        data_y: f64,
    ) -> Point<Pixels> {
        let nx = if self.width() == 0.0 {
            0.5
        } else {
            (data_x - self.min_x) / self.width()
        };
        let ny = if self.height() == 0.0 {
            0.5
        } else {
            1.0 - (data_y - self.min_y) / self.height()
        };
        point(
            screen_bounds.origin.x + screen_bounds.size.width * nx as f32,
            screen_bounds.origin.y + screen_bounds.size.height * ny as f32,
        )
    }

    /// Bake the screen transform into a branchless form for inner-loop use.
    pub fn screen_transform(&self, screen_bounds: Bounds<Pixels>) -> ScreenTransform {
        let sw = f32::from(screen_bounds.size.width) as f64;
        let sh = f32::from(screen_bounds.size.height) as f64;
        let ox = f32::from(screen_bounds.origin.x) as f64;
        let oy = f32::from(screen_bounds.origin.y) as f64;
        let dw = self.width();
        let dh = self.height();

        if dw == 0.0 || dh == 0.0 {
            return ScreenTransform {
                x_scale: 0.0,
                y_scale: 0.0,
                x_offset: ox + sw * 0.5,
                y_offset: oy + sh * 0.5,
            };
        }

        let x_scale = sw / dw;
        let y_scale = -sh / dh;
        ScreenTransform {
            x_scale,
            y_scale,
            x_offset: ox - self.min_x * x_scale,
            y_offset: oy + sh - self.min_y * y_scale,
        }
    }
}

/// Pre-computed data-to-screen transform.
///
/// `f64` is retained through the scale/offset multiplies so large
/// microsecond timestamps don't lose precision; the cast to `f32` only
/// happens on the final pixel coordinate.
#[derive(Clone, Copy)]
pub struct ScreenTransform {
    x_scale: f64,
    y_scale: f64,
    x_offset: f64,
    y_offset: f64,
}

impl ScreenTransform {
    #[inline(always)]
    pub fn apply(&self, data_x: f64, data_y: f64) -> Point<Pixels> {
        point(
            px((self.x_offset + data_x * self.x_scale) as f32),
            px((self.y_offset + data_y * self.y_scale) as f32),
        )
    }
}

/// Fraction of the Y span added past an auto-fit extreme so a trace riding
/// its own min or max clears the axis rule (a few pixels at typical plot
/// heights) instead of being painted over by it.
const AUTO_EDGE_PAD: f64 = 0.03;

/// Resolve a Y range for display: pad the auto-fit ends so data extremes
/// never land exactly on the plot border, while explicit overrides pass
/// through untouched.
///
/// `lo_is_auto` / `hi_is_auto` mark ends that came from bounds tracking
/// rather than a user override — the axis rules paint on top of the GPU
/// lines, so an unpadded auto fit hides any trace sitting at the bound. A
/// degenerate span (constant trace, or inverted overrides) defers to
/// [`pad_degenerate_range`] regardless, since a zero-height view can't
/// render at all.
pub fn pad_auto_range(lo: f64, hi: f64, lo_is_auto: bool, hi_is_auto: bool) -> (f64, f64) {
    if lo >= hi {
        return pad_degenerate_range(lo, hi);
    }
    let pad = (hi - lo) * AUTO_EDGE_PAD;
    (
        if lo_is_auto { lo - pad } else { lo },
        if hi_is_auto { hi + pad } else { hi },
    )
}

/// Widen a degenerate `[lo, hi]` Y span so the data renders vertically
/// centered instead of pinned to the bottom edge.
///
/// A constant-valued trace auto-fits to `min == max`; padding symmetrically
/// (5% of the magnitude, floored so a constant zero still gets a usable
/// range) keeps the flat line in the middle of the plot. Non-degenerate
/// spans pass through untouched.
pub fn pad_degenerate_range(lo: f64, hi: f64) -> (f64, f64) {
    if lo < hi {
        return (lo, hi);
    }
    let mid = (lo + hi) / 2.0;
    let pad = (mid.abs() * 0.05).max(0.5);
    (mid - pad, mid + pad)
}

/// Snap `num` to the nearest "nice" value — 1, 2, 2.5, or 5 times a power
/// of ten — so axis tick steps land on human-friendly intervals at any
/// scale (…, 0.25, 0.5, 1, 2, 2.5, 5, 10, 20, 25, 50, …).
pub fn pretty_round(num: f64) -> f64 {
    if num == 0.0 || !num.is_finite() {
        return num;
    }
    let abs = num.abs();
    let magnitude = 10f64.powi(abs.log10().floor() as i32);
    let mantissa = abs / magnitude;
    let nice = if mantissa < 1.5 {
        1.0
    } else if mantissa < 2.25 {
        2.0
    } else if mantissa < 3.75 {
        2.5
    } else if mantissa < 7.5 {
        5.0
    } else {
        10.0
    };
    let result = nice * magnitude;
    if num < 0.0 { -result } else { result }
}

#[cfg(test)]
mod tests {
    use super::{pad_auto_range, pad_degenerate_range, pretty_round};

    #[test]
    fn auto_range_strictly_contains_the_data() {
        // Data spanning [-1, 1]: neither extreme may land on the border.
        let (lo, hi) = pad_auto_range(-1.0, 1.0, true, true);
        assert!(lo < -1.0 && hi > 1.0);
        assert!(
            (-1.0 - lo - (hi - 1.0)).abs() < 1e-12,
            "edge padding not symmetric"
        );
    }

    #[test]
    fn explicit_overrides_are_respected_exactly() {
        assert_eq!(pad_auto_range(-1.0, 1.0, false, false), (-1.0, 1.0));

        // Mixed: only the auto end gets padded.
        let (lo, hi) = pad_auto_range(-1.0, 1.0, false, true);
        assert_eq!(lo, -1.0);
        assert!(hi > 1.0);

        let (lo, hi) = pad_auto_range(-1.0, 1.0, true, false);
        assert!(lo < -1.0);
        assert_eq!(hi, 1.0);
    }

    #[test]
    fn auto_range_centers_degenerate_spans() {
        // A constant trace stays centered rather than edge-padded.
        let (lo, hi) = pad_auto_range(3.0, 3.0, true, true);
        assert!(lo < 3.0 && hi > 3.0);
        assert!((3.0 - lo - (hi - 3.0)).abs() < 1e-12);
    }

    #[test]
    fn degenerate_range_centers_the_value() {
        let (lo, hi) = pad_degenerate_range(42.0, 42.0);
        assert!(lo < 42.0 && hi > 42.0);
        assert!(
            (42.0 - lo - (hi - 42.0)).abs() < 1e-12,
            "padding not symmetric"
        );

        // A constant zero still needs a non-empty span.
        let (lo, hi) = pad_degenerate_range(0.0, 0.0);
        assert!(lo < 0.0 && hi > 0.0);
    }

    #[test]
    fn real_range_passes_through() {
        assert_eq!(pad_degenerate_range(-1.0, 2.0), (-1.0, 2.0));
    }

    #[test]
    fn snaps_to_nice_mantissas() {
        assert_eq!(pretty_round(246.8), 250.0);
        assert_eq!(pretty_round(1.4), 1.0);
        assert_eq!(pretty_round(1.6), 2.0);
        assert_eq!(pretty_round(4.0), 5.0);
        assert_eq!(pretty_round(9.0), 10.0);
    }

    #[test]
    fn preserves_sign_and_scale() {
        assert_eq!(pretty_round(-246.8), -250.0);
        assert!((pretty_round(0.2) - 0.2).abs() < 1e-12);
        assert!((pretty_round(0.000_23) - 0.000_25).abs() < 1e-15);
    }

    #[test]
    fn passes_through_degenerate_inputs() {
        assert_eq!(pretty_round(0.0), 0.0);
        assert!(pretty_round(f64::INFINITY).is_infinite());
        assert!(pretty_round(f64::NAN).is_nan());
    }
}
