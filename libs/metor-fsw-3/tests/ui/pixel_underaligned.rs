use metor_fsw_3::metor_proto_wkt::ImageFormat;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{AsVTable, FrameImage, Pixel};

#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
struct Packed16(u16);

impl Pixel for Packed16 {
    type Sample = u16;
    const CHANNELS: usize = 1;
    const BITS: u8 = 16;
    const FORMAT: ImageFormat = ImageFormat::Mono16;
}

fn main() {
    let _ = FrameImage::<Packed16, 4, 4>::as_vtable();
}
