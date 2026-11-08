use metor_fsw_3::ring::Notifier;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Sample, SystemParams, Timestamp};

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
    fresh: &'a mut Input<Imu, Notifier>,
    held: &'a Sample<Imu>,
}

fn main() {}
