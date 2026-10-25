//! [`FrameArray`], `N` numbers a frame carries as one component, and the
//! [`Element`] trait for the samples of shaped components.

use core::iter;

use metor_proto::com_de::{AsComponentView, FromComponentView};
use metor_proto::error::Error;
use metor_proto::types::{ComponentView, PrimType};
use metor_proto::vtable::builder::{self, FieldBuilder, raw_field, schema};
use nox::array::ArrayView;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::{AsVTable, Metadatatize, path::ComponentPath};

/// A primitive that can be the element of a shaped component.
pub trait Element: FromBytes + IntoBytes + Immutable + KnownLayout + Copy + 'static {
    const PRIM: PrimType;

    fn view<'a>(samples: &'a [Self], shape: &'a [usize]) -> ComponentView<'a>;

    /// The view's samples, if its element is `Self`.
    fn samples(view: ComponentView<'_>) -> Option<&[Self]>;
}

macro_rules! impl_element {
    ($ty:ty, $prim:ident) => {
        impl Element for $ty {
            const PRIM: PrimType = PrimType::$prim;

            fn view<'a>(samples: &'a [Self], shape: &'a [usize]) -> ComponentView<'a> {
                ComponentView::$prim(ArrayView::from_buf_shape_unchecked(samples, shape))
            }

            fn samples(view: ComponentView<'_>) -> Option<&[Self]> {
                match view {
                    ComponentView::$prim(view) => Some(view.buf),
                    _ => None,
                }
            }
        }
    };
}

impl_element!(u8, U8);
impl_element!(u16, U16);
impl_element!(u32, U32);
impl_element!(i32, I32);
impl_element!(f32, F32);
impl_element!(f64, F64);

/// Copies `view` into `dst` when its element, shape, and length match.
/// On error `dst` is unchanged.
pub(crate) fn assign<T: Element>(
    dst: &mut [T],
    shape: &[usize],
    view: ComponentView<'_>,
) -> Result<(), Error> {
    if view.shape() != shape {
        return Err(Error::InvalidComponentData);
    }
    let src = T::samples(view)
        .filter(|src| src.len() == dst.len())
        .ok_or(Error::InvalidComponentData)?;
    dst.copy_from_slice(src);
    Ok(())
}

/// One raw field covering `len` bytes at offset 0, named by `path` or by
/// `fallback` at the root.
pub(crate) fn shaped_field(
    path: impl ComponentPath,
    fallback: &'static str,
    len: u32,
    prim: PrimType,
    dim: &[u64],
) -> FieldBuilder {
    let component = if path.is_empty() {
        builder::component(fallback)
    } else {
        builder::component(path.to_component_id())
    };
    raw_field(0, len, schema(prim, dim, component))
}

/// `N` numbers in a frame, as one tensor of shape `[N]`.
///
/// A plain `[T; N]` field of a derived struct is `N` components; this is one.
#[derive(Clone, Copy, Debug, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct FrameArray<T, const N: usize>(pub [T; N]);

impl<T: Element, const N: usize> FrameArray<T, N> {
    const SHAPE: &'static [usize] = &[N];
    const LAYOUT: () = assert!(
        N * T::PRIM.size() == size_of::<Self>() && size_of::<Self>() <= u32::MAX as usize,
        "FrameArray must be N packed elements of at most u32::MAX bytes"
    );
}

impl<T: Element, const N: usize> Default for FrameArray<T, N> {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl<T: Element, const N: usize> AsVTable for FrameArray<T, N> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let () = Self::LAYOUT;
        iter::once(shaped_field(
            path,
            "frame_array",
            size_of::<Self>() as u32,
            T::PRIM,
            &[N as u64],
        ))
    }
}

impl<T: Element, const N: usize> Metadatatize for FrameArray<T, N> {}

impl<T: Element, const N: usize> AsComponentView for FrameArray<T, N> {
    fn as_component_view(&self) -> ComponentView<'_> {
        T::view(&self.0, Self::SHAPE)
    }
}

impl<T: Element, const N: usize> FromComponentView for FrameArray<T, N> {
    fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
        let mut array = Self::new_zeroed();
        array.assign_component_view(view)?;
        Ok(array)
    }

    fn assign_component_view(&mut self, view: ComponentView<'_>) -> Result<(), Error> {
        assign(&mut self.0, Self::SHAPE, view)
    }
}

/// Each component a vtable reads from `bytes`, with its element and shape.
#[cfg(test)]
pub(crate) fn applied(
    vtable: &metor_proto::vtable::VTable,
    bytes: &[u8],
) -> Vec<(metor_proto::types::ComponentId, PrimType, Vec<usize>)> {
    let mut views = Vec::new();
    let mut sink = |id, view: ComponentView<'_>, _| {
        views.push((id, view.prim_type(), view.shape().to_vec()));
    };
    let applied = vtable.apply(bytes, &mut sink);
    assert!(matches!(applied, Ok(Ok(()))));
    views
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;

    use super::*;

    #[test]
    fn test_one_shaped_component() {
        let array = FrameArray([0.5f32; 32]);
        let views = applied(&FrameArray::<f32, 32>::as_vtable(), array.as_bytes());
        assert_eq!(
            views,
            [(ComponentId::new("frame_array"), PrimType::F32, vec![32])]
        );
        let metadata: Vec<_> = FrameArray::<f32, 32>::metadata("st.u").collect();
        assert_eq!(metadata.len(), 1);
        assert!(
            metadata[0]
                .metadata
                .keys()
                .all(|k| !k.starts_with("image."))
        );
        assert_eq!(metadata[0].image_format(), None);
    }

    #[test]
    fn test_view_round_trip() {
        let array = FrameArray([1u32, 2, u32::MAX]);
        let view = array.as_component_view();
        assert_eq!(view.prim_type(), PrimType::U32);
        assert_eq!(view.shape(), &[3]);
        let back = FrameArray::<u32, 3>::from_component_view(view).expect("same shape");
        assert_eq!(back, array);
    }

    #[test]
    fn test_one_element() {
        let array = FrameArray([7u8]);
        let back = FrameArray::<u8, 1>::from_component_view(array.as_component_view());
        assert_eq!(back.ok(), Some(array));
        assert!(FrameArray::<u8, 1>::from_component_view(7u8.as_component_view()).is_err());
    }

    #[test]
    fn test_mismatch_leaves_target_unchanged() {
        let mut array = FrameArray([9u16; 4]);
        let wrong_prim = FrameArray([1u32; 4]);
        let wrong_shape = FrameArray([1u16; 3]);
        let short = [1u16; 3];
        let wrong_len = ComponentView::U16(ArrayView::from_buf_shape_unchecked(&short, &[4]));
        for view in [
            wrong_prim.as_component_view(),
            wrong_shape.as_component_view(),
            wrong_len,
        ] {
            assert!(matches!(
                array.assign_component_view(view),
                Err(Error::InvalidComponentData)
            ));
            assert_eq!(array, FrameArray([9u16; 4]));
        }
    }
}
