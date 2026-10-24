//! The image star tracker: centroids each `StarImage`, identifies the stars
//! (tracking from the last solution and the gyro, or lost-in-space), and
//! writes the matched vectors nav fuses, its own solution, and the centroids
//! for the panel overlay.

use adcs_contracts::{
    MAX_STARS, StarCentroids, StarImage, StarMode, StarSolution, StarStatus, StarTrackerParams,
    StarVectors, UNIDENTIFIED,
};
use metor_fsw_3::{Input, Output, Timestamp, system};
use metor_star::camera::{Camera, Intrinsics};
use metor_star::catalog::Db;
use metor_star::geom::{self, M3, Quat, V3};
use metor_star::image::ImageConfig;
use metor_star::lis::{LisConfig, Match, NO_ROW};
use metor_star::tracker::{Failure, Mode, Outcome, Prior, TrackConfig, Tracker, predict, sigma_of};
use nox::{Matrix3, Quaternion, Vector};

/// The committed onboard DB (`star-db build`).
static DB: &[u8] = include_bytes!("../../../../../libs/metor-star/data/stars-u2000.stdb");

/// The sensor size of `StarImage`.
const SIZE: u32 = 1024;
/// A prior older than this (s) is not used.
const MAX_PRIOR_AGE_S: f64 = 2.0;
/// Attitude error the gyro adds per second of propagation (rad/s): bias and
/// noise, generously.
const GYRO_DRIFT_RAD_S: f64 = 2e-4;

/// The tracker's last solution, sensor←ECI.
#[derive(Debug, Clone, Copy)]
struct Last {
    q_st_eci: Quat,
    sigma_rad: f64,
    timestamp: Timestamp,
}

/// One frame's three outputs.
#[derive(Debug, Clone, Copy)]
pub struct Frames {
    pub vectors: Option<StarVectors>,
    pub solution: StarSolution,
    pub centroids: StarCentroids,
}

pub struct StarTracker {
    tracker: Tracker<'static>,
    q_b_st: Quat,
    budget: u32,
    last: Option<Last>,
    /// The frame being searched, `(timestamp, frame_id)`.
    pending: Option<(Timestamp, u64)>,
    last_frame_id: Option<u64>,
    /// Body rate from the gyro (rad/s).
    gyro_b: V3,
    /// Frames dropped because a new one arrived mid-search.
    pub dropped: u64,
}

impl StarTracker {
    pub fn new(params: StarTrackerParams) -> StarTracker {
        let q_b_st = Quat(params.q_b_st).normalized().unwrap_or(Quat::IDENTITY);
        let cam = Camera {
            intr: Intrinsics {
                f_m: params.f_m,
                pitch_m: params.pitch_m,
                cx: params.cx,
                cy: params.cy,
                k1: params.k1,
                k2: params.k2,
                p1: params.p1,
                p2: params.p2,
                width: SIZE,
                height: SIZE,
            },
            q_b_st,
        };
        // PANIC Safety: the committed DB parses (metor-star tests it), and a
        // 1024² sensor with the default image config is valid.
        let db = Db::parse(DB).expect("the committed star DB parses");
        let track = TrackConfig {
            sigma_px: params.sigma_px,
            ..TrackConfig::default()
        };
        let lis = LisConfig {
            fov_radius_rad: cam.half_diagonal_rad(),
            ..LisConfig::default()
        };
        let tracker = Tracker::new(db, cam, ImageConfig::default(), lis, track)
            .expect("a valid image config");
        StarTracker {
            tracker,
            q_b_st,
            budget: params.budget.max(1),
            last: None,
            pending: None,
            last_frame_id: None,
            gyro_b: [0.0; 3],
            dropped: 0,
        }
    }

    pub fn set_gyro(&mut self, gyro_b: V3) {
        self.gyro_b = gyro_b;
    }

    /// The last solution turned to `t` by the gyro, if recent.
    fn prior(&self, t: Timestamp) -> Option<Prior> {
        let last = self.last?;
        let dt = (t.0 - last.timestamp.0) as f64 * 1e-6;
        if !(0.0..=MAX_PRIOR_AGE_S).contains(&dt) {
            return None;
        }
        let omega_st = self.tracker.camera().body_to_st(self.gyro_b);
        Some(Prior {
            q_st_eci: predict(last.q_st_eci, omega_st, dt),
            sigma_rad: last.sigma_rad + GYRO_DRIFT_RAD_S * dt + 1e-5,
        })
    }

    /// Runs one cycle: starts `frame` if it is new, or resumes the search.
    /// Returns the outputs once a frame resolves.
    pub fn step(&mut self, frame: Option<(Timestamp, u64, &[u16])>) -> Option<Frames> {
        let outcome = match frame {
            Some((t, id, px)) if self.last_frame_id != Some(id) => {
                self.last_frame_id = Some(id);
                if self.pending.is_some() {
                    self.dropped += 1;
                }
                self.pending = Some((t, id));
                let prior = self.prior(t);
                self.tracker.process(px, prior, self.budget)
            }
            _ if self.pending.is_some() => self.tracker.resume(self.budget),
            _ => return None,
        };
        if outcome == Outcome::Searching {
            return None;
        }
        let (t, id) = self.pending.take()?;
        Some(self.frames(t, id, outcome))
    }

    fn frames(&mut self, t: Timestamp, id: u64, outcome: Outcome) -> Frames {
        let mut centroids = StarCentroids::empty(t, id);
        let rows = match outcome {
            Outcome::Solved(_, m) => m.rows,
            _ => [NO_ROW; metor_star::lis::MAX_OBS],
        };
        let db = self.tracker.db();
        let hip = |k: usize| {
            rows.get(k)
                .filter(|r| **r != NO_ROW)
                .and_then(|r| db.hip(*r))
                .unwrap_or(UNIDENTIFIED)
        };
        for (k, c) in self.tracker.centroids().iter().enumerate() {
            centroids.push(c.u as f32, c.v as f32, c.flux as f32, hip(k));
        }
        let n_centroids = self.tracker.centroids().len() as u16;
        let (mode, m) = match outcome {
            Outcome::Solved(mode, m) => (mode, m),
            Outcome::Failed(why) => {
                self.last = None;
                let status = match why {
                    Failure::NoStars | Failure::Flooded => StarStatus::NoStars,
                    Failure::NoMatch => StarStatus::NoMatch,
                };
                let mut solution = StarSolution::new(t, id, StarMode::Lost, status);
                solution.n_centroids = n_centroids;
                return Frames {
                    vectors: None,
                    solution,
                    centroids,
                };
            }
            Outcome::Searching => unreachable!("resolved above"),
        };
        self.last = Some(Last {
            q_st_eci: m.q,
            sigma_rad: sigma_of(m.p_theta),
            timestamp: t,
        });
        let mode = match mode {
            Mode::Lost => StarMode::Lost,
            Mode::Tracking => StarMode::Tracking,
        };
        Frames {
            vectors: Some(self.vectors(t, id, &m)),
            solution: self.solution(t, id, mode, &m, n_centroids),
            centroids,
        }
    }

    fn vectors(&self, t: Timestamp, id: u64, m: &Match) -> StarVectors {
        let mut out = StarVectors::empty(t, id);
        let (db, cam) = (self.tracker.db(), self.tracker.camera());
        let sigma = self.tracker.sigma_rad();
        for (b, row) in self.tracker.obs().iter().zip(m.rows).take(MAX_STARS) {
            let (Some(star), Some(hip)) = (db.star(row), db.hip(row)) else {
                continue;
            };
            out.push(cam.st_to_body(*b), star.unit(), sigma, hip);
        }
        out
    }

    fn solution(&self, t: Timestamp, id: u64, mode: StarMode, m: &Match, n: u16) -> StarSolution {
        let mut out = StarSolution::new(t, id, mode, StarStatus::Valid);
        // `A(q_b_eci) = A(q_b_st) A(q_st_eci)`, and `A(p ⊗ q) = A(q) A(p)`.
        let q_b_eci = (m.q * self.q_b_st).normalized().unwrap_or(m.q);
        out.q_b_eci = Quaternion(Vector::from_buf(q_b_eci.0));
        out.p_theta = Matrix3::from_buf(to_body(self.q_b_st.to_m3(), m.p_theta));
        out.residual_rms = m.rms_rad;
        out.p_false = m.p_false;
        out.n_centroids = n;
        out.n_matched = m.n_match as u16;
        out
    }
}

/// `R P Rᵀ`.
fn to_body(r: M3, p: M3) -> M3 {
    geom::mat_mul(geom::mat_mul(r, p), geom::transpose(r))
}

#[system]
/// Identifies the stars of each new `StarImage`, spreading a lost-in-space
/// search over cycles within `budget` triplets each, and writes its
/// vectors, solution and centroids once the frame resolves. Vectors are
/// written only for a verified solution.
pub fn star_tracker(
    #[state] state: &mut StarTracker,
    image: &mut Input<StarImage>,
    sensors: &mut Input<adcs_contracts::Sensors>,
    star_vectors: &mut Output<StarVectors>,
    star_solution: &mut Output<StarSolution>,
    star_centroids: &mut Output<StarCentroids>,
) {
    if let Ok(Some(s)) = sensors.latest() {
        state.set_gyro(s.gyro_b.into_buf());
    }
    let latest = image.latest();
    let frame = match &latest {
        Ok(Some(img)) => Some((img.timestamp, img.frame_id, img.pixels.pixels())),
        _ => None,
    };
    let Some(out) = state.step(frame) else {
        return;
    };
    if let Some(v) = &out.vectors {
        let _ = star_vectors.write(v);
    }
    let _ = star_solution.write(&out.solution);
    let _ = star_centroids.write(&out.centroids);
}

#[cfg(test)]
mod tests;
