//! Point overlays: star centroids drawn over the image, one shaped
//! component per field (`u`, `v`, `count`), each read at the view time.

use std::sync::Arc;

use gpui::{App, Context, Entity, Subscription};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::view::ImageView;
use crate::temporal::samples::{SelectedReader, acquire};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayColor {
    /// What the tracker found.
    #[default]
    Detected,
    /// Where the simulator put the stars.
    Truth,
}

/// One overlay: each field names a component.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OverlayConfig {
    pub u: String,
    pub v: String,
    pub count: String,
    pub color: OverlayColor,
}

impl OverlayConfig {
    /// The fields of a centroid record at `root`.
    pub fn from_root(root: &str, color: OverlayColor) -> Self {
        Self {
            u: format!("{root}.u"),
            v: format!("{root}.v"),
            count: format!("{root}.count"),
            color,
        }
    }
}

/// One point, in source pixels (pixel `x` spans `x..x + 1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Star {
    pub u: f32,
    pub v: f32,
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
pub fn decode(u: &[u8], v: &[u8], count_bytes: &[u8]) -> Result<Vec<Star>, OverlayError> {
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
    Ok((0..rows)
        .filter(|&i| u[i].is_finite() && v[i].is_finite())
        .map(|i| Star { u: u[i], v: v[i] })
        .collect())
}

/// An overlay bound to its components.
pub struct Overlay {
    pub config: OverlayConfig,
    u: Entity<SelectedReader>,
    v: Entity<SelectedReader>,
    count: Entity<SelectedReader>,
}

impl Overlay {
    pub fn new(db: &Arc<DB>, config: OverlayConfig, cx: &mut App) -> Self {
        let mut reader = |name: &str| acquire(db.clone(), ComponentId::new(name), cx);
        Self {
            u: reader(&config.u),
            v: reader(&config.v),
            count: reader(&config.count),
            config,
        }
    }

    /// `Ok(None)` until `u`, `v` and `count` all have a sample.
    pub fn points(&self, cx: &App) -> Result<Option<Vec<Star>>, OverlayError> {
        let sample = |reader: &Entity<SelectedReader>| reader.read(cx).selection.sample.clone();
        let (Some(u), Some(v), Some(count)) =
            (sample(&self.u), sample(&self.v), sample(&self.count))
        else {
            return Ok(None);
        };
        decode(&u.bytes, &v.bytes, &count.bytes).map(Some)
    }

    /// Repaints the view when any field changes.
    pub fn observe(&self, cx: &mut Context<ImageView>) -> Vec<Subscription> {
        [&self.u, &self.v, &self.count]
            .into_iter()
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

/// Strokes each point as a circle.
pub fn paint(
    stars: &[Star],
    color: gpui::Hsla,
    to_window: impl Fn(f64, f64) -> gpui::Point<gpui::Pixels>,
    window: &mut gpui::Window,
) {
    const SEGMENTS: usize = 24;
    const RADIUS: f32 = 4.0;
    for star in stars {
        let center = to_window(f64::from(star.u), f64::from(star.v));
        let at = |k: usize| {
            let angle = k as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            gpui::point(
                center.x + gpui::px(RADIUS * angle.cos()),
                center.y + gpui::px(RADIUS * angle.sin()),
            )
        };
        let mut path = gpui::PathBuilder::stroke(gpui::px(1.5));
        path.move_to(at(0));
        for k in 1..=SEGMENTS {
            path.line_to(at(k));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_f32(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn only_the_first_count_rows_decode() {
        let u = bytes_f32(&[1.5, 2.5, 3.5, 0.0]);
        let v = bytes_f32(&[4.5, 5.5, 6.5, 0.0]);
        let stars = decode(&u, &v, &2u64.to_le_bytes()).unwrap();
        assert_eq!(stars, [Star { u: 1.5, v: 4.5 }, Star { u: 2.5, v: 5.5 }]);
    }

    #[test]
    fn a_count_past_the_rows_clamps() {
        let u = bytes_f32(&[1.0; 32]);
        let stars = decode(&u, &u, &1000u64.to_le_bytes()).unwrap();
        assert_eq!(stars.len(), 32);
        let stars = decode(&u, &u, &u64::MAX.to_le_bytes()).unwrap();
        assert_eq!(stars.len(), 32);
        assert!(decode(&u, &u, &0u32.to_le_bytes()).unwrap().is_empty());
    }

    #[test]
    fn bad_records_are_errors_not_panics() {
        let u = bytes_f32(&[1.0; 32]);
        let v = bytes_f32(&[1.0; 31]);
        assert_eq!(
            decode(&u, &v, &1u64.to_le_bytes()),
            Err(OverlayError::LengthMismatch { u: 32, v: 31 })
        );
        assert_eq!(decode(&u, &u, &[1, 2, 3]), Err(OverlayError::BadCount(3)));
        // Non-finite positions are skipped.
        let nan = bytes_f32(&[f32::NAN, 2.0]);
        let ok = bytes_f32(&[1.0, 2.0]);
        assert_eq!(decode(&nan, &ok, &2u64.to_le_bytes()).unwrap().len(), 1);
    }
}
