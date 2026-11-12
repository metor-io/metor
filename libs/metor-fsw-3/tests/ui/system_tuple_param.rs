use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Output, Timestamp, system};

#[derive(Clone, Copy, Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    time: Timestamp,
    rate: f64,
}

#[system]
fn relay(io: (&mut Input<Imu>, &mut Output<Imu>)) {
    let _ = io;
}

fn main() {}
