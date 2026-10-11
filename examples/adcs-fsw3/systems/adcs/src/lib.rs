//! The `adcs` pack: the simulated spacecraft and the flight software that points it.
//!
//! [`plant`] simulates the spacecraft and its sensors. [`nav`] estimates
//! attitude, [`ctrl`] commands the wheels and magnetorquers, and [`mode`] holds
//! the commanded phase and pointing law.

pub mod ctrl;
pub mod mode;
pub mod nav;
pub mod plant;

pub use ctrl::CtrlSystem;
pub use mode::Mode;
pub use nav::{NavState, sun_from_css};
pub use plant::{
    DisturbanceTorques, PlantState, WheelDynamics, css_readings, disturbance_torques, propagate,
};

use metor_fsw_3::SystemTable;

/// Returns a table of the pack's systems, registered as `plant`, `nav`, `ctrl`, and `mode`.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: these static record schemas are compatible.
    table
        .register("plant", plant::plant(), PlantState::new)
        .expect("valid records");
    table
        .register("nav", nav::nav(), NavState::new)
        .expect("valid records");
    table
        .register("ctrl", ctrl::ctrl(), CtrlSystem::new)
        .expect("valid records");
    table
        .register("mode", mode::mode(), Mode::new)
        .expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
