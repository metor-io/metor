//! Turning a frame into a device-resolution `RenderImage`: the render key,
//! one-job-at-a-time scheduling, the job itself, and frame retirement.

use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame as ImageFrame, ImageBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::Stretch;
use smallvec::SmallVec;

use super::color::{ChannelLut, color_input, is_color, render_color, render_mosaic};
use super::histogram::{Histogram, RoiStats, finite_range, roi_stats};
use super::layout::{FrameLayout, Pixels};
use super::lut::{Colormap, Curve, Display, F32_BITS, Levels, Lut};
use super::resample::{Plane, Rect, RenderError, Scratch, Viewport, render_into};
use super::source::Frame;

/// Histogram bins for a `mono_f32` frame.
const F32_BINS: usize = 1 << F32_BITS;
/// Softening for a producer's `asinh` stretch hint.
const ASINH_SOFTENING: f32 = 0.1;

/// Everything a rendered image depends on. A job runs only when this
/// changes.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderKey {
    pub timestamp: Timestamp,
    /// The frame buffer's address: a new sample at the same timestamp is a
    /// new key.
    pub bytes: usize,
    pub display: Display,
    pub viewport: Viewport,
    pub roi: Option<Rect>,
    /// Whether the histogram strip wants this frame's histogram.
    pub histogram: bool,
    /// Device pixels of the whole output.
    pub out: (usize, usize),
    /// BGRA of the theme's saturation colour.
    pub saturated: [u8; 4],
}

impl RenderKey {
    pub fn new(
        frame: &Frame,
        display: Display,
        viewport: Viewport,
        out: (usize, usize),
        saturated: [u8; 4],
    ) -> Self {
        Self {
            timestamp: frame.timestamp,
            bytes: frame.bytes.as_ptr() as usize,
            display,
            viewport,
            roi: None,
            histogram: false,
            out,
            saturated,
        }
    }
}

/// At most one job in flight; a key that changes meanwhile starts the next
/// job when the current one finishes. Each key travels with the input `F`
/// it was made from, so a started job always has its input.
#[derive(Debug)]
pub struct Scheduler<F> {
    wanted: Option<(RenderKey, F)>,
    in_flight: Option<RenderKey>,
    installed: Option<RenderKey>,
}

impl<F> Default for Scheduler<F> {
    fn default() -> Self {
        Self {
            wanted: None,
            in_flight: None,
            installed: None,
        }
    }
}

impl<F: Clone> Scheduler<F> {
    /// Records `key` as wanted; returns it when a job should start now.
    pub fn request(&mut self, key: RenderKey, input: F) -> Option<(RenderKey, F)> {
        let unchanged = self.wanted.as_ref().is_some_and(|(k, _)| *k == key);
        self.wanted = Some((key, input));
        if unchanged || self.in_flight.is_some() {
            return None;
        }
        self.start_wanted()
    }

    /// Marks the job for `key` done. Returns whether its result should be
    /// installed, and the next job to start.
    pub fn finish(&mut self, key: &RenderKey) -> (bool, Option<(RenderKey, F)>) {
        self.in_flight = None;
        let install = self.wanted.as_ref().is_some_and(|(k, _)| k == key);
        if install {
            self.installed = Some(key.clone());
        }
        (install, self.start_wanted())
    }

    pub fn installed(&self) -> Option<&RenderKey> {
        self.installed.as_ref()
    }

    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Forgets the wanted and installed keys; a job in flight still
    /// finishes, uninstalled.
    pub fn reset(&mut self) {
        self.wanted = None;
        self.installed = None;
    }

    fn start_wanted(&mut self) -> Option<(RenderKey, F)> {
        let (key, input) = self.wanted.clone()?;
        if self.installed.as_ref() == Some(&key) {
            return None;
        }
        self.in_flight = Some(key.clone());
        Some((key, input))
    }
}

/// The source-centred viewport that fits the whole frame in `out`.
pub fn fit(layout: &FrameLayout, out: (usize, usize)) -> Viewport {
    let (w, h) = (layout.width as f64, layout.height as f64);
    let (dw, dh) = (out.0.max(1) as f64, out.1.max(1) as f64);
    let step = (w / dw).max(h / dh).max(f64::MIN_POSITIVE);
    Viewport {
        origin: (w / 2.0 - dw / 2.0 * step, h / 2.0 - dh / 2.0 * step),
        step,
    }
}

/// The source pixels under `out`, clipped to the frame.
pub fn source_rect(viewport: &Viewport, out: Rect, layout: &FrameLayout) -> Rect {
    let axis = |origin: f64, start: usize, len: usize, limit: usize| {
        let a = (origin + start as f64 * viewport.step).floor().max(0.0) as usize;
        let b = (origin + (start + len) as f64 * viewport.step)
            .ceil()
            .max(0.0) as usize;
        let (a, b) = (a.min(limit), b.min(limit));
        (a, b.saturating_sub(a))
    };
    let (x, w) = axis(viewport.origin.0, out.x, out.width, layout.width);
    let (y, h) = axis(viewport.origin.1, out.y, out.height, layout.height);
    Rect::new(x, y, w, h)
}

/// The display a fresh view starts from: the producer's level and stretch
/// hints over the defaults.
pub fn hinted(display: Display, layout: &FrameLayout) -> Display {
    let levels = match (layout.black, layout.white) {
        (Some(black), Some(white)) => Levels::Manual { black, white },
        _ => display.levels,
    };
    let curve = match layout.stretch {
        Stretch::Linear => Curve::Linear,
        Stretch::Sqrt => Curve::Sqrt,
        Stretch::Log => Curve::Log,
        Stretch::Asinh => Curve::Asinh {
            softening: ASINH_SOFTENING,
        },
        Stretch::Auto => display.curve,
    };
    Display {
        levels,
        curve,
        ..display
    }
}

/// The histogram of `region`: integer codes, or 4096 bins over the finite
/// range of a float frame.
pub fn histogram(pixels: Pixels<'_>, layout: &FrameLayout, region: Rect) -> Histogram {
    let w = layout.width;
    match pixels {
        Pixels::U8(p) if layout.channels > 1 => {
            Histogram::of_u8_color(p, w, layout.channels, region)
        }
        Pixels::U8(p) => Histogram::of_u8(p, w, region),
        Pixels::U16(p) => Histogram::of_u16(p, w, layout.bits, region),
        Pixels::F32(p) => {
            let (lo, hi) = finite_range(p, w, region).unwrap_or((0.0, 1.0));
            Histogram::of_f32(p, w, region, lo, hi, F32_BINS)
        }
    }
}

/// Black and white in sample units. Auto levels with no samples fall back
/// to the element's full range.
pub fn levels(
    display: &Display,
    histogram: Option<&Histogram>,
    layout: &FrameLayout,
) -> (f64, f64) {
    let full = match layout.format.element() {
        metor_proto::types::PrimType::F32 => (0.0, 1.0),
        _ => (0.0, f64::from((1u32 << layout.bits.clamp(1, 16)) - 1)),
    };
    match display.levels {
        Levels::Manual { black, white } => (black, white),
        Levels::Equalize => histogram.map_or(full, Histogram::range),
        Levels::Auto { low, high, .. } => {
            let levels = histogram
                .and_then(|h| {
                    let black = h.value(h.percentile(low)?);
                    let white = h.value(h.percentile(high)?);
                    Some((black, white))
                })
                .unwrap_or(full);
            let float = layout.format.element() == metor_proto::types::PrimType::F32;
            match display.colormap {
                Colormap::RedBlue if float => symmetric(levels),
                _ => levels,
            }
        }
    }
}

/// Levels centred on zero that cover both of `levels`.
fn symmetric((black, white): (f64, f64)) -> (f64, f64) {
    let reach = black.abs().max(white.abs());
    (-reach, reach)
}

/// What a job needs; everything is owned so it can run off the main thread.
pub struct Job {
    pub key: RenderKey,
    pub frame: Frame,
    pub scratch: Scratch,
    /// The LUT of the previous job and the inputs it was built from.
    pub lut: Option<(LutKey, Arc<Lut>)>,
}

/// The inputs a LUT is built from.
#[derive(Clone, Debug, PartialEq)]
pub struct LutKey {
    pub bits: u8,
    pub display: Display,
    pub levels: (f64, f64),
    /// The histogram an equalised LUT follows.
    pub histogram: usize,
}

pub struct Rendered {
    pub key: RenderKey,
    /// `None` when the frame is out of view or the render failed.
    pub image: Option<(Arc<RenderImage>, Rect)>,
    pub error: Option<RenderError>,
    pub levels: (f64, f64),
    /// Of the region auto levels use; `None` when neither auto levels nor
    /// the strip asked for it.
    pub histogram: Option<Arc<Histogram>>,
    /// Statistics of the ROI, when one is set and inside the frame.
    pub roi_stats: Option<RoiStats>,
    /// The ROI fell outside this frame and was ignored.
    pub roi_outside: bool,
    pub scratch: Scratch,
    pub lut: Option<(LutKey, Arc<Lut>)>,
}

/// Runs one render: histogram, levels, LUT, resample, image.
pub fn run(job: Job) -> Rendered {
    let Job {
        key,
        frame,
        mut scratch,
        lut,
    } = job;
    let layout = frame.layout;
    let pixels = match layout.pixels(&frame.bytes) {
        Ok(pixels) => pixels,
        Err(err) => {
            return Rendered {
                key,
                image: None,
                error: Some(RenderError::Layout(err)),
                levels: (0.0, 0.0),
                histogram: None,
                roi_stats: None,
                roi_outside: false,
                scratch,
                lut,
            };
        }
    };
    let out = key
        .viewport
        .visible(layout.width, layout.height, key.out.0, key.out.1);
    let roi = key
        .roi
        .map(|r| r.clip(layout.width, layout.height))
        .filter(|r| !r.is_empty());
    let roi_outside = key.roi.is_some() && roi.is_none();
    let roi_stats = roi.and_then(|r| roi_stats(pixels, &layout, r));
    let region = roi.unwrap_or_else(|| source_rect(&key.viewport, out, &layout));
    let hist = (key.histogram || !matches!(key.display.levels, Levels::Manual { .. }))
        .then(|| Arc::new(histogram(pixels, &layout, region)));
    let levels = levels(&key.display, hist.as_deref(), &layout);
    let lut = cached_lut(lut, &layout, &key, levels, hist.as_ref());
    if out.is_empty() {
        return Rendered {
            key,
            image: None,
            error: None,
            levels,
            histogram: hist,
            roi_stats,
            roi_outside,
            scratch,
            lut: Some(lut),
        };
    }
    let mut bgra = vec![0u8; out.width * out.height * 4];
    let drawn = match is_color(layout.format) {
        true => render_color_frame(
            pixels,
            &layout,
            &key,
            levels,
            out,
            &frame,
            hist.as_deref(),
            &mut bgra,
            &mut scratch,
        ),
        false => {
            let plane = Plane {
                pixels,
                layout: &layout,
            };
            render_into(
                plane,
                &lut.1,
                &key.viewport,
                out,
                0..out.height,
                &mut bgra,
                &mut scratch,
            )
        }
    }
    .and_then(|()| image(bgra, out).map(|image| (image, out)));
    let (image, error) = match drawn {
        Ok(image) => (Some(image), None),
        Err(err) => (None, Some(err)),
    };
    Rendered {
        key,
        image,
        error,
        levels,
        histogram: hist,
        roi_stats,
        roi_outside,
        scratch,
        lut: Some(lut),
    }
}

/// A colour frame (RGB or Bayer) through the channel LUT.
#[allow(clippy::too_many_arguments)]
fn render_color_frame(
    pixels: Pixels<'_>,
    layout: &FrameLayout,
    key: &RenderKey,
    (black, white): (f64, f64),
    out: Rect,
    frame: &Frame,
    histogram: Option<&Histogram>,
    bgra: &mut [u8],
    scratch: &mut Scratch,
) -> Result<(), RenderError> {
    let lut = match (key.display.levels, histogram) {
        (Levels::Equalize, Some(histogram)) => {
            ChannelLut::equalized(layout.bits, histogram, key.display.curve)
        }
        _ => ChannelLut::build(layout.bits, black, white, key.display.curve),
    };
    let bytes = frame.bytes.as_ptr() as usize;
    let color = &mut scratch.color;
    match color_input(pixels, layout, &key.viewport, bytes, color) {
        Some((input, seen)) => render_color(input.frame(), &lut, &seen, out, bgra, color),
        None => render_mosaic(pixels, layout, &lut, &key.viewport, out, bgra),
    }
}

fn cached_lut(
    cached: Option<(LutKey, Arc<Lut>)>,
    layout: &FrameLayout,
    key: &RenderKey,
    levels: (f64, f64),
    histogram: Option<&Arc<Histogram>>,
) -> (LutKey, Arc<Lut>) {
    let equalize = histogram.filter(|_| key.display.levels == Levels::Equalize);
    let want = LutKey {
        bits: layout.bits,
        display: key.display,
        levels,
        histogram: equalize.map_or(0, |h| Arc::as_ptr(h) as usize),
    };
    if let Some(cached) = cached.filter(|(k, _)| *k == want) {
        return cached;
    }
    let (black, white) = levels;
    let lut = match layout.format.element() {
        metor_proto::types::PrimType::F32 => {
            Lut::build_f32(black as f32, white as f32, &key.display)
        }
        _ => Lut::build(layout.bits, black, white, &key.display, key.saturated),
    };
    let lut = match equalize {
        Some(histogram) => lut.equalized(histogram, &key.display, key.saturated),
        None => lut,
    };
    (want, Arc::new(lut))
}

fn image(bgra: Vec<u8>, out: Rect) -> Result<Arc<RenderImage>, RenderError> {
    let actual = bgra.len();
    let buffer = ImageBuffer::from_raw(out.width as u32, out.height as u32, bgra).ok_or(
        RenderError::DstLength {
            expected: out.width * out.height * 4,
            actual,
        },
    )?;
    Ok(Arc::new(RenderImage::new(SmallVec::from_elem(
        ImageFrame::new(buffer),
        1,
    ))))
}

/// The painted image and the ones waiting for `drop_image`.
#[derive(Default)]
pub struct Frames {
    pub current: Option<(Arc<RenderImage>, Rect)>,
    pub pending_release: Vec<Arc<RenderImage>>,
}

impl Frames {
    /// Replaces the painted image; the old one waits for the next paint.
    pub fn install(&mut self, image: Option<(Arc<RenderImage>, Rect)>) {
        if let Some((old, _)) = std::mem::replace(&mut self.current, image) {
            self.pending_release.push(old);
        }
    }

    /// Every image still in the atlas or about to be, for release.
    pub fn take_all(&mut self) -> Vec<Arc<RenderImage>> {
        let mut all = std::mem::take(&mut self.pending_release);
        all.extend(self.current.take().map(|(image, _)| image));
        all
    }
}

#[cfg(test)]
mod tests {
    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;
    use metor_proto_wkt::{ComponentMetadata, ImageFormat};

    use super::*;

    fn layout(w: usize, h: usize) -> FrameLayout {
        FrameLayout::from_component(
            &ComponentSchema::new(PrimType::U16, &[h, w][..]),
            &ComponentMetadata::from("cam").with_image(ImageFormat::Mono16, 12),
        )
        .unwrap()
    }

    fn frame(w: usize, h: usize, value: impl Fn(usize, usize) -> u16) -> Frame {
        let bytes: Vec<u8> = (0..w * h)
            .flat_map(|i| value(i % w, i / w).to_le_bytes())
            .collect();
        Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
            layout: layout(w, h),
        }
    }

    fn key(n: i64) -> RenderKey {
        RenderKey {
            timestamp: Timestamp(n),
            bytes: 0,
            display: Display::default(),
            viewport: Viewport {
                origin: (0.0, 0.0),
                step: 1.0,
            },
            roi: None,
            histogram: false,
            out: (10, 10),
            saturated: [0; 4],
        }
    }

    /// The input is the key's timestamp, so each started job can be
    /// checked against its own request.
    fn request(s: &mut Scheduler<i64>, n: i64) -> Option<RenderKey> {
        let (key, input) = s.request(key(n), n)?;
        assert_eq!(key.timestamp.0, input);
        Some(key)
    }

    fn finish(s: &mut Scheduler<i64>, n: i64) -> (bool, Option<RenderKey>) {
        let (install, next) = s.finish(&key(n));
        if let Some((key, input)) = &next {
            assert_eq!(key.timestamp.0, *input);
        }
        (install, next.map(|(key, _)| key))
    }

    #[test]
    fn an_unchanged_key_starts_no_job() {
        let mut s = Scheduler::default();
        assert_eq!(request(&mut s, 1), Some(key(1)));
        assert_eq!(finish(&mut s, 1), (true, None));
        assert_eq!(request(&mut s, 1), None);
        assert!(!s.is_busy());
    }

    #[test]
    fn a_change_during_a_job_makes_one_follow_up() {
        let mut s = Scheduler::default();
        assert_eq!(request(&mut s, 1), Some(key(1)));
        assert_eq!(request(&mut s, 2), None);
        assert_eq!(request(&mut s, 3), None);
        assert_eq!(finish(&mut s, 1), (false, Some(key(3))));
        assert_eq!(finish(&mut s, 3), (true, None));
    }

    #[test]
    fn a_superseded_result_is_not_installed() {
        let mut s = Scheduler::default();
        request(&mut s, 1);
        request(&mut s, 2);
        assert_eq!(finish(&mut s, 1), (false, Some(key(2))));
        // Returning to an already-installed key renders nothing new.
        let mut s = Scheduler::default();
        request(&mut s, 1);
        finish(&mut s, 1);
        request(&mut s, 2);
        request(&mut s, 1);
        assert_eq!(finish(&mut s, 2), (false, None));
    }

    #[test]
    fn fit_centres_and_covers_the_frame() {
        let layout = layout(200, 100);
        let viewport = fit(&layout, (100, 100));
        assert_eq!(viewport.step, 2.0);
        assert_eq!(viewport.origin, (0.0, -50.0));
        assert_eq!(
            viewport.visible(200, 100, 100, 100),
            Rect::new(0, 25, 100, 50)
        );
    }

    #[test]
    fn auto_levels_follow_the_view_and_the_roi() {
        let f = frame(64, 32, |x, _| if x < 32 { 100 } else { 4000 });
        let layout = f.layout;
        let pixels = layout.pixels(&f.bytes).unwrap();
        let display = Display::default();
        // Zoomed onto the left half: the viewport shows columns 0..32.
        let viewport = Viewport {
            origin: (0.0, 0.0),
            step: 1.0,
        };
        let out = viewport.visible(64, 32, 32, 32);
        let left = histogram(pixels, &layout, source_rect(&viewport, out, &layout));
        assert_eq!(levels(&display, Some(&left), &layout), (100.0, 100.0));
        let right = histogram(pixels, &layout, Rect::new(32, 0, 32, 32));
        assert_eq!(levels(&display, Some(&right), &layout), (4000.0, 4000.0));
    }

    #[test]
    fn a_job_renders_the_visible_part() {
        let f = frame(4, 4, |x, y| (x + 4 * y) as u16 * 200);
        let key = RenderKey::new(
            &f,
            Display::default(),
            fit(&f.layout, (8, 8)),
            (8, 8),
            [0; 4],
        );
        let rendered = run(Job {
            key: key.clone(),
            frame: f,
            scratch: Scratch::default(),
            lut: None,
        });
        assert_eq!(rendered.error, None);
        let (image, out) = rendered.image.expect("an image");
        assert_eq!(out, Rect::new(0, 0, 8, 8));
        assert_eq!(image.size(0).width.0, 8);
        assert_eq!(rendered.levels, (0.0, 3000.0));
        assert_eq!(rendered.key, key);
    }

    #[test]
    fn manual_levels_and_hints() {
        let mut layout = layout(4, 4);
        layout.black = Some(10.0);
        layout.white = Some(20.0);
        layout.stretch = Stretch::Asinh;
        let display = hinted(Display::default(), &layout);
        assert_eq!(
            display.levels,
            Levels::Manual {
                black: 10.0,
                white: 20.0
            }
        );
        assert!(matches!(display.curve, Curve::Asinh { .. }));
        assert_eq!(levels(&display, None, &layout), (10.0, 20.0));
        let empty = Histogram::of_u8(&[], 0, Rect::default());
        assert_eq!(
            levels(&Display::default(), Some(&empty), &layout),
            (0.0, 4095.0)
        );
    }

    #[test]
    fn frames_retire_through_pending_release() {
        let image = || image(vec![0; 4], Rect::new(0, 0, 1, 1)).unwrap();
        assert!(super::image(vec![0; 3], Rect::new(0, 0, 1, 1)).is_err());
        let mut frames = Frames::default();
        frames.install(Some((image(), Rect::default())));
        frames.install(Some((image(), Rect::default())));
        frames.install(None);
        assert_eq!(frames.pending_release.len(), 2);
        assert_eq!(frames.take_all().len(), 2);
        assert!(frames.current.is_none());
    }

    /// The whole job (histogram, levels, LUT, resample, `RenderImage`) on a
    /// 1024² Mono16/12 star field.
    /// `cargo test -p metor-panel --release --lib -- --ignored --nocapture
    /// image_view::render::tests::bench_job`
    #[test]
    #[ignore]
    fn bench_job() {
        use crate::views::image_view::synthetic::{Kind, frame as synthetic};

        for kind in [Kind::StarField, Kind::TestCard, Kind::Bayer] {
            let (w, h) = (1024, 1024);
            let layout = FrameLayout::from_component(
                &kind.schema(w, h),
                &ComponentMetadata::from("cam").with_image(kind.format(), kind.bits()),
            )
            .unwrap();
            let frame = Frame {
                timestamp: Timestamp(1),
                bytes: synthetic(kind, w, h, 0).into(),
                layout,
            };
            let mut lut = None;
            let mut scratch = Some(Scratch::default());
            for (name, out) in [
                ("fit 1200x1200", (1200, 1200)),
                ("fit 1024x1024 (1:1)", (1024, 1024)),
                ("fit 700x700", (700, 700)),
                ("fit 400x400", (400, 400)),
                ("fit 2400x1600", (2400, 1600)),
            ] {
                let key = RenderKey::new(
                    &frame,
                    Display::default(),
                    fit(&frame.layout, out),
                    out,
                    [0; 4],
                );
                let mut ms: Vec<f64> = (0..33)
                    .map(|_| {
                        let mut s = scratch.take().unwrap_or_default();
                        // Every frame is new, so nothing debayered is reused.
                        s.color.debayered = None;
                        let start = std::time::Instant::now();
                        let rendered = run(Job {
                            key: key.clone(),
                            frame: frame.clone(),
                            scratch: s,
                            lut: lut.take(),
                        });
                        let elapsed = start.elapsed().as_secs_f64() * 1e3;
                        assert_eq!(rendered.error, None);
                        scratch = Some(rendered.scratch);
                        lut = rendered.lut;
                        elapsed
                    })
                    .skip(3)
                    .collect();
                ms.sort_by(f64::total_cmp);
                println!(
                    "job {:<10} {name:<24} median {:>7.3} ms  min {:>7.3} ms",
                    kind.slug(),
                    ms[15],
                    ms[0]
                );
            }
        }
    }

    fn color_frame(kind: crate::views::image_view::synthetic::Kind, w: usize, h: usize) -> Frame {
        let layout = FrameLayout::from_component(
            &kind.schema(w, h),
            &ComponentMetadata::from("cam").with_image(kind.format(), kind.bits()),
        )
        .unwrap();
        Frame {
            timestamp: Timestamp(1),
            bytes: crate::views::image_view::synthetic::frame(kind, w, h, 3).into(),
            layout,
        }
    }

    fn run_at(frame: &Frame, viewport: Viewport, out: (usize, usize)) -> Rendered {
        let mut key = RenderKey::new(frame, Display::default(), viewport, out, [0; 4]);
        key.display.levels = Levels::Manual {
            black: 0.0,
            white: f64::from((1u32 << frame.layout.bits) - 1),
        };
        run(Job {
            key,
            frame: frame.clone(),
            scratch: Scratch::default(),
            lut: None,
        })
    }

    /// The BGRA painted at device pixel `(x, y)`.
    fn painted(rendered: &Rendered, x: usize, y: usize) -> [u8; 4] {
        let (image, out) = rendered.image.clone().expect("an image");
        let bytes = image.as_bytes(0).unwrap();
        let at = ((y - out.y) * out.width + x - out.x) * 4;
        bytes[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn color_frames_render_their_bars() {
        use crate::views::image_view::synthetic::Kind;
        // Bar 5 of 8 is red; row 20 is away from the moving white line.
        let (w, h) = (64, 32);
        let red_x = 5 * w / 8 + 2;
        for kind in [Kind::TestCard, Kind::Bayer] {
            let frame = color_frame(kind, w, h);
            for step in [0.5, 1.0, 1.5, 2.0, 4.0] {
                let viewport = Viewport {
                    origin: (0.0, 0.0),
                    step,
                };
                let rendered = run_at(&frame, viewport, (128, 64));
                assert_eq!(rendered.error, None, "{kind:?} {step}");
                let at = ((red_x as f64 / step) as usize, (20.0 / step) as usize);
                let [b, g, r, a] = painted(&rendered, at.0, at.1);
                assert!(
                    r > 200 && g < 60 && b < 60 && a == 255,
                    "{kind:?} {step}: {:?}",
                    (r, g, b)
                );
            }
        }
    }

    #[test]
    fn mono_f32_renders_end_to_end_with_symmetric_red_blue() {
        let (w, h) = (16, 8);
        let values: Vec<f32> = (0..w * h).map(|i| (i % w) as f32 - 4.0).collect();
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(PrimType::F32, &[h, w][..]),
            &ComponentMetadata::from("cam").with_image(metor_proto_wkt::ImageFormat::MonoF32, 32),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
                .into(),
            layout,
        };
        let display = Display {
            levels: Levels::Auto {
                low: 0.0,
                high: 1.0,
                lock: false,
            },
            colormap: Colormap::RedBlue,
            ..Display::default()
        };
        let key = RenderKey::new(&frame, display, fit(&layout, (16, 8)), (16, 8), [0; 4]);
        let rendered = run(Job {
            key,
            frame,
            scratch: Scratch::default(),
            lut: None,
        });
        assert_eq!(rendered.error, None);
        let (black, white) = rendered.levels;
        assert_eq!(black, -white);
        assert!(white > 10.9, "{:?}", rendered.levels);
        // Zero sits mid-scale: neither strongly red nor strongly blue.
        let [b, _, r, _] = painted(&rendered, 4, 2);
        assert!((i32::from(r) - i32::from(b)).abs() < 60, "{r} {b}");
        let [b, _, r, _] = painted(&rendered, 15, 2);
        assert!(r > b);
    }

    #[test]
    fn equalized_levels_follow_the_histogram() {
        let f = frame(8, 8, |x, _| if x < 7 { 10 } else { 4000 });
        let mut key = RenderKey::new(
            &f,
            Display::default(),
            fit(&f.layout, (8, 8)),
            (8, 8),
            [0; 4],
        );
        key.display.levels = Levels::Equalize;
        let rendered = run(Job {
            key,
            frame: f,
            scratch: Scratch::default(),
            lut: None,
        });
        assert_eq!(rendered.levels, (0.0, 4096.0));
        // Seven eighths of the samples sit at 10, so 10 paints bright.
        let [_, _, r, _] = painted(&rendered, 0, 0);
        assert!(r > 200, "{r}");
    }

    #[test]
    fn a_job_reports_roi_statistics_or_drops_an_outside_roi() {
        let f = frame(4, 4, |x, y| (x + 4 * y) as u16);
        let mut key = RenderKey::new(
            &f,
            Display::default(),
            fit(&f.layout, (8, 8)),
            (8, 8),
            [0; 4],
        );
        key.roi = Some(Rect::new(2, 2, 2, 2));
        let job = |key: RenderKey| {
            run(Job {
                key,
                frame: f.clone(),
                scratch: Scratch::default(),
                lut: None,
            })
        };
        let inside = job(key.clone());
        let stats = inside.roi_stats.expect("statistics");
        assert_eq!((stats.count, stats.min, stats.max), (4, 10.0, 15.0));
        assert_eq!(inside.levels, (10.0, 15.0));
        assert!(!inside.roi_outside);
        key.roi = Some(Rect::new(9, 9, 2, 2));
        let outside = job(key);
        assert!(outside.roi_outside);
        assert_eq!(outside.roi_stats, None);
        assert_eq!(outside.levels, (0.0, 15.0));
    }
}
