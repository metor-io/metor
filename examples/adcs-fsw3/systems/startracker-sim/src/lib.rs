//! The `startracker_sim` pack: a star-tracker image simulator fed by the
//! plant's true attitude.
//!
//! It is its own pack, apart from the `adcs` pack, so a GPU backend can join
//! it without reaching the flight systems.

pub mod params;
pub mod sim;
pub mod system;

pub use params::{Backend, StarCamParams, StarOpticsParams, StarSensorParams, StarSimParams};
pub use sim::StarSim;

use metor_fsw_3::SystemTable;

/// Returns a table holding the `star_sim` system.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: this static record schema is compatible.
    table
        .register_async("star_sim", system::star_sim(), StarSim::new)
        .expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
