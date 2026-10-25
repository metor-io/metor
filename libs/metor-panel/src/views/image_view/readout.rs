//! The hover readout: one pixel's raw value.

use super::layout::Pixels;
use super::source::Frame;

#[derive(Clone, Debug, PartialEq)]
pub struct Readout {
    pub x: usize,
    pub y: usize,
    /// The raw sample: a DN, a float, or one value per channel.
    pub value: String,
    /// When zoomed out, the size of the block the device pixel pools; the
    /// value is the block's middle pixel.
    pub pooled: Option<(usize, usize)>,
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
}
