use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Timestamp};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(max_len = 64)]
#[repr(C)]
struct Padded {
    #[frame(timestamp)]
    timestamp: Timestamp,
    value: f64,
}

fn main() {}
