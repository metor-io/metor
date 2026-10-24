//! Star-tracker algorithms. The crate is `no_std` and uses plain `f64` arrays,
//! so it builds for any target the flight software runs on.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod camera;
pub mod catalog;
pub mod geom;
pub mod image;
pub mod lis;
#[cfg(feature = "sim")]
pub mod render;
pub mod starcat;
pub mod tracker;
pub mod wahba;
