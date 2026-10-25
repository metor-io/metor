//! [`FrameImage`], a fixed-size image a frame carries as one component, and
//! its [`Pixel`] types.

use core::{fmt, iter};

use metor_proto::com_de::{AsComponentView, FromComponentView};
use metor_proto::error::Error;
use metor_proto::types::ComponentView;
use metor_proto::vtable::builder::FieldBuilder;
use metor_proto_wkt::{ComponentMetadata, ImageFormat};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::array::{Element, assign, shaped_field};
use crate::{AsVTable, Metadatatize, path::ComponentPath};

/// One pixel: `CHANNELS` samples, of which the low `BITS` bits are used.
pub trait Pixel: FromBytes + IntoBytes + Immutable + KnownLayout + Copy + 'static {
    type Sample: Element;
    const CHANNELS: usize;
    const BITS: u8;
    const FORMAT: ImageFormat;
}

macro_rules! pixel {
    ($(#[$doc:meta])* $name:ident($inner:ty), $sample:ty, $channels:literal, $bits:literal, $format:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
        #[repr(transparent)]
        pub struct $name(pub $inner);

        impl Pixel for $name {
            type Sample = $sample;
            const CHANNELS: usize = $channels;
            const BITS: u8 = $bits;
            const FORMAT: ImageFormat = ImageFormat::$format;
        }
    };
}

pixel!(
    /// An 8-bit gray level.
    Mono8(u8), u8, 1, 8, Mono8
);
pixel!(
    /// Red, green, blue.
    Rgb8([u8; 3]), u8, 3, 8, Rgb8
);
pixel!(
    /// Red, green, blue, alpha.
    Rgba8([u8; 4]), u8, 4, 8, Rgba8
);

/// A gray level in a `u16`, of which the low `BITS` bits are used.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
#[repr(transparent)]
pub struct Mono16<const BITS: u8 = 16>(pub u16);

impl<const B: u8> Pixel for Mono16<B> {
    type Sample = u16;
    const CHANNELS: usize = 1;
    const BITS: u8 = {
        assert!(1 <= B && B <= 16, "Mono16 bits must be in 1..=16");
        B
    };
    const FORMAT: ImageFormat = ImageFormat::Mono16;
}

/// A `W`×`H` image in a frame, as one tensor of shape `[H, W]`, or
/// `[H, W, CHANNELS]` for color pixels.
///
/// Rows are stored top to bottom with no padding. Build a frame holding one
/// with [`FromZeros::new_box_zeroed`]; it is too large to move by value.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct FrameImage<P, const W: usize, const H: usize>([[P; W]; H]);

impl<P: Pixel, const W: usize, const H: usize> FrameImage<P, W, H> {
    pub const WIDTH: usize = W;
    pub const HEIGHT: usize = H;
    pub const BYTES: usize = size_of::<Self>();

    const SHAPE: &'static [usize] = if P::CHANNELS == 1 {
        &[H, W]
    } else {
        &[H, W, P::CHANNELS]
    };
    const DIM: &'static [u64] = if P::CHANNELS == 1 {
        &[H as u64, W as u64]
    } else {
        &[H as u64, W as u64, P::CHANNELS as u64]
    };
    const LAYOUT: () = assert!(
        size_of::<P>() == P::CHANNELS * size_of::<P::Sample>()
            && align_of::<P>().is_multiple_of(align_of::<P::Sample>())
            && H * W * P::CHANNELS * <P::Sample as Element>::PRIM.size() == size_of::<Self>()
            && size_of::<Self>() <= u32::MAX as usize
            && P::BITS as usize <= 8 * size_of::<P::Sample>(),
        "FrameImage pixels must be whole, aligned samples of at most u32::MAX bytes in all"
    );

    /// Every sample, row-major, `W * H * CHANNELS` long.
    pub fn pixels(&self) -> &[P::Sample] {
        let () = Self::LAYOUT;
        // PANIC Safety: `LAYOUT` makes `Self` whole samples and `P`'s
        // alignment a multiple of the sample's, and `Self` is aligned for `P`.
        <[P::Sample]>::ref_from_bytes(self.as_bytes()).expect("packed samples")
    }

    pub fn pixels_mut(&mut self) -> &mut [P::Sample] {
        let () = Self::LAYOUT;
        // PANIC Safety: as in `pixels`.
        <[P::Sample]>::mut_from_bytes(self.as_mut_bytes()).expect("packed samples")
    }

    pub fn rows(&self) -> &[[P; W]; H] {
        &self.0
    }

    pub fn rows_mut(&mut self) -> &mut [[P; W]; H] {
        &mut self.0
    }

    /// The pixel in column `x` of row `y`.
    pub fn get(&self, x: usize, y: usize) -> Option<P> {
        self.0.get(y)?.get(x).copied()
    }
}

impl<P: Pixel, const W: usize, const H: usize> fmt::Debug for FrameImage<P, W, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FrameImage<{}/{}, {W}, {H}>", P::FORMAT, P::BITS)
    }
}

impl<P: Pixel, const W: usize, const H: usize> AsVTable for FrameImage<P, W, H> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let () = Self::LAYOUT;
        iter::once(shaped_field(
            path,
            "frame_image",
            size_of::<Self>() as u32,
            <P::Sample as Element>::PRIM,
            Self::DIM,
        ))
    }
}

impl<P: Pixel, const W: usize, const H: usize> Metadatatize for FrameImage<P, W, H> {
    fn metadata(path: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        iter::once(path.to_metadata().with_image(P::FORMAT, P::BITS))
    }
}

impl<P: Pixel, const W: usize, const H: usize> AsComponentView for FrameImage<P, W, H> {
    fn as_component_view(&self) -> ComponentView<'_> {
        P::Sample::view(self.pixels(), Self::SHAPE)
    }
}

impl<P: Pixel, const W: usize, const H: usize> FromComponentView for FrameImage<P, W, H> {
    /// Builds the image on the stack. For a large image, call
    /// [`assign_component_view`](Self::assign_component_view) on a boxed one.
    fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
        let mut image = Self::new_zeroed();
        image.assign_component_view(view)?;
        Ok(image)
    }

    fn assign_component_view(&mut self, view: ComponentView<'_>) -> Result<(), Error> {
        assign(self.pixels_mut(), Self::SHAPE, view)
    }
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{ComponentId, PrimType};
    use metor_proto_wkt::{IMAGE_BITS_KEY, IMAGE_FORMAT_KEY, MetadataExt};

    use super::*;
    use crate::array::applied;

    type Small = FrameImage<Mono16<12>, 4, 2>;

    fn pattern() -> Box<Small> {
        let mut image = Small::new_box_zeroed().expect("alloc");
        for (i, px) in image.pixels_mut().iter_mut().enumerate() {
            *px = (i as u16 * 37) & 0xfff;
        }
        image
    }

    #[test]
    fn test_mono16_component() {
        let image = pattern();
        let views = applied(&Small::as_vtable(), image.as_bytes());
        assert_eq!(
            views,
            [(ComponentId::new("frame_image"), PrimType::U16, vec![2, 4])]
        );
        let metadata: Vec<_> = Small::metadata("cam.pixels").collect();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].get(IMAGE_FORMAT_KEY), Some("mono16"));
        assert_eq!(metadata[0].get(IMAGE_BITS_KEY), Some("12"));
        assert_eq!(metadata[0].lod(), Some(false));
    }

    #[test]
    fn test_rgb8_component() {
        type Rgb = FrameImage<Rgb8, 4, 2>;
        let image = Rgb::new_box_zeroed().expect("alloc");
        let views = applied(&Rgb::as_vtable(), image.as_bytes());
        assert_eq!(
            views,
            [(ComponentId::new("frame_image"), PrimType::U8, vec![2, 4, 3])]
        );
        assert_eq!(image.pixels().len(), 24);
        let metadata: Vec<_> = Rgb::metadata("cam").collect();
        assert_eq!(metadata[0].get(IMAGE_FORMAT_KEY), Some("rgb8"));
        assert_eq!(metadata[0].get(IMAGE_BITS_KEY), Some("8"));
    }

    #[test]
    fn test_view_round_trip() {
        let image = pattern();
        let back = Small::from_component_view(image.as_component_view()).expect("same shape");
        assert_eq!(back.pixels(), image.pixels());
    }

    #[test]
    fn test_single_row_and_column() {
        let row = FrameImage::<Mono8, 5, 1>::new_box_zeroed().expect("alloc");
        assert_eq!(row.as_component_view().shape(), &[1, 5]);
        let column = FrameImage::<Mono8, 1, 5>::new_box_zeroed().expect("alloc");
        assert_eq!(column.as_component_view().shape(), &[5, 1]);
        assert!(FrameImage::<Mono8, 1, 5>::from_component_view(row.as_component_view()).is_err());
    }

    #[test]
    fn test_mismatch_leaves_target_unchanged() {
        let mut target = pattern();
        let expected = pattern();
        let wrong_prim = FrameImage::<Mono8, 4, 2>::new_box_zeroed().expect("alloc");
        let wrong_shape = FrameImage::<Mono16<12>, 2, 4>::new_box_zeroed().expect("alloc");
        let short = [0u16; 7];
        let wrong_len = ComponentView::U16(nox::array::ArrayView::from_buf_shape_unchecked(
            &short,
            &[2, 4],
        ));
        for view in [
            wrong_prim.as_component_view(),
            wrong_shape.as_component_view(),
            wrong_len,
        ] {
            assert!(matches!(
                target.assign_component_view(view),
                Err(Error::InvalidComponentData)
            ));
            assert_eq!(target.pixels(), expected.pixels());
        }
    }
}
