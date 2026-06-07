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

/// Round `num` to the nearest `0.5 * 10^k` so axis steps land on
/// human-friendly values (1, 1.5, 2, 5, 10, 20, 50, …).
pub fn pretty_round(num: f64) -> f64 {
    if num == 0.0 || !num.is_finite() {
        return num;
    }
    let mut multiplier = 1.0;
    let mut n = num.abs();

    while n < 1.0 {
        n *= 10.0;
        multiplier *= 10.0;
    }

    let rounded = (n * 2.0).round() / 2.0;
    let result = rounded / multiplier;
    if num < 0.0 { -result } else { result }
}
