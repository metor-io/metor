//! Colour frames: rgb8 and rgba8 as they are, and Bayer mosaics through a
//! 2×2 superpixel (zoomed out) or bilinear demosaicing of just the pixels
//! nearest sampling picks (zoomed in). Each is resampled (nearest zoomed in, a box mean
//! through `fast_image_resize` zoomed out) and mapped through one levels
//! LUT shared by the three channels.

use std::sync::Arc;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use metor_proto_wkt::ImageFormat;

use super::layout::{FrameLayout, Pixels};
use super::lut::{Curve, level_of};
use super::resample::{Rect, RenderError, Viewport};

/// One of the colours a Bayer site samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    Red,
    Green,
    Blue,
}

impl Site {
    pub fn letter(self) -> &'static str {
        match self {
            Self::Red => "R",
            Self::Green => "G",
            Self::Blue => "B",
        }
    }
}

/// The 2×2 tile a Bayer format repeats, row-major from pixel (0, 0).
pub fn bayer_tile(format: ImageFormat) -> Option<[Site; 4]> {
    use ImageFormat::*;
    use Site::*;
    Some(match format {
        BayerRggb8 | BayerRggb16 => [Red, Green, Green, Blue],
        BayerGrbg8 | BayerGrbg16 => [Green, Red, Blue, Green],
        BayerGbrg8 | BayerGbrg16 => [Green, Blue, Red, Green],
        BayerBggr8 | BayerBggr16 => [Blue, Green, Green, Red],
        _ => return None,
    })
}

/// The colour pixel `(x, y)` of a Bayer frame samples.
pub fn site(format: ImageFormat, x: usize, y: usize) -> Option<Site> {
    bayer_tile(format).map(|tile| tile[(y % 2) * 2 + x % 2])
}

pub fn is_color(format: ImageFormat) -> bool {
    format.channels() > 1 || bayer_tile(format).is_some()
}

/// Code → 8-bit intensity: the levels and curve, without a colormap.
pub struct ChannelLut(Vec<u8>);

impl ChannelLut {
    pub fn build(bits: u8, black: f64, white: f64, curve: Curve) -> Self {
        let codes = 1usize << bits.clamp(1, 16);
        Self(
            (0..codes)
                .map(|code| {
                    let t = curve.apply(level_of(code as f64, black, white, codes));
                    (t * 255.0).round() as u8
                })
                .collect(),
        )
    }

    fn get(&self, code: u32) -> u8 {
        let last = self.0.len().saturating_sub(1);
        self.0.get((code as usize).min(last)).copied().unwrap_or(0)
    }
}

/// An owned interleaved RGB image, the output of debayering.
#[derive(Debug, PartialEq)]
pub struct Rgb16 {
    pub data: Vec<u16>,
    pub width: usize,
    pub height: usize,
}

/// Half-resolution RGB: each 2×2 tile becomes one pixel, green averaged.
pub fn superpixel(cfa: &[u16], width: usize, height: usize, tile: [Site; 4]) -> Rgb16 {
    let (w, h) = (width / 2, height / 2);
    let mut data = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let at = |dy: usize, dx: usize| u32::from(cfa[(2 * y + dy) * width + 2 * x + dx]);
            let mut rgb = [0u32; 3];
            let mut greens = 0;
            for (i, site) in tile.iter().enumerate() {
                let v = at(i / 2, i % 2);
                match site {
                    Site::Red => rgb[0] = v,
                    Site::Green => {
                        rgb[1] += v;
                        greens += 1;
                    }
                    Site::Blue => rgb[2] = v,
                }
            }
            rgb[1] /= greens.max(1);
            data.extend(rgb.iter().map(|&v| v as u16));
        }
    }
    Rgb16 {
        data,
        width: w,
        height: h,
    }
}

/// The interleaved samples a colour render reads.
#[derive(Clone, Copy)]
pub enum ColorPixels<'a> {
    /// `channels` of 3 or 4; a fourth (alpha) is ignored.
    U8 {
        data: &'a [u8],
        channels: usize,
    },
    U16(&'a [u16]),
}

/// A colour image to render: its samples and size in pixels.
#[derive(Clone, Copy)]
pub struct ColorFrame<'a> {
    pub pixels: ColorPixels<'a>,
    pub width: usize,
    pub height: usize,
}

impl ColorFrame<'_> {
    fn fir_type(&self) -> PixelType {
        match self.pixels {
            ColorPixels::U8 { channels: 4, .. } => PixelType::U8x4,
            ColorPixels::U8 { .. } => PixelType::U8x3,
            ColorPixels::U16(_) => PixelType::U16x3,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self.pixels {
            ColorPixels::U8 { data, .. } => data,
            ColorPixels::U16(data) => zerocopy::IntoBytes::as_bytes(data),
        }
    }
}

impl std::fmt::Debug for ColorScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorScratch").finish_non_exhaustive()
    }
}

/// Buffers a colour render reuses.
#[derive(Default)]
pub struct ColorScratch {
    resizer: Option<Resizer>,
    resized: Vec<u8>,
    /// The superpixel image and the sample it is of.
    pub(crate) debayered: Option<(usize, Arc<Rgb16>)>,
}

/// Renders `out` of a colour frame into `dst` (BGRA, `out` sized).
pub fn render_color(
    frame: ColorFrame<'_>,
    lut: &ChannelLut,
    viewport: &Viewport,
    out: Rect,
    dst: &mut [u8],
    scratch: &mut ColorScratch,
) -> Result<(), RenderError> {
    let expected = out.width * out.height * 4;
    if dst.len() != expected {
        return Err(RenderError::DstLength {
            expected,
            actual: dst.len(),
        });
    }
    if out.is_empty() {
        return Ok(());
    }
    match viewport.step > 1.0 {
        true => render_box(frame, lut, viewport, out, dst, scratch),
        false => {
            render_nearest(frame, lut, viewport, out, dst);
            Ok(())
        }
    }
}

/// Writes each device pixel from the source pixel `(xs[i], ys[j])`
/// through the LUT; `None` leaves the pixel as it is.
fn map_pixels(
    frame: ColorFrame<'_>,
    lut: &ChannelLut,
    xs: &[Option<usize>],
    ys: &[Option<usize>],
    dst: &mut [u8],
) {
    match frame.pixels {
        ColorPixels::U8 { data, channels } => {
            map_typed(data, channels, frame.width, lut, xs, ys, dst)
        }
        ColorPixels::U16(data) => map_typed(data, 3, frame.width, lut, xs, ys, dst),
    }
}

fn map_typed<T: Copy + Into<u32>>(
    data: &[T],
    channels: usize,
    width: usize,
    lut: &ChannelLut,
    xs: &[Option<usize>],
    ys: &[Option<usize>],
    dst: &mut [u8],
) {
    let rows = dst.chunks_exact_mut(xs.len() * 4);
    for (row, y) in rows.zip(ys) {
        let pixels = row.as_chunks_mut::<4>().0.iter_mut();
        for (px, x) in pixels.zip(xs) {
            let at = x.zip(*y).map(|(x, y)| (y * width + x) * channels);
            if let Some(rgb) = at.and_then(|at| data.get(at..at + 3)) {
                *px = [
                    lut.get(rgb[2].into()),
                    lut.get(rgb[1].into()),
                    lut.get(rgb[0].into()),
                    0xff,
                ];
            }
        }
    }
}

/// The source index of each device pixel along one axis (nearest
/// sampling), or `None` outside the frame.
fn nearest_axis(
    viewport: &Viewport,
    start: usize,
    len: usize,
    limit: usize,
    axis: usize,
) -> Vec<Option<usize>> {
    (start..start + len)
        .map(|i| {
            let (x, y) = viewport.pixel_at((i, i));
            let v = if axis == 0 { x } else { y };
            // Past 1:1 a visible edge pixel's centre can fall just outside
            // the frame while its span overlaps it; it reads the edge pixel.
            let reach = viewport.step.max(1.0);
            (v > -reach && v < limit as f64 - 1.0 + reach)
                .then(|| v.clamp(0.0, limit.saturating_sub(1) as f64) as usize)
        })
        .collect()
}

fn render_nearest(
    frame: ColorFrame<'_>,
    lut: &ChannelLut,
    viewport: &Viewport,
    out: Rect,
    dst: &mut [u8],
) {
    let xs = nearest_axis(viewport, out.x, out.width, frame.width, 0);
    let ys = nearest_axis(viewport, out.y, out.height, frame.height, 1);
    dst.fill(0);
    map_pixels(frame, lut, &xs, &ys, dst);
}

/// The device pixels in `start..start + len` whose whole span lies inside
/// `0..limit`.
fn inner_axis(
    origin: f64,
    step: f64,
    start: usize,
    len: usize,
    limit: usize,
) -> std::ops::Range<usize> {
    let lo = (-origin / step).ceil().max(start as f64) as usize;
    let hi = ((limit as f64 - origin) / step)
        .floor()
        .min((start + len) as f64);
    let hi = (hi.max(lo as f64)) as usize;
    lo..hi
}

/// A box mean of the source under each device pixel, through `fast_image_resize`.
fn render_box(
    frame: ColorFrame<'_>,
    lut: &ChannelLut,
    viewport: &Viewport,
    out: Rect,
    dst: &mut [u8],
    scratch: &mut ColorScratch,
) -> Result<(), RenderError> {
    let pixel_type = frame.fir_type();
    let src = ImageRef::new(
        frame.width as u32,
        frame.height as u32,
        frame.bytes(),
        pixel_type,
    )
    .map_err(|_| RenderError::PixelCount {
        expected: frame.width * frame.height,
        actual: frame.bytes().len() / pixel_type.size(),
    })?;
    // Edge rows and columns only partly over the image: clamped nearest.
    render_nearest(frame, lut, viewport, out, dst);
    let xs_in = inner_axis(
        viewport.origin.0,
        viewport.step,
        out.x,
        out.width,
        frame.width,
    );
    let ys_in = inner_axis(
        viewport.origin.1,
        viewport.step,
        out.y,
        out.height,
        frame.height,
    );
    if xs_in.is_empty() || ys_in.is_empty() {
        return Ok(());
    }
    // Device pixels fully over the image: a box mean of exactly their spans.
    let (left, top) = viewport.to_source((xs_in.start as f64, ys_in.start as f64));
    let (width, height) = (xs_in.len(), ys_in.len());
    let size = width * height * pixel_type.size();
    scratch.resized.resize(size, 0);
    let mut resized = Image::from_slice_u8(
        width as u32,
        height as u32,
        &mut scratch.resized,
        pixel_type,
    )
    .map_err(|_| RenderError::DstLength {
        expected: size,
        actual: 0,
    })?;
    let step = viewport.step;
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Box))
        .use_alpha(false)
        .crop(left, top, width as f64 * step, height as f64 * step);
    scratch
        .resizer
        .get_or_insert_with(Resizer::new)
        .resize(&src, &mut resized, &options)
        .map_err(|_| RenderError::InvalidViewport)?;
    let resized = ColorFrame {
        pixels: match frame.pixels {
            ColorPixels::U8 { channels, .. } => ColorPixels::U8 {
                data: &scratch.resized,
                channels,
            },
            ColorPixels::U16(_) => ColorPixels::U16(
                zerocopy::FromBytes::ref_from_bytes(&scratch.resized[..]).unwrap_or(&[]),
            ),
        },
        width,
        height,
    };
    let place = |range: &std::ops::Range<usize>, start: usize, len: usize| -> Vec<Option<usize>> {
        (start..start + len)
            .map(|i| range.contains(&i).then(|| i - range.start))
            .collect()
    };
    let xs = place(&xs_in, out.x, out.width);
    let ys = place(&ys_in, out.y, out.height);
    map_pixels(resized, lut, &xs, &ys, dst);
    Ok(())
}

/// What a colour render reads: the frame's own samples, or a debayered
/// image.
pub enum ColorInput<'a> {
    Samples(ColorFrame<'a>),
    Debayered(Arc<Rgb16>),
}

impl ColorInput<'_> {
    pub fn frame(&self) -> ColorFrame<'_> {
        match self {
            Self::Samples(frame) => *frame,
            Self::Debayered(image) => ColorFrame {
                pixels: ColorPixels::U16(&image.data),
                width: image.width,
                height: image.height,
            },
        }
    }
}

/// The colour input for `layout` and the viewport it is seen through: the
/// frame's own samples, or for a Bayer frame zoomed out by 2 or more its
/// half-resolution superpixel image (cached in `scratch` by `key`). `None`
/// for a Bayer frame closer in, which [`render_mosaic`] draws.
pub fn color_input<'a>(
    pixels: Pixels<'a>,
    layout: &FrameLayout,
    viewport: &Viewport,
    key: usize,
    scratch: &mut ColorScratch,
) -> Option<(ColorInput<'a>, Viewport)> {
    let (w, h) = (layout.width, layout.height);
    let Some(tile) = bayer_tile(layout.format) else {
        let Pixels::U8(data) = pixels else {
            return None;
        };
        let frame = ColorFrame {
            pixels: ColorPixels::U8 {
                data,
                channels: layout.channels,
            },
            width: w,
            height: h,
        };
        return Some((ColorInput::Samples(frame), *viewport));
    };
    if viewport.step < 2.0 {
        return None;
    }
    let cached = scratch
        .debayered
        .as_ref()
        .filter(|(k, _)| *k == key)
        .map(|(_, image)| image.clone());
    let image = match cached {
        Some(image) => image,
        None => {
            let cfa: Vec<u16> = match pixels {
                Pixels::U8(p) => p.iter().map(|&v| u16::from(v)).collect(),
                Pixels::U16(p) => p.iter().map(|v| v.get()).collect(),
            };
            let image = Arc::new(superpixel(&cfa, w, h, tile));
            scratch.debayered = Some((key, image.clone()));
            image
        }
    };
    let seen = Viewport {
        origin: (viewport.origin.0 / 2.0, viewport.origin.1 / 2.0),
        step: viewport.step / 2.0,
    };
    Some((ColorInput::Debayered(image), seen))
}

/// A Bayer mosaic read in place, one sample per pixel.
#[derive(Clone, Copy)]
pub enum Mosaic<'a> {
    U8(&'a [u8]),
    U16(&'a [zerocopy::byteorder::U16<zerocopy::byteorder::LE>]),
}

impl Mosaic<'_> {
    fn get(&self, i: usize) -> u32 {
        match self {
            Self::U8(p) => p.get(i).map_or(0, |&v| u32::from(v)),
            Self::U16(p) => p.get(i).map_or(0, |v| u32::from(v.get())),
        }
    }
}

/// `i + delta` on an axis of `len`, mirrored at the edges so the neighbour
/// keeps the Bayer parity of the one beyond the edge.
fn neighbour(i: usize, delta: isize, len: usize) -> usize {
    let j = i as isize + delta;
    match (0..len as isize).contains(&j) {
        true => j as usize,
        false => (i as isize - delta).clamp(0, len as isize - 1) as usize,
    }
}

/// Bilinear RGB at `(x, y)`: the pixel's own sample for its colour, and the
/// mean of its 3×3 neighbours of each other colour.
pub fn demosaic_at(
    mosaic: Mosaic<'_>,
    (width, height): (usize, usize),
    tile: [Site; 4],
    x: usize,
    y: usize,
) -> [u32; 3] {
    let index = |site: Site| match site {
        Site::Red => 0,
        Site::Green => 1,
        Site::Blue => 2,
    };
    let site_of = |x: usize, y: usize| tile[(y % 2) * 2 + x % 2];
    let own = index(site_of(x, y));
    let mut sum = [0u32; 3];
    let mut count = [0u32; 3];
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (nx, ny) = (neighbour(x, dx, width), neighbour(y, dy, height));
            let c = index(site_of(nx, ny));
            if c != own {
                sum[c] += mosaic.get(ny * width + nx);
                count[c] += 1;
            }
        }
    }
    let mut rgb = [0u32; 3];
    for (c, value) in rgb.iter_mut().enumerate() {
        *value = match c == own {
            true => mosaic.get(y * width + x),
            false => sum[c] / count[c].max(1),
        };
    }
    rgb
}

/// A mosaic sample as a code.
trait CfaSample: Copy {
    fn value(self) -> u32;
}

impl CfaSample for u8 {
    fn value(self) -> u32 {
        u32::from(self)
    }
}

impl CfaSample for zerocopy::byteorder::U16<zerocopy::byteorder::LE> {
    fn value(self) -> u32 {
        u32::from(self.get())
    }
}

/// One output row per `ys` entry: interior pixels read their row and the
/// rows above and below directly; border pixels go through the mirrored
/// [`demosaic_at`].
#[allow(clippy::too_many_arguments)]
fn mosaic_rows<T: CfaSample>(
    data: &[T],
    mosaic: Mosaic<'_>,
    (w, h): (usize, usize),
    tile: [Site; 4],
    lut: &ChannelLut,
    xs: &[Option<usize>],
    ys: &[Option<usize>],
    dst: &mut [u8],
) {
    let row_of = |y: usize| data.get(y * w..(y + 1) * w).unwrap_or(&[]);
    let stride = xs.len().max(1) * 4;
    let mut previous: Option<(usize, usize)> = None;
    for (j, y) in ys.iter().enumerate() {
        let line = j * stride..(j + 1) * stride;
        let Some(y) = *y else {
            dst[line].fill(0);
            continue;
        };
        // Zoomed in, neighbouring rows often sample the same source row.
        if let Some((prev_j, _)) = previous.filter(|&(_, prev_y)| prev_y == y) {
            dst.copy_within(prev_j * stride..(prev_j + 1) * stride, line.start);
            continue;
        }
        previous = Some((j, y));
        let interior_row = y >= 1 && y + 1 < h;
        let (up, mid, down) = match interior_row {
            true => (row_of(y - 1), row_of(y), row_of(y + 1)),
            false => (&[][..], &[][..], &[][..]),
        };
        // And neighbouring pixels the same source pixel.
        let mut last: Option<(usize, [u8; 4])> = None;
        for (px, x) in dst[line].as_chunks_mut::<4>().0.iter_mut().zip(xs) {
            let Some(x) = *x else {
                *px = [0; 4];
                continue;
            };
            if let Some((_, bgra)) = last.filter(|&(last_x, _)| last_x == x) {
                *px = bgra;
                continue;
            }
            let rgb = match interior_row && x >= 1 && x + 1 < w {
                true => {
                    let own = tile[(y % 2) * 2 + x % 2];
                    let beside = tile[(y % 2) * 2 + (x + 1) % 2];
                    interior(up, mid, down, x, own, beside)
                }
                false => demosaic_at(mosaic, (w, h), tile, x, y),
            };
            *px = [lut.get(rgb[2]), lut.get(rgb[1]), lut.get(rgb[0]), 0xff];
            last = Some((x, *px));
        }
    }
}

/// Bilinear RGB of an interior pixel of colour `own`, whose right-hand
/// neighbour is `beside`.
fn interior<T: CfaSample>(
    up: &[T],
    mid: &[T],
    down: &[T],
    x: usize,
    own: Site,
    beside: Site,
) -> [u32; 3] {
    let window = |row: &[T]| -> [u32; 3] {
        match row.get(x - 1..x + 2) {
            Some(&[a, b, c]) => [a.value(), b.value(), c.value()],
            _ => [0; 3],
        }
    };
    let ([ul, u, ur], [l, centre, r], [dl, d, dr]) = (window(up), window(mid), window(down));
    let cross = (l + r + u + d) / 4;
    let diagonal = (ul + ur + dl + dr) / 4;
    let across = (l + r) / 2;
    let along = (u + d) / 2;
    match (own, beside) {
        (Site::Red, _) => [centre, cross, diagonal],
        (Site::Blue, _) => [diagonal, cross, centre],
        (Site::Green, Site::Red) => [across, centre, along],
        (Site::Green, _) => [along, centre, across],
    }
}

/// A Bayer frame closer in than 2:1: each device pixel demosaics the one
/// source pixel nearest sampling picks, so the cost follows the screen.
pub fn render_mosaic(
    pixels: Pixels<'_>,
    layout: &FrameLayout,
    lut: &ChannelLut,
    viewport: &Viewport,
    out: Rect,
    dst: &mut [u8],
) -> Result<(), RenderError> {
    let tile = bayer_tile(layout.format).ok_or(RenderError::Unsupported(layout.format))?;
    let mosaic = match pixels {
        Pixels::U8(p) => Mosaic::U8(p),
        Pixels::U16(p) => Mosaic::U16(p),
    };
    let expected = out.width * out.height * 4;
    if dst.len() != expected {
        return Err(RenderError::DstLength {
            expected,
            actual: dst.len(),
        });
    }
    if out.is_empty() {
        return Ok(());
    }
    let size = (layout.width, layout.height);
    let xs = nearest_axis(viewport, out.x, out.width, size.0, 0);
    let ys = nearest_axis(viewport, out.y, out.height, size.1, 1);
    match mosaic {
        Mosaic::U8(data) => mosaic_rows(data, mosaic, size, tile, lut, &xs, &ys, dst),
        Mosaic::U16(data) => mosaic_rows(data, mosaic, size, tile, lut, &xs, &ys, dst),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::image_view::lut::Curve;

    fn identity() -> ChannelLut {
        ChannelLut::build(8, 0.0, 255.0, Curve::Linear)
    }

    fn view(x: f64, y: f64, step: f64) -> Viewport {
        Viewport {
            origin: (x, y),
            step,
        }
    }

    fn render(frame: ColorFrame<'_>, viewport: Viewport, out: (usize, usize)) -> Vec<u8> {
        let out = viewport.visible(frame.width, frame.height, out.0, out.1);
        let mut dst = vec![0u8; out.width * out.height * 4];
        let mut scratch = ColorScratch::default();
        render_color(frame, &identity(), &viewport, out, &mut dst, &mut scratch).unwrap();
        dst
    }

    #[test]
    fn rgb_one_to_one_is_the_identity_but_for_the_swizzle() {
        let data: Vec<u8> = (0..4 * 3 * 3).map(|v| (v * 7) as u8).collect();
        let frame = ColorFrame {
            pixels: ColorPixels::U8 {
                data: &data,
                channels: 3,
            },
            width: 4,
            height: 3,
        };
        let dst = render(frame, view(0.0, 0.0, 1.0), (4, 3));
        for (px, rgb) in dst.as_chunks::<4>().0.iter().zip(data.as_chunks::<3>().0) {
            assert_eq!(*px, [rgb[2], rgb[1], rgb[0], 255]);
        }
    }

    #[test]
    fn zooming_out_averages_and_ignores_alpha() {
        let data: Vec<u8> = (0..8 * 8).flat_map(|_| [200, 100, 50, 7]).collect();
        let frame = ColorFrame {
            pixels: ColorPixels::U8 {
                data: &data,
                channels: 4,
            },
            width: 8,
            height: 8,
        };
        let dst = render(frame, view(0.0, 0.0, 4.0), (2, 2));
        assert_eq!(dst.len(), 2 * 2 * 4);
        for px in dst.as_chunks::<4>().0 {
            assert_eq!(*px, [50, 100, 200, 255]);
        }
    }

    #[test]
    fn superpixel_blocks_match_their_tiles() {
        // A 4×2 RGGB mosaic: two tiles.
        let cfa = [10u16, 20, 30, 40, 50, 60, 70, 80];
        let tile = bayer_tile(ImageFormat::BayerRggb16).unwrap();
        let rgb = superpixel(&cfa, 4, 2, tile);
        assert_eq!((rgb.width, rgb.height), (2, 1));
        assert_eq!(rgb.data, [10, (20 + 50) / 2, 60, 30, (40 + 70) / 2, 80]);
        let bggr = superpixel(&cfa, 4, 2, bayer_tile(ImageFormat::BayerBggr8).unwrap());
        assert_eq!(&bggr.data[..3], [60, (20 + 50) / 2, 10]);
        let grbg = superpixel(&cfa, 4, 2, bayer_tile(ImageFormat::BayerGrbg16).unwrap());
        assert_eq!(&grbg.data[..3], [20, (10 + 60) / 2, 50]);
        // Odd edges drop the incomplete tile.
        assert_eq!(superpixel(&[1, 2, 3], 3, 1, tile).data, Vec::<u16>::new());
    }

    #[test]
    fn sites_follow_the_tile() {
        assert_eq!(site(ImageFormat::BayerRggb8, 0, 0), Some(Site::Red));
        assert_eq!(site(ImageFormat::BayerRggb8, 1, 1), Some(Site::Blue));
        assert_eq!(site(ImageFormat::BayerGbrg16, 0, 1), Some(Site::Red));
        assert_eq!(site(ImageFormat::Mono16, 0, 0), None);
        assert!(is_color(ImageFormat::Rgba8) && is_color(ImageFormat::BayerBggr16));
        assert!(!is_color(ImageFormat::Mono16));
    }

    type Le16 = zerocopy::byteorder::U16<zerocopy::byteorder::LE>;

    /// An RGGB mosaic of a flat colour.
    fn flat(w: usize, h: usize, rgb: [u16; 3]) -> Vec<Le16> {
        (0..w * h)
            .map(|i| match site(ImageFormat::BayerRggb16, i % w, i / w) {
                Some(Site::Red) => rgb[0],
                Some(Site::Green) => rgb[1],
                _ => rgb[2],
            })
            .map(Le16::new)
            .collect()
    }

    #[test]
    fn a_flat_mosaic_demosaics_flat_up_to_the_borders() {
        let tile = bayer_tile(ImageFormat::BayerRggb16).unwrap();
        for (w, h) in [(8, 8), (7, 5), (2, 2), (3, 2)] {
            let cfa = flat(w, h, [4000, 2000, 1000]);
            for y in 0..h {
                for x in 0..w {
                    let rgb = demosaic_at(Mosaic::U16(&cfa), (w, h), tile, x, y);
                    assert_eq!(rgb, [4000, 2000, 1000], "{w}x{h} ({x}, {y})");
                }
            }
        }
        // Too small to hold every colour: no panic, and a missing colour is 0.
        let tiny = flat(1, 1, [4000, 2000, 1000]);
        assert_eq!(
            demosaic_at(Mosaic::U16(&tiny), (1, 1), tile, 0, 0),
            [4000, 0, 0]
        );
    }

    #[test]
    fn interior_pixels_are_bilinear() {
        let tile = bayer_tile(ImageFormat::BayerRggb16).unwrap();
        let values: Vec<u16> = (0..6 * 6).map(|v| v as u16 * 10).collect();
        let cfa: Vec<Le16> = values.iter().map(|&v| Le16::new(v)).collect();
        let at = |x: usize, y: usize| u32::from(values[y * 6 + x]);
        // (2, 2) is red: green from the cross, blue from the diagonals.
        let rgb = demosaic_at(Mosaic::U16(&cfa), (6, 6), tile, 2, 2);
        assert_eq!(rgb[0], at(2, 2));
        assert_eq!(rgb[1], (at(1, 2) + at(3, 2) + at(2, 1) + at(2, 3)) / 4);
        assert_eq!(rgb[2], (at(1, 1) + at(3, 1) + at(1, 3) + at(3, 3)) / 4);
        // (3, 2) is green on a red row: red from left/right, blue from up/down.
        let rgb = demosaic_at(Mosaic::U16(&cfa), (6, 6), tile, 3, 2);
        assert_eq!(
            rgb,
            [
                (at(2, 2) + at(4, 2)) / 2,
                at(3, 2),
                (at(3, 1) + at(3, 3)) / 2
            ]
        );
        let u8s: Vec<u8> = (0..36).collect();
        assert_eq!(demosaic_at(Mosaic::U8(&u8s), (6, 6), tile, 2, 2)[0], 14);
    }

    #[test]
    fn the_interior_fast_path_matches_the_general_one() {
        let (w, h) = (9, 8);
        let values: Vec<Le16> = (0..w * h)
            .map(|i| Le16::new((i * 37 % 4096) as u16))
            .collect();
        for format in [
            ImageFormat::BayerRggb16,
            ImageFormat::BayerGrbg16,
            ImageFormat::BayerGbrg16,
            ImageFormat::BayerBggr16,
        ] {
            let tile = bayer_tile(format).unwrap();
            for y in 1..h - 1 {
                let row = |y: usize| &values[y * w..(y + 1) * w];
                for x in 1..w - 1 {
                    let own = tile[(y % 2) * 2 + x % 2];
                    let beside = tile[(y % 2) * 2 + (x + 1) % 2];
                    let fast = interior(row(y - 1), row(y), row(y + 1), x, own, beside);
                    let general = demosaic_at(Mosaic::U16(&values), (w, h), tile, x, y);
                    assert_eq!(fast, general, "{format:?} ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn mosaic_rendering_handles_odd_offsets_and_borders() {
        let (w, h) = (9, 7);
        let cfa = flat(w, h, [4000, 2000, 1000]);
        let layout = crate::views::image_view::FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[h, w][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(ImageFormat::BayerRggb16, 12),
        )
        .unwrap();
        let lut = ChannelLut::build(12, 0.0, 4095.0, Curve::Linear);
        let want = [lut.get(1000), lut.get(2000), lut.get(4000), 255];
        for (origin, step) in [
            ((0.0, 0.0), 1.0),
            ((-3.5, 1.25), 0.37),
            ((7.1, 5.9), 0.5),
            ((1.0, 1.0), 1.9),
            ((-0.3, -0.7), 1.3),
        ] {
            let viewport = view(origin.0, origin.1, step);
            let out = viewport.visible(w, h, 40, 40);
            let mut dst = vec![0u8; out.width * out.height * 4];
            render_mosaic(Pixels::U16(&cfa), &layout, &lut, &viewport, out, &mut dst).unwrap();
            assert!(!dst.is_empty());
            for px in dst.as_chunks::<4>().0 {
                assert_eq!(*px, want, "{origin:?} {step}");
            }
        }
        let mut short = vec![0u8; 3];
        let out = Rect::new(0, 0, 2, 2);
        let viewport = view(0.0, 0.0, 1.0);
        assert!(
            render_mosaic(Pixels::U16(&cfa), &layout, &lut, &viewport, out, &mut short).is_err()
        );
    }

    #[test]
    fn the_channel_lut_follows_the_levels() {
        let lut = ChannelLut::build(12, 1000.0, 3000.0, Curve::Linear);
        assert_eq!(
            (
                lut.get(0),
                lut.get(1000),
                lut.get(2000),
                lut.get(3000),
                lut.get(9999)
            ),
            (0, 0, 128, 255, 255)
        );
        let flat = ChannelLut::build(8, f64::NAN, 1.0, Curve::Linear);
        assert_eq!((flat.get(0), flat.get(255)), (0, 255));
    }

    #[test]
    fn an_impulse_lands_on_the_same_device_pixel_in_mono_and_rgb() {
        use crate::views::image_view::resample::{Plane, Scratch, render_into};
        use crate::views::image_view::{Display, FrameLayout, Lut};
        let (w, h) = (16, 16);
        let viewport = view(-3.0, -3.0, 4.0);
        let out = viewport.visible(w, h, 10, 10);
        let mut mono = vec![0u8; w * h];
        mono[5 * w + 9] = 255;
        let rgb: Vec<u8> = mono.iter().flat_map(|&v| [v, v, v]).collect();
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U8, &[h, w][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam").with_image(ImageFormat::Mono8, 8),
        )
        .unwrap();
        let mut mono_dst = vec![0u8; out.width * out.height * 4];
        let plane = Plane {
            pixels: Pixels::U8(&mono),
            layout: &layout,
        };
        let lut = Lut::build(8, 0.0, 255.0, &Display::default(), [0; 4]);
        let rows = 0..out.height;
        render_into(
            plane,
            &lut,
            &viewport,
            out,
            rows,
            &mut mono_dst,
            &mut Scratch::default(),
        )
        .unwrap();
        let frame = ColorFrame {
            pixels: ColorPixels::U8 {
                data: &rgb,
                channels: 3,
            },
            width: w,
            height: h,
        };
        let mut rgb_dst = vec![0u8; out.width * out.height * 4];
        let mut scratch = ColorScratch::default();
        render_color(
            frame,
            &identity(),
            &viewport,
            out,
            &mut rgb_dst,
            &mut scratch,
        )
        .unwrap();
        let brightest = |dst: &[u8]| {
            let reds: Vec<u8> = dst.as_chunks::<4>().0.iter().map(|px| px[2]).collect();
            let max = *reds.iter().max().unwrap();
            assert!(max > 0);
            let i = reds.iter().position(|&r| r == max).unwrap();
            (i % out.width, i / out.width)
        };
        assert_eq!(brightest(&mono_dst), brightest(&rgb_dst));
        // Span of device column 3 is source 9..13.
        assert_eq!(brightest(&rgb_dst), (3 - out.x, 2 - out.y));
    }

    #[test]
    fn an_empty_mosaic_output_returns_early() {
        let cfa = flat(4, 4, [1, 2, 3]);
        let layout = crate::views::image_view::FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[4usize, 4][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(ImageFormat::BayerRggb16, 12),
        )
        .unwrap();
        let lut = ChannelLut::build(12, 0.0, 4095.0, Curve::Linear);
        let out = Rect::new(0, 0, 0, 3);
        assert!(
            render_mosaic(
                Pixels::U16(&cfa),
                &layout,
                &lut,
                &view(0.0, 0.0, 1.0),
                out,
                &mut []
            )
            .is_ok()
        );
    }
}
