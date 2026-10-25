//! The `star_sim` system's params. The camera, optics and sensor are fixed:
//! `metor_star::render`'s 20° tracker with a 25 mm aperture.

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
    /// Shift stars by the observer's orbital velocity.
    #[serde(default)]
    pub aberration: bool,
}

fn default_rate_hz() -> f64 {
    10.0
}

fn default_exposure_s() -> f64 {
    0.1
}
