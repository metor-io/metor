//! The `adcs-fsw2` **mission host** — a closed-loop spacecraft attitude-determination-and-
//! control mission whose three systems run as `dlopen`'d `cdylib`s (dl-open.md §8).
//!
//! ```text
//!   plant ──sensors──▶ nav ──attitude_estimate──▶ ctrl
//!     ▲                                            │
//!     └──────────────── torque_cmd ───────────────┘   (one-cycle-delayed feedback)
//! ```
//!
//! The plant (rigid-body dynamics), nav (MEKF), and ctrl (Yang-LQR) systems each live in
//! their **own** `cdylib` crate (`adcs-plant`/`adcs-nav`/`adcs-ctrl`), built against the
//! shared [`adcs-contracts`] frame/param contract. This host crate links **none** of them
//! and **not** `adcs-contracts`: it describes the mission as a [`Wiring`] (via the KDL
//! front-end), runs the build driver to compile + locate the three `.so`s, and `resolve`s
//! them through the dlopen loader into a [`Coordinator`]. Frames are validated from the
//! serialized `VTable`s and params are encoded from each `.so`'s exported `Params` schema,
//! so the host stays **fully schema-agnostic** — the payoff of WP8's data/serialization
//! split (dl-open.md §6.3). Edit a control law, `cargo build -p adcs-ctrl` (a small crate),
//! and re-run — without recompiling this host or the telemetry stack.
//!
//! [`adcs-contracts`]: https://docs.rs/adcs-contracts

use std::net::SocketAddr;

use metor_fsw_2::wiring::Registry;
use metor_fsw_2::{
    BuildOptions, ClockSpec, Coordinator, TelemetryModeSpec, TelemetrySpec, build_artifacts, parse,
    resolve,
};

/// Physics / FSW step (1/120 s) — the coordinator's simulated logical step. The systems
/// integrate at the **same** step (the `DT` in `adcs-contracts`); both are 1/120 s so the
/// dynamics and the cycle timestamps advance together.
const DT: f64 = 1.0 / 120.0;

/// The telemetry / metor-db endpoint a live run streams to (the address metor-panel serves).
pub const PANEL_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 2240);

/// The platform shared-object file name for a system crate's `cdylib` (`lib{stem}.dylib`
/// on macOS, `lib{stem}.so` on Linux, `{stem}.dll` on Windows). `stem` is the crate name
/// with hyphens normalized to underscores (`adcs-plant` ⇒ `adcs_plant`).
pub fn cdylib_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// The mission as a KDL wiring document (dl-open.md §6.3): an `artifact` node per system
/// `cdylib` (the `type → .so` map), three dl `system` nodes (`lib=` references the artifact,
/// the remaining properties are the system's params — schema-encoded at `resolve` against
/// the `.so`'s exported schema, never linking the param types here), and the feedback graph
/// with the `ctrl → plant` torque back-edge marked `delayed=#true`.
///
/// The base document uses the free-running `Simulated` clock (no telemetry); a live run
/// overrides the clock to `Wall` and attaches the telemetry downlink ([`live_wiring`]).
pub fn kdl_doc() -> String {
    format!(
        r#"coordinator cycle_rate=120.0 sim_dt={dt}

artifact "plant" crate="adcs-plant" lib="{plant}" type="Plant"
artifact "nav"   crate="adcs-nav"   lib="{nav}"   type="Nav"
artifact "ctrl"  crate="adcs-ctrl"  lib="{ctrl}"  type="Ctrl"

system "plant" type="Plant" lib="plant" init_angle=0.5 init_rate=0.15 meas_sigma=0.002 seed=42
system "nav"   type="Nav"   lib="nav"   meas_sigma=0.02
system "ctrl"  type="Ctrl"  lib="ctrl"  q_weight=5.0 r_weight=8.0

connect "plant" -> "nav"  frame="sensors"
connect "nav"   -> "ctrl" frame="attitude_estimate"
connect "ctrl"  -> "plant" frame="torque_cmd" delayed=#true
"#,
        dt = DT,
        plant = cdylib_name("adcs_plant"),
        nav = cdylib_name("adcs_nav"),
        ctrl = cdylib_name("adcs_ctrl"),
    )
}

/// Build the three system `cdylib`s (`cargo build -p adcs-{plant,nav,ctrl}`) and `dlopen` +
/// resolve them into a ready-to-run [`Coordinator`], using the **free-running simulated
/// clock** and **no telemetry** — the headless/test configuration. The build driver only
/// recompiles crates cargo considers stale, so re-runs are incremental.
pub fn build_sim_coordinator() -> anyhow::Result<Coordinator> {
    let mut wiring = parse(&kdl_doc())?;
    build_artifacts(&mut wiring, &BuildOptions::default())?;
    Ok(resolve(&wiring, &Registry::new())?)
}

/// Build + resolve the same dlopen mission for a **live run against metor-panel**: a paced
/// [`Wall`](ClockSpec::Wall) clock and a TCP telemetry downlink streaming **every** output
/// frame to `addr`. This is what `cargo run` uses; it builds the system `cdylib`s first.
pub fn build_live_coordinator(addr: SocketAddr) -> anyhow::Result<Coordinator> {
    let mut wiring = parse(&kdl_doc())?;
    wiring.coordinator.clock = ClockSpec::Wall;
    wiring.telemetry = Some(TelemetrySpec { addr, mode: TelemetryModeSpec::All });
    build_artifacts(&mut wiring, &BuildOptions::default())?;
    Ok(resolve(&wiring, &Registry::new())?)
}
