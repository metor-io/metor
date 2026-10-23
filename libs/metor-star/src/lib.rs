//! Star-tracker algorithms. The crate is `no_std` and uses plain `f64` arrays,
//! so it builds for any target the flight software runs on.
#![no_std]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod camera;
pub mod catalog;
pub mod geom;
#[cfg(feature = "sim")]
pub mod render;
pub mod starcat;
pub mod wahba;
