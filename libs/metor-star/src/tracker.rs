//! The star tracker's per-frame state machine.
//!
//! With a usable prior (an attitude predicted from the last solution and the
//! gyro), [`Tracker`] projects the catalog around the predicted boresight,
//! matches each centroid to its nearest prediction inside a gate, solves
//! QUEST and verifies. Without one, or when tracking fails, it runs the
//! lost-in-space [`Pyramid`] on the same frame, within a triplet budget that
//! [`Tracker::resume`] continues on later calls.

use alloc::vec::Vec;

use crate::camera::Camera;
use crate::catalog::Db;
use crate::geom::{M3, Quat, V3, dot, norm, scale};
use crate::image::{Centroid, Extractor, ImageConfig, ImageError, MAX_CENTROIDS};
use crate::lis::{LisConfig, MAX_OBS, Match, NO_ROW, Pyramid, Step, verify};
use crate::wahba::{Obs, quest};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackConfig {
    /// Centroid 1σ (px), for per-star σ and the gate.
    pub sigma_px: f64,
    /// Gate in units of the combined 1σ.
    pub gate_k: f64,
    /// A prior with 1σ attitude error past this (rad) is not used.
    pub max_prior_sigma_rad: f64,
    /// Matches needed before QUEST.
    pub min_match: usize,
}

impl Default for TrackConfig {
    fn default() -> TrackConfig {
        TrackConfig {
            sigma_px: 0.1,
            gate_k: 4.0,
            // FOV/10 for a 20° field.
            max_prior_sigma_rad: 2f64.to_radians(),
            min_match: 3,
        }
    }
}

/// A predicted attitude: `b_st = A(q) r_eci`, with a 1σ error (rad).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prior {
    pub q_st_eci: Quat,
    pub sigma_rad: f64,
}

/// How a frame was solved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lost,
    Tracking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Fewer than three centroids.
    NoStars,
    /// The frame is flooded (Sun, Earth).
    Flooded,
    /// Lost-in-space tried every triplet.
    NoMatch,
}

// Boxing `Match` would allocate per frame; it is returned by value.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Solved(Mode, Match),
    Failed(Failure),
    /// The search ran out of budget; call [`Tracker::resume`].
    Searching,
}

/// `q` turned by the star-tracker-frame rate `omega_st` (rad/s) over `dt`:
/// `A(q′) = exp(−[ω×] dt) A(q)`.
pub fn predict(q: Quat, omega_st: V3, dt: f64) -> Quat {
    let phi = scale(omega_st, dt);
    let angle = norm(phi);
    if angle < 1e-15 {
        return q;
    }
    let axis = scale(phi, 1.0 / angle);
    let (s, c) = (libm::sin(0.5 * angle), libm::cos(0.5 * angle));
    let dq = Quat::from_parts(scale(axis, s), c);
    // `A(p ⊗ q) = A(q) A(p)`: apply the turn after `q`.
    (q * dq).normalized().unwrap_or(q)
}

/// The largest eigenvalue of a symmetric 3×3, closed form.
pub fn lambda_max(m: M3) -> f64 {
    let p1 = m[0][1] * m[0][1] + m[0][2] * m[0][2] + m[1][2] * m[1][2];
    let tr = (m[0][0] + m[1][1] + m[2][2]) / 3.0;
    if p1 < 1e-300 {
        return m[0][0].max(m[1][1]).max(m[2][2]);
    }
    let sq = |x: f64| x * x;
    let p2 = sq(m[0][0] - tr) + sq(m[1][1] - tr) + sq(m[2][2] - tr) + 2.0 * p1;
    let p = libm::sqrt(p2 / 6.0);
    let b: M3 = core::array::from_fn(|i| {
        core::array::from_fn(|j| (m[i][j] - if i == j { tr } else { 0.0 }) / p)
    });
    let r = (crate::geom::det(b) / 2.0).clamp(-1.0, 1.0);
    tr + 2.0 * p * libm::cos(libm::acos(r) / 3.0)
}

#[derive(Debug, Clone)]
pub struct Tracker<'a> {
    db: Db<'a>,
    cam: Camera,
    cfg: TrackConfig,
    extractor: Extractor,
    pyramid: Pyramid,
    centroids: Vec<Centroid>,
    /// Star-tracker-frame unit vectors of `centroids`.
    obs: Vec<V3>,
    cone: Vec<u16>,
    quest_obs: Vec<Obs>,
    searching: bool,
}

impl<'a> Tracker<'a> {
    pub fn new(
        db: Db<'a>,
        cam: Camera,
        image: ImageConfig,
        lis: LisConfig,
        cfg: TrackConfig,
    ) -> Result<Tracker<'a>, ImageError> {
        Ok(Tracker {
            extractor: Extractor::new(cam.intr.width, cam.intr.height, image)?,
            pyramid: Pyramid::new(db.n_stars(), lis),
            centroids: Vec::with_capacity(MAX_CENTROIDS),
            obs: Vec::with_capacity(MAX_CENTROIDS),
            cone: Vec::with_capacity(db.n_stars()),
            quest_obs: Vec::with_capacity(MAX_OBS),
            searching: false,
            db,
            cam,
            cfg,
        })
    }

    pub fn db(&self) -> &Db<'a> {
        &self.db
    }

    pub fn camera(&self) -> &Camera {
        &self.cam
    }

    /// The last frame's centroids, brightest first; a [`Match`]'s rows index
    /// them.
    pub fn centroids(&self) -> &[Centroid] {
        &self.centroids
    }

    /// The last frame's centroid directions in the star-tracker frame.
    pub fn obs(&self) -> &[V3] {
        &self.obs
    }

    /// One star's 1σ (rad).
    pub fn sigma_rad(&self) -> f64 {
        self.cam.pixel_sigma_to_rad(self.cfg.sigma_px)
    }

    /// Starts frame `px`: extracts centroids, tracks from `prior` when it is
    /// usable, and otherwise searches with up to `budget` triplets. A frame
    /// still searching is dropped.
    pub fn process(&mut self, px: &[u16], prior: Option<Prior>, budget: u32) -> Outcome {
        self.searching = false;
        self.centroids.clear();
        self.obs.clear();
        match self.extractor.run(px) {
            Ok(found) => {
                for c in found {
                    if let Some(b) = self.cam.pixel_to_st(c.u, c.v) {
                        self.centroids.push(*c);
                        self.obs.push(b);
                    }
                }
            }
            Err(ImageError::TooManyRuns) => return Outcome::Failed(Failure::Flooded),
            Err(_) => return Outcome::Failed(Failure::NoStars),
        }
        if self.obs.len() < 3 {
            return Outcome::Failed(Failure::NoStars);
        }
        let usable =
            prior.filter(|p| p.sigma_rad.is_finite() && p.sigma_rad < self.cfg.max_prior_sigma_rad);
        if let Some(prior) = usable
            && let Some(m) = self.track(prior)
        {
            return Outcome::Solved(Mode::Tracking, m);
        }
        self.pyramid.start(&self.obs);
        self.searching = true;
        self.resume(budget)
    }

    /// Continues the current frame's search with up to `budget` triplets.
    pub fn resume(&mut self, budget: u32) -> Outcome {
        if !self.searching {
            return Outcome::Failed(Failure::NoMatch);
        }
        match self.pyramid.step(&self.db, budget) {
            Step::Found(m) => {
                self.searching = false;
                Outcome::Solved(Mode::Lost, m)
            }
            Step::Exhausted => {
                self.searching = false;
                Outcome::Failed(Failure::NoMatch)
            }
            Step::More => Outcome::Searching,
        }
    }

    /// Matches the centroids to the catalog predicted at `prior`.
    fn track(&mut self, prior: Prior) -> Option<Match> {
        let q = prior.q_st_eci;
        let sigma_c = self.sigma_rad();
        let gate =
            self.cfg.gate_k * libm::sqrt(sigma_c * sigma_c + prior.sigma_rad * prior.sigma_rad);
        let lis = *self.pyramid.config();
        let boresight = q.to_m3()[2];
        self.cone.clear();
        self.db
            .cone(boresight, lis.fov_radius_rad + gate, &mut self.cone);
        let cos_gate = libm::cos(gate);
        let cos_far = libm::cos(2.0 * gate);
        self.quest_obs.clear();
        for b in self.obs.iter().take(MAX_OBS) {
            // The nearest prediction inside the gate, unless a second one is
            // inside twice the gate.
            let (mut best, mut best_c, mut crowded) = (NO_ROW, cos_gate, false);
            for &row in &self.cone {
                let Some(star) = self.db.star(row) else {
                    continue;
                };
                let c = dot(*b, q.rotate(star.unit()));
                if c < cos_far {
                    continue;
                }
                if c >= cos_gate && c > best_c {
                    crowded |= best != NO_ROW;
                    best = row;
                    best_c = c;
                } else {
                    crowded = true;
                }
            }
            if best != NO_ROW && !crowded {
                let r = self.db.star(best)?.unit();
                self.quest_obs.push(Obs {
                    b: *b,
                    r,
                    sigma: sigma_c,
                });
            }
        }
        if self.quest_obs.len() < self.cfg.min_match {
            return None;
        }
        let sol = quest(&self.quest_obs, None).ok()?;
        let v = verify(&self.db, sol.q, &self.obs, &lis, &mut self.cone);
        // A wide gate can admit a wrong match that skews this first
        // solution, so only the count is checked before QUEST runs again on
        // everything verification matched.
        if v.n_match < lis.min_match {
            return None;
        }
        self.quest_obs.clear();
        for (b, row) in self.obs.iter().zip(v.rows).filter(|(_, r)| *r != NO_ROW) {
            self.quest_obs.push(Obs {
                b: *b,
                r: self.db.star(row)?.unit(),
                sigma: sigma_c,
            });
        }
        let sol = quest(&self.quest_obs, sol.rotated).ok()?;
        let v = verify(&self.db, sol.q, &self.obs, &lis, &mut self.cone);
        v.accepted.then_some(Match {
            rows: v.rows,
            n_obs: self.obs.len().min(MAX_OBS),
            n_match: v.n_match,
            q: sol.q,
            p_theta: sol.p_theta,
            rms_rad: v.rms_rad,
            p_false: v.p_false,
            triplets: 0,
        })
    }
}

/// The 1σ (rad) of an attitude covariance's worst axis.
pub fn sigma_of(p_theta: M3) -> f64 {
    libm::sqrt(lambda_max(p_theta).max(0.0))
}

#[cfg(test)]
mod tests;
