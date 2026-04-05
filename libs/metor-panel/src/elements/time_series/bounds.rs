use gpui::{Bounds, Pixels, Point, point};

/// Data-space bounds for a time-series plot, with conversion between data and screen coordinates.
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
}

/// Round a step size to a "pretty" value (nearest 0.5 at the appropriate magnitude).
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
