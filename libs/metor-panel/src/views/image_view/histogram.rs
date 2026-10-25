//! Sample histograms over a source rectangle, and percentiles from them.

use zerocopy::byteorder::{F32, LE, U16};

use super::resample::Rect;

/// Counts per bin. Integer frames get one bin per code; `mono_f32` gets
/// `bins` equal bins over `lo..=hi`.
#[derive(Clone, Debug, PartialEq)]
pub struct Histogram {
    bins: Vec<u32>,
    count: u64,
    lo: f64,
    width: f64,
}

impl Histogram {
    pub fn of_u8(pixels: &[u8], frame_width: usize, rect: Rect) -> Self {
        let mut hist = Self::integer(256);
        hist.record(pixels, frame_width, rect, |v| Some(usize::from(v)));
        hist
    }

    /// `2^bits` bins, `bits` clamped to `1..=16`; codes above the top land
    /// in the last bin.
    pub fn of_u16(pixels: &[U16<LE>], frame_width: usize, bits: u8, rect: Rect) -> Self {
        let mut hist = Self::integer(1 << bits.clamp(1, 16));
        hist.record(pixels, frame_width, rect, |v| Some(usize::from(v.get())));
        hist
    }

    /// Values outside `lo..=hi` land in the end bins; NaN is skipped.
    pub fn of_f32(
        pixels: &[F32<LE>],
        frame_width: usize,
        rect: Rect,
        lo: f32,
        hi: f32,
        bins: usize,
    ) -> Self {
        let bins = bins.max(1);
        let width = match f64::from(hi - lo) / bins as f64 {
            w if w > 0.0 && w.is_finite() => w,
            _ => 1.0,
        };
        let mut hist = Self {
            bins: vec![0; bins],
            count: 0,
            lo: f64::from(lo),
            width,
        };
        hist.record(pixels, frame_width, rect, |v| {
            let v = f64::from(v.get());
            (!v.is_nan()).then(|| ((v - f64::from(lo)) / width).max(0.0) as usize)
        });
        hist
    }

    fn integer(bins: usize) -> Self {
        Self {
            bins: vec![0; bins],
            count: 0,
            lo: 0.0,
            width: 1.0,
        }
    }

    fn record<T: Copy>(
        &mut self,
        pixels: &[T],
        frame_width: usize,
        rect: Rect,
        bin: impl Fn(T) -> Option<usize>,
    ) {
        let frame_height = pixels.len().checked_div(frame_width).unwrap_or(0);
        let rect = rect.clip(frame_width, frame_height);
        let last = self.bins.len() - 1;
        for row in rect.rows() {
            let start = row * frame_width + rect.x;
            for &v in &pixels[start..start + rect.width] {
                if let Some(b) = bin(v) {
                    self.bins[b.min(last)] += 1;
                    self.count += 1;
                }
            }
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn bins(&self) -> &[u32] {
        &self.bins
    }

    /// The bin holding the `p` quantile, `p` in `0..=1`: 0 is the lowest
    /// occupied bin and 1 the highest. `None` when empty or `p` is NaN.
    pub fn percentile(&self, p: f64) -> Option<u32> {
        if self.count == 0 || p.is_nan() {
            return None;
        }
        let target = ((p.clamp(0.0, 1.0) * self.count as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        self.bins
            .iter()
            .position(|&n| {
                seen += u64::from(n);
                seen >= target
            })
            .map(|bin| bin as u32)
    }

    /// The share of samples in each bin or below it, in `0..=1`.
    pub fn cdf(&self) -> Vec<f32> {
        let total = self.count.max(1) as f64;
        let mut seen = 0u64;
        self.bins
            .iter()
            .map(|&n| {
                seen += u64::from(n);
                (seen as f64 / total) as f32
            })
            .collect()
    }

    /// Counts every colour channel of interleaved RGB or RGBA (alpha left
    /// out) in one 256-bin histogram.
    pub fn of_u8_color(pixels: &[u8], frame_width: usize, channels: usize, rect: Rect) -> Self {
        let channels = channels.max(1);
        let frame_height = pixels
            .len()
            .checked_div(frame_width * channels)
            .unwrap_or(0);
        let rect = rect.clip(frame_width, frame_height);
        // Four tables, so neighbouring samples of one value do not wait on
        // each other's increment.
        let mut counts = [[0u32; 256]; 4];
        for row in rect.rows() {
            let start = (row * frame_width + rect.x) * channels;
            let line = pixels
                .get(start..start + rect.width * channels)
                .unwrap_or(&[]);
            match channels {
                4 => {
                    for px in line.as_chunks::<4>().0 {
                        for c in 0..3 {
                            counts[c][usize::from(px[c])] += 1;
                        }
                    }
                }
                _ => {
                    let (quads, rest) = line.as_chunks::<4>();
                    for quad in quads {
                        for (table, &v) in counts.iter_mut().zip(quad) {
                            table[usize::from(v)] += 1;
                        }
                    }
                    for &v in rest {
                        counts[0][usize::from(v)] += 1;
                    }
                }
            }
        }
        let mut hist = Self::integer(256);
        for table in &counts {
            for (bin, &n) in hist.bins.iter_mut().zip(table) {
                *bin += n;
            }
        }
        hist.count = hist.bins.iter().map(|&n| u64::from(n)).sum();
        hist
    }

    /// The sample values the bins span.
    pub fn range(&self) -> (f64, f64) {
        (self.lo, self.lo + self.bins.len() as f64 * self.width)
    }

    /// The sample value at the low edge of `bin`; the code itself for
    /// integer frames.
    pub fn value(&self, bin: u32) -> f64 {
        self.lo + f64::from(bin) * self.width
    }
}

/// The finite min and max of `rect`, for sizing a `mono_f32` histogram.
pub fn finite_range(pixels: &[F32<LE>], frame_width: usize, rect: Rect) -> Option<(f32, f32)> {
    let frame_height = pixels.len().checked_div(frame_width).unwrap_or(0);
    let rect = rect.clip(frame_width, frame_height);
    rect.rows()
        .flat_map(|row| {
            let start = row * frame_width + rect.x;
            &pixels[start..start + rect.width]
        })
        .map(|v| v.get())
        .filter(|v| v.is_finite())
        .fold(None, |range, v| match range {
            None => Some((v, v)),
            Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16s(values: &[u16]) -> Vec<U16<LE>> {
        values.iter().map(|&v| U16::new(v)).collect()
    }

    fn f32s(values: &[f32]) -> Vec<F32<LE>> {
        values.iter().map(|&v| F32::new(v)).collect()
    }

    fn whole(width: usize, height: usize) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn counts_sum_to_the_pixel_count() {
        let pixels = u16s(&[0, 5, 5, 4095, 100, 7]);
        let hist = Histogram::of_u16(&pixels, 3, 12, whole(3, 2));
        assert_eq!(hist.count(), 6);
        assert_eq!(hist.bins().iter().map(|&n| u64::from(n)).sum::<u64>(), 6);
        assert_eq!(hist.bins()[5], 2);
        assert_eq!(hist.bins().len(), 4096);
    }

    #[test]
    fn percentile_ends_are_min_and_max() {
        let pixels: Vec<u8> = vec![9, 3, 200, 17, 3, 44];
        let hist = Histogram::of_u8(&pixels, 6, whole(6, 1));
        assert_eq!(hist.percentile(0.0), Some(3));
        assert_eq!(hist.percentile(1.0), Some(200));
        assert_eq!(hist.percentile(0.5), Some(9));
        assert_eq!(hist.percentile(-1.0), Some(3));
        assert_eq!(hist.percentile(2.0), Some(200));
    }

    #[test]
    fn constant_image_has_constant_percentiles() {
        let pixels = u16s(&[300; 16]);
        let hist = Histogram::of_u16(&pixels, 4, 12, whole(4, 4));
        for p in [0.0, 0.001, 0.5, 0.9995, 1.0] {
            assert_eq!(hist.percentile(p), Some(300));
        }
    }

    #[test]
    fn all_zero_and_all_saturated() {
        let zero = Histogram::of_u16(&u16s(&[0; 9]), 3, 16, whole(3, 3));
        assert_eq!(
            (zero.percentile(0.0), zero.percentile(1.0)),
            (Some(0), Some(0))
        );
        let full = Histogram::of_u16(&u16s(&[u16::MAX; 9]), 3, 16, whole(3, 3));
        assert_eq!(full.percentile(0.5), Some(65535));
        assert_eq!(full.bins().len(), 65536);
    }

    #[test]
    fn one_bit_frame() {
        let hist = Histogram::of_u16(&u16s(&[0, 1, 1, 1]), 2, 1, whole(2, 2));
        assert_eq!(hist.bins(), [1, 3]);
    }

    #[test]
    fn empty_input_has_no_percentile() {
        assert_eq!(Histogram::of_u8(&[], 0, whole(4, 4)).percentile(0.5), None);
        let pixels = [1u8, 2, 3, 4];
        let empty = Histogram::of_u8(&pixels, 2, Rect::new(0, 0, 0, 2));
        assert_eq!((empty.count(), empty.percentile(0.5)), (0, None));
        let hist = Histogram::of_u8(&pixels, 2, whole(2, 2));
        assert_eq!(hist.percentile(f64::NAN), None);
    }

    #[test]
    fn out_of_range_codes_land_in_the_last_bin() {
        let hist = Histogram::of_u16(&u16s(&[4095, 4096, 65535, 1]), 4, 12, whole(4, 1));
        assert_eq!(hist.bins()[4095], 3);
        assert_eq!(hist.percentile(1.0), Some(4095));
    }

    #[test]
    fn f32_bins_skip_nan_and_clamp() {
        let pixels = f32s(&[0.0, 0.5, f32::NAN, 1.0, -3.0, 9.0]);
        let hist = Histogram::of_f32(&pixels, 6, whole(6, 1), 0.0, 1.0, 4);
        assert_eq!(hist.count(), 5);
        assert_eq!(hist.bins(), [2, 0, 1, 2]);
        assert_eq!(hist.value(2), 0.5);
        assert_eq!(finite_range(&pixels, 6, whole(6, 1)), Some((-3.0, 9.0)));
    }

    #[test]
    fn f32_degenerate_range_does_not_panic() {
        let pixels = f32s(&[2.0, 2.0, f32::INFINITY]);
        let hist = Histogram::of_f32(&pixels, 3, whole(3, 1), 2.0, 2.0, 0);
        assert_eq!((hist.bins(), hist.count()), (&[3][..], 3));
        assert_eq!(finite_range(&f32s(&[f32::NAN]), 1, whole(1, 1)), None);
    }

    #[test]
    fn rect_counts_only_its_pixels() {
        #[rustfmt::skip]
        let pixels: Vec<u8> = vec![
            1, 1, 1, 1,
            1, 9, 9, 1,
            1, 9, 9, 1,
        ];
        let inner = Histogram::of_u8(&pixels, 4, Rect::new(1, 1, 2, 2));
        assert_eq!((inner.count(), inner.bins()[9]), (4, 4));
        let clipped = Histogram::of_u8(&pixels, 4, Rect::new(2, 1, 10, 10));
        assert_eq!((clipped.count(), clipped.bins()[9]), (4, 2));
        let outside = Histogram::of_u8(&pixels, 4, Rect::new(7, 7, 2, 2));
        assert_eq!(outside.count(), 0);
    }

    #[test]
    fn range_spans_the_bins() {
        let hist = Histogram::of_u16(&u16s(&[1]), 1, 12, whole(1, 1));
        assert_eq!(hist.range(), (0.0, 4096.0));
        let floats = f32s(&[0.0, 1.0]);
        let hist = Histogram::of_f32(&floats, 2, whole(2, 1), -1.0, 3.0, 8);
        assert_eq!(hist.range(), (-1.0, 3.0));
    }

    #[test]
    fn colour_histograms_count_rgb_and_skip_alpha() {
        let rgb: Vec<u8> = vec![1, 2, 3, 1, 1, 1, 9, 9, 9, 4, 4, 4];
        let hist = Histogram::of_u8_color(&rgb, 2, 3, whole(2, 2));
        assert_eq!(hist.count(), 12);
        assert_eq!((hist.bins()[1], hist.bins()[9], hist.bins()[4]), (4, 3, 3));
        let rgba: Vec<u8> = vec![5, 6, 7, 255, 5, 5, 5, 255];
        let hist = Histogram::of_u8_color(&rgba, 2, 4, whole(2, 1));
        assert_eq!((hist.count(), hist.bins()[5], hist.bins()[255]), (6, 4, 0));
        let inner = Histogram::of_u8_color(&rgb, 2, 3, Rect::new(1, 1, 5, 5));
        assert_eq!((inner.count(), inner.bins()[4]), (3, 3));
    }
}
