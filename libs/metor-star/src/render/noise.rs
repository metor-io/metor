//! The per-frame noise inputs the GPU's `expose` kernel reads.

/// The largest sky mean the table serves; above it the GPU draws per pixel.
pub const TABLE_MAX: f32 = 100.0;
/// Table entries: the CDF at 0 to `TABLE_LEN − 1` counts, which reaches past
/// `TABLE_MAX + 6√TABLE_MAX`.
pub const TABLE_LEN: usize = 168;

/// Everything the readout needs that is the same for every pixel.
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
}

/// The Poisson CDF of one mean, built in `f64`, for the GPU to draw star-free
/// pixels by inversion with one uniform.
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

    /// The CDF at 0 to `TABLE_LEN − 1` counts.
    pub fn cdf(&self) -> &[f32; TABLE_LEN] {
        &self.cdf
    }
}
