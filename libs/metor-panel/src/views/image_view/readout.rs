//! The hover readout: one pixel's raw value, and the scalar components
//! beside the image (exposure, frame id) at the same view time.

use std::sync::Arc;

use gpui::{App, Entity, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use super::layout::Pixels;
use super::overlay::OverlayColor;
use super::source::Frame;
use crate::temporal::samples::{SelectedReader, acquire};

/// Siblings shown at most; a frame record has a handful of fields.
const MAX_SIBLINGS: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Readout {
    pub x: usize,
    pub y: usize,
    /// The raw sample: a DN, a float, or one value per channel.
    pub value: String,
    /// When zoomed out, the size of the block the device pixel pools; the
    /// value is the block's middle pixel.
    pub pooled: Option<(usize, usize)>,
    pub siblings: Vec<(SharedString, String)>,
    /// Overlay points under the cursor: the overlay's colour role and the
    /// point described (`cat 12`, `uncatalogued`).
    pub stars: Vec<(OverlayColor, String)>,
}

/// The raw value of pixel `(x, y)`, read from the frame bytes, with the
/// colour a Bayer site samples; `None` outside the frame.
pub fn raw_value(frame: &Frame, x: usize, y: usize) -> Option<String> {
    let layout = &frame.layout;
    if x >= layout.width || y >= layout.height {
        return None;
    }
    let c = layout.channels;
    let at = (y * layout.width + x) * c..(y * layout.width + x + 1) * c;
    let values: Vec<String> = match layout.pixels(&frame.bytes).ok()? {
        Pixels::U8(p) => p.get(at)?.iter().map(u8::to_string).collect(),
        Pixels::U16(p) => p.get(at)?.iter().map(|v| v.get().to_string()).collect(),
    };
    let value = values.join(", ");
    Some(match super::color::site(layout.format, x, y) {
        Some(site) => format!("{value} ({})", site.letter()),
        None => value,
    })
}

/// The scalar components under the image's parent path, each with a
/// reader that follows the view time.
#[derive(Default)]
pub struct Siblings {
    image: Option<ComponentId>,
    generation: Option<u64>,
    entries: Vec<(SharedString, ComponentId, Entity<SelectedReader>)>,
}

impl Siblings {
    /// Rebuilds the list when the image or the component set changed.
    pub fn refresh(&mut self, db: &Arc<DB>, image: ComponentId, cx: &mut App) {
        let generation = db.vtable_gen.latest();
        if self.image == Some(image) && self.generation == Some(generation) {
            return;
        }
        self.image = Some(image);
        self.generation = Some(generation);
        self.entries = scalar_siblings(db, image)
            .into_iter()
            .map(|(label, id)| (label, id, acquire(db.clone(), id, cx)))
            .collect();
    }

    pub fn values(&self, db: &Arc<DB>, cx: &App) -> Vec<(SharedString, String)> {
        self.entries
            .iter()
            .map(|(label, id, _)| {
                let text = crate::views::copy::selected_value_text(db, *id, None, cx);
                (label.clone(), text.unwrap_or_else(|| "—".into()))
            })
            .collect()
    }
}

/// `(label, id)` of each scalar component directly under the image's
/// parent path, labelled by its last segment.
fn scalar_siblings(db: &DB, image: ComponentId) -> Vec<(SharedString, ComponentId)> {
    db.with_state(|state| {
        let Some(name) = state.get_component_metadata(image).map(|m| m.name.as_str()) else {
            return Vec::new();
        };
        let Some((parent, _)) = name.rsplit_once('.') else {
            return Vec::new();
        };
        let prefix = format!("{parent}.");
        let mut siblings: Vec<(SharedString, ComponentId)> = state
            .component_metadata_iter()
            .filter(|(id, meta)| {
                **id != image
                    && !meta.is_hidden()
                    && meta.image_format().is_none()
                    && meta
                        .name
                        .strip_prefix(&prefix)
                        .is_some_and(|rest| !rest.contains('.'))
                    && state
                        .get_component(**id)
                        .is_some_and(|c| c.schema.dim.iter().product::<usize>() == 1)
            })
            .map(|(id, meta)| {
                (
                    SharedString::from(meta.name[prefix.len()..].to_string()),
                    *id,
                )
            })
            .collect();
        siblings.sort_by(|a, b| a.0.cmp(&b.0));
        siblings.truncate(MAX_SIBLINGS);
        siblings
    })
}

#[cfg(test)]
mod tests {
    use metor_db::ComponentSchema;
    use metor_proto::types::{PrimType, Timestamp};
    use metor_proto_wkt::{ComponentMetadata, ImageFormat};

    use super::*;
    use crate::views::image_view::FrameLayout;

    fn frame(format: ImageFormat, prim: PrimType, dim: &[usize], bytes: Vec<u8>) -> Frame {
        let layout = FrameLayout::from_component(
            &ComponentSchema::new(prim, dim),
            &ComponentMetadata::from("cam").with_image(format, format.element().size() as u8 * 8),
        )
        .unwrap();
        Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
            layout,
        }
    }

    #[test]
    fn raw_values_come_from_the_bytes() {
        let mono = frame(
            ImageFormat::Mono16,
            PrimType::U16,
            &[2, 2],
            [1u16, 2, 3, 4000]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        );
        assert_eq!(raw_value(&mono, 1, 1).as_deref(), Some("4000"));
        assert_eq!(raw_value(&mono, 0, 1).as_deref(), Some("3"));
        assert_eq!(raw_value(&mono, 2, 0), None);
        let rgb = frame(
            ImageFormat::Rgb8,
            PrimType::U8,
            &[1, 2, 3],
            vec![1, 2, 3, 4, 5, 6],
        );
        assert_eq!(raw_value(&rgb, 1, 0).as_deref(), Some("4, 5, 6"));
        let short = Frame {
            bytes: vec![0u8; 3].into(),
            ..mono
        };
        assert_eq!(raw_value(&short, 0, 0), None);
    }

    #[test]
    fn siblings_are_scalars_beside_the_image() {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        for (name, prim, dim, image) in [
            ("st.image.pixels", PrimType::U16, &[4usize, 4][..], true),
            ("st.image.exposure", PrimType::F64, &[][..], false),
            ("st.image.frame_id", PrimType::U64, &[][..], false),
            ("st.image.gains", PrimType::F64, &[3][..], false),
            ("st.other", PrimType::F64, &[][..], false),
        ] {
            let mut meta = ComponentMetadata::from(name);
            if image {
                meta = meta.with_image(ImageFormat::Mono16, 12);
            }
            let id = meta.component_id;
            db.with_state_mut(|s| {
                s.set_component_metadata(meta, &db.path)?;
                s.insert_component(id, ComponentSchema::new(prim, dim), &db.path)
            })
            .unwrap();
        }
        let labels: Vec<_> = scalar_siblings(&db, ComponentId::new("st.image.pixels"))
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels, ["exposure", "frame_id"]);
        assert!(scalar_siblings(&db, ComponentId::new("st.missing")).is_empty());
        assert!(scalar_siblings(&db, ComponentId::new("st.other")).is_empty());
    }
}
