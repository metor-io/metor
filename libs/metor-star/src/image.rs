//! Star extraction from a mono frame: a block background, a per-block
//! threshold, 8-connected labelling over runs, and centre-of-gravity
//! centroids. Every buffer is sized in [`Extractor::new`]; [`Extractor::run`]
//! allocates nothing and its work is bounded by the frame size and
//! `max_runs`.
//!
//! Pixel `(i, j)` covers `[i, i + 1) × [j, j + 1)`, as in [`crate::camera`],
//! so a centroid of a single pixel is at `(i + 0.5, j + 0.5)`.

pub mod background;
pub mod label;

use alloc::vec;
use alloc::vec::Vec;

use background::Background;
use label::{Blob, Labeller};

/// The most centroids [`Extractor::run`] returns.
pub const MAX_CENTROIDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageConfig {
    /// Background block edge (px).
    pub block: u16,
    /// Subsample stride inside a block for the median.
    pub step: u8,
    /// Threshold above the block median, in units of the frame's σ.
    pub k_sigma: f32,
    pub min_area: u16,
    pub max_area: u16,
    /// A blob with any pixel at or above this (DN) is rejected.
    pub saturation: u16,
    /// The largest ratio of a blob's principal axes.
    pub max_axis_ratio: f32,
    /// Runs past this reject the frame as flooded (the Sun, the Earth).
    pub max_runs: u32,
    /// Centroids kept, brightest first, at most [`MAX_CENTROIDS`].
    pub keep: u8,
}

impl Default for ImageConfig {
    fn default() -> ImageConfig {
        ImageConfig {
            block: 32,
            step: 2,
            k_sigma: 5.0,
            min_area: 3,
            max_area: 100,
            saturation: 4095,
            max_axis_ratio: 3.0,
            max_runs: 16_384,
            keep: 20,
        }
    }
}

/// A star image's centre of gravity (px) and its background-subtracted
/// flux (DN).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Centroid {
    pub u: f64,
    pub v: f64,
    pub flux: f64,
    pub area: u16,
    pub peak: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// The pixel slice is not `width × height`.
    Size,
    /// Width, height, block or step is zero.
    Config,
    /// More than `max_runs` runs: the frame is flooded and not processed.
    TooManyRuns,
}

#[derive(Debug, Clone)]
pub struct Extractor {
    width: usize,
    height: usize,
    cfg: ImageConfig,
    background: Background,
    labeller: Labeller,
    out: Vec<Centroid>,
}

impl Extractor {
    pub fn new(width: u32, height: u32, cfg: ImageConfig) -> Result<Extractor, ImageError> {
        let (width, height) = (width as usize, height as usize);
        if width == 0 || height == 0 || cfg.block == 0 || cfg.step == 0 {
            return Err(ImageError::Config);
        }
        Ok(Extractor {
            width,
            height,
            background: Background::new(width, height, cfg.block, cfg.step),
            labeller: Labeller::new(cfg.max_runs),
            out: vec![
                Centroid {
                    u: 0.0,
                    v: 0.0,
                    flux: 0.0,
                    area: 0,
                    peak: 0,
                };
                MAX_CENTROIDS
            ],
            cfg,
        })
    }

    pub fn config(&self) -> &ImageConfig {
        &self.cfg
    }

    /// The frame's background σ (DN) from the last [`run`](Self::run).
    pub fn sigma(&self) -> f32 {
        self.background.sigma()
    }

    /// The centroids of `px` (row-major), brightest first.
    pub fn run(&mut self, px: &[u16]) -> Result<&[Centroid], ImageError> {
        if px.len() != self.width * self.height {
            return Err(ImageError::Size);
        }
        self.background.estimate(px, self.cfg.k_sigma);
        self.labeller.label(px, self.width, &self.background)?;
        let mut n = 0;
        for blob in self.labeller.blobs() {
            if let Some(c) = accept(blob, &self.cfg) {
                if n < self.out.len() {
                    self.out[n] = c;
                    n += 1;
                } else {
                    // Full: replace the faintest if this one is brighter.
                    let (i, faintest) = faintest(&self.out);
                    if c.flux > faintest {
                        self.out[i] = c;
                    }
                }
            }
        }
        let kept = &mut self.out[..n];
        kept.sort_unstable_by(|a, b| b.flux.total_cmp(&a.flux));
        let keep = usize::from(self.cfg.keep).min(n);
        Ok(&self.out[..keep])
    }
}

fn faintest(out: &[Centroid]) -> (usize, f64) {
    out.iter()
        .enumerate()
        .fold((0, f64::INFINITY), |(i, f), (j, c)| {
            if c.flux < f { (j, c.flux) } else { (i, f) }
        })
}

/// The centroid of `blob`, or `None` when its area, peak or shape rule it out.
pub fn accept(blob: &Blob, cfg: &ImageConfig) -> Option<Centroid> {
    let area = blob.area;
    if area < u32::from(cfg.min_area) || area > u32::from(cfg.max_area) {
        return None;
    }
    if blob.peak >= cfg.saturation || blob.w <= 0.0 {
        return None;
    }
    let (cx, cy) = (blob.wx / blob.w, blob.wy / blob.w);
    let sxx = blob.wxx / blob.w - cx * cx;
    let syy = blob.wyy / blob.w - cy * cy;
    let sxy = blob.wxy / blob.w - cx * cy;
    // Eigenvalues of the 2×2 second-moment matrix.
    let mean = 0.5 * (sxx + syy);
    let spread = {
        let half = 0.5 * (sxx - syy);
        libm::sqrt(half * half + sxy * sxy)
    };
    let (major, minor) = (mean + spread, (mean - spread).max(0.0));
    let ratio = f64::from(cfg.max_axis_ratio);
    if major > ratio * ratio * minor.max(1.0 / 12.0) {
        return None;
    }
    Some(Centroid {
        u: cx + 0.5,
        v: cy + 0.5,
        flux: blob.w,
        area: area.min(u32::from(u16::MAX)) as u16,
        peak: blob.peak,
    })
}

#[cfg(test)]
mod tests;
