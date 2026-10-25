//! Wahba's problem: the attitude that best maps reference vectors onto their
//! measured body vectors. Solved with QUEST (Shuster & Oh 1981) and its
//! sequential rotations near 180°.

use crate::geom::{
    I3, M3, Quat, V3, add, adj, cross, det, dot, inv_sym, mat_add, mat_scale, mat_vec, norm, outer,
    scale, trace, transpose,
};

/// Two vectors closer than this (rad) are treated as parallel.
const PARALLEL: f64 = 1e-9;
/// The smallest `|γ| / ‖[x; γ]‖` accepted without trying the rotated frames.
const MIN_GAMMA: f64 = 0.1;
const NEWTON_STEPS: usize = 5;
const NEWTON_TOL: f64 = 1e-14;
/// Below this `|f′(λ_max)|`, λ_max is a double root and the attitude is not
/// determined.
const MIN_SLOPE: f64 = 1e-12;
/// The smallest `det(info) / (tr(info) / 3)³` accepted, about the ratio of
/// the weakest axis's information to the mean.
const MIN_INFO_RATIO: f64 = 1e-12;

/// A measured body-frame unit vector, its reference-frame unit vector, and
/// the measurement's standard deviation (rad).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obs {
    pub b: V3,
    pub r: V3,
    pub sigma: f64,
}

/// An axis of the reference frame that QUEST may turn the references 180°
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X = 0,
    Y = 1,
    Z = 2,
}

impl Axis {
    pub const ALL: [Axis; 3] = [Axis::X, Axis::Y, Axis::Z];

    /// The 180° rotation about this axis.
    pub fn half_turn(self) -> Quat {
        match self {
            Axis::X => Quat([1.0, 0.0, 0.0, 0.0]),
            Axis::Y => Quat([0.0, 1.0, 0.0, 0.0]),
            Axis::Z => Quat([0.0, 0.0, 1.0, 0.0]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WahbaError {
    /// Fewer than two observations.
    TooFew,
    /// The body or the reference vectors are all parallel, or the solution is
    /// otherwise undetermined.
    Degenerate,
    /// Newton's method did not find `λ_max`.
    NoConvergence,
    /// An observation is not finite or has a non-positive `sigma`.
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Solution {
    /// The body←reference attitude, `b = A(q) r`.
    pub q: Quat,
    /// Covariance (rad²) of the body-frame attitude error.
    pub p_theta: M3,
    /// Wahba's loss at `q`, from [`wahba_loss`].
    pub loss: f64,
    /// The frame QUEST solved in, if it turned the references.
    pub rotated: Option<Axis>,
}

/// The attitude profile `B = Σ aᵢ bᵢ rᵢᵀ` with `Σ aᵢ = 1`, and the parts of it
/// QUEST uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub b: M3,
    /// `B + Bᵀ`.
    pub s: M3,
    /// `Σ aᵢ bᵢ × rᵢ`.
    pub z: V3,
    /// `tr B`.
    pub sigma: f64,
    /// `Σ σᵢ⁻²`, the scale removed from the weights.
    pub weight_sum: f64,
}

impl Profile {
    /// Builds the profile with `aᵢ = σᵢ⁻² / Σ σⱼ⁻²`.
    pub fn new(obs: &[Obs]) -> Profile {
        let weight_sum = weight_sum(obs);
        let b = obs.iter().fold([[0.0; 3]; 3], |b, o| {
            mat_add(b, mat_scale(outer(o.b, o.r), weight(o) / weight_sum))
        });
        Profile::from_b(b, weight_sum)
    }

    fn from_b(b: M3, weight_sum: f64) -> Profile {
        Profile {
            b,
            s: mat_add(b, transpose(b)),
            z: [b[1][2] - b[2][1], b[2][0] - b[0][2], b[0][1] - b[1][0]],
            sigma: trace(b),
            weight_sum,
        }
    }

    /// The profile of the same observations with every reference vector
    /// turned 180° about `axis`, a sign flip of two columns of `B`.
    pub fn rotated(&self, axis: Axis) -> Profile {
        let mut b = self.b;
        for row in &mut b {
            for (j, x) in row.iter_mut().enumerate() {
                if j != axis as usize {
                    *x = -*x;
                }
            }
        }
        Profile::from_b(b, self.weight_sum)
    }
}

fn weight(o: &Obs) -> f64 {
    1.0 / (o.sigma * o.sigma)
}

fn weight_sum(obs: &[Obs]) -> f64 {
    obs.iter().map(weight).sum()
}

/// QUEST's characteristic quartic `λ⁴ − (a + b)λ² − cλ + (ab + cσ − d)`,
/// whose roots are the eigenvalues of Davenport's `K`.
struct Quartic {
    a_plus_b: f64,
    c: f64,
    k: f64,
}

impl Quartic {
    fn new(p: &Profile) -> Quartic {
        let sz = mat_vec(p.s, p.z);
        let sigma2 = p.sigma * p.sigma;
        let a = sigma2 - trace(adj(p.s));
        let b = sigma2 + dot(p.z, p.z);
        let c = det(p.s) + dot(p.z, sz);
        Quartic {
            a_plus_b: a + b,
            c,
            k: a * b + c * p.sigma - dot(sz, sz),
        }
    }

    fn eval(&self, l: f64) -> f64 {
        ((l * l - self.a_plus_b) * l - self.c) * l + self.k
    }

    fn slope(&self, l: f64) -> f64 {
        (4.0 * l * l - 2.0 * self.a_plus_b) * l - self.c
    }

    /// Newton's method from `λ₀ = 1`, at or above the largest root, until a
    /// step is below [`NEWTON_TOL`] or stops shrinking.
    fn largest_root(&self) -> Option<f64> {
        let mut lambda = 1.0;
        let mut last = f64::INFINITY;
        for _ in 0..NEWTON_STEPS {
            let step = self.eval(lambda) / self.slope(lambda);
            if !step.is_finite() {
                return None;
            }
            if step.abs() >= last {
                return Some(lambda);
            }
            lambda -= step;
            if step.abs() < NEWTON_TOL {
                return Some(lambda);
            }
            last = step.abs();
        }
        None
    }
}

/// The largest eigenvalue of Davenport's `K`, or `None` if Newton's method
/// has not converged within `NEWTON_STEPS` steps.
pub fn lambda_max(p: &Profile) -> Option<f64> {
    Quartic::new(p).largest_root()
}

/// QUEST's quaternion in the frame `axis` names, normalised, and `|γ|` of
/// the unit quaternion: `‖[x; γ]‖ = f′(λ) |γ_unit|`, which stays measurable
/// near 180° where `x` and `γ` both vanish.
fn solve_in(p: &Profile, lambda: f64, slope: f64, axis: Option<Axis>) -> Option<(Quat, f64)> {
    let rotated;
    let p = match axis {
        None => p,
        Some(a) => {
            rotated = p.rotated(a);
            &rotated
        }
    };
    let alpha = lambda * lambda - p.sigma * p.sigma + trace(adj(p.s));
    let beta = lambda - p.sigma;
    let gamma = (lambda + p.sigma) * alpha - det(p.s);
    let sz = mat_vec(p.s, p.z);
    let x = add(add(scale(p.z, alpha), scale(sz, beta)), mat_vec(p.s, sz));
    let q = Quat::from_parts(x, gamma).normalized()?;
    Some((q, libm::hypot(norm(x), gamma) / slope.abs()))
}

/// Maps a solution in the frame `axis` names back to the unrotated frame.
fn unrotate(q: Quat, axis: Option<Axis>) -> Quat {
    axis.map_or(q, |a| a.half_turn() * q)
}

/// Tries `hint` first. If `|γ| / ‖[x; γ]‖` is below [`MIN_GAMMA`] there,
/// solves in all four frames and keeps the one where it is largest.
fn solve(p: &Profile, lambda: f64, hint: Option<Axis>) -> Option<(Quat, Option<Axis>)> {
    let slope = Quartic::new(p).slope(lambda);
    if slope.is_nan() || slope.abs() < MIN_SLOPE {
        return None;
    }
    let first = solve_in(p, lambda, slope, hint).map(|(q, g)| (q, g, hint));
    let best = match first {
        Some((_, g, _)) if g >= MIN_GAMMA => first,
        _ => [None, Some(Axis::X), Some(Axis::Y), Some(Axis::Z)]
            .into_iter()
            .filter(|a| *a != hint)
            .filter_map(|a| solve_in(p, lambda, slope, a).map(|(q, g)| (q, g, a)))
            .chain(first)
            .max_by(|(_, g1, _), (_, g2, _)| g1.total_cmp(g2)),
    };
    best.map(|(q, _, axis)| (unrotate(q, axis), axis))
}

fn is_valid(o: &Obs) -> bool {
    let finite = o.b.iter().chain(&o.r).all(|x| x.is_finite());
    finite && o.sigma > 0.0 && o.sigma.is_finite()
}

fn all_parallel(mut vs: impl Iterator<Item = V3>) -> bool {
    let Some(first) = vs.next() else {
        return true;
    };
    vs.all(|v| norm(cross(first, v)) < PARALLEL)
}

fn validate(obs: &[Obs]) -> Result<(), WahbaError> {
    if obs.len() < 2 {
        return Err(WahbaError::TooFew);
    }
    if !obs.iter().all(is_valid) {
        return Err(WahbaError::Invalid);
    }
    if all_parallel(obs.iter().map(|o| o.b)) || all_parallel(obs.iter().map(|o| o.r)) {
        return Err(WahbaError::Degenerate);
    }
    Ok(())
}

/// The QUEST measurement model's covariance `[Σ σᵢ⁻² (I − bᵢbᵢᵀ)]⁻¹`, or
/// `None` when an axis is unobserved to within `MIN_INFO_RATIO`.
pub fn attitude_covariance(obs: &[Obs]) -> Option<M3> {
    let info = obs.iter().fold([[0.0; 3]; 3], |m, o| {
        let tangent = mat_add(I3, mat_scale(outer(o.b, o.b), -1.0));
        mat_add(m, mat_scale(tangent, weight(o)))
    });
    let mean = trace(info) / 3.0;
    if det(info) < MIN_INFO_RATIO * mean * mean * mean {
        return None;
    }
    inv_sym(info)
}

/// Wahba's loss `½ Σ aᵢ ‖bᵢ − A(q) rᵢ‖²` with `aᵢ = σᵢ⁻² / Σ σⱼ⁻²`.
pub fn wahba_loss(obs: &[Obs], q: Quat) -> f64 {
    let a = q.to_m3();
    let sum = weight_sum(obs);
    obs.iter()
        .map(|o| {
            let e = add(o.b, scale(mat_vec(a, o.r), -1.0));
            weight(o) / sum * dot(e, e)
        })
        .sum::<f64>()
        * 0.5
}

/// Solves Wahba's problem with QUEST. `hint` names the frame to try first, for
/// example the one the previous solution used.
pub fn quest(obs: &[Obs], hint: Option<Axis>) -> Result<Solution, WahbaError> {
    validate(obs)?;
    let p_theta = attitude_covariance(obs).ok_or(WahbaError::Degenerate)?;
    let profile = Profile::new(obs);
    let lambda = lambda_max(&profile).ok_or(WahbaError::NoConvergence)?;
    let (q, rotated) = solve(&profile, lambda, hint).ok_or(WahbaError::Degenerate)?;
    Ok(Solution {
        q,
        p_theta,
        loss: wahba_loss(obs, q),
        rotated,
    })
}

#[cfg(test)]
mod tests;
