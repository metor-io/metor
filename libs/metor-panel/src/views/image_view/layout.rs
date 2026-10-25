//! An image component's frame layout, read from its schema and `image.*`
//! metadata, and typed access to a frame's samples.

use std::fmt;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;
use metor_proto_wkt::{ComponentMetadata, ImageFormat, ImageMetaError, Stretch};
use smallvec::SmallVec;
use zerocopy::byteorder::{LE, U16};
use zerocopy::{FromBytes, Immutable, KnownLayout};

/// The shape and display hints of one image component.
///
/// `black` and `white` are kept as written, even when `black > white`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameLayout {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub format: ImageFormat,
    pub bits: u8,
    pub black: Option<f64>,
    pub white: Option<f64>,
    pub stretch: Stretch,
}

/// A frame's samples, borrowed in their stored element type.
#[derive(Clone, Copy, Debug)]
pub enum Pixels<'a> {
    U8(&'a [u8]),
    U16(&'a [U16<LE>]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    NotImage,
    Meta(ImageMetaError),
    Length { expected: usize, actual: usize },
}

impl FrameLayout {
    pub fn from_component(
        schema: &ComponentSchema,
        meta: &ComponentMetadata,
    ) -> Result<Self, LayoutError> {
        let image = meta.image()?.ok_or(LayoutError::NotImage)?;
        let dim: SmallVec<[u64; 4]> = schema.dim.iter().map(|&d| d as u64).collect();
        image.check(schema.prim_type, &dim)?;
        Ok(Self {
            // PANIC Safety: `check` accepts only rank 2 or 3.
            width: schema.dim[1],
            height: schema.dim[0],
            channels: image.format.channels(),
            format: image.format,
            bits: image.bits,
            black: image.black,
            white: image.white,
            stretch: image.stretch,
        })
    }

    pub fn frame_bytes(&self) -> usize {
        self.width * self.height * self.channels * self.format.element().size()
    }

    pub fn pixels<'a>(&self, bytes: &'a [u8]) -> Result<Pixels<'a>, LayoutError> {
        let length = LayoutError::Length {
            expected: self.frame_bytes(),
            actual: bytes.len(),
        };
        if bytes.len() != self.frame_bytes() {
            return Err(length);
        }
        match self.format.element() {
            PrimType::U8 => Ok(Pixels::U8(bytes)),
            PrimType::U16 => cast(bytes, length).map(Pixels::U16),
            element => Err(ImageMetaError::ElementMismatch {
                format: self.format,
                element,
            }
            .into()),
        }
    }
}

fn cast<T: FromBytes + KnownLayout + Immutable>(
    bytes: &[u8],
    err: LayoutError,
) -> Result<&[T], LayoutError> {
    <[T]>::ref_from_bytes(bytes).map_err(|_| err)
}

/// `1024×1024 mono16/12`; the bit depth is shown only when it is narrower
/// than the element.
impl fmt::Display for FrameLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}×{} {}", self.width, self.height, self.format)?;
        if usize::from(self.bits) < self.format.element().size() * 8 {
            write!(f, "/{}", self.bits)?;
        }
        Ok(())
    }
}

impl From<ImageMetaError> for LayoutError {
    fn from(err: ImageMetaError) -> Self {
        Self::Meta(err)
    }
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImage => f.write_str("not an image"),
            Self::Meta(err) => err.fmt(f),
            Self::Length { expected, actual } => {
                write!(f, "frame is {actual} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_proto_wkt::MetadataExt;
    use metor_proto_wkt::{IMAGE_BITS_KEY, IMAGE_BLACK_KEY, IMAGE_FORMAT_KEY, IMAGE_WHITE_KEY};

    fn schema(prim: PrimType, dim: &[usize]) -> ComponentSchema {
        ComponentSchema::new(prim, dim)
    }

    fn image(format: ImageFormat, bits: u8) -> ComponentMetadata {
        ComponentMetadata::from("cam.image.pixels").with_image(format, bits)
    }

    fn parse(
        prim: PrimType,
        dim: &[usize],
        meta: &ComponentMetadata,
    ) -> Result<FrameLayout, LayoutError> {
        FrameLayout::from_component(&schema(prim, dim), meta)
    }

    #[test]
    fn mono16_parses_width_and_height() {
        let layout = parse(PrimType::U16, &[4, 6], &image(ImageFormat::Mono16, 12)).unwrap();
        assert_eq!((layout.width, layout.height, layout.channels), (6, 4, 1));
        assert_eq!((layout.format, layout.bits), (ImageFormat::Mono16, 12));
        assert_eq!((layout.black, layout.white), (None, None));
        assert_eq!(layout.stretch, Stretch::Auto);
        assert_eq!(layout.frame_bytes(), 48);
        assert_eq!(layout.to_string(), "6×4 mono16/12");
    }

    #[test]
    fn rgb8_parses() {
        let layout = parse(PrimType::U8, &[2, 2, 3], &image(ImageFormat::Rgb8, 8)).unwrap();
        assert_eq!((layout.width, layout.height, layout.channels), (2, 2, 3));
        assert_eq!(layout.frame_bytes(), 12);
        assert_eq!(layout.to_string(), "2×2 rgb8");
    }

    #[test]
    fn writer_metadata_parses_with_lod_off() {
        let meta = image(ImageFormat::Mono16, 12).with_lod(false);
        assert!(parse(PrimType::U16, &[4, 6], &meta).is_ok());
    }

    #[test]
    fn pixels_read_little_endian() {
        let layout = parse(PrimType::U16, &[1, 2], &image(ImageFormat::Mono16, 12)).unwrap();
        let Ok(Pixels::U16(px)) = layout.pixels(&[0x34, 0x12, 0xff, 0x0f]) else {
            panic!("mono16 is u16 pixels");
        };
        assert_eq!(
            px.iter().map(|p| p.get()).collect::<Vec<_>>(),
            [0x1234, 0x0fff]
        );

        let layout = parse(PrimType::U8, &[1, 2], &image(ImageFormat::Mono8, 8)).unwrap();
        assert!(matches!(layout.pixels(&[7, 9]), Ok(Pixels::U8([7, 9]))));
    }

    #[test]
    fn one_by_one_frame() {
        let layout = parse(PrimType::U16, &[1, 1], &image(ImageFormat::Mono16, 16)).unwrap();
        assert_eq!((layout.width, layout.height, layout.bits), (1, 1, 16));
        assert_eq!(layout.to_string(), "1×1 mono16");
        assert!(layout.pixels(&[0, 0]).is_ok());
    }

    #[test]
    fn inverted_levels_are_kept() {
        let mut meta = image(ImageFormat::Mono16, 12);
        meta.set(IMAGE_BLACK_KEY, "4000");
        meta.set(IMAGE_WHITE_KEY, "100");
        let layout = parse(PrimType::U16, &[2, 2], &meta).unwrap();
        assert_eq!((layout.black, layout.white), (Some(4000.0), Some(100.0)));
    }

    #[test]
    fn missing_bits_is_element_width() {
        let mut meta = ComponentMetadata::from("cam");
        meta.set(IMAGE_FORMAT_KEY, "mono16");
        let layout = parse(PrimType::U16, &[2, 2], &meta).unwrap();
        assert_eq!(layout.bits, 16);
    }

    #[test]
    fn schema_mismatches_are_errors() {
        let mono16 = image(ImageFormat::Mono16, 12);
        assert_eq!(
            parse(PrimType::U8, &[4, 6], &mono16),
            Err(LayoutError::Meta(ImageMetaError::ElementMismatch {
                format: ImageFormat::Mono16,
                element: PrimType::U8,
            }))
        );
        assert_eq!(
            parse(PrimType::U8, &[2, 2, 4], &image(ImageFormat::Rgb8, 8)),
            Err(LayoutError::Meta(ImageMetaError::ChannelMismatch {
                format: ImageFormat::Rgb8,
                channels: 4,
            }))
        );
        assert_eq!(
            parse(PrimType::U16, &[24], &mono16),
            Err(LayoutError::Meta(ImageMetaError::RankMismatch {
                format: ImageFormat::Mono16,
                rank: 1,
            }))
        );
    }

    #[test]
    fn bad_metadata_is_an_error() {
        let mut meta = image(ImageFormat::Mono16, 12);
        meta.set(IMAGE_BITS_KEY, "17");
        assert_eq!(
            parse(PrimType::U16, &[2, 2], &meta),
            Err(LayoutError::Meta(ImageMetaError::BitsExceedElement {
                bits: 17,
                element: PrimType::U16,
            }))
        );
        meta.set(IMAGE_BITS_KEY, "twelve");
        assert_eq!(
            parse(PrimType::U16, &[2, 2], &meta),
            Err(LayoutError::Meta(ImageMetaError::InvalidBits))
        );
        meta.set(IMAGE_FORMAT_KEY, "mono12");
        assert_eq!(
            parse(PrimType::U16, &[2, 2], &meta),
            Err(LayoutError::Meta(ImageMetaError::UnknownFormat))
        );
        assert_eq!(
            parse(PrimType::U16, &[2, 2], &ComponentMetadata::from("cam")),
            Err(LayoutError::NotImage)
        );
    }

    #[test]
    fn wrong_length_is_an_error() {
        let layout = parse(PrimType::U16, &[4, 6], &image(ImageFormat::Mono16, 12)).unwrap();
        assert_eq!(
            layout.pixels(&[0; 47]).unwrap_err(),
            LayoutError::Length {
                expected: 48,
                actual: 47,
            }
        );
        assert_eq!(
            layout.pixels(&[0; 49]).unwrap_err(),
            LayoutError::Length {
                expected: 48,
                actual: 49,
            }
        );
    }
}
