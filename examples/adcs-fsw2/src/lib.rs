//! The `adcs-fsw2` **mission host** — a closed-loop spacecraft attitude-determination-and-
//! control mission whose systems ride two `dlopen`'d pack `cdylib`s (dl-open.md §8;
//! docs/design-packs-authoring.md): `adcs-systems` (plant/nav/ctrl) and `adcs-sequences`
//! (the `mode` slot's occupants).
//!
//! ```text
//!   plant ──sensors──▶ nav ──attitude_estimate──▶ ctrl
//!     ▲                                            │
//!     └──────────────── torque_cmd ───────────────┘   (one-cycle-delayed feedback)
//! ```
//!
//! The mission is described in [`mission.py`](../mission.py) — the `metor_config` Python
//! front-end — and run by the framework's own CLI (cli-runner.md): the binary's `main` is
//! just [`metor_fsw_2::cli::main()`] (see `src/main.rs`), so
//!
//! ```text
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.py --build            # headless sim
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.py --build --wall \
//!     --telemetry 127.0.0.1:2240                                                 # live → panel
//! ```
//!
//! This crate links **none** of the system crates and **not** `adcs-contracts`: the runner
//! describes the mission as a [`Wiring`], builds + `dlopen`s the pack `.so`s, and resolves
//! them schema-agnostically (dl-open.md §6.3). The only library surface left here is
//! [`build_sim_coordinator`], which the convergence test ([`tests/closed_loop.rs`]) uses to
//! get the dlopen'd sim coordinator and compare it against a statically-linked build.

use std::path::Path;

use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{eval_python_mission, provision_artifacts, resolve};
use metor_fsw_2::{BuildOptions, Coordinator};

/// The mission file the CLI runner reads, resolved against this crate's manifest so the
/// headless convergence check finds it regardless of the working directory.
fn mission_py() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("mission.py")
}

/// Build the pack `cdylib`s (`cargo build -p adcs-systems -p adcs-sequences`) and `dlopen` +
/// resolve them into a ready-to-run [`Coordinator`], using the mission's base config (the
/// free-running simulated clock, no telemetry) — the headless/test configuration. The build
/// driver only recompiles crates cargo considers stale, so re-runs are incremental.
///
/// This mirrors exactly what `metor-fsw run mission.py --build` does internally (evaluate →
/// `provision_artifacts` → `resolve`), minus the CLI overrides — it is the test's entry point.
pub fn build_sim_coordinator() -> anyhow::Result<Coordinator> {
    let mut wiring = eval_python_mission(&mission_py()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // A `process=#true` system re-execs the current binary as its worker, which only the CLI
    // runner's `main` supports (`metor_fsw_2::proc::worker_entry`) — a test binary would hang
    // the describe handshake. The headless/test configuration runs every system in-process.
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    provision_artifacts(&mut wiring, &BuildOptions::default())?;
    Ok(resolve(&wiring, &Registry::with_builtins())?)
}
