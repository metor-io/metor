use metor_fsw_3::{Fields, Frame};
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Inner {
    x: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Outer {
    #[frame(timestamp)]
    timestamp: metor_fsw_3::Timestamp,
    #[frame(nest)]
    inner: Inner,
}

fn main() {}
