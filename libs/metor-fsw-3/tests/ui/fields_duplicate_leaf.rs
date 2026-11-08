use metor_fsw_3::Fields;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Twice {
    x: f64,
    #[fsw(rename = "x")]
    y: f64,
}

fn main() {}
