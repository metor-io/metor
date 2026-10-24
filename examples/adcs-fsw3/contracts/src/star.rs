//! The star tracker's frames: the matched vectors nav fuses, and the
//! tracker's own attitude solution for telemetry.

use metor_fsw_3::{FrameArray, Timestamp};
use nox::{ArrayRepr, Matrix, Matrix3, Vector};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{Quat, UnknownCode, V3};

/// Rows in a [`StarVectors`] frame.
pub const MAX_STARS: usize = 32;

/// The catalog id of a star the tracker did not identify.
pub const UNIDENTIFIED: u32 = u32::MAX;

/// Matched star vectors from one image. Rows at or past `count` are zero.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "star_vectors")]
pub struct StarVectors {
    /// Mid-exposure time of the image.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    pub count: u64,
    /// Measured unit vectors in the body frame, one per row.
    pub b_body: Matrix<f64, MAX_STARS, 3, ArrayRepr>,
    /// Their catalog unit vectors in ECI, one per row.
    pub r_eci: Matrix<f64, MAX_STARS, 3, ArrayRepr>,
    /// Per-star 1σ (rad).
    pub sigma: Vector<f64, MAX_STARS, ArrayRepr>,
    /// Hipparcos numbers, or [`UNIDENTIFIED`].
    pub cat_id: FrameArray<u32, MAX_STARS>,
}

impl StarVectors {
    /// An empty frame.
    pub fn empty(timestamp: Timestamp, frame_id: u64) -> StarVectors {
        StarVectors {
            timestamp,
            frame_id,
            count: 0,
            b_body: Matrix::zeros(),
            r_eci: Matrix::zeros(),
            sigma: Vector::zeros(),
            cat_id: FrameArray([0; MAX_STARS]),
        }
    }

    /// The first `count.min(MAX_STARS)` rows as `(b_body, r_eci, σ)`.
    pub fn obs(&self) -> impl Iterator<Item = (V3, V3, f64)> + '_ {
        let n = (self.count as usize).min(MAX_STARS);
        let b = self.b_body.inner().buf;
        let r = self.r_eci.inner().buf;
        let sigma = self.sigma.inner().buf;
        (0..n).map(move |i| (V3::from_buf(b[i]), V3::from_buf(r[i]), sigma[i]))
    }

    /// Appends a row, or returns false when the frame is full.
    pub fn push(&mut self, b_body: [f64; 3], r_eci: [f64; 3], sigma: f64, cat_id: u32) -> bool {
        let i = self.count as usize;
        if i >= MAX_STARS {
            return false;
        }
        self.b_body.inner_mut().buf[i] = b_body;
        self.r_eci.inner_mut().buf[i] = r_eci;
        self.sigma.inner_mut().buf[i] = sigma;
        self.cat_id.0[i] = cat_id;
        self.count += 1;
        true
    }
}

/// The tracker's centroids from one image, for the panel overlay. Entries
/// past `count` are zero with `cat_id` [`UNIDENTIFIED`]. Field names match
/// [`crate::StarTruth`] so one overlay binds to both; renaming one breaks the
/// overlay preset.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "star_centroids")]
pub struct StarCentroids {
    /// Mid-exposure time of the image.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    pub count: u64,
    /// Centroid column (px), pixel centres at integer + ½.
    pub u: FrameArray<f32, MAX_STARS>,
    /// Centroid row (px).
    pub v: FrameArray<f32, MAX_STARS>,
    /// Background-subtracted flux (DN).
    pub flux: FrameArray<f32, MAX_STARS>,
    /// Hipparcos number, or [`UNIDENTIFIED`].
    pub cat_id: FrameArray<u32, MAX_STARS>,
}

impl StarCentroids {
    pub fn empty(timestamp: Timestamp, frame_id: u64) -> StarCentroids {
        StarCentroids {
            timestamp,
            frame_id,
            count: 0,
            u: FrameArray([0.0; MAX_STARS]),
            v: FrameArray([0.0; MAX_STARS]),
            flux: FrameArray([0.0; MAX_STARS]),
            cat_id: FrameArray([UNIDENTIFIED; MAX_STARS]),
        }
    }

    /// Appends a centroid, or returns false when the frame is full.
    pub fn push(&mut self, u: f32, v: f32, flux: f32, cat_id: u32) -> bool {
        let i = self.count as usize;
        if i >= MAX_STARS {
            return false;
        }
        self.u.0[i] = u;
        self.v.0[i] = v;
        self.flux.0[i] = flux;
        self.cat_id.0[i] = cat_id;
        self.count += 1;
        true
    }
}

/// How the tracker found its stars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StarMode {
    /// From truth, with no image.
    Ideal = 0,
    Lost = 1,
    Tracking = 2,
}

impl TryFrom<u8> for StarMode {
    type Error = UnknownCode;

    fn try_from(code: u8) -> Result<Self, UnknownCode> {
        match code {
            0 => Ok(Self::Ideal),
            1 => Ok(Self::Lost),
            2 => Ok(Self::Tracking),
            _ => Err(UnknownCode(code)),
        }
    }
}

/// Whether the tracker's solution is usable, and why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StarStatus {
    Valid = 0,
    NoStars = 1,
    NoMatch = 2,
    VerifyFailed = 3,
    NoConvergence = 4,
    Degenerate = 5,
}

impl TryFrom<u8> for StarStatus {
    type Error = UnknownCode;

    fn try_from(code: u8) -> Result<Self, UnknownCode> {
        match code {
            0 => Ok(Self::Valid),
            1 => Ok(Self::NoStars),
            2 => Ok(Self::NoMatch),
            3 => Ok(Self::VerifyFailed),
            4 => Ok(Self::NoConvergence),
            5 => Ok(Self::Degenerate),
            _ => Err(UnknownCode(code)),
        }
    }
}

/// The tracker's own attitude solution, for telemetry. Nav does not read it.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "star_solution")]
pub struct StarSolution {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    /// Attitude, body←ECI.
    pub q_b_eci: Quat,
    /// Attitude error covariance in the body frame (rad²).
    pub p_theta: Matrix3<f64, ArrayRepr>,
    /// RMS of the matched stars' residuals (rad).
    pub residual_rms: f64,
    /// Probability that the match is false.
    pub p_false: f64,
    /// The [`StarMode`]'s code.
    mode: u8,
    /// The [`StarStatus`]'s code.
    status: u8,
    pub n_centroids: u16,
    pub n_matched: u16,
    _pad: [u8; 2],
}

impl StarSolution {
    /// A solution with no attitude: identity, zero covariance.
    pub fn new(timestamp: Timestamp, frame_id: u64, mode: StarMode, status: StarStatus) -> Self {
        StarSolution {
            timestamp,
            frame_id,
            q_b_eci: Quat::identity(),
            p_theta: Matrix3::zeros(),
            residual_rms: 0.0,
            p_false: 0.0,
            mode: mode as u8,
            status: status as u8,
            n_centroids: 0,
            n_matched: 0,
            _pad: [0; 2],
        }
    }

    pub fn mode(&self) -> Result<StarMode, UnknownCode> {
        StarMode::try_from(self.mode)
    }

    pub fn status(&self) -> Result<StarStatus, UnknownCode> {
        StarStatus::try_from(self.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for mode in [StarMode::Ideal, StarMode::Lost, StarMode::Tracking] {
            assert_eq!(StarMode::try_from(mode as u8), Ok(mode));
        }
        for status in [
            StarStatus::Valid,
            StarStatus::NoStars,
            StarStatus::NoMatch,
            StarStatus::VerifyFailed,
            StarStatus::NoConvergence,
            StarStatus::Degenerate,
        ] {
            assert_eq!(StarStatus::try_from(status as u8), Ok(status));
        }
        assert_eq!(StarMode::try_from(3), Err(UnknownCode(3)));
        assert_eq!(StarStatus::try_from(6), Err(UnknownCode(6)));
    }

    #[test]
    fn rows_past_count_are_not_observations() {
        let mut sv = StarVectors::empty(Timestamp(0), 1);
        assert_eq!(sv.obs().count(), 0);
        assert!(sv.push([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], 3e-5, 7));
        let rows: Vec<_> = sv.obs().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.into_buf(), [0.0, 0.0, 1.0]);
        assert_eq!(rows[0].2, 3e-5);
    }

    #[test]
    fn count_is_clamped_and_push_stops_when_full() {
        let mut sv = StarVectors::empty(Timestamp(0), 1);
        for i in 0..MAX_STARS {
            assert!(sv.push([0.0, 0.0, 1.0], [0.0, 0.0, 1.0], 1.0, i as u32));
        }
        assert!(!sv.push([0.0, 0.0, 1.0], [0.0, 0.0, 1.0], 1.0, 99));
        sv.count = 1000;
        assert_eq!(sv.obs().count(), MAX_STARS);
    }
}
