use metor_fsw_3::Frame;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "no_stamp")]
#[repr(C)]
struct NoStamp {
    value: u64,
}

fn main() {}
