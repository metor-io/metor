//! Turning a frame into a device-resolution `RenderImage`: the render key,
//! one-job-at-a-time scheduling, the job itself, and frame retirement.

use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame as ImageFrame, ImageBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::Stretch;
use smallvec::SmallVec;

use super::color::{ChannelLut, color_input, is_color, render_color, render_mosaic};
use super::histogram::Histogram;
use super::layout::{FrameLayout, Pixels};
use super::lut::{Curve, Display, Levels, Lut};
use super::resample::{Plane, Rect, RenderError, Scratch, Viewport, render_into};
use super::source::Frame;

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
            out,
            saturated,
        }
    }
}

/// At most one job in flight; keys that change meanwhile coalesce into one
/// follow-up job for the latest, started when the current one finishes.
/// Each key travels with the input `F` it was made from, so a started job
/// always has its input.
#[derive(Debug)]
pub struct Scheduler<F> {
    wanted: Option<(RenderKey, F)>,
    in_flight: Option<RenderKey>,
    installed: Option<RenderKey>,
    /// The job in flight was started before a `reset`.
    orphaned: bool,
}

impl<F> Default for Scheduler<F> {
    fn default() -> Self {
        Self {
            wanted: None,
            in_flight: None,
            installed: None,
            orphaned: false,
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
    ///
    /// A superseded result is still installed: jobs finish in request
    /// order, so it is newer than what is painted, and the view places it
    /// under the current viewport. Dropping it would leave a continuous drag,
    /// whose key changes during every job, painting nothing new until the
    /// cursor stops.
    pub fn finish(&mut self, key: &RenderKey) -> (bool, Option<(RenderKey, F)>) {
        self.in_flight = None;
        let install = !std::mem::take(&mut self.orphaned) && self.wanted.is_some();
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
        self.orphaned = self.in_flight.is_some();
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
        Stretch::Sqrt | Stretch::Log | Stretch::Asinh => Curve::Asinh {
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

/// The histogram of `region`, one bin per code.
pub fn histogram(pixels: Pixels<'_>, layout: &FrameLayout, region: Rect) -> Histogram {
    let w = layout.width;
    match pixels {
        Pixels::U8(p) if layout.channels > 1 => {
            Histogram::of_u8_color(p, w, layout.channels, region)
        }
        Pixels::U8(p) => Histogram::of_u8(p, w, region),
        Pixels::U16(p) => Histogram::of_u16(p, w, layout.bits, region),
    }
}

/// Black and white in sample units. Auto levels with no samples fall back
/// to the element's full range.
pub fn levels(
    display: &Display,
    histogram: Option<&Histogram>,
    layout: &FrameLayout,
) -> (f64, f64) {
    let full = (0.0, f64::from((1u32 << layout.bits.clamp(1, 16)) - 1));
    match display.levels {
        Levels::Manual { black, white } => (black, white),
        Levels::Auto { low, high, .. } => histogram
            .and_then(|h| {
                Some((
                    f64::from(h.percentile(low)?),
                    f64::from(h.percentile(high)?),
                ))
            })
            .unwrap_or(full),
    }
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
                scratch,
                lut,
            };
        }
    };
    let out = key
        .viewport
        .visible(layout.width, layout.height, key.out.0, key.out.1);
    let region = source_rect(&key.viewport, out, &layout);
    let hist = Arc::new(histogram(pixels, &layout, region));
    let levels = levels(&key.display, Some(&hist), &layout);
    let lut = cached_lut(lut, &layout, &key, levels);
    if out.is_empty() {
        return Rendered {
            key,
            image: None,
            error: None,
            levels,
            histogram: Some(hist),
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
        histogram: Some(hist),
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
    bgra: &mut [u8],
    scratch: &mut Scratch,
) -> Result<(), RenderError> {
    let lut = ChannelLut::build(layout.bits, black, white, key.display.curve);
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
) -> (LutKey, Arc<Lut>) {
    let want = LutKey {
        bits: layout.bits,
        display: key.display,
        levels,
    };
    if let Some(cached) = cached.filter(|(k, _)| *k == want) {
        return cached;
    }
    let (black, white) = levels;
    let lut = Lut::build(layout.bits, black, white, &key.display, key.saturated);
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

/// The painted image and the replaced ones waiting to leave the atlas.
///
/// Retired images go to [`crate::atlas::release_image`], never straight to
/// `drop_image`: frames still on the GPU may sample a replaced image, and a
/// slot freed under them is refilled by the next upload (often a plot's
/// frame), which then flashes inside this view.
#[derive(Default)]
pub struct Frames {
    pub current: Option<(Arc<RenderImage>, Rect)>,
    pending_release: Vec<Arc<RenderImage>>,
}

impl Frames {
    /// Replaces the painted image; the old one waits for the next paint.
    /// Reinstalling the painted image retires nothing.
    pub fn install(&mut self, image: Option<(Arc<RenderImage>, Rect)>) {
        let same = match (&self.current, &image) {
            (Some((old, _)), Some((new, _))) => Arc::ptr_eq(old, new),
            _ => false,
        };
        if let Some((old, _)) = std::mem::replace(&mut self.current, image)
            && !same
        {
            self.pending_release.push(old);
        }
    }

    /// The replaced images to hand to the deferred release; never the one
    /// being painted.
    pub fn take_retired(&mut self) -> Vec<Arc<RenderImage>> {
        let retired = std::mem::take(&mut self.pending_release);
        debug_assert!(self.current.as_ref().is_none_or(|(current, _)| {
            !retired.iter().any(|image| Arc::ptr_eq(image, current))
        }));
        retired
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
        assert_eq!(finish(&mut s, 1), (true, Some(key(3))));
        assert_eq!(finish(&mut s, 3), (true, None));
    }

    /// A drag changes the key during every job; each result still lands.
    #[test]
    fn a_superseded_result_is_installed() {
        let mut s = Scheduler::default();
        request(&mut s, 1);
        for n in 2..10 {
            request(&mut s, n);
            assert_eq!(finish(&mut s, n - 1), (true, Some(key(n))));
            assert_eq!(s.installed(), Some(&key(n - 1)));
        }
        // Returning to the installed key after a superseded one renders it
        // again.
        let mut s = Scheduler::default();
        request(&mut s, 1);
        finish(&mut s, 1);
        request(&mut s, 2);
        request(&mut s, 1);
        assert_eq!(finish(&mut s, 2), (true, Some(key(1))));
    }

    #[test]
    fn a_job_from_before_a_reset_is_dropped() {
        let mut s = Scheduler::default();
        request(&mut s, 1);
        s.reset();
        assert_eq!(request(&mut s, 2), None);
        assert_eq!(finish(&mut s, 1), (false, Some(key(2))));
        assert_eq!(finish(&mut s, 2), (true, None));
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
    fn auto_levels_follow_the_view() {
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

    /// A paint retires only replaced images, each once, and never the one it
    /// is about to draw.
    #[test]
    fn retiring_never_hands_over_the_painted_image() {
        let image = || image(vec![0; 4], Rect::new(0, 0, 1, 1)).unwrap();
        let (a, b, c) = (image(), image(), image());
        let mut frames = Frames::default();
        frames.install(Some((a.clone(), Rect::default())));
        assert!(frames.take_retired().is_empty());

        // Two renders land between paints, as during a zoom.
        frames.install(Some((b.clone(), Rect::default())));
        frames.install(Some((c.clone(), Rect::default())));
        let retired = frames.take_retired();
        assert_eq!(retired.len(), 2);
        assert!(Arc::ptr_eq(&retired[0], &a) && Arc::ptr_eq(&retired[1], &b));
        assert!(frames.take_retired().is_empty(), "retired twice");

        // Reinstalling the painted image keeps it in the atlas.
        frames.install(Some((c.clone(), Rect::default())));
        assert!(frames.take_retired().is_empty());
        assert!(Arc::ptr_eq(&frames.current.as_ref().unwrap().0, &c));

        let all = frames.take_all();
        assert_eq!(all.len(), 1);
        assert!(Arc::ptr_eq(&all[0], &c));
    }

    /// Eight colour bars (bar 5 red) as RGB8, or an RGGB 12-bit mosaic.
    fn color_frame(bayer: bool, w: usize, h: usize) -> Frame {
        const BARS: [[u8; 3]; 8] = [
            [255, 255, 255],
            [255, 255, 0],
            [0, 255, 255],
            [0, 255, 0],
            [255, 0, 255],
            [255, 0, 0],
            [0, 0, 255],
            [0, 0, 0],
        ];
        let rgb = |x: usize| BARS[x * BARS.len() / w];
        let (format, bits, schema, bytes): (_, _, _, Vec<u8>) = match bayer {
            false => (
                ImageFormat::Rgb8,
                8,
                ComponentSchema::new(PrimType::U8, &[h, w, 3][..]),
                (0..w * h).flat_map(|i| rgb(i % w)).collect(),
            ),
            true => (
                ImageFormat::BayerRggb16,
                12,
                ComponentSchema::new(PrimType::U16, &[h, w][..]),
                (0..w * h)
                    .flat_map(|i| {
                        let (x, y) = (i % w, i / w);
                        let channel = match (y % 2, x % 2) {
                            (0, 0) => 0,
                            (1, 1) => 2,
                            _ => 1,
                        };
                        (u16::from(rgb(x)[channel]) * 16).to_le_bytes()
                    })
                    .collect(),
            ),
        };
        let layout = FrameLayout::from_component(
            &schema,
            &ComponentMetadata::from("cam").with_image(format, bits),
        )
        .unwrap();
        Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
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
        // Bar 5 of 8 is red.
        let (w, h) = (64, 32);
        let red_x = 5 * w / 8 + 2;
        for bayer in [false, true] {
            let frame = color_frame(bayer, w, h);
            for step in [0.5, 1.0, 1.5, 2.0, 4.0] {
                let viewport = Viewport {
                    origin: (0.0, 0.0),
                    step,
                };
                let rendered = run_at(&frame, viewport, (128, 64));
                assert_eq!(rendered.error, None, "{bayer} {step}");
                let at = ((red_x as f64 / step) as usize, (20.0 / step) as usize);
                let [b, g, r, a] = painted(&rendered, at.0, at.1);
                assert!(
                    r > 200 && g < 60 && b < 60 && a == 255,
                    "{bayer} {step}: {:?}",
                    (r, g, b)
                );
            }
        }
    }
}
