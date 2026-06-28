//! Runs the ADCS closed loop **live** against a running metor-panel, with the three
//! systems loaded from `dlopen`'d `cdylib`s (dl-open.md §8).
//!
//! On startup the host builds the three system `cdylib`s (`cargo build -p adcs-{plant,nav,
//! ctrl}` — incremental, so only changed crates recompile), `dlopen`s them, and resolves
//! the wiring into a coordinator. The loop is then paced on a wall clock at 120 Hz and
//! streams every output frame (`sensors` / `attitude_estimate` / `torque_cmd` / `truth`
//! plus each system's health/log) to metor-panel over TCP, in metor-proto's wire format.
//!
//! Start metor-panel first, then `cargo run -p adcs-fsw2`, and watch the spacecraft
//! converge in the UI (plot `nav.attitude_estimate.q_hat` against `plant.truth.q_true`).
//! Ctrl-C to stop. If nothing is listening on the panel port the downlink simply fails to
//! connect and the control loop runs unaffected. The fast, headless convergence check
//! lives in `tests/closed_loop.rs`.
//!
//! The terminal prints only a heartbeat: this binary stays **schema-agnostic** (it links
//! neither the system crates nor `adcs-contracts`), so it does not decode the `truth`
//! frame to read out convergence — the panel plots it, and the test asserts it.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use adcs_fsw2::{PANEL_ADDR, build_live_coordinator};

#[stellarator::main]
async fn main() -> anyhow::Result<()> {
    let addr: SocketAddr = PANEL_ADDR.into();
    println!("ADCS mission (dlopen) — building the plant/nav/ctrl cdylibs…");
    let mut coord = build_live_coordinator(addr)?;
    println!("systems loaded; streaming telemetry to metor-panel at {addr}.");
    println!("(start metor-panel first; it serves the db on :2240).  Ctrl-C to stop.\n");

    // Heartbeat side task: this binary doesn't decode frames, so it just reports that the
    // mission is alive; convergence is watched in the panel (or asserted by `cargo test`).
    let start = Instant::now();
    let _printer = stellarator::spawn(async move {
        loop {
            stellarator::sleep(Duration::from_secs(5)).await;
            println!(
                "t={:5.0}s — streaming dlopen'd ADCS telemetry to the panel",
                start.elapsed().as_secs_f64()
            );
        }
    });

    // One continuous run: init once (fsw_bind_init per system, sender spawned, downlink
    // opened), then cycle on the wall clock until the process is interrupted.
    coord.run_for(usize::MAX).await;
    Ok(())
}
