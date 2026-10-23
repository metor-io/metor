//! Point overlays: star centroids drawn over the image, one shaped
//! component per field (`u`, `v`, `count`, and optionally a size and
//! `cat_id`), each read at the view time.

use std::sync::Arc;

use gpui::{App, Context, Entity, Subscription};
use metor_db::DB;
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};

use super::view::ImageView;
use crate::temporal::samples::{SelectedReader, acquire};

/// `cat_id` of a point with no catalogue match.
pub const NO_CATALOGUE_ID: u32 = u32::MAX;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayColor {
    /// What the tracker found.
    #[default]
    Detected,
    /// Where the simulator put the stars.
    Truth,
}

/// What a size field holds, which sets how it maps to a radius.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SizeKind {
    /// Brightness in DN; brighter is larger.
    #[default]
    Flux,
    /// Magnitude; brighter (smaller) is larger.
    Magnitude,
}

/// One overlay: each field names a component.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OverlayConfig {
    pub u: String,
    pub v: String,
    pub count: String,
    pub size: Option<String>,
    pub size_kind: SizeKind,
    pub cat_id: Option<String>,
    pub color: OverlayColor,
}

impl OverlayConfig {
    /// The fields of a centroid record at `root`: truth sized by `mag`,
    /// detections by `flux`.
    pub fn from_root(root: &str, color: OverlayColor) -> Self {
        let (size, size_kind) = match color {
            OverlayColor::Truth => ("mag", SizeKind::Magnitude),
            OverlayColor::Detected => ("flux", SizeKind::Flux),
        };
        Self {
            u: format!("{root}.u"),
            v: format!("{root}.v"),
            count: format!("{root}.count"),
            size: Some(format!("{root}.{size}")),
            size_kind,
            cat_id: Some(format!("{root}.cat_id")),
            color,
        }
    }
}

/// One point, in source pixels (pixel `x` spans `x..x + 1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Star {
    pub u: f32,
    pub v: f32,
    pub size: Option<f32>,
    pub cat_id: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayError {
    LengthMismatch { u: usize, v: usize },
    BadCount(usize),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthMismatch { u, v } => write!(f, "overlay u has {u} values, v has {v}"),
            Self::BadCount(len) => write!(f, "overlay count is {len} bytes"),
        }
    }
}

fn f32s(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect()
}

/// A `u64` or `u32` count.
fn count(bytes: &[u8]) -> Result<u64, OverlayError> {
    match bytes.len() {
        8 => Ok(u64::from_le_bytes(bytes.try_into().unwrap_or_default())),
        4 => Ok(u64::from(u32::from_le_bytes(
            bytes.try_into().unwrap_or_default(),
        ))),
        len => Err(OverlayError::BadCount(len)),
    }
}

/// The first `count` rows of a centroid record; rows with a non-finite
/// position are skipped.
pub fn decode(
    u: &[u8],
    v: &[u8],
    count_bytes: &[u8],
    size: Option<&[u8]>,
    cat_id: Option<&[u8]>,
) -> Result<Vec<Star>, OverlayError> {
    let (u, v) = (f32s(u), f32s(v));
    if u.len() != v.len() {
        return Err(OverlayError::LengthMismatch {
            u: u.len(),
            v: v.len(),
        });
    }
    let rows = usize::try_from(count(count_bytes)?)
        .unwrap_or(usize::MAX)
        .min(u.len());
    let size = size.map(f32s).unwrap_or_default();
    let cat_id = cat_id.map(u32s).unwrap_or_default();
    Ok((0..rows)
        .filter(|&i| u[i].is_finite() && v[i].is_finite())
        .map(|i| Star {
            u: u[i],
            v: v[i],
            size: size.get(i).copied().filter(|s| s.is_finite()),
            cat_id: cat_id.get(i).copied().filter(|&id| id != NO_CATALOGUE_ID),
        })
        .collect())
}

/// Marker radius in logical pixels.
pub fn radius(size: Option<f32>, kind: SizeKind) -> f32 {
    const DEFAULT: f32 = 4.0;
    let Some(size) = size else {
        return DEFAULT;
    };
    let r = match kind {
        SizeKind::Flux => 1.5 * size.max(1.0).log10(),
        SizeKind::Magnitude => 7.0 - 0.6 * size,
    };
    r.clamp(2.5, 9.0)
}

/// An overlay's points at the view time.
#[derive(Clone, Debug, PartialEq)]
pub struct Points {
    pub stars: Vec<Star>,
    /// When the `u` sample was taken; differs from the frame's when the
    /// overlay is out of step.
    pub timestamp: Timestamp,
    /// Whether every bound field's sample has that same timestamp.
    pub consistent: bool,
}

/// An overlay bound to its components.
pub struct Overlay {
    pub config: OverlayConfig,
    u: Entity<SelectedReader>,
    v: Entity<SelectedReader>,
    count: Entity<SelectedReader>,
    size: Option<Entity<SelectedReader>>,
    cat_id: Option<Entity<SelectedReader>>,
}

impl Overlay {
    pub fn new(db: &Arc<DB>, config: OverlayConfig, cx: &mut App) -> Self {
        let mut reader = |name: &str| acquire(db.clone(), ComponentId::new(name), cx);
        Self {
            u: reader(&config.u),
            v: reader(&config.v),
            count: reader(&config.count),
            size: config.size.as_deref().map(&mut reader),
            cat_id: config.cat_id.as_deref().map(&mut reader),
            config,
        }
    }

    /// `Ok(None)` until `u`, `v` and `count` all have a sample.
    pub fn points(&self, cx: &App) -> Result<Option<Points>, OverlayError> {
        let sample = |reader: &Entity<SelectedReader>| reader.read(cx).selection.sample.clone();
        let (Some(u), Some(v), Some(count)) =
            (sample(&self.u), sample(&self.v), sample(&self.count))
        else {
            return Ok(None);
        };
        let size = self.size.as_ref().and_then(sample);
        let cat_id = self.cat_id.as_ref().and_then(sample);
        let consistent = [Some(&v), Some(&count), size.as_ref()]
            .into_iter()
            .flatten()
            .all(|s| s.timestamp == u.timestamp)
            && (self.size.is_none() || size.is_some());
        let stars = decode(
            &u.bytes,
            &v.bytes,
            &count.bytes,
            size.as_ref().map(|s| &*s.bytes),
            cat_id.as_ref().map(|s| &*s.bytes),
        )?;
        Ok(Some(Points {
            stars,
            timestamp: u.timestamp,
            consistent,
        }))
    }

    /// Repaints the view when any field changes.
    pub fn observe(&self, cx: &mut Context<ImageView>) -> Vec<Subscription> {
        [&self.u, &self.v, &self.count]
            .into_iter()
            .chain(self.size.as_ref())
            .chain(self.cat_id.as_ref())
            .map(|reader| cx.observe(reader, |_, _, cx| cx.notify()))
            .collect()
    }
}

/// Roots of the centroid records in the db: each `<root>.u` with a
/// `<root>.v` and a `<root>.count` beside it.
pub fn record_roots(db: &DB) -> Vec<String> {
    let names: std::collections::HashSet<String> =
        crate::inspector::trace_picker::list_components(db)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
    let mut roots: Vec<String> = names
        .iter()
        .filter_map(|name| name.strip_suffix(".u"))
        .filter(|root| {
            names.contains(&format!("{root}.v")) && names.contains(&format!("{root}.count"))
        })
        .map(str::to_string)
        .collect();
    roots.sort();
    roots
}

/// Strokes each point as a circle; out-of-step points are dashed.
pub fn paint(
    points: &Points,
    kind: SizeKind,
    color: gpui::Hsla,
    dashed: bool,
    to_window: impl Fn(f64, f64) -> gpui::Point<gpui::Pixels>,
    window: &mut gpui::Window,
) {
    const SEGMENTS: usize = 24;
    for star in &points.stars {
        let center = to_window(f64::from(star.u), f64::from(star.v));
        let r = radius(star.size, kind);
        let at = |k: usize| {
            let angle = k as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            gpui::point(
                center.x + gpui::px(r * angle.cos()),
                center.y + gpui::px(r * angle.sin()),
            )
        };
        let mut path = gpui::PathBuilder::stroke(gpui::px(1.5));
        for k in (0..SEGMENTS).filter(|k| !dashed || k % 2 == 0) {
            path.move_to(at(k));
            path.line_to(at(k + 1));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

/// The point drawn under `cursor`, as `(cat_id, distance)`.
pub fn under(
    points: &Points,
    kind: SizeKind,
    cursor: gpui::Point<gpui::Pixels>,
    to_window: impl Fn(f64, f64) -> gpui::Point<gpui::Pixels>,
) -> Option<(Option<u32>, f32)> {
    points
        .stars
        .iter()
        .map(|star| {
            let at = to_window(f64::from(star.u), f64::from(star.v));
            let (dx, dy) = (f32::from(at.x - cursor.x), f32::from(at.y - cursor.y));
            (star, (dx * dx + dy * dy).sqrt())
        })
        .filter(|(star, d)| *d <= radius(star.size, kind) + 2.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(star, d)| (star.cat_id, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_f32(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn bytes_u32(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn only_the_first_count_rows_decode() {
        let u = bytes_f32(&[1.5, 2.5, 3.5, 0.0]);
        let v = bytes_f32(&[4.5, 5.5, 6.5, 0.0]);
        let cat = bytes_u32(&[7, NO_CATALOGUE_ID, 9, NO_CATALOGUE_ID]);
        let stars = decode(&u, &v, &2u64.to_le_bytes(), None, Some(&cat)).unwrap();
        assert_eq!(
            stars,
            [
                Star {
                    u: 1.5,
                    v: 4.5,
                    size: None,
                    cat_id: Some(7)
                },
                Star {
                    u: 2.5,
                    v: 5.5,
                    size: None,
                    cat_id: None
                },
            ]
        );
    }

    #[test]
    fn a_count_past_the_rows_clamps() {
        let u = bytes_f32(&[1.0; 32]);
        let stars = decode(&u, &u, &1000u64.to_le_bytes(), None, None).unwrap();
        assert_eq!(stars.len(), 32);
        let stars = decode(&u, &u, &u64::MAX.to_le_bytes(), None, None).unwrap();
        assert_eq!(stars.len(), 32);
        assert!(
            decode(&u, &u, &0u32.to_le_bytes(), None, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn bad_records_are_errors_not_panics() {
        let u = bytes_f32(&[1.0; 32]);
        let v = bytes_f32(&[1.0; 31]);
        assert_eq!(
            decode(&u, &v, &1u64.to_le_bytes(), None, None),
            Err(OverlayError::LengthMismatch { u: 32, v: 31 })
        );
        assert_eq!(
            decode(&u, &u, &[1, 2, 3], None, None),
            Err(OverlayError::BadCount(3))
        );
        // A short size field leaves the rows past it unsized.
        let size = bytes_f32(&[3.0]);
        let stars = decode(&u, &u, &2u64.to_le_bytes(), Some(&size), None).unwrap();
        assert_eq!((stars[0].size, stars[1].size), (Some(3.0), None));
        // Non-finite positions are skipped.
        let nan = bytes_f32(&[f32::NAN, 2.0]);
        let ok = bytes_f32(&[1.0, 2.0]);
        assert_eq!(
            decode(&nan, &ok, &2u64.to_le_bytes(), None, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn radius_grows_with_brightness() {
        assert!(radius(Some(1.0), SizeKind::Magnitude) > radius(Some(6.0), SizeKind::Magnitude));
        assert!(radius(Some(4000.0), SizeKind::Flux) > radius(Some(20.0), SizeKind::Flux));
        for r in [
            radius(Some(-30.0), SizeKind::Magnitude),
            radius(Some(1e30), SizeKind::Flux),
            radius(Some(0.0), SizeKind::Flux),
            radius(None, SizeKind::Flux),
        ] {
            assert!((2.5..=9.0).contains(&r));
        }
    }

    #[test]
    fn from_root_names_every_field() {
        let truth = OverlayConfig::from_root("sim.star_truth", OverlayColor::Truth);
        assert_eq!(truth.u, "sim.star_truth.u");
        assert_eq!(truth.count, "sim.star_truth.count");
        assert_eq!(truth.size.as_deref(), Some("sim.star_truth.mag"));
        assert_eq!(truth.size_kind, SizeKind::Magnitude);
        let detected = OverlayConfig::from_root("st.star_centroids", OverlayColor::Detected);
        assert_eq!(detected.size.as_deref(), Some("st.star_centroids.flux"));
    }
}
