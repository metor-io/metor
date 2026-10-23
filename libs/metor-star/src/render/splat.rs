//! The PSF splat into a fixed-point electron accumulator.

use super::psf::{MAX_TAPS, Psf};

/// Accumulator counts per electron: 1 count = 1/256 e⁻, which leaves
/// 16.7 M e⁻ of range per pixel.
pub const FIXED_SCALE: f32 = 256.0;

/// A row-major `u32` accumulator, one count per `1/FIXED_SCALE` e⁻.
#[derive(Debug)]
pub struct Accumulator<'a> {
    pub counts: &'a mut [u32],
    pub width: usize,
    pub height: usize,
}

impl Accumulator<'_> {
    /// Adds `e` electrons through `psf` at `(u, v)`. Each pixel gets
    /// `floor(e · wx · wy · FIXED_SCALE + ½)`, multiplied in that order as the
    /// GPU does. Returns whether any pixel on the sensor gained a count.
    pub fn splat(&mut self, psf: &Psf, u: f32, v: f32, e: f32) -> bool {
        let (mut wx, mut wy) = ([0.0; MAX_TAPS], [0.0; MAX_TAPS]);
        let x0 = psf.weights(u, &mut wx);
        let y0 = psf.weights(v, &mut wy);
        let taps = psf.taps();
        let mut any = false;
        for (y, &w_y) in (y0..).zip(&wy[..taps]) {
            let Some(row) = index(y, self.height) else {
                continue;
            };
            for (x, &w_x) in (x0..).zip(&wx[..taps]) {
                let Some(col) = index(x, self.width) else {
                    continue;
                };
                let add = libm::floorf(e * w_x * w_y * FIXED_SCALE + 0.5) as u32;
                if let Some(px) = self.counts.get_mut(row * self.width + col) {
                    *px = px.saturating_add(add);
                    any |= add > 0;
                }
            }
        }
        any
    }
}

fn index(i: i32, len: usize) -> Option<usize> {
    usize::try_from(i).ok().filter(|&i| i < len)
}
