//! Verification: project the catalog around the solved boresight and match
//! each observation to its nearest star.

use alloc::vec::Vec;

use super::{LisConfig, MAX_OBS, NO_ROW};
use crate::catalog::Db;
use crate::geom::{Quat, V3, dot};

/// What verification found for one attitude.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    pub rows: [u16; MAX_OBS],
    pub n_match: usize,
    pub rms_rad: f64,
    pub p_false: f64,
    /// All three tests passed.
    pub accepted: bool,
}

/// Matches `obs` (star-tracker frame, at most [`MAX_OBS`]) against the
/// catalog seen at `q`. `scratch` holds the cone's rows.
pub fn verify(db: &Db, q: Quat, obs: &[V3], cfg: &LisConfig, scratch: &mut Vec<u16>) -> Verdict {
    let obs = &obs[..obs.len().min(MAX_OBS)];
    let a = q.to_m3();
    // The boresight in ECI is the third row of A.
    let boresight = a[2];
    scratch.clear();
    db.cone(boresight, cfg.fov_radius_rad + cfg.match_rad, scratch);
    let cos_match = libm::cos(cfg.match_rad);
    let mut rows = [NO_ROW; MAX_OBS];
    let (mut n_match, mut sq) = (0usize, 0.0);
    for (slot, b) in rows.iter_mut().zip(obs) {
        // The nearest star and whether a second one is also within reach.
        let (mut best, mut best_cos, mut second) = (NO_ROW, cos_match, false);
        for &row in scratch.iter() {
            let Some(star) = db.star(row) else { continue };
            let c = dot(*b, q.rotate(star.unit()));
            if c >= cos_match {
                second |= best != NO_ROW;
                if c > best_cos || best == NO_ROW {
                    best = row;
                    best_cos = c.max(best_cos);
                }
            }
        }
        if best != NO_ROW && !second {
            *slot = best;
            n_match += 1;
            let ang = libm::acos(best_cos.min(1.0));
            sq += ang * ang;
        }
    }
    let rms_rad = if n_match > 0 {
        libm::sqrt(sq / n_match as f64)
    } else {
        f64::INFINITY
    };
    // The chance that one observation lands on some star by luck.
    let cap = |r: f64| 1.0 - libm::cos(r);
    let p_one = (scratch.len() as f64 * cap(cfg.match_rad) / cap(cfg.fov_radius_rad)).min(1.0);
    let p_false = binomial_sf(n_match, obs.len(), p_one);
    let accepted =
        n_match >= cfg.min_match && rms_rad < 3.0 * cfg.sigma_rad && p_false < cfg.p_false_max;
    Verdict {
        rows,
        n_match,
        rms_rad,
        p_false,
        accepted,
    }
}

/// `P(X ≥ k)` for `X ~ Binomial(n, p)`.
pub fn binomial_sf(k: usize, n: usize, p: f64) -> f64 {
    if k == 0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return 1.0;
    }
    let ln_choose = |n: usize, i: usize| {
        let f = |x: usize| libm::lgamma(x as f64 + 1.0);
        f(n) - f(i) - f(n - i)
    };
    let (lp, lq) = (libm::log(p), libm::log1p(-p));
    (k..=n)
        .map(|i| libm::exp(ln_choose(n, i) + i as f64 * lp + (n - i) as f64 * lq))
        .sum::<f64>()
        .min(1.0)
}
