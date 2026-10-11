//! [`FrameBytes`], a fixed-size byte string a frame carries as one component.

use metor_proto::com_de::{AsComponentView, FromComponentView};
use metor_proto::error::Error;
use metor_proto::types::{ComponentView, PrimType};
use metor_proto::vtable::builder::{self, FieldBuilder, raw_field, schema};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{AsVTable, Metadatatize, path::ComponentPath};

/// `N` bytes in a frame, as one `U8` tensor of shape `[N]`.
///
/// A plain `[u8; N]` field is `N` components; this is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct FrameBytes<const N: usize>(pub [u8; N]);

impl<const N: usize> FrameBytes<N> {
    pub const ZERO: Self = Self([0; N]);
}

impl<const N: usize> Default for FrameBytes<N> {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<const N: usize> AsVTable for FrameBytes<N> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("frame_bytes")
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

impl<const N: usize> Metadatatize for FrameBytes<N> {}

impl<const N: usize> AsComponentView for FrameBytes<N> {
    fn as_component_view(&self) -> ComponentView<'_> {
        self.0.as_component_view()
    }
}

impl<const N: usize> FromComponentView for FrameBytes<N> {
    fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
        <[u8; N]>::from_component_view(view).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_byte_component() {
        let fields: Vec<_> = FrameBytes::<32>::vtable_fields("hash").collect();
        assert_eq!(fields.len(), 1);
        let metadata: Vec<_> = FrameBytes::<32>::metadata("hash").collect();
        assert_eq!(metadata.len(), 1);
        assert!(!metadata[0].is_string());
    }

    #[test]
    fn test_view_round_trip() {
        let bytes = FrameBytes([7u8; 4]);
        let back = FrameBytes::<4>::from_component_view(bytes.as_component_view()).expect("same");
        assert_eq!(back, bytes);
        assert_eq!(FrameBytes::<4>::default(), FrameBytes::ZERO);
        assert!(FrameBytes::<3>::from_component_view(bytes.as_component_view()).is_err());
    }
}
