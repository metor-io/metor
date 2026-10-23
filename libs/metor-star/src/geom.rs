//! Small fixed-size vector, matrix and quaternion math on `f64` arrays.

use core::ops::Mul;

pub type V3 = [f64; 3];
/// Row-major: `m[row][col]`.
pub type M3 = [[f64; 3]; 3];

pub const I3: M3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn scale(a: V3, s: f64) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub fn norm(a: V3) -> f64 {
    libm::sqrt(dot(a, a))
}

/// Returns `a` scaled to unit length, or `None` when `a` is not finite or its
/// largest component is below 1e-300.
pub fn normalize(a: V3) -> Option<V3> {
    if !a.iter().all(|x| x.is_finite()) {
        return None;
    }
    let m = a[0].abs().max(a[1].abs()).max(a[2].abs());
    if m < 1e-300 {
        return None;
    }
    let u = scale(a, 1.0 / m);
    Some(scale(u, 1.0 / norm(u)))
}

pub fn mat_vec(m: M3, v: V3) -> V3 {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

pub fn transpose(m: M3) -> M3 {
    core::array::from_fn(|i| core::array::from_fn(|j| m[j][i]))
}

pub fn mat_mul(a: M3, b: M3) -> M3 {
    let bt = transpose(b);
    core::array::from_fn(|i| core::array::from_fn(|j| dot(a[i], bt[j])))
}

pub fn mat_add(a: M3, b: M3) -> M3 {
    core::array::from_fn(|i| add(a[i], b[i]))
}

pub fn mat_scale(a: M3, s: f64) -> M3 {
    a.map(|row| scale(row, s))
}

/// `a bᵀ`.
pub fn outer(a: V3, b: V3) -> M3 {
    a.map(|ai| scale(b, ai))
}

pub fn trace(m: M3) -> f64 {
    m[0][0] + m[1][1] + m[2][2]
}

pub fn det(m: M3) -> f64 {
    dot(m[0], cross(m[1], m[2]))
}

/// The adjugate, so that `m · adj(m) = det(m) I`.
pub fn adj(m: M3) -> M3 {
    transpose([cross(m[1], m[2]), cross(m[2], m[0]), cross(m[0], m[1])])
}

/// Inverts a symmetric matrix with a positive determinant, or returns `None`.
pub fn inv_sym(m: M3) -> Option<M3> {
    let d = det(m);
    let inv = mat_scale(adj(m), 1.0 / d);
    let finite = inv.as_flattened().iter().all(|x| x.is_finite());
    (d > 0.0 && d.is_finite() && finite).then_some(inv)
}

/// A unit quaternion stored scalar-last, `[x, y, z, w]`, with Hamilton's
/// product. As an attitude it maps reference vectors into the body frame,
/// `b = A(q) r`.
///
/// The four numbers are the same as nox's `Quaternion` for a body←ECI
/// attitude, for which `b = q⁻¹ * r`:
///
/// ```
/// use metor_star::geom::Quat;
/// use nox::{ArrayRepr, Quaternion, Vector, tensor};
///
/// let q = Quat([0.1, -0.5, 0.3, 0.8]).normalized().unwrap();
/// let r = [0.6, 0.0, 0.8];
/// let q_b_eci: Quaternion<f64, ArrayRepr> = Quaternion(tensor![q.0[0], q.0[1], q.0[2], q.0[3]]);
/// let b: Vector<f64, 3, ArrayRepr> = q_b_eci.inverse() * tensor![r[0], r[1], r[2]];
/// let [bx, by, bz] = b.into_buf();
/// let ours = q.rotate(r);
/// assert!((ours[0] - bx).abs() < 1e-15);
/// assert!((ours[1] - by).abs() < 1e-15);
/// assert!((ours[2] - bz).abs() < 1e-15);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat(pub [f64; 4]);

impl Quat {
    pub const IDENTITY: Quat = Quat([0.0, 0.0, 0.0, 1.0]);

    pub fn vec(self) -> V3 {
        [self.0[0], self.0[1], self.0[2]]
    }

    pub fn w(self) -> f64 {
        self.0[3]
    }

    pub fn from_parts(v: V3, w: f64) -> Quat {
        Quat([v[0], v[1], v[2], w])
    }

    /// Returns the quaternion scaled to unit norm, or `None` if it is not finite
    /// or is near zero.
    pub fn normalized(self) -> Option<Quat> {
        let n = libm::sqrt(self.0.iter().map(|x| x * x).sum::<f64>());
        if !n.is_finite() || n < 1e-300 {
            return None;
        }
        Some(Quat(self.0.map(|x| x / n)))
    }

    pub fn conj(self) -> Quat {
        Quat::from_parts(scale(self.vec(), -1.0), self.w())
    }

    /// `A(q) r`.
    pub fn rotate(self, r: V3) -> V3 {
        mat_vec(self.to_m3(), r)
    }

    /// Shuster's attitude matrix `A(q) = (w² − |e|²) I + 2 e eᵀ − 2 w [e×]`.
    pub fn to_m3(self) -> M3 {
        let ([x, y, z], w) = (self.vec(), self.w());
        let d = w * w - (x * x + y * y + z * z);
        [
            [
                d + 2.0 * x * x,
                2.0 * (x * y + w * z),
                2.0 * (x * z - w * y),
            ],
            [
                2.0 * (x * y - w * z),
                d + 2.0 * y * y,
                2.0 * (y * z + w * x),
            ],
            [
                2.0 * (x * z + w * y),
                2.0 * (y * z - w * x),
                d + 2.0 * z * z,
            ],
        ]
    }

    /// Inverts `to_m3` for a rotation matrix, using the largest of `w`, `x`,
    /// `y` and `z` as the pivot.
    pub fn from_m3(a: M3) -> Quat {
        let t = trace(a);
        let pivots = [
            1.0 + 2.0 * a[0][0] - t,
            1.0 + 2.0 * a[1][1] - t,
            1.0 + 2.0 * a[2][2] - t,
            1.0 + t,
        ];
        let k = (0..4).fold(0, |k, i| if pivots[i] > pivots[k] { i } else { k });
        let p = 0.5 * libm::sqrt(pivots[k].max(0.0));
        let f = 0.25 / p;
        let q = match k {
            0 => [
                p,
                (a[0][1] + a[1][0]) * f,
                (a[0][2] + a[2][0]) * f,
                (a[1][2] - a[2][1]) * f,
            ],
            1 => [
                (a[0][1] + a[1][0]) * f,
                p,
                (a[1][2] + a[2][1]) * f,
                (a[2][0] - a[0][2]) * f,
            ],
            2 => [
                (a[0][2] + a[2][0]) * f,
                (a[1][2] + a[2][1]) * f,
                p,
                (a[0][1] - a[1][0]) * f,
            ],
            _ => [
                (a[1][2] - a[2][1]) * f,
                (a[2][0] - a[0][2]) * f,
                (a[0][1] - a[1][0]) * f,
                p,
            ],
        };
        Quat(q)
    }

    /// The rotation angle from `self` to `other`, ignoring sign: `2 atan2(|v|, |w|)`
    /// of `self⁻¹ ⊗ other`.
    pub fn angle_to(self, other: Quat) -> f64 {
        let d = self.conj() * other;
        2.0 * libm::atan2(norm(d.vec()), d.w().abs())
    }
}

/// Hamilton's product `self ⊗ rhs`. `A(p ⊗ q) = A(q) A(p)`.
impl Mul for Quat {
    type Output = Quat;

    fn mul(self, rhs: Quat) -> Quat {
        let (a, aw) = (self.vec(), self.w());
        let (b, bw) = (rhs.vec(), rhs.w());
        let v = add(add(scale(b, aw), scale(a, bw)), cross(a, b));
        Quat::from_parts(v, aw * bw - dot(a, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f64], b: &[f64], tol: f64) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
    }

    const Q: Quat = Quat([
        0.18257418583505536,
        -0.3651483716701107,
        0.5477225575051661,
        0.7302967433402214,
    ]);

    #[test]
    fn cross_is_orthogonal_and_right_handed() {
        assert_eq!(cross([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]);
        let (a, b) = ([0.3, -1.2, 2.0], [4.0, 0.5, -0.7]);
        let c = cross(a, b);
        assert!(dot(a, c).abs() < 1e-14 && dot(b, c).abs() < 1e-14);
    }

    #[test]
    fn normalize_scales_to_unit_length() {
        for v in [
            [3.0, 4.0, 0.0],
            [1e-250, 0.0, -1e-250],
            [1e200, 1e200, 1e200],
        ] {
            let u = normalize(v).unwrap();
            assert!((norm(u) - 1.0).abs() < 1e-15, "{v:?}");
        }
    }

    /// A seeded sweep over signs and exponents from 1e-300 to 1e300.
    #[test]
    fn normalize_is_unit_or_none_across_exponents() {
        let mut s = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..200_000 {
            let v: V3 = core::array::from_fn(|_| {
                let bits = next();
                let mantissa = (bits >> 11) as f64 / (1u64 << 53) as f64;
                let exponent = (bits % 601) as i32 - 300;
                let x = mantissa * libm::pow(10.0, exponent as f64);
                if bits & 1 == 0 { x } else { -x }
            });
            if let Some(u) = normalize(v) {
                assert!((norm(u) - 1.0).abs() < 1e-15, "{v:?} -> {u:?}");
                assert!(dot(u, v) > 0.0, "{v:?} reversed");
            } else {
                assert!(v.iter().all(|x| x.abs() < 1e-300), "{v:?} rejected");
            }
        }
    }

    #[test]
    fn normalize_rejects_zero_tiny_and_non_finite() {
        assert_eq!(normalize([0.0; 3]), None);
        assert_eq!(normalize([1e-301, 0.0, 0.0]), None);
        assert_eq!(normalize([f64::NAN, 1.0, 0.0]), None);
        assert_eq!(normalize([f64::INFINITY, 1.0, 0.0]), None);
    }

    #[test]
    fn adj_and_det_invert() {
        let m = [[4.0, 1.0, 0.5], [1.0, 3.0, -0.2], [0.5, -0.2, 2.0]];
        let inv = inv_sym(m).unwrap();
        assert!(close(
            mat_mul(m, inv).as_flattened(),
            I3.as_flattened(),
            1e-15
        ));
        assert!((det(m) - 20.89).abs() < 1e-12);
    }

    #[test]
    fn inv_sym_rejects_singular_and_indefinite() {
        let rank2 = outer([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        assert_eq!(
            inv_sym(mat_add(rank2, outer([0.0, 1.0, 0.0], [0.0, 1.0, 0.0]))),
            None
        );
        assert_eq!(inv_sym(mat_scale(I3, -1.0)), None);
        assert_eq!(inv_sym([[f64::NAN; 3]; 3]), None);
    }

    #[test]
    fn attitude_matrix_is_hamilton_transpose() {
        let a = Q.to_m3();
        assert!(close(
            mat_mul(a, transpose(a)).as_flattened(),
            I3.as_flattened(),
            1e-15
        ));
        assert!((det(a) - 1.0).abs() < 1e-15);
        let r = [0.2, 0.9, -0.4];
        let hamilton = (Q * Quat::from_parts(r, 0.0) * Q.conj()).vec();
        assert!(close(&mat_vec(transpose(a), r), &hamilton, 1e-15));
    }

    #[test]
    fn product_composes_attitudes_in_reverse() {
        let p = Quat([0.5, 0.5, -0.5, 0.5]);
        let lhs = (p * Q).to_m3();
        let rhs = mat_mul(Q.to_m3(), p.to_m3());
        assert!(close(lhs.as_flattened(), rhs.as_flattened(), 1e-15));
    }

    #[test]
    fn from_m3_inverts_to_m3_on_every_pivot() {
        for q in [
            Q,
            Quat([1.0, 0.0, 0.0, 0.0]),
            Quat([0.0, 1.0, 0.0, 0.0]),
            Quat([0.0, 0.0, -1.0, 0.0]),
            Quat::IDENTITY,
            Quat([0.6, 0.0, 0.0, 0.8]),
        ] {
            let back = Quat::from_m3(q.to_m3());
            assert!(q.angle_to(back) < 1e-15, "{q:?} -> {back:?}");
        }
    }

    #[test]
    fn angle_to_ignores_sign() {
        let half_turn = Quat([0.0, 0.0, 1.0, 0.0]);
        assert!((Quat::IDENTITY.angle_to(half_turn) - core::f64::consts::PI).abs() < 1e-15);
        let negated = Quat(Q.0.map(|x| -x));
        assert_eq!(Q.angle_to(negated), 0.0);
    }

    #[test]
    fn normalized_rejects_zero_and_nan() {
        assert_eq!(Quat([0.0; 4]).normalized(), None);
        assert_eq!(Quat([f64::NAN, 0.0, 0.0, 1.0]).normalized(), None);
    }
}
