//! The ideal star tracker. It reads the true attitude, as the plant does, and
//! writes the synthetic catalog's stars in its field of view as matched body
//! vectors with tangent-plane noise. There is no image and no identification.

use adcs_contracts::{
    BodyState, MAX_STARS, Quat, StarMode, StarParams, StarSolution, StarStatus, StarVectors,
};
use metor_fsw_3::{Input, Output, Timestamp, system};
use metor_star::catalog::synthetic;
use metor_star::geom::{self, V3};
use metor_star::wahba::{self, Obs, Solution, WahbaError};
use nox::{Matrix3, Quaternion, Vector};
use rand::{Rng, SeedableRng, rngs::StdRng};
use rand_distr::StandardNormal;

/// Stars in the synthetic catalog, about 38 in a 20° field of view.
pub const CATALOG_SIZE: usize = 5000;

/// How early (s) a frame may fall due, so a 120 Hz clock keeps a 10 Hz rate.
const RATE_SLACK: f64 = 0.5e-3;

pub struct StarIdeal {
    params: StarParams,
    q_b_st: geom::Quat,
    catalog: Vec<V3>,
    /// Catalog rows in the field of view, sized for the whole catalog.
    in_fov: Vec<u32>,
    /// Observations for QUEST, sized for one frame.
    obs: Vec<Obs>,
    rng: StdRng,
    last: Option<Timestamp>,
    frame_id: u64,
}

impl StarIdeal {
    pub fn new(params: StarParams) -> StarIdeal {
        let q_b_st = geom::Quat(params.q_b_st)
            .normalized()
            .unwrap_or(geom::Quat::IDENTITY);
        StarIdeal {
            q_b_st,
            catalog: synthetic::uniform(CATALOG_SIZE, params.seed),
            in_fov: Vec::with_capacity(CATALOG_SIZE),
            obs: Vec::with_capacity(MAX_STARS),
            rng: StdRng::seed_from_u64(params.seed),
            last: None,
            frame_id: 0,
            params,
        }
    }

    /// Whether a frame falls due at `now`, and if so marks it taken.
    fn due(&mut self, now: Timestamp) -> bool {
        let period = 1.0 / self.params.rate_hz;
        let due = self
            .last
            .is_none_or(|last| seconds(now, last) + RATE_SLACK >= period);
        if due {
            self.last = Some(now);
        }
        due
    }

    /// The frame of stars seen at attitude `q_b_eci` at `timestamp`.
    pub fn observe(&mut self, q_b_eci: geom::Quat, timestamp: Timestamp) -> StarVectors {
        self.frame_id += 1;
        let mut frame = StarVectors::empty(timestamp, self.frame_id);
        let boresight = boresight_eci(q_b_eci, self.q_b_st);
        let cos_half = (self.params.fov_deg.to_radians() / 2.0).cos();
        select_in_fov(&self.catalog, boresight, cos_half, &mut self.in_fov);
        let n = (self.params.n_stars_max as usize).min(MAX_STARS);
        let chosen = choose(&mut self.in_fov, n, &mut self.rng);
        let sigma = self.params.sigma_rad;
        for &row in chosen.iter() {
            let r = self.catalog[row as usize];
            let b = perturb(q_b_eci.rotate(r), sigma, &mut self.rng);
            frame.push(b, r, sigma, row + 1);
        }
        frame
    }

    /// The tracker's QUEST solution for `frame`.
    pub fn solve(&mut self, frame: &StarVectors) -> StarSolution {
        self.obs.clear();
        self.obs.extend(frame.obs().map(to_obs));
        let result = wahba::quest(&self.obs, None);
        solution(frame, result)
    }
}

fn seconds(a: Timestamp, b: Timestamp) -> f64 {
    (a.0 - b.0) as f64 * 1e-6
}

/// The star tracker's boresight in ECI.
pub fn boresight_eci(q_b_eci: geom::Quat, q_b_st: geom::Quat) -> V3 {
    q_b_eci.conj().rotate(q_b_st.rotate([0.0, 0.0, 1.0]))
}

/// Fills `out` with the rows of `catalog` within the cone `r · boresight ≥
/// cos_half`.
pub fn select_in_fov(catalog: &[V3], boresight: V3, cos_half: f64, out: &mut Vec<u32>) {
    out.clear();
    let rows = (0u32..).zip(catalog);
    out.extend(
        rows.filter(|(_, r)| geom::dot(**r, boresight) >= cos_half)
            .map(|(i, _)| i),
    );
}

/// Moves `k` rows picked uniformly at random to the front and returns them.
pub fn choose<'a>(rows: &'a mut [u32], k: usize, rng: &mut StdRng) -> &'a [u32] {
    let k = k.min(rows.len());
    for i in 0..k {
        let j = rng.random_range(i..rows.len());
        rows.swap(i, j);
    }
    &rows[..k]
}

/// `b` turned by an isotropic tangent-plane error of `sigma` (rad) per axis.
pub fn perturb(b: V3, sigma: f64, rng: &mut StdRng) -> V3 {
    if sigma == 0.0 {
        return b;
    }
    let least = (0..3).fold(0, |m, i| if b[i].abs() < b[m].abs() { i } else { m });
    let mut e = [0.0; 3];
    e[least] = 1.0;
    let Some(t1) = geom::normalize(geom::cross(b, e)) else {
        return b;
    };
    let t2 = geom::cross(b, t1);
    let n1: f64 = rng.sample(StandardNormal);
    let n2: f64 = rng.sample(StandardNormal);
    let offset = geom::add(geom::scale(t1, sigma * n1), geom::scale(t2, sigma * n2));
    geom::normalize(geom::add(b, offset)).unwrap_or(b)
}

pub fn to_obs((b, r, sigma): (adcs_contracts::V3, adcs_contracts::V3, f64)) -> Obs {
    Obs {
        b: b.into_buf(),
        r: r.into_buf(),
        sigma,
    }
}

fn status(error: WahbaError, count: u64) -> StarStatus {
    match error {
        _ if count == 0 => StarStatus::NoStars,
        WahbaError::TooFew => StarStatus::NoStars,
        WahbaError::NoConvergence => StarStatus::NoConvergence,
        WahbaError::Degenerate | WahbaError::Invalid => StarStatus::Degenerate,
    }
}

fn solution(frame: &StarVectors, result: Result<Solution, WahbaError>) -> StarSolution {
    let n = frame.count.min(MAX_STARS as u64);
    let mut out = match result {
        Ok(_) => StarSolution::new(
            frame.timestamp,
            frame.frame_id,
            StarMode::Ideal,
            StarStatus::Valid,
        ),
        Err(e) => StarSolution::new(
            frame.timestamp,
            frame.frame_id,
            StarMode::Ideal,
            status(e, n),
        ),
    };
    out.n_centroids = n as u16;
    if let Ok(sol) = result {
        out.q_b_eci = Quaternion(Vector::from_buf(sol.q.0));
        out.p_theta = Matrix3::from_buf(sol.p_theta);
        out.residual_rms = (2.0 * sol.loss).sqrt();
        out.n_matched = n as u16;
    }
    out
}

fn to_star_quat(q: &Quat) -> geom::Quat {
    geom::Quat(q.0.into_buf())
}

#[system]
/// Writes a frame of star vectors and the tracker's solution at `rate_hz`.
pub fn star_ideal(
    #[state] state: &mut StarIdeal,
    now: Timestamp,
    body: &mut Input<BodyState>,
    star_vectors: &mut Output<StarVectors>,
    star_solution: &mut Output<StarSolution>,
) {
    let Ok(Some(body)) = body.latest() else {
        return; // no truth yet
    };
    let (q, stamp) = (to_star_quat(&body.q_b_eci), body.timestamp);
    if !state.due(now) {
        return;
    }
    let frame = state.observe(q, stamp);
    let solution = state.solve(&frame);
    let _ = star_vectors.write(&frame);
    let _ = star_solution.write(&solution);
}

#[cfg(test)]
mod tests;
