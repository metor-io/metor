use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Stop, Timestamp, system};

#[derive(Clone, Copy, Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    time: Timestamp,
    rate: f64,
}

#[system]
async fn watch(imu: Imu, stop: Stop) {
    let _ = imu;
    stop.wait().await;
}

fn main() {}
