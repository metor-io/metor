//! Viewport resampling fused with the LUT: nearest when zoomed in, max-pool
//! when zoomed out, so a single hot pixel survives any reduction.

use std::fmt;
use std::ops::Range;

use metor_proto_wkt::ImageFormat;
use zerocopy::byteorder::{F32, LE, U16};

use super::layout::{FrameLayout, LayoutError, Pixels};
use super::lut::Lut;

/// An axis-aligned rectangle of pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The part inside a `width × height` frame.
    pub fn clip(self, width: usize, height: usize) -> Self {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Self {
            x,
            y,
            width: self.width.min(width - x),
            height: self.height.min(height - y),
        }
    }

    pub fn rows(&self) -> Range<usize> {
        self.y..self.y + self.height
    }
}

/// Maps device pixels onto source pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// The source point at the top-left corner of device pixel (0, 0).
    pub origin: (f64, f64),
    /// Source pixels per device pixel.
    pub step: f64,
}

/// The zoom range, in source pixels per device pixel.
pub const MIN_STEP: f64 = 1.0 / 64.0;
pub const MAX_STEP: f64 = 64.0;

impl Viewport {
    /// The viewport of an `out`-pixel output centred on source point
    /// `center`.
    pub fn centered(center: (f64, f64), step: f64, out: (usize, usize)) -> Self {
        Self {
            origin: (
                center.0 - out.0 as f64 / 2.0 * step,
                center.1 - out.1 as f64 / 2.0 * step,
            ),
            step,
        }
    }

    /// The source point at the centre of an `out`-pixel output.
    pub fn center(&self, out: (usize, usize)) -> (f64, f64) {
        self.to_source((out.0 as f64 / 2.0, out.1 as f64 / 2.0))
    }

    /// The source point under a device point.
    pub fn to_source(&self, device: (f64, f64)) -> (f64, f64) {
        (
            self.origin.0 + device.0 * self.step,
            self.origin.1 + device.1 * self.step,
        )
    }

    /// The device point over a source point.
    pub fn to_device(&self, source: (f64, f64)) -> (f64, f64) {
        (
            (source.0 - self.origin.0) / self.step,
            (source.1 - self.origin.1) / self.step,
        )
    }

    /// The source pixel the kernel paints at a device pixel: its sample
    /// when zoomed in, the middle of its pooled span when zoomed out.
    pub fn pixel_at(&self, device: (usize, usize)) -> (f64, f64) {
        let sample = |origin: f64, i: usize| (origin + (i as f64 + 0.5) * self.step).floor();
        (
            sample(self.origin.0, device.0),
            sample(self.origin.1, device.1),
        )
    }

    /// The size of the source block pooled into a device pixel; `None`
    /// when zoomed in.
    pub fn pooled(&self, device: (usize, usize)) -> Option<(usize, usize)> {
        let len = |origin: f64, i: usize| {
            let edge = |k: f64| (origin + k * self.step).floor();
            (edge(i as f64 + 1.0) - edge(i as f64)) as usize
        };
        (self.step > 1.0).then(|| (len(self.origin.0, device.0), len(self.origin.1, device.1)))
    }

    /// Zooms by `factor` (above 1 zooms in) keeping the source point under
    /// `device` where it is. The step stays within `MIN_STEP..=MAX_STEP`.
    pub fn zoom_about(&self, device: (f64, f64), factor: f64) -> Self {
        let anchor = self.to_source(device);
        let step = (self.step / factor).clamp(MIN_STEP, MAX_STEP);
        Self {
            origin: (anchor.0 - device.0 * step, anchor.1 - device.1 * step),
            step,
        }
    }

    /// Moves the image by `delta` device pixels.
    pub fn panned(&self, delta: (f64, f64)) -> Self {
        Self {
            origin: (
                self.origin.0 - delta.0 * self.step,
                self.origin.1 - delta.1 * self.step,
            ),
            step: self.step,
        }
    }

    /// Keeps at least `margin` device pixels of an `img`-sized image inside
    /// an `out`-sized output on each axis, or all of it when it is smaller.
    pub fn clamped(&self, img: (usize, usize), out: (usize, usize), margin: f64) -> Self {
        let step = self.step;
        let axis = |origin: f64, len: usize, out: usize| {
            let keep = margin.min(len as f64 / step).min(out as f64);
            let lo = -(out as f64 - keep) * step;
            let hi = len as f64 - keep * step;
            origin.clamp(lo.min(hi), hi.max(lo))
        };
        Self {
            origin: (
                axis(self.origin.0, img.0, out.0),
                axis(self.origin.1, img.1, out.1),
            ),
            step,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.step.is_finite()
            && self.step > 0.0
            && self.origin.0.is_finite()
            && self.origin.1.is_finite()
    }

    /// The device pixels of a `dst_w × dst_h` output that the image covers.
    pub fn visible(&self, img_w: usize, img_h: usize, dst_w: usize, dst_h: usize) -> Rect {
        if !self.is_valid() {
            return Rect::default();
        }
        let xs = covered(self.origin.0, self.step, dst_w, img_w);
        let ys = covered(self.origin.1, self.step, dst_h, img_h);
        if xs.is_empty() || ys.is_empty() {
            return Rect::default();
        }
        Rect::new(xs.start, ys.start, xs.len(), ys.len())
    }
}

/// `n` half-open source spans, one per device pixel, clamped to
/// `0..limit`. Together they partition the covered source range.
pub fn spans(origin: f64, step: f64, n: usize, limit: usize) -> Vec<Range<usize>> {
    (0..n).map(|i| span(origin, step, i, limit)).collect()
}

fn span(origin: f64, step: f64, i: usize, limit: usize) -> Range<usize> {
    let edge = |k: f64| (origin + k * step).floor().clamp(0.0, limit as f64) as usize;
    edge(i as f64)..edge(i as f64 + 1.0)
}

/// What device pixel `i` reads on one axis: the nearest source pixel when
/// zoomed in, its pooled span when zoomed out. Empty outside the image.
fn source(origin: f64, step: f64, i: usize, limit: usize) -> Range<usize> {
    if step > 1.0 {
        return span(origin, step, i, limit);
    }
    let x = (origin + (i as f64 + 0.5) * step).floor();
    if x < 0.0 || x >= limit as f64 {
        return 0..0;
    }
    let x = x as usize;
    x..x + 1
}

fn covered(origin: f64, step: f64, n: usize, limit: usize) -> Range<usize> {
    let hits = |i: &usize| !source(origin, step, *i, limit).is_empty();
    let Some(first) = (0..n).find(hits) else {
        return 0..0;
    };
    let last = (first..n).rev().find(hits).unwrap_or(first);
    first..last + 1
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    Layout(LayoutError),
    Unsupported(ImageFormat),
    PixelCount { expected: usize, actual: usize },
    InvalidViewport,
    Rows { rows: Range<usize>, height: usize },
    DstLength { expected: usize, actual: usize },
}

/// A mono frame and its layout.
#[derive(Clone, Copy, Debug)]
pub struct Plane<'a> {
    pub pixels: Pixels<'a>,
    pub layout: &'a FrameLayout,
}

/// Buffers a render reuses, so steady-state rendering does not allocate.
#[derive(Debug, Default)]
pub struct Scratch {
    xs: Vec<Range<usize>>,
    acc: Vec<u32>,
    /// Colour frames' resize buffers and debayer cache.
    pub(crate) color: super::color::ColorScratch,
}

/// Renders `rows` of `out` (rows relative to `out`) into `dst`, which holds
/// exactly those rows as BGRA. Bands of rows can render independently, each
/// with its own [`Scratch`]. Zoomed out, each device pixel shows the
/// maximum sample of its span, which is not the brightest display value
/// when the levels are inverted.
pub fn render_into(
    frame: Plane<'_>,
    lut: &Lut,
    viewport: &Viewport,
    out: Rect,
    rows: Range<usize>,
    dst: &mut [u8],
    scratch: &mut Scratch,
) -> Result<(), RenderError> {
    let Plane { pixels, layout } = frame;
    check(pixels, layout, viewport, out, &rows, dst.len())?;
    if out.width == 0 || rows.is_empty() {
        return Ok(());
    }
    let job = Job {
        lut,
        viewport,
        out,
        width: layout.width,
        height: layout.height,
    };
    match pixels {
        Pixels::U8(data) => job.render(data, rows, dst, scratch),
        Pixels::U16(data) => job.render(data, rows, dst, scratch),
        Pixels::F32(data) => job.render(data, rows, dst, scratch),
    }
    Ok(())
}

fn check(
    pixels: Pixels<'_>,
    layout: &FrameLayout,
    viewport: &Viewport,
    out: Rect,
    rows: &Range<usize>,
    dst_len: usize,
) -> Result<(), RenderError> {
    if layout.channels != 1 {
        return Err(RenderError::Unsupported(layout.format));
    }
    let expected = layout.width * layout.height;
    let actual = match pixels {
        Pixels::U8(p) => p.len(),
        Pixels::U16(p) => p.len(),
        Pixels::F32(p) => p.len(),
    };
    if actual != expected {
        return Err(RenderError::PixelCount { expected, actual });
    }
    if !viewport.is_valid() {
        return Err(RenderError::InvalidViewport);
    }
    if rows.start > rows.end || rows.end > out.height {
        return Err(RenderError::Rows {
            rows: rows.clone(),
            height: out.height,
        });
    }
    let expected = rows
        .len()
        .checked_mul(out.width)
        .and_then(|n| n.checked_mul(4));
    if expected != Some(dst_len) {
        return Err(RenderError::DstLength {
            expected: expected.unwrap_or(usize::MAX),
            actual: dst_len,
        });
    }
    Ok(())
}

trait Sample: Copy {
    fn code(self, lut: &Lut) -> u32;
}

impl Sample for u8 {
    fn code(self, _: &Lut) -> u32 {
        u32::from(self)
    }
}

impl Sample for U16<LE> {
    fn code(self, _: &Lut) -> u32 {
        u32::from(self.get())
    }
}

impl Sample for F32<LE> {
    fn code(self, lut: &Lut) -> u32 {
        lut.f32_code(self.get())
    }
}

struct Job<'a> {
    lut: &'a Lut,
    viewport: &'a Viewport,
    out: Rect,
    width: usize,
    height: usize,
}

impl Job<'_> {
    fn render<T: Sample>(
        &self,
        data: &[T],
        rows: Range<usize>,
        dst: &mut [u8],
        scratch: &mut Scratch,
    ) {
        let Viewport { origin, step } = *self.viewport;
        let Scratch { xs, acc, .. } = scratch;
        xs.clear();
        xs.extend(
            (self.out.x..self.out.x + self.out.width)
                .map(|i| source(origin.0, step, i, self.width)),
        );
        let x0 = xs.iter().filter(|r| !r.is_empty()).map(|r| r.start).min();
        let x0 = x0.unwrap_or(0);
        let x1 = xs.iter().map(|r| r.end).max().unwrap_or(0).max(x0);
        acc.clear();
        acc.resize(x1 - x0, 0);
        for (j, line) in rows.zip(dst.chunks_exact_mut(self.out.width * 4)) {
            let ys = source(origin.1, step, self.out.y + j, self.height);
            if ys.is_empty() {
                line.fill(0);
                continue;
            }
            self.pool(data, ys, x0, acc);
            self.write(xs, x0, acc, line);
        }
    }

    /// The column-wise max code of the source rows `ys`, from column `x0`.
    fn pool<T: Sample>(&self, data: &[T], ys: Range<usize>, x0: usize, acc: &mut [u32]) {
        // PANIC Safety: `check` sized `data` to the frame, and `ys` and
        // `x0..x0 + acc.len()` are clamped to it.
        let len = acc.len();
        let row = |y: usize| &data[y * self.width + x0..][..len];
        let first = ys.start;
        for (a, &v) in acc.iter_mut().zip(row(first)) {
            *a = v.code(self.lut);
        }
        for y in first + 1..ys.end {
            for (a, &v) in acc.iter_mut().zip(row(y)) {
                *a = (*a).max(v.code(self.lut));
            }
        }
    }

    fn write(&self, xs: &[Range<usize>], x0: usize, acc: &[u32], line: &mut [u8]) {
        for (px, span) in line.as_chunks_mut::<4>().0.iter_mut().zip(xs) {
            let codes = span
                .start
                .checked_sub(x0)
                .and_then(|a| acc.get(a..a + span.len()));
            let color = codes
                .and_then(|codes| codes.iter().max())
                .map_or(0, |&code| self.lut.get(code));
            *px = color.to_le_bytes();
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(err) => err.fmt(f),
            Self::Unsupported(format) => write!(f, "{format} is not a mono format"),
            Self::PixelCount { expected, actual } => {
                write!(f, "{actual} pixels, expected {expected}")
            }
            Self::InvalidViewport => f.write_str("the viewport is not finite and positive"),
            Self::Rows { rows, height } => write!(f, "rows {rows:?} outside 0..{height}"),
            Self::DstLength { expected, actual } => {
                write!(f, "destination is {actual} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use metor_proto_wkt::Stretch;

    use super::*;
    use crate::views::image_view::lut::{Colormap, Display};

    const SAT: [u8; 4] = [0, 0, 255, 255];

    fn layout(format: ImageFormat, bits: u8, width: usize, height: usize) -> FrameLayout {
        FrameLayout {
            width,
            height,
            channels: format.channels(),
            format,
            bits,
            black: None,
            white: None,
            stretch: Stretch::Auto,
        }
    }

    fn gray(bits: u8) -> Lut {
        let top = f64::from((1u32 << bits) - 1);
        Lut::build(bits, 0.0, top, &Display::default(), SAT)
    }

    fn draw(
        pixels: Pixels<'_>,
        layout: &FrameLayout,
        lut: &Lut,
        viewport: &Viewport,
        out: Rect,
        rows: Range<usize>,
        dst: &mut [u8],
    ) -> Result<(), RenderError> {
        let frame = Plane { pixels, layout };
        render_into(
            frame,
            lut,
            viewport,
            out,
            rows,
            dst,
            &mut Scratch::default(),
        )
    }

    /// Renders the whole visible area of a `dst_w × dst_h` output.
    fn render(
        pixels: Pixels<'_>,
        layout: &FrameLayout,
        lut: &Lut,
        viewport: Viewport,
        dst: (usize, usize),
    ) -> (Rect, Vec<u8>) {
        let out = viewport.visible(layout.width, layout.height, dst.0, dst.1);
        let mut buf = vec![0u8; out.width * out.height * 4];
        draw(pixels, layout, lut, &viewport, out, 0..out.height, &mut buf).unwrap();
        (out, buf)
    }

    fn view(x: f64, y: f64, step: f64) -> Viewport {
        Viewport {
            origin: (x, y),
            step,
        }
    }

    /// The red channel of each output pixel; gray LUTs make it the value.
    fn reds(buf: &[u8]) -> Vec<u8> {
        buf.as_chunks::<4>().0.iter().map(|px| px[2]).collect()
    }

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 37 % 251) as u8).collect()
    }

    #[test]
    fn spans_partition_the_covered_range() {
        let limit = 50;
        for step in [1.0, 1.463, 2.0, 4.0, 7.3] {
            for origin in [0.0, 0.5, -3.2, 48.7, 60.0] {
                let n = 40;
                let mut seen = vec![0u32; limit];
                for span in spans(origin, step, n, limit) {
                    for x in span {
                        seen[x] += 1;
                    }
                }
                let edge = |k: f64| (origin + k * step).floor().clamp(0.0, limit as f64) as usize;
                for (x, &n_seen) in seen.iter().enumerate() {
                    let covered = (edge(0.0)..edge(n as f64)).contains(&x);
                    assert_eq!(
                        n_seen,
                        u32::from(covered),
                        "step {step} origin {origin} x {x}"
                    );
                }
            }
        }
    }

    fn close(a: (f64, f64), b: (f64, f64), tolerance: f64) -> bool {
        (a.0 - b.0).abs() <= tolerance && (a.1 - b.1).abs() <= tolerance
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let viewport = view(-12.5, 40.0, 1.7);
        for scale in [1.0, 2.0] {
            let device = (123.0 * scale, 77.5 * scale);
            let before = viewport.to_source(device);
            for factor in [1.25, 0.8, 1.25f64.powi(6)] {
                let zoomed = viewport.zoom_about(device, factor);
                assert!(
                    close(zoomed.to_source(device), before, 1e-9),
                    "{scale} {factor}"
                );
            }
        }
        let deep = view(0.0, 0.0, 1.0).zoom_about((0.0, 0.0), 1e6);
        assert_eq!(deep.step, MIN_STEP);
        let far = view(0.0, 0.0, 1.0).zoom_about((0.0, 0.0), 1e-6);
        assert_eq!(far.step, MAX_STEP);
    }

    #[test]
    fn pan_clamping_keeps_part_of_the_image_in_view() {
        let (img, out) = ((512, 256), (800, 600));
        let mut step = MIN_STEP;
        while step <= MAX_STEP {
            for delta in [(1e7, 1e7), (-1e7, -1e7), (1e7, -1e7), (0.0, 0.0)] {
                let viewport = view(100.0, 50.0, step)
                    .panned(delta)
                    .clamped(img, out, 32.0);
                let visible = viewport.visible(img.0, img.1, out.0, out.1);
                let want =
                    |len: usize, out: usize| (32.0f64).min(len as f64 / step).min(out as f64);
                assert!(
                    visible.width as f64 + 1.0 >= want(img.0, out.0),
                    "{step} {delta:?}"
                );
                assert!(
                    visible.height as f64 + 1.0 >= want(img.1, out.1),
                    "{step} {delta:?}"
                );
            }
            step *= 2.0;
        }
    }

    #[test]
    fn one_to_one_is_one_device_pixel_per_source_pixel() {
        for scale in [1.0f64, 2.0] {
            let viewport = Viewport::centered((50.0, 50.0), 1.0, (200, 100));
            // One source pixel spans `1 / scale` logical pixels.
            let a = viewport.to_device((10.0, 10.0));
            let b = viewport.to_device((11.0, 10.0));
            assert_eq!((b.0 - a.0) / scale, 1.0 / scale);
            assert_eq!(viewport.center((200, 100)), (50.0, 50.0));
        }
    }

    #[test]
    fn to_source_inverts_to_device() {
        for viewport in [
            view(0.0, 0.0, 1.0),
            view(-3.2, 7.9, 0.25),
            view(100.0, -40.0, 7.3),
        ] {
            for device in [(0.0, 0.0), (17.5, 3.25), (640.0, 480.0)] {
                let back = viewport.to_device(viewport.to_source(device));
                assert!(close(back, device, 0.5), "{viewport:?} {device:?}");
            }
        }
    }

    #[test]
    fn step_one_is_the_identity() {
        let data = ramp(5 * 4);
        let layout = layout(ImageFormat::Mono8, 8, 5, 4);
        let (out, buf) = render(
            Pixels::U8(&data),
            &layout,
            &gray(8),
            view(0.0, 0.0, 1.0),
            (5, 4),
        );
        assert_eq!(out, Rect::new(0, 0, 5, 4));
        assert_eq!(reds(&buf), data);
        for px in buf.as_chunks::<4>().0 {
            assert_eq!((px[0], px[1], px[3]), (px[2], px[2], 255));
        }
    }

    #[test]
    fn quarter_step_makes_four_by_four_blocks() {
        let data = ramp(3 * 2);
        let layout = layout(ImageFormat::Mono8, 8, 3, 2);
        let (out, buf) = render(
            Pixels::U8(&data),
            &layout,
            &gray(8),
            view(0.0, 0.0, 0.25),
            (12, 8),
        );
        assert_eq!(out, Rect::new(0, 0, 12, 8));
        let reds = reds(&buf);
        for y in 0..8 {
            for x in 0..12 {
                assert_eq!(reds[y * 12 + x], data[(y / 4) * 3 + x / 4], "({x}, {y})");
            }
        }
    }

    /// Whether source pixel `x` lies in the pooled span of `n` device pixels.
    fn on_screen(origin: f64, step: f64, n: usize, limit: usize, x: usize) -> bool {
        let edge = |k: f64| (origin + k * step).floor().clamp(0.0, limit as f64) as usize;
        (edge(0.0)..edge(n as f64)).contains(&x)
    }

    #[test]
    fn every_hot_pixel_survives_downsample_exactly_once() {
        let (w, h) = (23, 19);
        let (dw, dh) = (40, 40);
        let layout = layout(ImageFormat::Mono8, 8, w, h);
        let lut = gray(8);
        let mut failures = vec![];
        for step in [1.0001, 1.463, 2.0, 2.5, 3.7, 4.0, 7.3, 8.0, 23.0 / 7.0] {
            for ox in [0.0, 0.5, -3.2, 2.7, -0.001, 5.999] {
                for oy in [0.0, 0.5, -1.7, 3.3] {
                    let viewport = view(ox, oy, step);
                    for y in 0..h {
                        for x in 0..w {
                            if !on_screen(ox, step, dw, w, x) || !on_screen(oy, step, dh, h, y) {
                                continue;
                            }
                            let mut data = vec![10u8; w * h];
                            data[y * w + x] = 255;
                            let (_, buf) =
                                render(Pixels::U8(&data), &layout, &lut, viewport, (dw, dh));
                            let hot = reds(&buf).iter().filter(|&&r| r == 255).count();
                            if hot != 1 {
                                failures.push((step, ox, oy, x, y, hot));
                            }
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn scratch_is_reused_across_renders() {
        let data = ramp(16 * 16);
        let layout = layout(ImageFormat::Mono8, 8, 16, 16);
        let lut = gray(8);
        let mut scratch = Scratch::default();
        let viewport = view(0.0, 0.0, 2.0);
        let out = viewport.visible(16, 16, 8, 8);
        let mut first = vec![0u8; out.width * out.height * 4];
        let frame = Plane {
            pixels: Pixels::U8(&data),
            layout: &layout,
        };
        render_into(frame, &lut, &viewport, out, 0..8, &mut first, &mut scratch).unwrap();
        let capacity = (scratch.xs.capacity(), scratch.acc.capacity());
        let mut second = vec![0u8; first.len()];
        render_into(frame, &lut, &viewport, out, 0..8, &mut second, &mut scratch).unwrap();
        assert_eq!(first, second);
        assert_eq!((scratch.xs.capacity(), scratch.acc.capacity()), capacity);
    }

    #[test]
    fn dst_length_overflow_is_an_error() {
        let layout = layout(ImageFormat::Mono8, 8, 2, 2);
        let out = Rect::new(0, 0, usize::MAX / 2, 4);
        assert_eq!(
            draw(
                Pixels::U8(&[0; 4]),
                &layout,
                &gray(8),
                &view(0.0, 0.0, 1.0),
                out,
                0..4,
                &mut []
            ),
            Err(RenderError::DstLength {
                expected: usize::MAX,
                actual: 0
            })
        );
    }

    #[test]
    fn mono16_max_pool_reaches_full_white() {
        let (w, h) = (32, 32);
        let mut data = vec![U16::<LE>::new(200); w * h];
        data[17 * w + 5] = U16::new(4095);
        let layout = layout(ImageFormat::Mono16, 12, w, h);
        let (_, buf) = render(
            Pixels::U16(&data),
            &layout,
            &gray(12),
            view(0.0, 0.0, 4.0),
            (8, 8),
        );
        assert_eq!(reds(&buf).iter().filter(|&&r| r == 255).count(), 1);
    }

    #[test]
    fn sixteen_bit_and_one_bit_frames() {
        let data: Vec<U16<LE>> = [0u16, 65535, 32768, 1].map(U16::new).to_vec();
        let layout16 = layout(ImageFormat::Mono16, 16, 2, 2);
        let (_, buf) = render(
            Pixels::U16(&data),
            &layout16,
            &gray(16),
            view(0.0, 0.0, 1.0),
            (2, 2),
        );
        assert_eq!(reds(&buf), [0, 255, 128, 0]);
        let bits1: Vec<U16<LE>> = [0u16, 1, 1, 0].map(U16::new).to_vec();
        let layout1 = layout(ImageFormat::Mono16, 1, 2, 2);
        let (_, buf) = render(
            Pixels::U16(&bits1),
            &layout1,
            &gray(1),
            view(0.0, 0.0, 1.0),
            (2, 2),
        );
        assert_eq!(reds(&buf), [0, 255, 255, 0]);
    }

    #[test]
    fn all_zero_and_all_saturated() {
        let layout = layout(ImageFormat::Mono8, 8, 4, 4);
        let display = Display {
            saturation: true,
            colormap: Colormap::Inferno,
            ..Display::default()
        };
        let lut = Lut::build(8, 0.0, 255.0, &display, SAT);
        let zero = vec![0u8; 16];
        let (_, buf) = render(
            Pixels::U8(&zero),
            &layout,
            &lut,
            view(0.0, 0.0, 2.0),
            (2, 2),
        );
        assert!(
            buf.as_chunks::<4>()
                .0
                .iter()
                .all(|&px| u32::from_le_bytes(px) == lut.get(0))
        );
        let full = vec![255u8; 16];
        let (_, buf) = render(
            Pixels::U8(&full),
            &layout,
            &lut,
            view(0.0, 0.0, 0.5),
            (8, 8),
        );
        assert!(buf.as_chunks::<4>().0.iter().all(|&px| px == SAT));
    }

    #[test]
    fn mono_f32_maps_through_the_levels() {
        let data: Vec<F32<LE>> = [-1.0f32, 0.0, 1.0, f32::NAN].map(F32::new).to_vec();
        let layout = layout(ImageFormat::MonoF32, 32, 4, 1);
        let lut = Lut::build_f32(-1.0, 1.0, &Display::default());
        let (_, buf) = render(
            Pixels::F32(&data),
            &layout,
            &lut,
            view(0.0, 0.0, 1.0),
            (4, 1),
        );
        assert_eq!(reds(&buf), [0, 127, 255, 0]);
    }

    #[test]
    fn one_by_one_fills_the_output() {
        let layout = layout(ImageFormat::Mono8, 8, 1, 1);
        let (out, buf) = render(
            Pixels::U8(&[77]),
            &layout,
            &gray(8),
            view(0.0, 0.0, 0.01),
            (100, 100),
        );
        assert_eq!(out, Rect::new(0, 0, 100, 100));
        assert!(reds(&buf).iter().all(|&r| r == 77));
        let (out, buf) = render(
            Pixels::U8(&[77]),
            &layout,
            &gray(8),
            view(0.0, 0.0, 1000.0),
            (10, 10),
        );
        assert_eq!(out, Rect::new(0, 0, 1, 1));
        assert_eq!(reds(&buf), [77]);
    }

    #[test]
    fn extreme_zoom_does_not_panic() {
        let data = ramp(64 * 64);
        let layout = layout(ImageFormat::Mono8, 8, 64, 64);
        let (out, buf) = render(
            Pixels::U8(&data),
            &layout,
            &gray(8),
            view(10.0, 20.0, 1e-6),
            (50, 50),
        );
        assert_eq!(out, Rect::new(0, 0, 50, 50));
        assert!(reds(&buf).iter().all(|&r| r == data[20 * 64 + 10]));
        let (out, _) = render(
            Pixels::U8(&data),
            &layout,
            &gray(8),
            view(-1e9, -1e9, 1e9),
            (50, 50),
        );
        assert_eq!(out, Rect::new(1, 1, 1, 1));
    }

    #[test]
    fn a_viewport_outside_renders_nothing() {
        let layout = layout(ImageFormat::Mono8, 8, 8, 8);
        for viewport in [
            view(100.0, 0.0, 1.0),
            view(-50.0, -50.0, 0.5),
            view(0.0, 9.0, 3.0),
        ] {
            assert!(viewport.visible(8, 8, 16, 16).is_empty(), "{viewport:?}");
            let (out, buf) = render(Pixels::U8(&[0; 64]), &layout, &gray(8), viewport, (16, 16));
            assert!(out.is_empty() && buf.is_empty());
        }
    }

    #[test]
    fn zero_sized_outputs_return_early() {
        let layout = layout(ImageFormat::Mono8, 8, 2, 2);
        let viewport = view(0.0, 0.0, 1.0);
        assert!(viewport.visible(2, 2, 0, 5).is_empty());
        assert!(viewport.visible(0, 0, 5, 5).is_empty());
        let out = Rect::new(0, 0, 0, 2);
        draw(
            Pixels::U8(&[1; 4]),
            &layout,
            &gray(8),
            &viewport,
            out,
            0..2,
            &mut [],
        )
        .unwrap();
        let out = Rect::new(0, 0, 2, 2);
        draw(
            Pixels::U8(&[1; 4]),
            &layout,
            &gray(8),
            &viewport,
            out,
            1..1,
            &mut [],
        )
        .unwrap();
    }

    #[test]
    fn rows_render_only_their_band() {
        let data = ramp(16 * 16);
        let layout = layout(ImageFormat::Mono8, 8, 16, 16);
        let lut = gray(8);
        for step in [0.5, 1.0, 2.5] {
            let viewport = view(0.0, 0.0, step);
            let out = viewport.visible(16, 16, 40, 40);
            let stride = out.width * 4;
            let mut whole = vec![0u8; stride * out.height];
            draw(
                Pixels::U8(&data),
                &layout,
                &lut,
                &viewport,
                out,
                0..out.height,
                &mut whole,
            )
            .unwrap();
            let mut banded = vec![0xab; whole.len()];
            let band = 2..5;
            let dst = &mut banded[band.start * stride..band.end * stride];
            draw(
                Pixels::U8(&data),
                &layout,
                &lut,
                &viewport,
                out,
                band.clone(),
                dst,
            )
            .unwrap();
            for (row, (a, b)) in whole.chunks(stride).zip(banded.chunks(stride)).enumerate() {
                match band.contains(&row) {
                    true => assert_eq!(a, b, "step {step} row {row}"),
                    false => assert!(b.iter().all(|&v| v == 0xab), "step {step} row {row}"),
                }
            }
        }
    }

    #[test]
    fn bad_inputs_are_errors() {
        let mono = layout(ImageFormat::Mono8, 8, 2, 2);
        let viewport = view(0.0, 0.0, 1.0);
        let out = Rect::new(0, 0, 2, 2);
        let lut = gray(8);
        let mut dst = vec![0u8; 16];
        let rgb = layout(ImageFormat::Rgb8, 8, 2, 2);
        assert_eq!(
            draw(
                Pixels::U8(&[0; 12]),
                &rgb,
                &lut,
                &viewport,
                out,
                0..2,
                &mut dst
            ),
            Err(RenderError::Unsupported(ImageFormat::Rgb8))
        );
        assert_eq!(
            draw(
                Pixels::U8(&[0; 3]),
                &mono,
                &lut,
                &viewport,
                out,
                0..2,
                &mut dst
            ),
            Err(RenderError::PixelCount {
                expected: 4,
                actual: 3
            })
        );
        for step in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                draw(
                    Pixels::U8(&[0; 4]),
                    &mono,
                    &lut,
                    &view(0.0, 0.0, step),
                    out,
                    0..2,
                    &mut dst
                ),
                Err(RenderError::InvalidViewport)
            );
            assert!(view(0.0, 0.0, step).visible(2, 2, 2, 2).is_empty());
        }
        assert!(matches!(
            draw(
                Pixels::U8(&[0; 4]),
                &mono,
                &lut,
                &viewport,
                out,
                1..3,
                &mut dst[..8]
            ),
            Err(RenderError::Rows { .. })
        ));
        assert_eq!(
            draw(
                Pixels::U8(&[0; 4]),
                &mono,
                &lut,
                &viewport,
                out,
                0..2,
                &mut dst[..15]
            ),
            Err(RenderError::DstLength {
                expected: 16,
                actual: 15
            })
        );
    }

    /// `cargo test -p metor-panel --release -- --ignored --nocapture
    /// image_view::resample::tests::bench`
    #[test]
    #[ignore]
    fn bench() {
        use std::hint::black_box;

        use super::super::histogram::Histogram;

        let (w, h) = (1024, 1024);
        let data = star_field(w, h);
        let layout = layout(ImageFormat::Mono16, 12, w, h);
        let display = Display {
            colormap: Colormap::Inferno,
            ..Display::default()
        };
        let lut = Lut::build(12, 200.0, 4095.0, &display, SAT);
        time("lut build 4096 inferno", || {
            black_box(Lut::build(12, 200.0, 4095.0, &display, SAT));
        });
        time("histogram 4096 bins, whole frame", || {
            black_box(Histogram::of_u16(&data, w, 12, Rect::new(0, 0, w, h)));
        });
        let cases = [
            (
                "fit 1200x1200 (0.853)",
                1200,
                1200,
                view(0.0, 0.0, 1024.0 / 1200.0),
            ),
            ("1:1 1024x1024", 1024, 1024, view(0.0, 0.0, 1.0)),
            ("zoom 4x 1600x1200", 1600, 1200, view(300.0, 300.0, 0.25)),
            (
                "retina full 3400x2100 fit",
                3400,
                2100,
                view(-700.0, 0.0, 1024.0 / 2100.0),
            ),
            (
                "retina full 3400x2100 4x",
                3400,
                2100,
                view(100.0, 100.0, 0.25),
            ),
            ("down 512x512 (2.0)", 512, 512, view(0.0, 0.0, 2.0)),
            (
                "down 700x700 (1.463)",
                700,
                700,
                view(0.0, 0.0, 1024.0 / 700.0),
            ),
            ("down 256x256 (4.0)", 256, 256, view(0.0, 0.0, 4.0)),
        ];
        for (name, dw, dh, viewport) in cases {
            let out = viewport.visible(w, h, dw, dh);
            let mut dst = vec![0u8; out.width * out.height * 4];
            time(name, || {
                draw(
                    Pixels::U16(&data),
                    &layout,
                    &lut,
                    &viewport,
                    out,
                    0..out.height,
                    &mut dst,
                )
                .unwrap();
            });
        }
    }

    fn time(name: &str, mut f: impl FnMut()) {
        for _ in 0..3 {
            f();
        }
        let mut ms: Vec<f64> = (0..30)
            .map(|_| {
                let start = std::time::Instant::now();
                f();
                start.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!(
            "{name:<40} median {:>7.3} ms  min {:>7.3} ms",
            ms[15], ms[0]
        );
    }

    /// A 12-bit field: noise near 200, Gaussian stars, and hot pixels.
    fn star_field(w: usize, h: usize) -> Vec<U16<LE>> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut img: Vec<u16> = (0..w * h).map(|_| 200 + (rnd() % 24) as u16).collect();
        for _ in 0..300 {
            let (cx, cy) = ((rnd() % w as u64) as i64, (rnd() % h as u64) as i64);
            let amp = (rnd() % 3800) as f32;
            for dy in -4..=4i64 {
                for dx in -4..=4i64 {
                    let (x, y) = (cx + dx, cy + dy);
                    if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                        continue;
                    }
                    let r2 = (dx * dx + dy * dy) as f32;
                    let p = &mut img[y as usize * w + x as usize];
                    *p = (f32::from(*p) + amp * (-r2 / 1.8).exp()).min(4095.0) as u16;
                }
            }
        }
        for _ in 0..50 {
            img[(rnd() % (w * h) as u64) as usize] = 4095;
        }
        img.into_iter().map(U16::new).collect()
    }
}
