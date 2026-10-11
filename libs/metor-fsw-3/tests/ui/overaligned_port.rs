use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Timestamp};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C, align(32))]
struct Overaligned {
    #[frame(timestamp)]
    timestamp: Timestamp,
    value: [u64; 3],
}

fn main() {
    let _ = Input::<Overaligned>::new(Vec::new());
}
