//! [`FrameStr`], fixed-size text a frame can carry.

use core::fmt;

use metor_proto::com_de::{AsComponentView, FromComponentView};
use metor_proto::error::Error;
use metor_proto::types::{ComponentView, PrimType};
use metor_proto::vtable::builder::{self, FieldBuilder, raw_field, schema};
use metor_proto_wkt::ComponentMetadata;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{AsVTable, Metadatatize, path::ComponentPath};

/// Text in a frame: `N` bytes, NUL-terminated, shown by the panel as a string.
///
/// One component, a `U8` tensor of shape `[N]`, whose metadata marks it as text.
#[derive(Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct FrameStr<const N: usize>([u8; N]);

/// Why text does not fit a [`FrameStr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStrError {
    TooLong { len: usize, capacity: usize },
    Nul,
}

impl<const N: usize> FrameStr<N> {
    /// Text bytes it holds; one byte is always the terminator.
    pub const CAPACITY: usize = N - 1;
    pub const EMPTY: Self = Self([0; N]);

    /// Refuses text longer than [`CAPACITY`](Self::CAPACITY) or holding a NUL.
    pub fn new(text: &str) -> Result<Self, FrameStrError> {
        if text.len() > Self::CAPACITY {
            return Err(FrameStrError::TooLong {
                len: text.len(),
                capacity: Self::CAPACITY,
            });
        }
        if text.as_bytes().contains(&0) {
            return Err(FrameStrError::Nul);
        }
        let mut bytes = [0; N];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self(bytes))
    }

    /// The text up to the first NUL.
    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error> {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(N);
        core::str::from_utf8(&self.0[..len])
    }

    pub fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }
}

impl<const N: usize> Default for FrameStr<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> fmt::Debug for FrameStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_str() {
            Ok(text) => fmt::Debug::fmt(text, f),
            Err(_) => fmt::Debug::fmt(&self.0[..], f),
        }
    }
}

impl fmt::Display for FrameStrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { len, capacity } => {
                write!(f, "text of {len} bytes exceeds {capacity}")
            }
            Self::Nul => f.write_str("text holds a NUL"),
        }
    }
}

impl std::error::Error for FrameStrError {}

impl<const N: usize> AsVTable for FrameStr<N> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("frame_str")
        } else {
            builder::component(path.to_component_id())
        };
        core::iter::once(raw_field(
            0,
            N as u32,
            schema(PrimType::U8, &[N as u64], component),
        ))
    }
}

impl<const N: usize> Metadatatize for FrameStr<N> {
    fn metadata(path: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        core::iter::once(path.to_metadata().with_string())
    }
}

impl<const N: usize> AsComponentView for FrameStr<N> {
    fn as_component_view(&self) -> ComponentView<'_> {
        self.0.as_component_view()
    }
}

impl<const N: usize> FromComponentView for FrameStr<N> {
    fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
        <[u8; N]>::from_component_view(view).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_round_trip() {
        let text = FrameStr::<8>::new("libab.so").err();
        assert_eq!(
            text,
            Some(FrameStrError::TooLong {
                len: 8,
                capacity: 7
            })
        );
        let text = FrameStr::<8>::new("liba.so").expect("fits at capacity");
        assert_eq!(text.to_str(), Ok("liba.so"));
        assert_eq!(text.as_bytes()[7], 0);
        assert_eq!(FrameStr::<8>::EMPTY.to_str(), Ok(""));
        assert_eq!(FrameStr::<8>::default(), FrameStr::<8>::EMPTY);
    }

    #[test]
    fn test_reject_nul() {
        assert_eq!(FrameStr::<8>::new("a\0b").err(), Some(FrameStrError::Nul));
    }

    #[test]
    fn test_decoded_bytes_checked() {
        let unterminated = FrameStr::<3>::read_from_bytes(b"abc").expect("any bytes");
        assert_eq!(unterminated.to_str(), Ok("abc"));
        let invalid = FrameStr::<3>::read_from_bytes(&[0xff, 0xfe, 0]).expect("any bytes");
        assert!(invalid.to_str().is_err());
    }

    #[test]
    fn test_one_string_component() {
        let fields: Vec<_> = FrameStr::<16>::vtable_fields("name").collect();
        assert_eq!(fields.len(), 1);
        let metadata: Vec<_> = FrameStr::<16>::metadata("name").collect();
        assert_eq!(metadata.len(), 1);
        assert!(metadata[0].is_string());
        let text = FrameStr::<16>::new("seq").expect("fits");
        let back =
            FrameStr::<16>::from_component_view(text.as_component_view()).expect("same shape");
        assert_eq!(back, text);
        assert!(FrameStr::<4>::from_component_view(text.as_component_view()).is_err());
    }
}
