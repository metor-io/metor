//! Deterministic test images, and a debug source that streams them into a
//! component with the metadata 26a's `FrameImage` emits. A star field also
//! publishes a truth record and a detection record shaped like 26c's
//! `StarTruth` and 26b's `StarCentroids`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{App, Global, Task};
use metor_db::{Component, ComponentSchema, DB};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{ComponentMetadata, ImageFormat, MetadataExt};

/// Gaussian stars in a [`Kind::StarField`], and rows in a centroid record.
pub const STARS: usize = 32;
/// Detections the synthetic tracker misses: the faintest few stars.
const MISSED: usize = 2;
/// Hot pixels in a [`Kind::StarField`].
const HOT_PIXELS: usize = 16;
const STAR_BITS: u8 = 12;
const STAR_TOP: u16 = (1 << STAR_BITS) - 1;
const BACKGROUND: u16 = 200;
pub const DEFAULT_RATE: f64 = 10.0;
/// Fixed seed for star and hot-pixel placement; frame noise is seeded by
/// the frame number.
const LAYOUT_SEED: u64 = 0x5eed_57a2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Mono16/12: noisy background, drifting stars, hot pixels, one
    /// saturated star, and a gradient.
    StarField,
    /// Mono8.
    Ramp,
    /// `mono_f32`, a signed field.
    Float,
    /// RGB8 colour bars.
    TestCard,
    /// The colour bars as an RGGB 12-bit Bayer mosaic.
    Bayer,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::StarField,
        Self::Ramp,
        Self::Float,
        Self::TestCard,
        Self::Bayer,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Self::StarField => "star_field",
            Self::Ramp => "ramp",
            Self::Float => "float",
            Self::TestCard => "test_card",
            Self::Bayer => "bayer",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::StarField => "Star field",
            Self::Ramp => "Ramp",
            Self::Float => "Float",
            Self::TestCard => "Test card",
            Self::Bayer => "Bayer test card (RGGB 12-bit)",
        }
    }

    pub fn format(self) -> ImageFormat {
        match self {
            Self::StarField => ImageFormat::Mono16,
            Self::Ramp => ImageFormat::Mono8,
            Self::Float => ImageFormat::MonoF32,
            Self::TestCard => ImageFormat::Rgb8,
            Self::Bayer => ImageFormat::BayerRggb16,
        }
    }

    pub fn bits(self) -> u8 {
        match self {
            Self::StarField | Self::Bayer => STAR_BITS,
            Self::Float => 32,
            Self::Ramp | Self::TestCard => 8,
        }
    }

    pub fn schema(self, w: usize, h: usize) -> ComponentSchema {
        let format = self.format();
        match format.channels() {
            1 => ComponentSchema::new(format.element(), &[h, w][..]),
            c => ComponentSchema::new(format.element(), &[h, w, c][..]),
        }
    }
}

/// Frame `n` of `kind` at `w × h`, as the component's bytes.
pub fn frame(kind: Kind, w: usize, h: usize, n: u64) -> Vec<u8> {
    match kind {
        Kind::StarField => star_field(w, h, n)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
        Kind::Ramp => ramp(w, h, n),
        Kind::Float => float_field(w, h, n)
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect(),
        Kind::TestCard => test_card(w, h, n),
        Kind::Bayer => bayer_card(w, h, n)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A star's centre and peak at frame `n`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Star {
    pub x: usize,
    pub y: usize,
    pub peak: f32,
}

/// The stars of frame `n`; each drifts one pixel right per frame and wraps.
/// The first is bright enough to saturate.
pub fn stars(w: usize, h: usize, n: u64) -> Vec<Star> {
    let mut rng = Rng::new(LAYOUT_SEED);
    let drift = (n % w.max(1) as u64) as usize;
    (0..STARS)
        .map(|i| {
            let x0 = rng.below(w);
            let y = rng.below(h);
            let peak = match i {
                0 => 4.0 * f32::from(STAR_TOP),
                _ => 300.0 + rng.below(3000) as f32,
            };
            Star {
                x: (x0 + drift) % w.max(1),
                y,
                peak,
            }
        })
        .collect()
}

fn star_field(w: usize, h: usize, n: u64) -> Vec<u16> {
    let mut noise = Rng::new(n);
    let gradient = |x: usize| (x * 64 / w.max(1)) as f32;
    let mut img: Vec<f32> = (0..w * h)
        .map(|i| f32::from(BACKGROUND) + gradient(i % w.max(1)) + noise.below(24) as f32)
        .collect();
    for star in stars(w, h, n) {
        add_star(&mut img, w, h, star);
    }
    let mut hot = Rng::new(LAYOUT_SEED ^ 0xff);
    for _ in 0..HOT_PIXELS.min(w * h) {
        let i = hot.below(w * h);
        img[i] = img[i].max(3000.0);
    }
    img.into_iter()
        .map(|v| v.clamp(0.0, f32::from(STAR_TOP)) as u16)
        .collect()
}

fn add_star(img: &mut [f32], w: usize, h: usize, star: Star) {
    const R: isize = 4;
    for dy in -R..=R {
        for dx in -R..=R {
            let (x, y) = (star.x as isize + dx, star.y as isize + dy);
            if x < 0 || y < 0 || x >= w as isize || y >= h as isize {
                continue;
            }
            let r2 = (dx * dx + dy * dy) as f32;
            img[y as usize * w + x as usize] += star.peak * (-r2 / 1.8).exp();
        }
    }
}

fn ramp(w: usize, h: usize, n: u64) -> Vec<u8> {
    (0..w * h)
        .map(|i| ((i % w.max(1)) * 256 / w.max(1)) as u8)
        .map(|v| v.wrapping_add(n as u8))
        .collect()
}

fn float_field(w: usize, h: usize, n: u64) -> Vec<f32> {
    let tau = std::f32::consts::TAU;
    let phase = n as f32 * 0.1;
    (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w.max(1)) as f32, (i / w.max(1)) as f32);
            2.0 * (tau * x / w as f32 + phase).sin() * (tau * y / h.max(1) as f32).cos()
        })
        .collect()
}

/// White, yellow, cyan, green, magenta, red, blue, black; a white line
/// steps down one row per frame.
fn test_card(w: usize, h: usize, n: u64) -> Vec<u8> {
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
    let line = (n % h.max(1) as u64) as usize;
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w.max(1), i / w.max(1));
            match y == line {
                true => [255; 3],
                false => BARS[x * BARS.len() / w.max(1)],
            }
        })
        .collect()
}

/// The test card sampled through an RGGB mosaic, scaled to 12 bits.
fn bayer_card(w: usize, h: usize, n: u64) -> Vec<u16> {
    let card = test_card(w, h, n);
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w.max(1), i / w.max(1));
            let channel = match (y % 2, x % 2) {
                (0, 0) => 0,
                (1, 1) => 2,
                _ => 1,
            };
            u16::from(card[i * 3 + channel]) * 16
        })
        .collect()
}

/// Inserts `name` as an image component of `kind`, or reuses it when it
/// already has that schema.
pub fn install(
    db: &DB,
    name: &str,
    kind: Kind,
    w: usize,
    h: usize,
) -> Result<Component, metor_db::Error> {
    let mut meta = ComponentMetadata::from(name)
        .with_image(kind.format(), kind.bits())
        .with_lod(false);
    meta.set("source", "synthetic");
    install_component(db, meta, kind.schema(w, h))
}

/// The metadata lands before the component, so no watcher sees it
/// untagged; `insert_component` keeps metadata that already exists.
fn install_component(
    db: &DB,
    meta: ComponentMetadata,
    schema: ComponentSchema,
) -> Result<Component, metor_db::Error> {
    let id = meta.component_id;
    let (component, was_new) = db.with_state_mut(|s| {
        let existing = s.get_component(id).map(|c| c.schema.clone());
        if existing.as_ref().is_some_and(|e| *e != schema) {
            return Err(metor_db::Error::SchemaMismatch);
        }
        s.set_component_metadata(meta, &db.path)?;
        s.insert_component(id, schema, &db.path)?;
        let component = s.get_component(id).cloned();
        Ok((component, existing.is_none()))
    })?;
    if was_new {
        db.vtable_gen.fetch_add(1, Ordering::SeqCst);
    }
    component.ok_or(metor_db::Error::ComponentNotFound(id))
}

/// A centroid record: `count` rows, `u`/`v` at pixel centres (integer
/// + ½), a size (magnitude or flux) and a catalogue id per row.
#[derive(Clone, Debug, PartialEq)]
pub struct Centroids {
    pub u: [f32; STARS],
    pub v: [f32; STARS],
    pub size: [f32; STARS],
    pub cat_id: [u32; STARS],
    pub count: u64,
}

impl Centroids {
    fn empty() -> Self {
        Self {
            u: [0.0; STARS],
            v: [0.0; STARS],
            size: [0.0; STARS],
            cat_id: [u32::MAX; STARS],
            count: 0,
        }
    }

    /// `(field, bytes)` in record order, with `size_name` for the size.
    fn fields(&self, size_name: &'static str) -> [(&'static str, Vec<u8>); 5] {
        let f32s = |a: &[f32; STARS]| a.iter().flat_map(|v| v.to_le_bytes()).collect();
        [
            ("u", f32s(&self.u)),
            ("v", f32s(&self.v)),
            (size_name, f32s(&self.size)),
            (
                "cat_id",
                self.cat_id.iter().flat_map(|v| v.to_le_bytes()).collect(),
            ),
            ("count", self.count.to_le_bytes().to_vec()),
        ]
    }
}

/// The magnitude of a star of `peak` DN.
fn magnitude(peak: f32) -> f32 {
    6.0 - 2.5 * (peak / 300.0).log10()
}

/// Where frame `n`'s stars are: every star, `cat_id` its index, sized by
/// magnitude.
pub fn truth(w: usize, h: usize, n: u64) -> Centroids {
    let mut record = Centroids::empty();
    for (i, star) in stars(w, h, n).into_iter().enumerate() {
        record.u[i] = star.x as f32 + 0.5;
        record.v[i] = star.y as f32 + 0.5;
        record.size[i] = magnitude(star.peak);
        record.cat_id[i] = i as u32;
        record.count += 1;
    }
    record
}

/// What a tracker might report for frame `n`: each star but the last
/// [`MISSED`], jittered by up to ±0.3 px, sized by flux.
pub fn detections(w: usize, h: usize, n: u64) -> Centroids {
    let mut record = Centroids::empty();
    let mut jitter = Rng::new(n ^ 0xd37e_c7ed);
    let mut offset = || (jitter.below(601) as f32 - 300.0) / 1000.0;
    for (i, star) in stars(w, h, n).into_iter().take(STARS - MISSED).enumerate() {
        record.u[i] = star.x as f32 + 0.5 + offset();
        record.v[i] = star.y as f32 + 0.5 + offset();
        record.size[i] = star.peak;
        record.cat_id[i] = i as u32;
        record.count += 1;
    }
    record
}

/// The components of one centroid record, by field name.
pub struct RecordComponents {
    size_name: &'static str,
    fields: Vec<(&'static str, Component)>,
}

impl RecordComponents {
    fn install(db: &DB, root: &str, size_name: &'static str) -> Result<Self, metor_db::Error> {
        use metor_proto::types::PrimType;
        let row = |prim| ComponentSchema::new(prim, &[STARS][..]);
        let schemas = [
            ("u", row(PrimType::F32)),
            ("v", row(PrimType::F32)),
            (size_name, row(PrimType::F32)),
            ("cat_id", row(PrimType::U32)),
            ("count", ComponentSchema::new(PrimType::U64, &[][..])),
        ];
        let fields = schemas
            .into_iter()
            .map(|(field, schema)| {
                let meta = ComponentMetadata::from(format!("{root}.{field}").as_str());
                install_component(db, meta, schema).map(|c| (field, c))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { size_name, fields })
    }

    fn push(&self, timestamp: Timestamp, record: &Centroids) {
        for ((_, component), (_, bytes)) in self.fields.iter().zip(record.fields(self.size_name)) {
            if let Err(err) = component.push_buf(timestamp, &bytes) {
                tracing::warn!(?err, "synthetic centroids: push_buf failed");
            }
        }
    }
}

/// What a source publishes beside the image: the frame number, and for a
/// star field its truth and detections.
pub struct Records {
    frame_id: Component,
    stars: Option<(RecordComponents, RecordComponents)>,
}

impl Records {
    /// The records of an image at `<base>.pixels`.
    pub fn install(db: &DB, base: &str, kind: Kind) -> Result<Self, metor_db::Error> {
        use metor_proto::types::PrimType;
        let frame_id = install_component(
            db,
            ComponentMetadata::from(format!("{base}.frame_id").as_str()),
            ComponentSchema::new(PrimType::U64, &[][..]),
        )?;
        let stars = match kind {
            Kind::StarField => Some((
                RecordComponents::install(db, &format!("{base}.truth"), "mag")?,
                RecordComponents::install(db, &format!("{base}.centroids"), "flux")?,
            )),
            _ => None,
        };
        Ok(Self { frame_id, stars })
    }

    fn push(&self, timestamp: Timestamp, (w, h): (usize, usize), n: u64) {
        if let Err(err) = self.frame_id.push_buf(timestamp, &n.to_le_bytes()) {
            tracing::warn!(?err, "synthetic frame id: push_buf failed");
        }
        if let Some((truth_components, detected_components)) = &self.stars {
            truth_components.push(timestamp, &truth(w, h, n));
            detected_components.push(timestamp, &detections(w, h, n));
        }
    }
}

/// Pushes frame after frame of `kind` into `component` at `rate` Hz,
/// clamped to `0.1..=1000`, until the task drops. A non-finite or
/// non-positive rate is [`DEFAULT_RATE`].
pub fn run(
    component: Component,
    kind: Kind,
    rate: f64,
    records: Option<Records>,
    cx: &mut App,
) -> Task<()> {
    let (w, h) = match component.schema.dim.as_slice() {
        &[h, w, ..] => (w, h),
        _ => (0, 0),
    };
    let rate = match rate.is_finite() && rate > 0.0 {
        true => rate.clamp(0.1, 1000.0),
        false => DEFAULT_RATE,
    };
    let period = Duration::from_secs_f64(rate.recip());
    let component = Arc::new(component);
    cx.spawn(async move |cx| {
        for n in 0..u64::MAX {
            let bytes = cx
                .background_executor()
                .spawn(async move { frame(kind, w, h, n) })
                .await;
            let timestamp = Timestamp::now();
            if let Err(err) = component.push_buf(timestamp, &bytes) {
                tracing::warn!(?err, "synthetic image: push_buf failed");
            }
            if let Some(records) = &records {
                records.push(timestamp, (w, h), n);
            }
            cx.background_executor().timer(period).await;
        }
    })
}

/// The one running debug source; dropping its task stops it.
#[derive(Default)]
struct Running(Option<Task<()>>);

impl Global for Running {}

pub fn is_running(cx: &App) -> bool {
    cx.try_global::<Running>().is_some_and(|r| r.0.is_some())
}

/// The path a source of `kind` at `size` publishes under.
pub fn source_base(kind: Kind, size: usize) -> String {
    format!("debug.synthetic.{}_{size}", kind.slug())
}

/// Streams `kind` at `size × size` and `rate` Hz into
/// `<source_base>.pixels` and its records, replacing any running source.
pub fn start(
    db: &DB,
    kind: Kind,
    size: usize,
    rate: f64,
    cx: &mut App,
) -> Result<(), metor_db::Error> {
    let base = source_base(kind, size);
    let component = install(db, &format!("{base}.pixels"), kind, size, size)?;
    let records = Records::install(db, &base, kind)?;
    let task = run(component, kind, rate, Some(records), cx);
    cx.set_global(Running(Some(task)));
    Ok(())
}

pub fn stop(cx: &mut App) {
    cx.set_global(Running(None));
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;

    use super::*;
    use crate::views::image_view::FrameLayout;

    #[test]
    fn frames_are_deterministic() {
        for kind in Kind::ALL {
            assert_eq!(frame(kind, 33, 17, 5), frame(kind, 33, 17, 5), "{kind:?}");
            assert_ne!(frame(kind, 33, 17, 5), frame(kind, 33, 17, 6), "{kind:?}");
        }
    }

    #[test]
    fn frames_match_their_schema() {
        for kind in Kind::ALL {
            let bytes = frame(kind, 33, 17, 0);
            assert_eq!(bytes.len(), kind.schema(33, 17).size(), "{kind:?}");
        }
    }

    #[test]
    fn saturated_star_reaches_the_top_code() {
        let (w, h) = (64, 48);
        for n in [0, 1, 63, 64, 1000] {
            let pixels = star_field(w, h, n);
            let star = stars(w, h, n)[0];
            assert_eq!(pixels[star.y * w + star.x], STAR_TOP, "frame {n}");
            assert!(pixels.iter().all(|&p| p <= STAR_TOP));
        }
    }

    #[test]
    fn stars_drift_one_pixel_per_frame() {
        let (a, b) = (stars(64, 48, 10), stars(64, 48, 11));
        for (a, b) in a.iter().zip(&b) {
            assert_eq!(((a.x + 1) % 64, a.y), (b.x, b.y));
        }
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for kind in Kind::ALL {
            assert!(frame(kind, 0, 0, 3).is_empty(), "{kind:?}");
            assert_eq!(frame(kind, 1, 1, 3).len(), kind.schema(1, 1).size());
        }
    }

    fn temp_db() -> (tempfile::TempDir, Arc<DB>) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        (temp, db)
    }

    #[test]
    fn installed_components_parse_as_images() {
        let (_temp, db) = temp_db();
        for kind in Kind::ALL {
            let name = format!("debug.synthetic.{kind:?}");
            let component = install(&db, &name, kind, 8, 6).unwrap();
            let meta = db
                .with_state(|s| s.get_component_metadata(component.component_id).cloned())
                .unwrap();
            assert_eq!(meta.lod(), Some(false));
            let layout = FrameLayout::from_component(&component.schema, &meta).unwrap();
            assert_eq!((layout.width, layout.height), (8, 6));
            assert_eq!((layout.format, layout.bits), (kind.format(), kind.bits()));
            assert!(layout.pixels(&frame(kind, 8, 6, 0)).is_ok());
        }
    }

    #[test]
    fn reinstalling_with_another_size_fails() {
        let (_temp, db) = temp_db();
        install(&db, "debug.cam", Kind::Ramp, 8, 6).unwrap();
        assert!(install(&db, "debug.cam", Kind::Ramp, 8, 6).is_ok());
        assert!(install(&db, "debug.cam", Kind::Ramp, 9, 6).is_err());
    }

    /// Every WAL byte written since `reader` was made.
    fn drain(reader: &mut metor_db::disruptor::Reader) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(grant) = reader.try_next() {
            out.extend_from_slice(&grant);
        }
        out
    }

    #[gpui::test]
    fn run_pushes_numbered_frames(cx: &mut gpui::TestAppContext) {
        let (_temp, db) = temp_db();
        let component = install(&db, "debug.cam", Kind::StarField, 8, 6).unwrap();
        let mut reader = component.wal.reader();
        let stamp = component.last_timestamp.clone();
        let before = stamp.latest();
        let record = |n| {
            let mut bytes = vec![0; size_of::<Timestamp>()];
            bytes.extend(frame(Kind::StarField, 8, 6, n));
            bytes
        };
        let task = cx.update(|cx| run(component, Kind::StarField, 10.0, None, cx));
        cx.run_until_parked();
        assert!(stamp.latest() > before);
        let first = drain(&mut reader);
        assert_eq!(first.len(), record(0).len());
        assert_eq!(first[8..], record(0)[8..]);
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(drain(&mut reader)[8..], record(1)[8..]);
        drop(task);
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(drain(&mut reader).is_empty());
    }

    #[gpui::test]
    fn start_then_stop(cx: &mut gpui::TestAppContext) {
        let (_temp, db) = temp_db();
        cx.update(|cx| {
            assert!(!is_running(cx));
            start(&db, Kind::Ramp, 16, 10.0, cx).unwrap();
            assert!(is_running(cx));
        });
        cx.run_until_parked();
        let id = ComponentId::new("debug.synthetic.ramp_16.pixels");
        let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        let stamp = component.last_timestamp.latest();
        assert!(stamp > Timestamp(i64::MIN));
        cx.update(stop);
        cx.update(|cx| assert!(!is_running(cx)));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(component.last_timestamp.latest(), stamp);
    }

    #[test]
    fn a_large_source_gets_a_bounded_wal_and_no_lod() {
        let (_temp, db) = temp_db();
        let component = install(&db, "debug.big", Kind::StarField, 1024, 1024).unwrap();
        assert!(
            component.wal.capacity() <= 64 << 20,
            "{}",
            component.wal.capacity()
        );
        let meta = db
            .with_state(|s| s.get_component_metadata(component.component_id).cloned())
            .unwrap();
        // The LoD engine skips `image.format` and `lod = "false"`.
        assert_eq!(meta.image_format(), Some("mono16"));
        assert_eq!(meta.lod(), Some(false));
    }

    #[test]
    fn a_mismatched_reinstall_keeps_the_old_metadata() {
        let (_temp, db) = temp_db();
        install(&db, "debug.cam", Kind::Ramp, 8, 6).unwrap();
        assert!(install(&db, "debug.cam", Kind::StarField, 8, 6).is_err());
        let meta = db
            .with_state(|s| {
                s.get_component_metadata(ComponentId::new("debug.cam"))
                    .cloned()
            })
            .unwrap();
        assert_eq!(meta.image_format(), Some("mono8"));
    }

    #[gpui::test]
    fn bad_rates_fall_back_to_the_default(cx: &mut gpui::TestAppContext) {
        let (_temp, db) = temp_db();
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let component = install(&db, "debug.cam", Kind::Ramp, 4, 4).unwrap();
            let task = cx.update(|cx| run(component, Kind::Ramp, rate, None, cx));
            cx.run_until_parked();
            drop(task);
        }
    }

    #[gpui::test]
    fn starting_twice_writes_once_per_tick(cx: &mut gpui::TestAppContext) {
        let (_temp, db) = temp_db();
        let (w, h) = (16, 16);
        let record = size_of::<Timestamp>() + Kind::Ramp.schema(w, h).size();
        let component = install(&db, "debug.synthetic.ramp_16.pixels", Kind::Ramp, w, h).unwrap();
        let mut reader = component.wal.reader();
        let mut records = |cx: &mut gpui::TestAppContext| {
            cx.run_until_parked();
            let bytes = drain(&mut reader);
            assert_eq!(bytes.len() % record, 0);
            bytes.len() / record
        };
        cx.update(|cx| start(&db, Kind::Ramp, 16, 10.0, cx))
            .unwrap();
        assert_eq!(records(cx), 1);
        cx.update(|cx| start(&db, Kind::Ramp, 16, 10.0, cx))
            .unwrap();
        assert_eq!(records(cx), 1);
        for _ in 0..3 {
            cx.executor().advance_clock(Duration::from_millis(100));
            assert_eq!(records(cx), 1);
        }
        cx.update(stop);
    }

    #[test]
    fn truth_marks_each_drawn_star_at_its_pixel_centre() {
        let (w, h) = (64, 48);
        for n in [0, 5, 70] {
            let record = truth(w, h, n);
            let drawn = stars(w, h, n);
            assert_eq!(record.count, STARS as u64);
            for (i, star) in drawn.iter().enumerate() {
                assert_eq!(
                    (record.u[i], record.v[i]),
                    (star.x as f32 + 0.5, star.y as f32 + 0.5)
                );
                assert_eq!(record.cat_id[i], i as u32);
            }
            // The saturated star is the brightest, so the smallest magnitude.
            let brightest = record.size.iter().copied().fold(f32::INFINITY, f32::min);
            assert_eq!(record.size[0], brightest);
            let pixels = star_field(w, h, n);
            assert_eq!(
                pixels[(record.v[0] as usize) * w + record.u[0] as usize],
                STAR_TOP
            );
        }
    }

    #[test]
    fn detections_miss_a_few_and_stay_close() {
        let (t, d) = (truth(64, 48, 3), detections(64, 48, 3));
        assert_eq!(d.count, (STARS - MISSED) as u64);
        for i in 0..d.count as usize {
            assert!((d.u[i] - t.u[i]).abs() <= 0.3 && (d.v[i] - t.v[i]).abs() <= 0.3);
            assert_eq!(d.cat_id[i], t.cat_id[i]);
        }
        assert_eq!(d.cat_id[STARS - 1], u32::MAX);
        assert_eq!(detections(64, 48, 3), d);
    }

    #[test]
    fn records_decode_as_overlays() {
        let record = truth(64, 48, 1);
        let fields = record.fields("mag");
        let bytes = |name: &str| fields.iter().find(|(f, _)| *f == name).unwrap().1.clone();
        let stars = crate::views::image_view::overlay::decode(
            &bytes("u"),
            &bytes("v"),
            &bytes("count"),
            Some(&bytes("mag")),
            Some(&bytes("cat_id")),
        )
        .unwrap();
        assert_eq!(stars.len(), STARS);
        assert_eq!(stars[3].u, record.u[3]);
        assert_eq!(stars[3].cat_id, Some(3));
    }

    #[gpui::test]
    fn a_star_field_source_publishes_its_records(cx: &mut gpui::TestAppContext) {
        let (_temp, db) = temp_db();
        cx.update(|cx| start(&db, Kind::StarField, 32, 10.0, cx))
            .unwrap();
        cx.run_until_parked();
        let base = source_base(Kind::StarField, 32);
        for field in [
            "pixels",
            "frame_id",
            "truth.u",
            "truth.mag",
            "truth.count",
            "centroids.flux",
        ] {
            let id = ComponentId::new(&format!("{base}.{field}"));
            let component = db.with_state(|s| s.get_component(id).cloned());
            let stamp = component.expect(field).last_timestamp.latest();
            assert!(stamp > Timestamp(i64::MIN), "{field}");
        }
        cx.update(stop);
    }

    #[test]
    fn the_bayer_card_mosaics_the_test_card() {
        let (w, h) = (16, 4);
        let card = test_card(w, h, 0);
        let mosaic = bayer_card(w, h, 0);
        // Row 1 (no moving line at frame 0 is on row 0): G at even x, B at odd.
        let rgb = |x: usize, y: usize, c: usize| u16::from(card[(y * w + x) * 3 + c]) * 16;
        assert_eq!(mosaic[w + 1], rgb(1, 1, 2));
        assert_eq!(mosaic[w + 2], rgb(2, 1, 1));
        assert_eq!(mosaic[2 * w + 2], rgb(2, 2, 0));
        assert!(mosaic.iter().all(|&v| v <= 4095));
    }
}
