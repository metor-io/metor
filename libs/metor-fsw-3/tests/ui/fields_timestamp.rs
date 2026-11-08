use metor_fsw_3::Fields;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Stamped {
    #[fsw(timestamp)]
    timestamp: metor_fsw_3::Timestamp,
    x: f64,
}

fn main() {}
