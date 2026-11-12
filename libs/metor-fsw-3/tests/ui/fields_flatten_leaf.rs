use metor_fsw_3::Fields;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Scalar {
    #[fsw(flatten)]
    x: f64,
}

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Array {
    #[fsw(flatten)]
    xs: [f64; 3],
}

fn main() {}
