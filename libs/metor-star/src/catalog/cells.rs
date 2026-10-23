//! The sky index: each cube face split into `n × n` cells by the tangent-plane
//! ratios, `6n²` cells in all. A direction's cell takes one division.

use crate::geom::{V3, dot, normalize};

/// A direction's face (`2·axis` for `+axis`, `2·axis + 1` for `−axis`) and its
/// tangent-plane coordinates `(u, v)` in `[−1, 1]` on that face.
fn face_of(r: V3) -> (u32, f64, f64) {
    let a = r.map(f64::abs);
    let axis = if a[0] >= a[1] && a[0] >= a[2] {
        0
    } else if a[1] >= a[2] {
        1
    } else {
        2
    };
    let (iu, iv) = ((axis + 1) % 3, (axis + 2) % 3);
    let inv = 1.0 / a[axis];
    let face = 2 * axis as u32 + u32::from(r[axis] < 0.0);
    (face, r[iu] * inv, r[iv] * inv)
}

/// The column or row `t ∈ [−1, 1]` falls in, of `n`.
fn bin(t: f64, n: u32) -> u32 {
    let x = (t + 1.0) * 0.5 * f64::from(n);
    if x.is_nan() || x <= 0.0 {
        0
    } else {
        (x as u32).min(n - 1)
    }
}

/// The cell index of `(face, i, j)`, clamped into `0..6n²`.
pub fn cell_index(face: u32, i: u32, j: u32, n: u32) -> u32 {
    let n = n.max(1);
    let (face, i, j) = (face.min(5), i.min(n - 1), j.min(n - 1));
    (face * n + j) * n + i
}

/// The cell holding direction `r`, of `6n²`.
pub fn cell_of(r: V3, n: u32) -> u32 {
    let n = n.max(1);
    let (face, u, v) = face_of(r);
    cell_index(face, bin(u, n), bin(v, n), n)
}

/// The unit direction through the centre of cell `c`.
pub fn cell_center(c: u32, n: u32) -> V3 {
    let n = n.max(1);
    let (face, rest) = ((c / (n * n)).min(5), c % (n * n));
    let (j, i) = (rest / n, rest % n);
    let t = |k: u32| (f64::from(k) + 0.5) / f64::from(n) * 2.0 - 1.0;
    let axis = (face / 2) as usize;
    let sign = if face % 2 == 0 { 1.0 } else { -1.0 };
    let mut r = [0.0; 3];
    r[axis] = sign;
    r[(axis + 1) % 3] = t(i);
    r[(axis + 2) % 3] = t(j);
    normalize(r).unwrap_or(r)
}

/// An upper bound (rad) on the angle from any cell's centre to any point in
/// it. Cells are largest at a face's centre, where a cell's half-diagonal is
/// `√2/n` in the tangent plane.
pub fn cell_radius(n: u32) -> f64 {
    libm::atan(core::f64::consts::SQRT_2 / f64::from(n.max(1))) * (1.0 + 1e-9)
}

/// Whether cell `c`'s centre lies within `radius + cell_radius(n)` of `axis`,
/// so that the cell may hold directions within `radius` of it.
pub fn may_touch(c: u32, n: u32, axis: V3, radius: f64) -> bool {
    let reach = radius + cell_radius(n);
    reach >= core::f64::consts::PI || dot(cell_center(c, n), axis) >= libm::cos(reach)
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Every `(face, i, j)` lands in `0..6n²`.
    #[kani::proof]
    fn cell_index_is_in_bounds() {
        let (face, i, j, n): (u32, u32, u32, u32) = kani::any();
        kani::assume(n >= 1 && n <= 64);
        assert!(cell_index(face, i, j, n) < 6 * n * n);
    }
}
