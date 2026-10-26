//! The background: per block, the median of a strided subsample, and one
//! frame σ, 1.4826 × the median of the blocks' MADs, floored at 1 DN.

use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub struct Background {
    width: usize,
    height: usize,
    block: usize,
    step: usize,
    /// Blocks per row and per column.
    bx: usize,
    by: usize,
    median: Vec<u16>,
    mad: Vec<u16>,
    /// The first DN above the threshold, per block.
    threshold: Vec<u16>,
    scratch: Vec<u16>,
    sigma: f32,
}

impl Background {
    pub fn new(width: usize, height: usize, block: u16, step: u8) -> Background {
        let (block, step) = (usize::from(block.max(1)), usize::from(step.max(1)));
        let (bx, by) = (width.div_ceil(block), height.div_ceil(block));
        let per = block.div_ceil(step);
        Background {
            width,
            height,
            block,
            step,
            bx,
            by,
            median: vec![0; bx * by],
            mad: vec![0; bx * by],
            threshold: vec![0; bx * by],
            scratch: Vec::with_capacity(per * per),
            sigma: 1.0,
        }
    }

    pub fn block(&self) -> usize {
        self.block
    }

    pub fn blocks_per_row(&self) -> usize {
        self.bx
    }

    pub fn sigma(&self) -> f32 {
        self.sigma
    }

    /// Block `b`'s median (DN).
    pub fn median(&self, b: usize) -> u16 {
        self.median.get(b).copied().unwrap_or(0)
    }

    /// Block `b`'s threshold: a pixel is lit when it is at or above it.
    pub fn threshold(&self, b: usize) -> u16 {
        self.threshold.get(b).copied().unwrap_or(u16::MAX)
    }

    /// Fills the medians, σ and thresholds for `px`, `width × height`.
    pub fn estimate(&mut self, px: &[u16], k_sigma: f32) {
        for by in 0..self.by {
            for bx in 0..self.bx {
                let (x0, y0) = (bx * self.block, by * self.block);
                let (x1, y1) = (
                    (x0 + self.block).min(self.width),
                    (y0 + self.block).min(self.height),
                );
                self.scratch.clear();
                for y in (y0..y1).step_by(self.step) {
                    let row = &px[y * self.width..][x0..x1];
                    self.scratch.extend(row.iter().step_by(self.step));
                }
                let median = median_of(&mut self.scratch);
                for s in &mut self.scratch {
                    *s = s.abs_diff(median);
                }
                let b = by * self.bx + bx;
                self.median[b] = median;
                self.mad[b] = median_of(&mut self.scratch);
            }
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.mad);
        let mad = median_of(&mut self.scratch);
        self.sigma = (1.4826 * f32::from(mad)).max(1.0);
        let lift = libm::ceilf(k_sigma.max(0.0) * self.sigma);
        for (t, m) in self.threshold.iter_mut().zip(&self.median) {
            // At least 1 DN above the median, so a flat frame lights nothing.
            *t = (f32::from(*m) + lift.max(1.0)).min(f32::from(u16::MAX)) as u16;
        }
    }
}

/// The lower median of `xs` (reordered), or 0 when empty.
fn median_of(xs: &mut [u16]) -> u16 {
    if xs.is_empty() {
        return 0;
    }
    let mid = (xs.len() - 1) / 2;
    *xs.select_nth_unstable(mid).1
}
