//! Shot noise, read noise and the ADC, per pixel from the stateless hash.
//!
//! Every pixel's Poisson draw takes one of three paths:
//! - a pixel with no star counts and no hot-pixel multiplier has the frame's
//!   sky-plus-dark mean, and draws from a [`PoissonTable`] built once per
//!   frame, with one uniform, while that mean is at most [`TABLE_MAX`];
//! - any other pixel below [`KNUTH_MAX`] uses Knuth's product of uniforms;
//! - the rest draw a normal of the same mean and variance, rounded.

use super::hash::{normal, uniform};
use super::splat::FIXED_SCALE;

/// Below this mean, per-pixel Poisson draws use Knuth's method.
pub const KNUTH_MAX: f32 = 30.0;
/// Knuth's draws per pixel are capped here.
pub const KNUTH_DRAWS: u32 = 128;
/// The largest sky mean the table serves.
pub const TABLE_MAX: f32 = 100.0;
/// Table entries: the CDF at 0 to `TABLE_LEN − 1` counts, which reaches past
/// `TABLE_MAX + 6√TABLE_MAX`.
pub const TABLE_LEN: usize = 168;
const NORMAL_DRAW: u32 = KNUTH_DRAWS;
const READ_DRAW: u32 = KNUTH_DRAWS + 1;

/// Everything `expose` needs that is the same for every pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exposure {
    /// Sky background (e⁻ per pixel).
    pub background_e: f32,
    /// Dark electrons per pixel before the hot-pixel multiplier.
    pub dark_e: f32,
    pub read_noise_e: f32,
    pub full_well_e: f32,
    /// Gain (milli-e⁻/DN), non-zero.
    pub gain_milli: u32,
    pub bias_dn: f32,
    /// The DN ceiling, `2^BITS − 1`.
    pub max_dn: u16,
    pub seed: u64,
}

impl Exposure {
    /// The mean of a pixel with no star counts and no hot-pixel multiplier.
    pub fn sky_e(&self) -> f32 {
        self.background_e + self.dark_e
    }

    fn dn(&self, e: f32) -> u16 {
        let dn = libm::floorf(e * 1000.0 / self.gain_milli as f32 + self.bias_dn + 0.5);
        dn.clamp(0.0, f32::from(self.max_dn)) as u16
    }
}

/// Reads out the accumulator into `out`: a Poisson draw of signal, sky and
/// dark, the full-well clamp, then read noise, gain, bias and the DN clamp.
/// Stops at the shortest of `acc`, `hot` and `out`.
pub fn expose(acc: &[u32], hot: &[f32], exp: &Exposure, out: &mut [u16]) {
    let sky = PoissonTable::new(exp.sky_e());
    let pixels = acc.iter().zip(hot).zip(out.iter_mut());
    for (p, ((&counts, &hot), out)) in (0u32..).zip(pixels) {
        let shot = match &sky {
            Some(sky) if counts == 0 && hot == 1.0 => sky.sample(uniform(p, 0, exp.seed)),
            _ => {
                let mean = counts as f32 / FIXED_SCALE + exp.background_e + exp.dark_e * hot;
                poisson(mean, p, exp.seed)
            }
        };
        let read = exp.read_noise_e * normal(p, READ_DRAW, exp.seed);
        *out = exp.dn(shot.min(exp.full_well_e) + read);
    }
}

/// A Poisson draw of mean `lambda`: Knuth below [`KNUTH_MAX`], else a normal
/// draw rounded to the nearest integer and floored at 0.
pub fn poisson(lambda: f32, pixel: u32, seed: u64) -> f32 {
    if lambda >= KNUTH_MAX {
        return rounded_normal(lambda, pixel, seed);
    }
    let limit = libm::expf(-lambda);
    let mut p = 1.0;
    for k in 0..KNUTH_DRAWS {
        p *= uniform(pixel, k, seed);
        if p <= limit {
            return k as f32;
        }
    }
    KNUTH_DRAWS as f32
}

fn rounded_normal(lambda: f32, pixel: u32, seed: u64) -> f32 {
    let x = lambda + libm::sqrtf(lambda) * normal(pixel, NORMAL_DRAW, seed);
    libm::floorf(x + 0.5).max(0.0)
}

/// The Poisson CDF of one mean, for drawing by inversion. Built in `f64` on
/// the CPU, so the GPU can upload the same `f32` values.
#[derive(Debug, Clone, PartialEq)]
pub struct PoissonTable {
    cdf: [f32; TABLE_LEN],
}

impl PoissonTable {
    /// `None` unless `0 ≤ lambda ≤ TABLE_MAX`.
    pub fn new(lambda: f32) -> Option<PoissonTable> {
        if !(0.0..=TABLE_MAX).contains(&lambda) {
            return None;
        }
        let lambda = f64::from(lambda);
        let mut pmf = libm::exp(-lambda);
        let mut sum = 0.0;
        let cdf = core::array::from_fn(|k| {
            sum += pmf;
            pmf *= lambda / (k + 1) as f64;
            sum as f32
        });
        Some(PoissonTable { cdf })
    }

    /// The count whose CDF step holds `u`; `TABLE_LEN` past the table's end.
    pub fn sample(&self, u: f32) -> f32 {
        self.cdf.partition_point(|&c| c < u) as f32
    }
}
