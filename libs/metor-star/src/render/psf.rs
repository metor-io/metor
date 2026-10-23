//! The pixel-integrated Gaussian PSF, in `f32` as the GPU evaluates it.

use core::f32::consts::SQRT_2;

/// The largest half-width `K` (px), reached at σ = 1.5.
pub const MAX_K: usize = 6;
pub const MAX_TAPS: usize = 2 * MAX_K + 1;
pub const MAX_SIGMA: f32 = 1.5;

/// `erf` by Abramowitz & Stegun 7.1.26: |ε| ≤ 1.5e-7 in exact arithmetic,
/// and under 5e-7 in `f32`.
pub fn erf_as(x: f32) -> f32 {
    const P: f32 = 0.327_591_1;
    const A: [f32; 5] = [
        0.254_829_6,
        -0.284_496_74,
        1.421_413_8,
        -1.453_152,
        1.061_405_4,
    ];
    let t = 1.0 / (1.0 + P * x.abs());
    let poly = t * (A[0] + t * (A[1] + t * (A[2] + t * (A[3] + t * A[4]))));
    let y = 1.0 - poly * libm::expf(-x * x);
    if x < 0.0 { -y } else { y }
}

/// A Gaussian PSF of width `sigma` (px), cut at `K = ceil(4σ)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psf {
    sigma: f32,
    k: usize,
}

impl Psf {
    /// `None` unless `0 < sigma ≤ MAX_SIGMA`.
    pub fn new(sigma: f32) -> Option<Psf> {
        let ok = sigma > 0.0 && sigma <= MAX_SIGMA;
        ok.then(|| Psf {
            sigma,
            k: libm::ceilf(4.0 * sigma) as usize,
        })
    }

    pub fn sigma(&self) -> f32 {
        self.sigma
    }

    /// The number of pixels the PSF spans on each axis, `2K + 1`.
    pub fn taps(&self) -> usize {
        2 * self.k + 1
    }

    /// Fills `out[..taps]` with the weights of the pixels from the returned
    /// first index on, for a star at `x0`. Pixel `i` covers `[i, i + 1)` and
    /// gets `erf((i + 1 − x0)/√2σ) − erf((i − x0)/√2σ)`, normalised so the
    /// taps sum to 1.
    pub fn weights(&self, x0: f32, out: &mut [f32; MAX_TAPS]) -> i32 {
        let first = libm::floorf(x0) as i32 - self.k as i32;
        let s = 1.0 / (SQRT_2 * self.sigma);
        let start = erf_as((first as f32 - x0) * s);
        let mut lo = start;
        for (i, w) in (1..).zip(out[..self.taps()].iter_mut()) {
            let hi = erf_as(((first + i) as f32 - x0) * s);
            *w = hi - lo;
            lo = hi;
        }
        let total = lo - start;
        out[..self.taps()].iter_mut().for_each(|w| *w /= total);
        first
    }

    /// The fraction of the flux in the peak pixel of a pixel-centred star.
    pub fn peak_fraction(&self) -> f32 {
        let w = erf_as(0.5 / (SQRT_2 * self.sigma));
        w * w
    }
}
