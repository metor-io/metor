//! Metadata keys and types for image components.
//!
//! An image component is one tensor: element `U8`, `U16`, or `F32`; `dim`
//! `[H, W]`, or `[H, W, C]` for color formats; row-major, no row padding,
//! little-endian. Pixel (0, 0) is the first sample.

use core::fmt;
use core::str::FromStr;

use metor_proto::types::PrimType;

use crate::{ComponentMetadata, MetadataExt};

pub const IMAGE_FORMAT_KEY: &str = "image.format";
pub const IMAGE_BITS_KEY: &str = "image.bits";
pub const IMAGE_BLACK_KEY: &str = "image.black";
pub const IMAGE_WHITE_KEY: &str = "image.white";
pub const IMAGE_STRETCH_KEY: &str = "image.stretch";

/// How an image's samples are laid out.
///
/// The Bayer formats are one channel; the pattern names the top-left 2×2 tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Mono8,
    Mono16,
    MonoF32,
    Rgb8,
    Rgba8,
    BayerRggb8,
    BayerGrbg8,
    BayerGbrg8,
    BayerBggr8,
    BayerRggb16,
    BayerGrbg16,
    BayerGbrg16,
    BayerBggr16,
}

impl ImageFormat {
    pub const ALL: [Self; 13] = [
        Self::Mono8,
        Self::Mono16,
        Self::MonoF32,
        Self::Rgb8,
        Self::Rgba8,
        Self::BayerRggb8,
        Self::BayerGrbg8,
        Self::BayerGbrg8,
        Self::BayerBggr8,
        Self::BayerRggb16,
        Self::BayerGrbg16,
        Self::BayerGbrg16,
        Self::BayerBggr16,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono8 => "mono8",
            Self::Mono16 => "mono16",
            Self::MonoF32 => "mono_f32",
            Self::Rgb8 => "rgb8",
            Self::Rgba8 => "rgba8",
            Self::BayerRggb8 => "bayer_rggb8",
            Self::BayerGrbg8 => "bayer_grbg8",
            Self::BayerGbrg8 => "bayer_gbrg8",
            Self::BayerBggr8 => "bayer_bggr8",
            Self::BayerRggb16 => "bayer_rggb16",
            Self::BayerGrbg16 => "bayer_grbg16",
            Self::BayerGbrg16 => "bayer_gbrg16",
            Self::BayerBggr16 => "bayer_bggr16",
        }
    }

    pub const fn element(self) -> PrimType {
        match self {
            Self::Mono8
            | Self::Rgb8
            | Self::Rgba8
            | Self::BayerRggb8
            | Self::BayerGrbg8
            | Self::BayerGbrg8
            | Self::BayerBggr8 => PrimType::U8,
            Self::Mono16
            | Self::BayerRggb16
            | Self::BayerGrbg16
            | Self::BayerGbrg16
            | Self::BayerBggr16 => PrimType::U16,
            Self::MonoF32 => PrimType::F32,
        }
    }

    pub const fn channels(self) -> usize {
        match self {
            Self::Rgb8 => 3,
            Self::Rgba8 => 4,
            _ => 1,
        }
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ImageFormat {
    type Err = ImageMetaError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or(ImageMetaError::UnknownFormat)
    }
}

/// The display curve the panel maps samples through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Stretch {
    Linear,
    Sqrt,
    Asinh,
    Log,
    #[default]
    Auto,
}

impl Stretch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Sqrt => "sqrt",
            Self::Asinh => "asinh",
            Self::Log => "log",
            Self::Auto => "auto",
        }
    }
}

impl FromStr for Stretch {
    type Err = core::convert::Infallible;

    /// An unknown name is [`Stretch::Auto`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "linear" => Self::Linear,
            "sqrt" => Self::Sqrt,
            "asinh" => Self::Asinh,
            "log" => Self::Log,
            _ => Self::Auto,
        })
    }
}

/// An image component's metadata, parsed.
///
/// `black` and `white` are raw sample values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageMeta {
    pub format: ImageFormat,
    pub bits: u8,
    pub black: Option<f64>,
    pub white: Option<f64>,
    pub stretch: Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMetaError {
    UnknownFormat,
    InvalidBits,
    InvalidLevel,
    ElementMismatch {
        format: ImageFormat,
        element: PrimType,
    },
    RankMismatch {
        format: ImageFormat,
        rank: usize,
    },
    ChannelMismatch {
        format: ImageFormat,
        channels: u64,
    },
    BitsExceedElement {
        bits: u8,
        element: PrimType,
    },
}

impl ImageMeta {
    /// `Ok(None)` when the metadata has no `image.format`. A missing
    /// `image.bits` is the element's width.
    pub fn parse(meta: &ComponentMetadata) -> Result<Option<Self>, ImageMetaError> {
        let Some(format) = meta.get(IMAGE_FORMAT_KEY) else {
            return Ok(None);
        };
        let format = format.parse::<ImageFormat>()?;
        let bits = match meta.get(IMAGE_BITS_KEY) {
            Some(bits) => parse_bits(bits)?,
            None => element_bits(format.element()),
        };
        Ok(Some(Self {
            format,
            bits,
            black: parse_level(meta.get(IMAGE_BLACK_KEY))?,
            white: parse_level(meta.get(IMAGE_WHITE_KEY))?,
            stretch: meta
                .get(IMAGE_STRETCH_KEY)
                .map_or(Stretch::Auto, |s| s.parse().unwrap_or_default()),
        }))
    }

    /// Checks a component schema against the format.
    pub fn check(&self, element: PrimType, dim: &[u64]) -> Result<(), ImageMetaError> {
        let format = self.format;
        if element != format.element() {
            return Err(ImageMetaError::ElementMismatch { format, element });
        }
        let channels = format.channels();
        let rank = if channels == 1 { 2 } else { 3 };
        if dim.len() != rank {
            return Err(ImageMetaError::RankMismatch {
                format,
                rank: dim.len(),
            });
        }
        if let Some(&c) = dim.get(2)
            && c != channels as u64
        {
            return Err(ImageMetaError::ChannelMismatch {
                format,
                channels: c,
            });
        }
        if self.bits > element_bits(element) {
            return Err(ImageMetaError::BitsExceedElement {
                bits: self.bits,
                element,
            });
        }
        Ok(())
    }
}

fn element_bits(element: PrimType) -> u8 {
    // An element is at most 8 bytes, so its width fits a `u8`.
    (element.size() * 8) as u8
}

fn parse_bits(bits: &str) -> Result<u8, ImageMetaError> {
    match bits.parse::<u8>() {
        Ok(0) | Err(_) => Err(ImageMetaError::InvalidBits),
        Ok(bits) => Ok(bits),
    }
}

fn parse_level(level: Option<&str>) -> Result<Option<f64>, ImageMetaError> {
    level
        .map(|level| level.parse().map_err(|_| ImageMetaError::InvalidLevel))
        .transpose()
}

impl fmt::Display for ImageMetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat => f.write_str("unknown image.format"),
            Self::InvalidBits => f.write_str("image.bits is not an integer in 1..=255"),
            Self::InvalidLevel => f.write_str("image.black or image.white is not a number"),
            Self::ElementMismatch { format, element } => {
                write!(
                    f,
                    "{format} needs {} elements, not {element}",
                    format.element()
                )
            }
            Self::RankMismatch { format, rank } => {
                write!(f, "{format} cannot have {rank} dimensions")
            }
            Self::ChannelMismatch { format, channels } => {
                write!(f, "{format} cannot have {channels} channels")
            }
            Self::BitsExceedElement { bits, element } => {
                write!(f, "{bits} bits do not fit a {element} element")
            }
        }
    }
}

impl std::error::Error for ImageMetaError {}

impl ComponentMetadata {
    /// Marks the component as an image and turns LoD off for it.
    pub fn with_image(mut self, format: ImageFormat, bits: u8) -> Self {
        self.set(IMAGE_FORMAT_KEY, format.as_str());
        self.set(IMAGE_BITS_KEY, &bits.to_string());
        self.with_lod(false)
    }

    /// The raw `image.format` value; present on every image component.
    pub fn image_format(&self) -> Option<&str> {
        self.get(IMAGE_FORMAT_KEY)
    }

    /// `image.bits`, or `None` when it is missing or invalid.
    pub fn image_bits(&self) -> Option<u8> {
        parse_bits(self.get(IMAGE_BITS_KEY)?).ok()
    }

    pub fn image(&self) -> Result<Option<ImageMeta>, ImageMetaError> {
        ImageMeta::parse(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono16(bits: u8) -> ComponentMetadata {
        ComponentMetadata::from("cam.pixels").with_image(ImageFormat::Mono16, bits)
    }

    #[test]
    fn test_with_image_round_trip() {
        let meta = mono16(12);
        assert_eq!(meta.get(IMAGE_FORMAT_KEY), Some("mono16"));
        assert_eq!(meta.get(IMAGE_BITS_KEY), Some("12"));
        assert_eq!(meta.lod(), Some(false));
        assert_eq!(meta.image_format(), Some("mono16"));
        assert_eq!(meta.image_bits(), Some(12));
        let image = meta.image().expect("valid").expect("an image");
        assert_eq!(
            image,
            ImageMeta {
                format: ImageFormat::Mono16,
                bits: 12,
                black: None,
                white: None,
                stretch: Stretch::Auto,
            }
        );
        assert_eq!(image.check(PrimType::U16, &[1024, 1024]), Ok(()));
    }

    #[test]
    fn test_display_hints() {
        let mut meta = mono16(12);
        meta.set(IMAGE_BLACK_KEY, "200");
        meta.set(IMAGE_WHITE_KEY, "4095.5");
        meta.set(IMAGE_STRETCH_KEY, "asinh");
        let image = ImageMeta::parse(&meta).expect("valid").expect("an image");
        assert_eq!(image.black, Some(200.0));
        assert_eq!(image.white, Some(4095.5));
        assert_eq!(image.stretch, Stretch::Asinh);
    }

    #[test]
    fn test_color_check() {
        let meta = ComponentMetadata::from("cam").with_image(ImageFormat::Rgb8, 8);
        let image = meta.image().expect("valid").expect("an image");
        assert_eq!(image.check(PrimType::U8, &[2, 4, 3]), Ok(()));
    }

    #[test]
    fn test_format_names_round_trip() {
        for format in ImageFormat::ALL {
            assert_eq!(format.as_str().parse(), Ok(format));
        }
        assert_eq!(ImageFormat::BayerGbrg16.element(), PrimType::U16);
        assert_eq!(ImageFormat::BayerGbrg16.channels(), 1);
    }

    #[test]
    fn test_stretch_names() {
        for stretch in [
            Stretch::Linear,
            Stretch::Sqrt,
            Stretch::Asinh,
            Stretch::Log,
            Stretch::Auto,
        ] {
            assert_eq!(stretch.as_str().parse(), Ok(stretch));
        }
        assert_eq!("foo".parse(), Ok(Stretch::Auto));
    }

    #[test]
    fn test_missing_bits_is_element_width() {
        for (format, bits) in [
            (ImageFormat::Mono8, 8),
            (ImageFormat::Mono16, 16),
            (ImageFormat::MonoF32, 32),
        ] {
            let mut meta = ComponentMetadata::from("cam");
            meta.set(IMAGE_FORMAT_KEY, format.as_str());
            let image = meta.image().expect("valid").expect("an image");
            assert_eq!(image.bits, bits);
        }
    }

    #[test]
    fn test_not_an_image() {
        assert_eq!(ComponentMetadata::from("cam").image(), Ok(None));
        assert_eq!(ComponentMetadata::from("cam").image_format(), None);
        assert_eq!(ComponentMetadata::from("cam").image_bits(), None);
    }

    #[test]
    fn test_unknown_format_is_error() {
        let mut meta = ComponentMetadata::from("cam");
        meta.set(IMAGE_FORMAT_KEY, "mono12");
        assert_eq!(meta.image(), Err(ImageMetaError::UnknownFormat));
    }

    #[test]
    fn test_bad_values_are_errors() {
        for bits in ["0", "-1", "256", "twelve"] {
            let mut meta = mono16(12);
            meta.set(IMAGE_BITS_KEY, bits);
            assert_eq!(meta.image(), Err(ImageMetaError::InvalidBits), "{bits}");
            assert_eq!(meta.image_bits(), None, "{bits}");
        }
        let mut meta = mono16(12);
        meta.set(IMAGE_WHITE_KEY, "bright");
        assert_eq!(meta.image(), Err(ImageMetaError::InvalidLevel));
    }

    #[test]
    fn test_check_rejects_mismatches() {
        let image = |format, bits| ImageMeta {
            format,
            bits,
            black: None,
            white: None,
            stretch: Stretch::Auto,
        };
        assert_eq!(
            image(ImageFormat::Mono16, 12).check(PrimType::U8, &[4, 4]),
            Err(ImageMetaError::ElementMismatch {
                format: ImageFormat::Mono16,
                element: PrimType::U8,
            })
        );
        assert_eq!(
            image(ImageFormat::Rgb8, 8).check(PrimType::U8, &[4, 4, 4]),
            Err(ImageMetaError::ChannelMismatch {
                format: ImageFormat::Rgb8,
                channels: 4,
            })
        );
        assert_eq!(
            image(ImageFormat::Mono16, 17).check(PrimType::U16, &[4, 4]),
            Err(ImageMetaError::BitsExceedElement {
                bits: 17,
                element: PrimType::U16,
            })
        );
        assert_eq!(
            image(ImageFormat::Mono16, 12).check(PrimType::U16, &[16]),
            Err(ImageMetaError::RankMismatch {
                format: ImageFormat::Mono16,
                rank: 1,
            })
        );
        assert_eq!(
            image(ImageFormat::Rgb8, 8).check(PrimType::U8, &[4, 4]),
            Err(ImageMetaError::RankMismatch {
                format: ImageFormat::Rgb8,
                rank: 2,
            })
        );
    }
}
