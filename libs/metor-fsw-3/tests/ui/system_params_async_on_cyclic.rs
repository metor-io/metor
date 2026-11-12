use metor_fsw_3::ring::Notifier;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, SystemParams, Timestamp, system};

#[derive(Clone, Copy, Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    time: Timestamp,
    rate: f64,
}

#[derive(SystemParams)]
#[system_params(async)]
struct Feeds<'a> {
    imu: &'a mut Input<Imu, Notifier>,
}

#[system]
fn cyclic(feeds: Feeds<'_>) {
    let _ = feeds.imu;
}

fn main() {}
