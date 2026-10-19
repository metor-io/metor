use metor_fsw_3::Frame;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "two_stamps")]
#[repr(C)]
struct TwoStamps {
    #[frame(timestamp)]
    first: metor_fsw_3::Timestamp,
    #[frame(timestamp)]
    second: metor_fsw_3::Timestamp,
}

fn main() {}
