//! Lost-in-space identification: Pyramid (Mortari et al. 2004) over the
//! onboard DB's k-vector, then verification against every observed star.
//!
//! [`Pyramid`] is resumable: [`Pyramid::step`] tries at most `budget`
//! triplets and returns [`Step::More`] when it stopped early, so a tracker
//! can spread one hard frame over several cycles.

pub mod pyramid;
pub mod verify;

pub use pyramid::{Pyramid, Step};
pub use verify::{Verdict, binomial_sf, verify};

use crate::geom::Quat;

/// The most observations a [`Match`] carries.
pub const MAX_OBS: usize = 32;
/// Marks an observation with no catalog star.
pub const NO_ROW: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LisConfig {
    /// Observations the triplet search draws from, brightest first.
    pub search_obs: usize,
    /// Pair-angle tolerance (rad): `3√2 σ_b` plus calibration error.
    pub eps_rad: f64,
    /// An edge with more catalog pairs than this skips its triplet.
    pub max_candidates: usize,
    /// Triplets per frame before the search gives up.
    pub max_triplets: u32,
    /// The cone (rad) about the boresight that verification searches, the
    /// field's half-diagonal.
    pub fov_radius_rad: f64,
    /// How far (rad) an observation may sit from its catalog star.
    pub match_rad: f64,
    /// One observation's σ (rad), for the residual test and QUEST. The
    /// tracker sets it from its centroid σ.
    pub sigma_rad: f64,
    pub min_match: usize,
    /// The largest chance that the matches are coincidence.
    pub p_false_max: f64,
}

impl Default for LisConfig {
    /// For the 20°, 1024² camera at about 0.2 px centroid error.
    fn default() -> LisConfig {
        LisConfig {
            search_obs: 16,
            eps_rad: 3e-5,
            max_candidates: 1000,
            max_triplets: 2000,
            fov_radius_rad: 14.2f64.to_radians(),
            match_rad: 7e-5,
            sigma_rad: 1e-5,
            min_match: 4,
            p_false_max: 1e-9,
        }
    }
}

/// An identified frame: the DB row of each observation (or [`NO_ROW`]) and
/// the attitude, `b_st = A(q) r_eci`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Match {
    pub rows: [u16; MAX_OBS],
    pub n_obs: usize,
    pub n_match: usize,
    pub q: Quat,
    /// The attitude covariance (rad²) from QUEST.
    pub p_theta: [[f64; 3]; 3],
    /// Residual RMS (rad) of the matches.
    pub rms_rad: f64,
    pub p_false: f64,
    /// Triplets tried before the match.
    pub triplets: u32,
}

#[cfg(test)]
mod tests;
