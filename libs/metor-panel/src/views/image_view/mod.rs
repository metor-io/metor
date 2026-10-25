//! Image components: one tensor per frame, tagged with `image.format`.
//!
//! A frame can hold millions of elements, so views that expand a component
//! per element (strips, sparklines, previews, element pickers) check
//! [`expands`] first. The element ceiling also covers a large component
//! whose tag has not arrived yet.

mod color;
mod histogram;
mod layout;
mod lut;
mod overlay;
mod readout;
mod render;
mod resample;
mod source;
mod stretch;
mod strip;
#[cfg(any(debug_assertions, test))]
pub mod synthetic;
mod view;

pub use histogram::{Histogram, RoiStats, finite_range, roi_stats};
pub use layout::{FrameLayout, LayoutError, Pixels};
pub use lut::{Colormap, Curve, Display, F32_BITS, Levels, Lut};
pub use overlay::{OverlayColor, OverlayConfig, SizeKind, record_roots};
pub use readout::Readout;
pub use render::{RenderKey, Scheduler};
pub use resample::{Plane, Rect, RenderError, Scratch, Viewport, render_into, spans};
pub use source::{Frame, FrameSource, FrameState, RecordSource, StaticSource};
pub use stretch::stretch_rows;
pub use strip::HistogramStrip;
pub(crate) use view::image_view_config;
pub use view::{ImageView, ImageViewConfig, ViewMode};

use std::sync::Arc;

use metor_db::{DB, State};
use metor_proto::types::ComponentId;

use crate::inspector::rows::{CommandRow, HeaderRow, InspectorRow};
use crate::inspector::trace_picker::list_components;

/// Per-element views refuse a component with more elements than this,
/// whatever its metadata says.
pub const MAX_EXPANDED_ELEMENTS: usize = 4096;

/// `None` unless the component is an image with a valid layout.
pub fn image_layout(db: &DB, id: ComponentId) -> Option<FrameLayout> {
    db.with_state(|state| layout_in(state, id)?.ok())
}

/// One row per image component; picking one calls `on_select` with its
/// name.
pub(crate) fn image_picker_rows(
    db: Arc<DB>,
    on_select: impl Fn(String, &mut gpui::App) + 'static,
) -> Vec<Box<dyn InspectorRow>> {
    let on_select = Arc::new(on_select);
    let rows: Vec<Box<dyn InspectorRow>> = list_image_components(&db)
        .into_iter()
        .map(|(_, name)| {
            let on_select = on_select.clone();
            Box::new(CommandRow::new(
                name.clone(),
                Arc::new(move |_window, cx| on_select(name.clone(), cx)),
            )) as Box<dyn InspectorRow>
        })
        .collect();
    match rows.is_empty() {
        true => vec![Box::new(HeaderRow::new("No image components"))],
        false => rows,
    }
}

/// Image components, sorted by name.
pub fn list_image_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut list = list_components(db);
    db.with_state(|state| list.retain(|(id, _)| is_image_in(state, *id)));
    list
}

/// Components a per-element picker may offer, sorted by name.
pub(crate) fn list_expandable_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut list = list_components(db);
    db.with_state(|state| list.retain(|(id, _)| expands_in(state, *id)));
    list
}

/// Whether the component carries `image.format`, valid or not.
pub fn is_image(db: &DB, id: ComponentId) -> bool {
    db.with_state(|state| is_image_in(state, id))
}

/// Whether a view may build one cell, trace, or row per element: the schema
/// is registered, there is no image tag, and the count is under the ceiling.
pub fn expands(db: &DB, id: ComponentId) -> bool {
    db.with_state(|state| expands_in(state, id))
}

/// The text standing in for a component that does not expand: an image's
/// layout, or an element count. `None` when it expands.
pub(crate) fn collapsed_label(db: &DB, id: ComponentId) -> Option<String> {
    db.with_state(|state| {
        if expands_in(state, id) {
            return None;
        }
        let label = image_label_in(state, id).or_else(|| {
            let count = element_count(&state.get_component(id)?.schema);
            Some(format!("{count} elements"))
        });
        Some(label.unwrap_or_default())
    })
}

fn layout_in(state: &State, id: ComponentId) -> Option<Result<FrameLayout, LayoutError>> {
    let meta = state.get_component_metadata(id)?;
    meta.image_format()?;
    let schema = &state.get_component(id)?.schema;
    Some(FrameLayout::from_component(schema, meta))
}

/// An image's layout, why it has none, or its format until its schema
/// registers.
fn image_label_in(state: &State, id: ComponentId) -> Option<String> {
    let meta = state.get_component_metadata(id)?;
    let format = meta.image_format()?;
    let Some(component) = state.get_component(id) else {
        return Some(format.to_string());
    };
    Some(match FrameLayout::from_component(&component.schema, meta) {
        Ok(layout) => layout.to_string(),
        Err(err) => err.to_string(),
    })
}

fn is_image_in(state: &State, id: ComponentId) -> bool {
    state
        .get_component_metadata(id)
        .is_some_and(|meta| meta.image_format().is_some())
}

fn expands_in(state: &State, id: ComponentId) -> bool {
    !is_image_in(state, id)
        && state
            .get_component(id)
            .is_some_and(|c| element_count(&c.schema) <= MAX_EXPANDED_ELEMENTS)
}

fn element_count(schema: &metor_db::ComponentSchema) -> usize {
    schema.dim.iter().product()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;
    use metor_proto_wkt::{ComponentMetadata, IMAGE_FORMAT_KEY, ImageFormat, MetadataExt};

    use super::*;

    #[derive(Clone, Copy)]
    pub(crate) enum Kind {
        /// `mono16/12`, `[4, 6]`.
        Image,
        /// Tagged `mono12`, an unknown format, over `[4, 6]`.
        Broken,
        /// An untagged `u16[64, 128]`, above the element ceiling.
        Raw,
        Scalar,
    }

    pub(crate) fn db_with(components: &[(&str, Kind)]) -> (Arc<DB>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        for &(name, kind) in components {
            let mut meta = ComponentMetadata::from(name);
            let (prim, dim) = match kind {
                Kind::Image => {
                    meta = meta.with_image(ImageFormat::Mono16, 12);
                    (PrimType::U16, &[4usize, 6][..])
                }
                Kind::Broken => {
                    meta.set(IMAGE_FORMAT_KEY, "mono12");
                    (PrimType::U16, &[4, 6][..])
                }
                Kind::Raw => (PrimType::U16, &[64, 128][..]),
                Kind::Scalar => (PrimType::F64, &[][..]),
            };
            let id = meta.component_id;
            db.with_state_mut(|s| {
                s.insert_component(id, ComponentSchema::new(prim, dim), &db.path)?;
                s.set_component_metadata(meta, &db.path)
            })
            .unwrap();
        }
        (Arc::new(db), temp)
    }

    pub(crate) fn image_db() -> (Arc<DB>, tempfile::TempDir) {
        db_with(&[
            ("cam.pixels", Kind::Image),
            ("cam.exposure", Kind::Scalar),
            ("cam.broken", Kind::Broken),
            ("cam.raw", Kind::Raw),
        ])
    }

    /// Components that must never expand per element.
    pub(crate) const COLLAPSED: [&str; 3] = ["cam.pixels", "cam.broken", "cam.raw"];

    fn names(list: Vec<(ComponentId, String)>) -> Vec<String> {
        list.into_iter().map(|(_, name)| name).collect()
    }

    #[test]
    fn lists_split_on_tag_and_ceiling() {
        let (db, _temp) = image_db();
        assert_eq!(
            names(list_image_components(&db)),
            ["cam.broken", "cam.pixels"]
        );
        assert_eq!(names(list_expandable_components(&db)), ["cam.exposure"]);
    }

    #[test]
    fn layout_and_label() {
        let (db, _temp) = image_db();
        let pixels = ComponentId::new("cam.pixels");
        assert_eq!(image_layout(&db, pixels).map(|l| l.width), Some(6));
        assert_eq!(
            collapsed_label(&db, pixels).as_deref(),
            Some("6×4 mono16/12")
        );
        assert!(is_image(&db, pixels));
        assert!(!expands(&db, pixels));
    }

    #[test]
    fn scalar_expands() {
        let (db, _temp) = image_db();
        let exposure = ComponentId::new("cam.exposure");
        assert_eq!(image_layout(&db, exposure), None);
        assert_eq!(collapsed_label(&db, exposure), None);
        assert!(!is_image(&db, exposure));
        assert!(expands(&db, exposure));
    }

    #[test]
    fn oversized_untagged_component_collapses() {
        let (db, _temp) = image_db();
        let raw = ComponentId::new("cam.raw");
        assert!(!is_image(&db, raw));
        assert!(!expands(&db, raw));
        assert_eq!(collapsed_label(&db, raw).as_deref(), Some("8192 elements"));
    }

    #[test]
    fn unregistered_component_does_not_expand() {
        let (db, _temp) = image_db();
        let missing = ComponentId::new("cam.missing");
        assert!(!is_image(&db, missing));
        assert!(!expands(&db, missing));
        assert_eq!(collapsed_label(&db, missing).as_deref(), Some(""));
    }

    #[test]
    fn invalid_image_is_still_an_image() {
        let (db, _temp) = image_db();
        let broken = ComponentId::new("cam.broken");
        assert!(is_image(&db, broken));
        assert!(!expands(&db, broken));
        assert_eq!(image_layout(&db, broken), None);
        assert_eq!(
            collapsed_label(&db, broken).as_deref(),
            Some("unknown image.format")
        );
    }
}
