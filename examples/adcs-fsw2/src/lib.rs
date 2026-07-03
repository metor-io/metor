//! The `adcs-fsw2` **mission host** — a closed-loop spacecraft attitude-determination-and-
//! control mission whose three systems run as `dlopen`'d `cdylib`s (dl-open.md §8).
//!
//! ```text
//!   plant ──sensors──▶ nav ──attitude_estimate──▶ ctrl
//!     ▲                                            │
//!     └──────────────── torque_cmd ───────────────┘   (one-cycle-delayed feedback)
//! ```
//!
//! The mission is described declaratively in [`mission.kdl`](../mission.kdl) and run by the
//! framework's own CLI (cli-runner.md): the binary's `main` is just
//! [`metor_fsw_2::cli::main()`] (see `src/main.rs`), so
//!
//! ```text
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build            # headless sim
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build --wall \
//!     --telemetry 127.0.0.1:2240                                                  # live → panel
//! ```
//!
//! This crate links **none** of the system crates and **not** `adcs-contracts`: the runner
//! describes the mission as a [`Wiring`], builds + `dlopen`s the three `.so`s, and resolves
//! them schema-agnostically (dl-open.md §6.3). The only library surface left here is
//! [`build_sim_coordinator`], which the convergence test ([`tests/closed_loop.rs`]) uses to
//! get the dlopen'd sim coordinator and compare it against a statically-linked build.

use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{build_artifacts, parse, resolve};
use metor_fsw_2::{BuildOptions, Coordinator};

/// The mission wiring document, compiled into the test binary so the headless convergence
/// check needs no on-disk path. This is the **same** file the CLI runner reads.
const MISSION_KDL: &str = include_str!("../mission.kdl");

/// Build the three system `cdylib`s (`cargo build -p adcs-{plant,nav,ctrl}`) and `dlopen` +
/// resolve them into a ready-to-run [`Coordinator`], using the mission's base config (the
/// free-running simulated clock, no telemetry) — the headless/test configuration. The build
/// driver only recompiles crates cargo considers stale, so re-runs are incremental.
///
/// This mirrors exactly what `metor-fsw run mission.kdl --build` does internally (parse →
/// `build_artifacts` → `resolve`), minus the CLI overrides — it is the test's entry point.
pub fn build_sim_coordinator() -> anyhow::Result<Coordinator> {
    let mut wiring = parse(MISSION_KDL)?;
    build_artifacts(&mut wiring, &BuildOptions::default())?;
    Ok(resolve(&wiring, &Registry::new())?)
}
