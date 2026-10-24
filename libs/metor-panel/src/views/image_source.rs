//! Loading the images widgets draw, from a local path or bytes inlined in
//! the layout.
//!
//! Inline bytes win over a path. A target ships its presets over a link and
//! the panel may be running on a machine that has never seen the target's
//! filesystem, so an image referenced only by path would be a broken tile
//! there. `path` remains for layouts a user authored locally against their
//! own files.

use std::sync::Arc;

use gpui::{Corners, IntoElement, RenderImage, SharedString, canvas, prelude::*};
use image::{Frame, ImageBuffer, Rgba};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Persisted reference to one image: `data` is base64 (optionally a
/// `data:` URI), `path` the local-authoring form.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ImageRef {
    pub path: String,
    pub data: String,
}

impl ImageRef {
    pub fn is_empty(&self) -> bool {
        self.path.is_empty() && self.data.is_empty()
    }
}

/// A decoded image ready to paint, or why there isn't one.
pub(crate) type LoadedImage = Result<Arc<RenderImage>, SharedString>;

/// Decode `image`, inline bytes first. The error names the source so a
/// broken tile says which file it wanted.
pub(crate) fn load(image: &ImageRef) -> LoadedImage {
    let (bytes, source) = match decode_inline(&image.data) {
        Some(bytes) => (Some(bytes), SharedString::new_static("inline image")),
        None => (
            std::fs::read(&image.path).ok(),
            SharedString::from(image.path.clone()),
        ),
    };
    bytes
        .and_then(|bytes| image::load_from_memory(&bytes).ok())
        .map(|img| {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, rgba.into_raw())
                .expect("buffer size mismatch");
            Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)))
        })
        .ok_or_else(|| SharedString::from(format!("Failed to load: {source}")))
}

/// Decode an inline payload, accepting either raw base64 or a `data:` URI
/// so a preset author can paste either.
pub(crate) fn decode_inline(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if data.is_empty() {
        return None;
    }
    let payload = match data.split_once("base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => data,
    };
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()
}

/// One image slot whose path the inspector edits, decoded once per path.
///
/// Inline bytes travel with the path they were recorded for: editing the
/// path means "show this file instead", so the old bytes stop applying.
#[derive(Default)]
pub(crate) struct CachedImage {
    data: String,
    data_for: SharedString,
    loaded: Option<(SharedString, LoadedImage)>,
}

impl CachedImage {
    pub(crate) fn new(image: &ImageRef) -> Self {
        Self {
            data: image.data.clone(),
            data_for: SharedString::from(image.path.clone()),
            loaded: None,
        }
    }

    /// The image for `path`, `None` when the slot is unset.
    pub(crate) fn resolve(&mut self, path: &SharedString) -> Option<LoadedImage> {
        let image = self.image_ref(path);
        if image.is_empty() {
            return None;
        }
        if self.loaded.as_ref().is_none_or(|(at, _)| at != path) {
            self.loaded = Some((path.clone(), load(&image)));
        }
        self.loaded.as_ref().map(|(_, loaded)| loaded.clone())
    }

    /// What to save for `path`: the inline bytes only while they still
    /// belong to it.
    pub(crate) fn image_ref(&self, path: &SharedString) -> ImageRef {
        ImageRef {
            path: path.to_string(),
            data: if *path == self.data_for {
                self.data.clone()
            } else {
                String::new()
            },
        }
    }
}

/// Paint `image` stretched to its bounds; `grayscale` washes it out, as a
/// stale value is washed out elsewhere.
pub(crate) fn image_canvas(image: Arc<RenderImage>, grayscale: bool) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
            let _ = window.paint_image(
                bounds,
                bounds,
                Corners::default(),
                image.clone(),
                0,
                grayscale,
            );
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_image_data_accepts_raw_base64_and_data_uris() {
        // "hi" in base64.
        assert_eq!(decode_inline("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(
            decode_inline("data:image/png;base64,aGk=").as_deref(),
            Some(&b"hi"[..])
        );
        // Whitespace from a wrapped literal in a preset file.
        assert_eq!(decode_inline(" aGk= \n").as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn undecodable_inline_data_falls_through_to_the_path() {
        assert!(decode_inline("").is_none());
        assert!(decode_inline("not base64!!").is_none());
    }

    #[test]
    fn inline_bytes_stop_applying_once_the_path_is_edited() {
        let cached = CachedImage::new(&ImageRef {
            path: "on.png".into(),
            data: "aGk=".into(),
        });
        assert_eq!(cached.image_ref(&"on.png".into()).data, "aGk=");
        let edited = cached.image_ref(&"other.png".into());
        assert_eq!(edited.path, "other.png");
        assert!(edited.data.is_empty());
    }

    #[test]
    fn an_unset_slot_resolves_to_nothing_and_a_bad_one_to_an_error() {
        let mut empty = CachedImage::default();
        assert!(empty.resolve(&SharedString::default()).is_none());

        let mut missing = CachedImage::default();
        let err = missing
            .resolve(&"/nonexistent/x.png".into())
            .unwrap()
            .unwrap_err();
        assert!(err.contains("/nonexistent/x.png"));
    }
}
