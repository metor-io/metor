//! The star tracker's camera: a pinhole with Brown–Conrady distortion, and
//! its mount on the body.
//!
//! Pixel coordinates `(u, v)` are continuous. Pixel `(i, j)` covers
//! `[i, i + 1) × [j, j + 1)`, so its centre is at `(i + 0.5, j + 0.5)`. The
//! star-tracker frame has `+z` along the boresight and `+x`, `+y` along `+u`,
//! `+v`. Focal-plane coordinates `(x, y)` are in metres.

use crate::geom::{Quat, V3, add, dot, normalize, scale};

/// How far (px) past the sensor edge [`Camera::st_to_pixel`] still projects.
pub const EDGE_MARGIN_PX: f64 = 8.0;
const UNDISTORT_STEPS: usize = 20;
/// The last fixed-point step (m) accepted as converged, about 2e-10 px.
const UNDISTORT_TOL_M: f64 = 1e-15;

/// The camera's lens and sensor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Intrinsics {
    /// Focal length (m).
    pub f_m: f64,
    /// Pixel pitch (m).
    pub pitch_m: f64,
    /// Principal point (px).
    pub cx: f64,
    pub cy: f64,
    /// Radial distortion (1/m², 1/m⁴).
    pub k1: f64,
    pub k2: f64,
    /// Tangential distortion (1/m).
    pub p1: f64,
    pub p2: f64,
    /// Sensor size (px).
    pub width: u32,
    pub height: u32,
}

impl Intrinsics {
    /// Brown–Conrady: the distorted focal-plane point of the ideal `(x, y)`.
    pub fn distort(&self, x: f64, y: f64) -> (f64, f64) {
        let r2 = x * x + y * y;
        let radial = 1.0 + self.k1 * r2 + self.k2 * r2 * r2;
        let xd = x * radial + 2.0 * self.p1 * x * y + self.p2 * (r2 + 2.0 * x * x);
        let yd = y * radial + self.p1 * (r2 + 2.0 * y * y) + 2.0 * self.p2 * x * y;
        (xd, yd)
    }

    /// The Jacobian of [`distort`](Self::distort) as `(∂x_d/∂x, ∂x_d/∂y,
    /// ∂y_d/∂y)`; it is symmetric.
    fn jacobian(&self, x: f64, y: f64) -> (f64, f64, f64) {
        let r2 = x * x + y * y;
        let radial = 1.0 + self.k1 * r2 + self.k2 * r2 * r2;
        let slope = 2.0 * (self.k1 + 2.0 * self.k2 * r2);
        let xx = radial + x * x * slope + 2.0 * self.p1 * y + 6.0 * self.p2 * x;
        let xy = x * y * slope + 2.0 * self.p1 * x + 2.0 * self.p2 * y;
        let yy = radial + y * y * slope + 6.0 * self.p1 * y + 2.0 * self.p2 * x;
        (xx, xy, yy)
    }

    /// Whether the field is unfolded at the ideal point `(x, y)`: the
    /// Jacobian determinant is positive there, and the radial map
    /// `r (1 + k₁r² + k₂r⁴)` is increasing all the way out to it, so the point
    /// is on the central branch.
    pub fn unfolded(&self, x: f64, y: f64) -> bool {
        let (xx, xy, yy) = self.jacobian(x, y);
        let r2 = x * x + y * y;
        // d/dr of the radial map, as a quadratic in u = r².
        let slope = |u: f64| 1.0 + 3.0 * self.k1 * u + 5.0 * self.k2 * u * u;
        let vertex = -3.0 * self.k1 / (10.0 * self.k2);
        let lowest = if self.k2 > 0.0 && (0.0..r2).contains(&vertex) {
            slope(vertex)
        } else {
            slope(r2).min(1.0)
        };
        xx * yy - xy * xy > 0.0 && lowest > 0.0
    }

    /// Inverts [`distort`](Self::distort) by Newton's method from `(x_d,
    /// y_d)`. Returns `None` if it has not converged to [`UNDISTORT_TOL_M`]
    /// within [`UNDISTORT_STEPS`] steps, or if the point found is not on the
    /// field's central, unfolded branch.
    pub fn undistort(&self, xd: f64, yd: f64) -> Option<(f64, f64)> {
        let (mut x, mut y) = (xd, yd);
        for _ in 0..UNDISTORT_STEPS {
            let (fx, fy) = self.distort(x, y);
            let (rx, ry) = (xd - fx, yd - fy);
            let (xx, xy, yy) = self.jacobian(x, y);
            let det = xx * yy - xy * xy;
            let (dx, dy) = ((yy * rx - xy * ry) / det, (xx * ry - xy * rx) / det);
            (x, y) = (x + dx, y + dy);
            if dx.abs().max(dy.abs()) < UNDISTORT_TOL_M {
                return self.unfolded(x, y).then_some((x, y));
            }
        }
        None
    }
}

/// The intrinsics and the mount, `b_body = A(q_b_st) b_st`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub intr: Intrinsics,
    pub q_b_st: Quat,
}

impl Camera {
    /// The star-tracker-frame unit vector seen at pixel `(u, v)`.
    pub fn pixel_to_st(&self, u: f64, v: f64) -> Option<V3> {
        let i = &self.intr;
        let (x, y) = i.undistort((u - i.cx) * i.pitch_m, (v - i.cy) * i.pitch_m)?;
        normalize([x, y, i.f_m])
    }

    /// The pixel a star-tracker-frame direction lands on, or `None` when it is
    /// behind the camera, past a fold in the distortion, or more than
    /// [`EDGE_MARGIN_PX`] off the sensor.
    pub fn st_to_pixel(&self, b: V3) -> Option<(f64, f64)> {
        let i = &self.intr;
        if b[2].is_nan() || b[2] <= 0.0 {
            return None;
        }
        let (x, y) = (i.f_m * b[0] / b[2], i.f_m * b[1] / b[2]);
        if !i.unfolded(x, y) {
            return None;
        }
        let (xd, yd) = i.distort(x, y);
        let (u, v) = (xd / i.pitch_m + i.cx, yd / i.pitch_m + i.cy);
        let on = |p: f64, size: u32| (-EDGE_MARGIN_PX..=size as f64 + EDGE_MARGIN_PX).contains(&p);
        (on(u, i.width) && on(v, i.height)).then_some((u, v))
    }

    pub fn st_to_body(&self, b: V3) -> V3 {
        self.q_b_st.rotate(b)
    }

    pub fn body_to_st(&self, b: V3) -> V3 {
        self.q_b_st.conj().rotate(b)
    }

    /// A centroid error (px) as an angle (rad).
    pub fn pixel_sigma_to_rad(&self, sigma_px: f64) -> f64 {
        sigma_px * self.intr.pitch_m / self.intr.f_m
    }

    /// The largest angle (rad) from the boresight to a sensor corner, the
    /// radius of a cone that covers the field of view.
    pub fn half_diagonal_rad(&self) -> f64 {
        let (w, h) = (self.intr.width as f64, self.intr.height as f64);
        [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)]
            .into_iter()
            .map(|(u, v)| self.corner_angle(u, v))
            .fold(0.0, f64::max)
    }

    /// The angle from the boresight to `(u, v)`, through the pinhole if the
    /// distortion cannot be inverted there.
    fn corner_angle(&self, u: f64, v: f64) -> f64 {
        let i = &self.intr;
        let pinhole = [(u - i.cx) * i.pitch_m, (v - i.cy) * i.pitch_m, i.f_m];
        let b = self.pixel_to_st(u, v).or_else(|| normalize(pinhole));
        b.map_or(0.0, |b| libm::acos(b[2].clamp(-1.0, 1.0)))
    }
}

/// First-order aberration: `b` as seen by an observer moving at `beta = v/c`.
/// Off until P7; nothing calls it yet.
pub fn aberrate(b: V3, beta: V3) -> Option<V3> {
    normalize(add(add(b, beta), scale(b, -dot(b, beta))))
}

#[cfg(test)]
mod tests;
