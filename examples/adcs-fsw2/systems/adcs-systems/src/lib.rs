//! The three cyclic systems of the `adcs-fsw2` mission — the rigid-body [`plant`], the MEKF
//! [`nav`] filter, and the Yang-LQR [`ctrl`] controller — as **one pack in one crate**
//! (docs/design-packs-authoring.md): a single `cdylib` the host `dlopen`s, a single `fn
//! pack()` every loading mode shares (static registry, dlopen, process worker), and the
//! mission's `system` nodes selecting entries by `type=`.
//!
//! The pack deliberately mixes both authoring styles: `plant` and `nav` are fn-authored
//! (a plain state struct, an `init` fn, an `execute` fn whose signature is the port set),
//! while `ctrl` stays `#[system]` struct-authored and rides in via `system_type` — the
//! example suite keeps both styles exercised end to end.

pub mod ctrl;
pub mod nav;
pub mod plant;

pub use ctrl::CtrlSystem;
pub use nav::sun_from_css;
pub use plant::{DisturbanceTorques, WheelDynamics, css_readings, disturbance_torques, propagate};

use metor_fsw_2::{Pack, system};

use crate::{nav::NavState, plant::PlantState};

/// This crate's pack: the three entries under the names the mission's `system` nodes select
/// (`type="Plant"` / `type="Nav"` / `type="Ctrl"`).
pub fn pack() -> Pack {
    Pack::new()
        .system("Plant", system(plant::execute).init(PlantState::new))
        .system("Nav", system(NavState::execute).init(NavState::new))
        // Deliberately struct-authored (`#[system]`), so the pack exercises both styles.
        .system_type::<CtrlSystem, _>("Ctrl")
}
metor_fsw_2::export_pack!(pack, feature = "export");
