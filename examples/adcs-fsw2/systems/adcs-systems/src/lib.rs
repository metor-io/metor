//! The three cyclic systems of the `adcs-fsw2` target — the rigid-body [`plant`], the MEKF
//! [`nav`] filter, and the Yang-LQR [`ctrl`] controller — as **one pack in one crate**
//! (docs/design-packs-authoring.md): a single `cdylib` the host `dlopen`s, a single `fn
//! pack()` every loading mode shares (static registry, dlopen, process worker), and the
//! target's `system` nodes selecting entries by `type=`.
//!
//! All three entries are function-authored: a state struct, an init function,
//! and an execute function whose signature declares the port set.

pub mod ctrl;
pub mod nav;
pub mod plant;

pub use ctrl::CtrlSystem;
pub use nav::sun_from_css;
pub use plant::{DisturbanceTorques, WheelDynamics, css_readings, disturbance_torques, propagate};

use metor_fsw_2_core::{Pack, system};

use crate::{nav::NavState, plant::PlantState};

/// This crate's pack: the three entries under the names the target's `system` nodes select
/// (`type="Plant"` / `type="Nav"` / `type="Ctrl"`).
pub fn pack() -> Pack {
    Pack::new()
        .system("Plant", system(plant::execute).init(PlantState::new))
        .system("Nav", system(NavState::execute).init(NavState::new))
        .system(
            "Ctrl",
            system(CtrlSystem::execute)
                .init(CtrlSystem::new)
                .defaults(adcs_contracts::CtrlParams::default()),
        )
}
metor_fsw_2_core::export_pack!(pack, feature = "export");

#[cfg(test)]
mod tests {
    /// `Ctrl` explicitly declares defaults, so a target node may spell only
    /// overrides. The other entries declare none.
    #[test]
    fn ctrl_entry_declares_default_params() {
        let mut pack = super::pack();
        assert!(pack.entry_mut("Ctrl").unwrap().params_default().is_some());
        assert!(pack.entry_mut("Plant").unwrap().params_default().is_none());
        assert!(pack.entry_mut("Nav").unwrap().params_default().is_none());
    }
}
