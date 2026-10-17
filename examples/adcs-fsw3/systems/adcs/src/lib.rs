//! The `adcs` pack: the simulated spacecraft and the flight software that points it.
//!
//! [`plant`] simulates the spacecraft and its sensors, and [`gps_receiver`]
//! a GPS receiver on a serial port. [`gps`] reads that receiver, [`nav`]
//! estimates attitude, [`ctrl`] commands the wheels and magnetorquers, and
//! [`mode`] holds the commanded phase and pointing law.

pub mod ctrl;
pub mod gps;
pub mod gps_receiver;
pub mod mode;
pub mod nav;
pub mod plant;

pub use ctrl::CtrlSystem;
pub use gps::GpsDriver;
pub use gps_receiver::GpsReceiver;
pub use mode::Mode;
pub use nav::{NavState, sun_from_css};
pub use plant::{
    DisturbanceTorques, PlantState, TrueState, WheelDynamics, css_readings, propagate,
};

use metor_fsw_3::SystemTable;

/// Returns a table of the pack's systems, registered as `plant`, `nav`, `ctrl`,
/// `mode`, `gps_driver`, and `gps_receiver`.
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
        .register_sequence("gps_driver", gps::gps_driver(), GpsDriver::new)
        .expect("valid records");
    table
        .register_sequence(
            "gps_receiver",
            gps_receiver::gps_receiver(),
            GpsReceiver::new,
        )
        .expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
