//! The vector update: one sequential Joseph-form pass from the prior, gated
//! on the fit residuals.

use nox::{ArrayRepr, Const, Matrix, Vector, tensor};

use super::{M6, Quat, State, UpdateCounts, V3, V6, VectorObs};

type V2 = Vector<f64, 2, ArrayRepr>;
type M2 = Matrix<f64, 2, 2, ArrayRepr>;
type M2x6 = Matrix<f64, 2, 6, ArrayRepr>;
type M6x2 = Matrix<f64, 6, 2, ArrayRepr>;

/// The most observations one update considers; later ones count as gated.
pub const MAX_OBS: usize = 64;
/// The least fraction of `R` the post-fit residual covariance may shrink to.
const RESIDUAL_FLOOR: f64 = 0.01;

/// Which of up to [`MAX_OBS`] observations take part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mask(u64);

impl Mask {
    /// The observations that are finite and have `sigma > 0`.
    fn valid(obs: &[VectorObs]) -> Mask {
        let bits = obs
            .iter()
            .take(MAX_OBS)
            .enumerate()
            .filter(|(_, o)| is_valid(o))
            .fold(0u64, |m, (i, _)| m | 1 << i);
        Mask(bits)
    }

    fn has(self, i: usize) -> bool {
        i < MAX_OBS && self.0 & (1 << i) != 0
    }

    fn without(self, i: usize) -> Mask {
        Mask(self.0 & !(1 << i))
    }

    fn count(self) -> u32 {
        self.0.count_ones()
    }

    fn iter(self, obs: &[VectorObs]) -> impl Iterator<Item = (usize, &VectorObs)> {
        obs.iter().enumerate().filter(move |(i, _)| self.has(*i))
    }
}

fn is_valid(o: &VectorObs) -> bool {
    let finite = |v: V3| v.into_buf().iter().all(|x| x.is_finite());
    finite(o.body) && finite(o.reference) && o.sigma.is_finite() && o.sigma > 0.0
}

/// An estimate of attitude and bias with its covariance.
#[derive(Clone, Copy)]
struct Estimate {
    q: Quat,
    b: V3,
    p: M6,
}

impl Estimate {
    /// Fuses the observations in `mask` in sequence from this prior and
    /// returns the posterior.
    fn pass(&self, obs: &[VectorObs], mask: Mask) -> Estimate {
        let (mut p, mut dx) = (self.p, V6::zeros());
        let q_inv = self.q.inverse();
        for (_, o) in mask.iter(obs) {
            fuse(&mut p, &mut dx, q_inv, o);
        }
        Estimate {
            q: self.q.integrate_body(dx.fixed_slice::<Const<3>>(&[0])),
            b: self.b + dx.fixed_slice::<Const<3>>(&[3]),
            p,
        }
    }

    /// Passes and drops the observation whose fit residual is worst while it
    /// exceeds `gate`. Returns `None` if fewer than half of `mask`, rounded
    /// up, would remain.
    fn gated_fit(&self, obs: &[VectorObs], mut mask: Mask, gate: f64) -> Option<(Estimate, Mask)> {
        let n = mask.count();
        let floor = n - n / 2;
        loop {
            let post = self.pass(obs, mask);
            let worst = mask
                .iter(obs)
                .map(|(i, o)| (i, fit_d2(&post, o)))
                .max_by(|a, b| a.1.total_cmp(&b.1));
            match worst {
                Some((i, d2)) if d2.is_nan() || d2 > gate => mask = mask.without(i),
                _ => return Some((post, mask)),
            }
            if mask.count() < floor {
                return None;
            }
        }
    }
}

impl State {
    /// Fuses `obs` in sequence about the current `q_hat` and applies the
    /// correction once. Observations that are not finite or have `sigma ≤ 0`
    /// are gated. Then, while the worst fit residual's squared Mahalanobis
    /// distance against `R + H P⁺ Hᵀ` exceeds `gate`, that observation is
    /// dropped and the pass rerun; a frame that would lose more than half is
    /// rejected whole. With `gate = ∞` this is one pass.
    pub fn update_vectors(&mut self, obs: &[VectorObs], gate: f64) -> UpdateCounts {
        let prior = Estimate {
            q: self.q_hat,
            b: self.b_hat,
            p: self.p,
        };
        let fit = prior.gated_fit(obs, Mask::valid(obs), gate);
        self.accept(fit, obs.len())
    }

    /// Sets the attitude block of `p` to `variance·I` (rad²) and clears its
    /// correlation with the bias, keeping the bias block.
    pub fn reset_attitude_variance(&mut self, variance: f64) {
        let mut p = self.p.into_buf();
        for (i, row) in p.iter_mut().enumerate() {
            for (j, x) in row.iter_mut().enumerate() {
                if i < 3 || j < 3 {
                    *x = if i == j { variance } else { 0.0 };
                }
            }
        }
        self.p = Matrix::from_buf(p);
    }

    /// Moves to `fit`'s estimate, or stays at the prior when there is none.
    fn accept(&mut self, fit: Option<(Estimate, Mask)>, n: usize) -> UpdateCounts {
        let n = n as u32;
        match fit {
            Some((e, mask)) if mask.count() > 0 => {
                (self.q_hat, self.b_hat, self.p) = (e.q, e.b, e.p);
                UpdateCounts {
                    used: mask.count(),
                    gated: n - mask.count(),
                }
            }
            _ => UpdateCounts { used: 0, gated: n },
        }
    }
}

/// The squared Mahalanobis distance of `o`'s post-fit residual at `e`,
/// across the predicted vector, against its covariance `R − H P⁺ Hᵀ`. That
/// covariance is floored at [`RESIDUAL_FLOOR`]`·R` so rounding cannot make it
/// singular.
fn fit_d2(e: &Estimate, o: &VectorObs) -> f64 {
    let Some(m) = Tangent::at(e.q.inverse() * o.reference, o.sigma) else {
        return f64::NAN;
    };
    let c = m.r - m.h.dot(&e.p).dot(&m.h.transpose());
    let Some(c_inv) = finite_inverse(floor_residual(c, o.sigma)) else {
        return f64::NAN;
    };
    let nu = m.residual(o.body);
    nu.dot(&c_inv.dot(&nu)).into_buf()
}

/// `c` if both its eigenvalues are at least [`RESIDUAL_FLOOR`]`·σ²`, else
/// `c` with its eigenvalues raised to that floor.
fn floor_residual(c: M2, sigma: f64) -> M2 {
    let floor = RESIDUAL_FLOOR * sigma * sigma;
    let [[a, b], [_, d]] = c.into_buf();
    let mean = (a + d) / 2.0;
    let spread = (((a - d) / 2.0).powi(2) + b * b).sqrt();
    if mean - spread >= floor {
        return c;
    }
    // Shift both eigenvalues up so the smaller one sits on the floor.
    c + M2::eye() * (floor - (mean - spread))
}

/// A unit vector measurement in the two directions across the predicted
/// vector `body_r`, the only ones it constrains. Keeping the radial direction
/// out leaves the innovation covariance well conditioned when `σ²` is far
/// below the prior.
struct Tangent {
    /// `[t1ᵀ; t2ᵀ]`, an orthonormal basis across `body_r`.
    basis: [V3; 2],
    body_r: V3,
    h: M2x6,
    r: M2,
}

impl Tangent {
    fn at(body_r: V3, sigma: f64) -> Option<Tangent> {
        let b = body_r.into_buf();
        let least = (0..3).fold(0, |m, i| if b[i].abs() < b[m].abs() { i } else { m });
        let mut e = [0.0; 3];
        e[least] = 1.0;
        let t1 = body_r.cross(&V3::from_buf(e));
        let n: f64 = t1.norm().into_buf();
        if n.is_nan() || n <= 0.0 {
            return None;
        }
        let t1 = t1 * (1.0 / n);
        let t2 = body_r.cross(&t1);
        // tᵀ[b×] = (t × b)ᵀ, with t1 × b = −t2 and t2 × b = t1.
        let [a0, a1, a2] = (t2 * -1.0).into_buf();
        let [c0, c1, c2] = t1.into_buf();
        let h = Matrix::from_buf([[a0, a1, a2, 0.0, 0.0, 0.0], [c0, c1, c2, 0.0, 0.0, 0.0]]);
        Some(Tangent {
            basis: [t1, t2],
            body_r,
            h,
            r: M2::eye() * (sigma * sigma),
        })
    }

    /// The measured `body` less the predicted vector, across it.
    fn residual(&self, body: V3) -> V2 {
        let e = body - self.body_r;
        tensor![
            e.dot(&self.basis[0]).into_buf(),
            e.dot(&self.basis[1]).into_buf()
        ]
    }
}

/// Folds one observation into `p` and `dx`, linearised about `q_inv`.
/// Skips an observation whose innovation covariance cannot be inverted.
fn fuse(p: &mut M6, dx: &mut V6, q_inv: Quat, o: &VectorObs) {
    let Some(m) = Tangent::at(q_inv * o.reference, o.sigma) else {
        return;
    };
    let h_t = m.h.transpose();
    let Some(s_inv) = finite_inverse(m.h.dot(p).dot(&h_t) + m.r) else {
        return;
    };
    let nu = m.residual(o.body) - m.h.dot(dx);
    let k = p.dot(&h_t.dot(&s_inv));
    *p = joseph(*p, k, m.h, m.r);
    *dx = *dx + k.dot(&nu);
}

/// Inverts a 2x2 matrix, or returns `None` if any entry of the result is not finite.
fn finite_inverse(m: M2) -> Option<M2> {
    m.try_inverse()
        .ok()
        .filter(|m| m.into_buf().as_flattened().iter().all(|x| x.is_finite()))
}

/// The Joseph-form update `(I − KH)P(I − KH)ᵀ + KRKᵀ`, symmetrised.
fn joseph(p: M6, k: M6x2, h: M2x6, r: M2) -> M6 {
    let a = M6::eye() - k.dot(&h);
    let p = a.dot(&p).dot(&a.transpose()) + k.dot(&r).dot(&k.transpose());
    (p + p.transpose()) * 0.5
}
