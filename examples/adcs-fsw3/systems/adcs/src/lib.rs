//! The `adcs` pack: the rigid-body [`plant`], the MEKF [`nav`] filter, the
//! Yang-LQR [`ctrl`] controller, and the fixed [`mode`] command, in one cdylib
//! the host loads and one [`pack`] table every loading mode shares.

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

/// This pack's table, under the names a target's system nodes select.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: these static record schemas are compatible.
    table
        .register("plant", PlantState::new)
        .expect("valid records");
    table.register("nav", NavState::new).expect("valid records");
    table
        .register("ctrl", CtrlSystem::new)
        .expect("valid records");
    table.register("mode", Mode::new).expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
