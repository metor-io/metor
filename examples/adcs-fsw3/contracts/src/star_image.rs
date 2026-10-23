//! The star simulator's image and the truth it was rendered from.
//!
//! Pixel centres are at integer + ½: pixel `i` covers `[i, i + 1)`.

use metor_fsw_3::{FrameArray, FrameImage, Mono16, Timestamp};
use zerocopy::{AllocError, FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::{Quat, V3};

/// The most truth centroids a [`StarTruth`] carries.
pub const MAX_TRUTH: usize = 32;
/// The `cat_id` of a star with no Hipparcos number.
pub const NO_CAT_ID: u32 = u32::MAX;
/// [`StarTruth::blinded`] bit: the sun is within the exclusion angle.
pub const BLINDED_SUN: u32 = 1 << 0;
/// [`StarTruth::blinded`] bit: the Earth's disk intersects the field of view.
pub const BLINDED_EARTH: u32 = 1 << 1;
/// [`StarTruth::blinded`] bit: the Moon's disk intersects the field of view.
pub const BLINDED_MOON: u32 = 1 << 2;

/// The star camera's 12-bit, 1024×1024 detector.
pub type StarPixels = FrameImage<Mono16<12>, 1024, 1024>;

/// One star camera exposure.
///
/// It is 2 MiB, so build it with [`StarImage::new_box`]; never move one by value.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
#[repr(C)]
#[frame(name = "star_image")]
pub struct StarImage {
    /// Mid-exposure time.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    pub exposure_us: u32,
    /// Gain in thousandths of an electron per DN (milli-e⁻/DN).
    pub gain_milli: u32,
    pub pixels: StarPixels,
}

impl StarImage {
    /// A zeroed image on the heap.
    pub fn new_box() -> Result<Box<Self>, AllocError> {
        Self::new_box_zeroed()
    }
}

/// What the star simulator rendered into the [`StarImage`] with the same
/// `frame_id`.
///
/// The centroid fields match `StarCentroids`, so an overlay binds to both the
/// same way. Entries past `count` are zero, with `cat_id = NO_CAT_ID`, as
/// [`StarTruth::default`] builds them; `new_zeroed` does not.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "star_truth")]
pub struct StarTruth {
    /// Mid-exposure time.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    /// Attitude, sensor←ECI: `q_b_eci ⊗ q_b_st⁻¹` (Hamilton, nox's
    /// product), with `q_b_st` the camera mount (body←`st`).
    pub q_cam_eci: Quat,
    /// Sensor-frame rate (rad/s).
    pub omega_cam: V3,
    /// Stars drawn into the image.
    pub n_rendered: u32,
    /// Stars bright enough to detect.
    pub n_detectable: u32,
    /// `BLINDED_SUN | BLINDED_EARTH | BLINDED_MOON` bits.
    pub blinded: u32,
    _pad: u32,
    /// Valid entries in `u`, `v`, `mag` and `cat_id`, at most [`MAX_TRUTH`].
    pub count: u64,
    /// Centroid column (px).
    pub u: FrameArray<f32, MAX_TRUTH>,
    /// Centroid row (px).
    pub v: FrameArray<f32, MAX_TRUTH>,
    /// Band magnitude.
    pub mag: FrameArray<f32, MAX_TRUTH>,
    /// Hipparcos number, or [`NO_CAT_ID`].
    pub cat_id: FrameArray<u32, MAX_TRUTH>,
}

impl Default for StarTruth {
    /// No centroids, every `cat_id` [`NO_CAT_ID`], everything else zero.
    fn default() -> Self {
        let mut truth = Self::new_zeroed();
        truth.cat_id = FrameArray([NO_CAT_ID; MAX_TRUTH]);
        truth
    }
}

#[cfg(test)]
mod tests;
