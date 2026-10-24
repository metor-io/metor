//! The `star_sim` system's params. Every field has a default, a 20° tracker
//! with a 25 mm aperture on the committed G ≤ 8 catalog. Python sends an
//! unset nested table as null, so those are `Option`s.

use metor_star::render::photometry::{G_TO_V, V0_PH_S_M2};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StarSimParams {
    /// Frame rate (Hz) in sim time.
    #[serde(default = "default_rate_hz")]
    pub rate_hz: f64,
    /// Exposure time (s).
    #[serde(default = "default_exposure_s")]
    pub exposure_s: f64,
    /// Keys the hot-pixel map and, with the frame id, each frame's noise.
    #[serde(default)]
    pub seed: u64,
    /// A `starcat.bin` path; the committed G ≤ 8 catalog when absent.
    #[serde(default)]
    pub catalog: Option<String>,
    /// Stars at or fainter than this G are skipped.
    #[serde(default = "default_mag_limit")]
    pub mag_limit: f32,
    #[serde(default)]
    pub camera: Option<StarCamParams>,
    #[serde(default)]
    pub optics: Option<StarOpticsParams>,
    #[serde(default)]
    pub sensor: Option<StarSensorParams>,
    /// The share of pixels with extra dark current.
    #[serde(default = "default_hot_pixel_fraction")]
    pub hot_pixel_fraction: f32,
    /// The largest hot-pixel dark multiplier; multipliers are log-uniform
    /// from 1.
    #[serde(default = "default_hot_pixel_max")]
    pub hot_pixel_max: f32,
    /// Shift stars by the observer's orbital velocity.
    #[serde(default)]
    pub aberration: bool,
    /// The most motion-blur sub-steps per frame, 1 to 32.
    #[serde(default = "default_max_substeps")]
    pub max_substeps: u32,
}

/// The lens and its mount. The sensor is the record's 1024 × 1024.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StarCamParams {
    /// Focal length (m).
    pub f_m: f64,
    /// Pixel pitch (m).
    pub pitch_m: f64,
    /// Principal point (px).
    pub cx: f64,
    pub cy: f64,
    /// Brown–Conrady radial (1/m², 1/m⁴) and tangential (1/m) terms.
    pub k1: f64,
    pub k2: f64,
    pub p1: f64,
    pub p2: f64,
    /// Mount, `[x, y, z, w]`: `b_body = A(q_b_st) b_st`.
    pub q_b_st: [f64; 4],
}

impl Default for StarCamParams {
    fn default() -> Self {
        StarCamParams {
            f_m: 16e-3,
            pitch_m: 5.5e-6,
            cx: 512.0,
            cy: 512.0,
            k1: 0.0,
            k2: 0.0,
            p1: 0.0,
            p2: 0.0,
            q_b_st: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

/// Light collection, from photons to electrons.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StarOpticsParams {
    /// Aperture diameter (m).
    pub aperture_m: f32,
    /// Optics transmission times quantum efficiency.
    pub throughput: f32,
    /// Photon flux of a band magnitude 0 star (ph/s/m²).
    pub flux0_ph_s_m2: f32,
    /// `m_band − G` as ascending powers of `BP − RP`; Johnson V by default.
    pub colour: [f32; 4],
}

impl Default for StarOpticsParams {
    fn default() -> Self {
        StarOpticsParams {
            aperture_m: 0.025,
            throughput: 0.5,
            flux0_ph_s_m2: V0_PH_S_M2,
            colour: G_TO_V,
        }
    }
}

/// The detector.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StarSensorParams {
    pub psf_sigma_px: f32,
    pub full_well_e: f32,
    pub read_noise_e: f32,
    /// Dark current at `dark_t0_c` (e⁻/s); it doubles every 6.5 °C.
    pub dark_e_s: f32,
    pub dark_t0_c: f32,
    pub temp_c: f32,
    /// Gain (milli-e⁻/DN).
    pub gain_milli: u32,
    pub bias_dn: f32,
    /// Sky background (mag/arcsec²), or none.
    pub zodi_mag_arcsec2: Option<f32>,
}

impl Default for StarSensorParams {
    fn default() -> Self {
        StarSensorParams {
            psf_sigma_px: 1.0,
            full_well_e: 20_000.0,
            read_noise_e: 5.0,
            dark_e_s: 10.0,
            dark_t0_c: 20.0,
            temp_c: 20.0,
            gain_milli: 5_000,
            bias_dn: 100.0,
            zodi_mag_arcsec2: Some(22.5),
        }
    }
}

fn default_rate_hz() -> f64 {
    10.0
}

fn default_exposure_s() -> f64 {
    0.1
}

fn default_mag_limit() -> f32 {
    12.0
}

fn default_hot_pixel_fraction() -> f32 {
    1e-4
}

fn default_hot_pixel_max() -> f32 {
    50.0
}

fn default_max_substeps() -> u32 {
    16
}

impl StarSimParams {
    pub fn camera(&self) -> StarCamParams {
        self.camera.clone().unwrap_or_default()
    }

    pub fn optics(&self) -> StarOpticsParams {
        self.optics.clone().unwrap_or_default()
    }

    pub fn sensor(&self) -> StarSensorParams {
        self.sensor.clone().unwrap_or_default()
    }
}
