//! A port taken by value in an execute fn (a forgotten `&mut`) is rejected;
//! only the async task form owns its ports.

use metor_fsw_2::{Input, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "ui_imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

fn bad(_state: &mut u64, _imu: Input<Imu>) {}

fn main() {
    let _ = metor_fsw_2::Pack::new().system("bad", metor_fsw_2::system(bad));
}
