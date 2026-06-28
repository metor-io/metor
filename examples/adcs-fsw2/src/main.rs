//! Runs the ADCS closed loop **live** against a running metor-panel.
//!
//! The loop is paced on a wall clock at 120 Hz and streams every output frame
//! (`sensors` / `attitude_estimate` / `torque_cmd` / `truth` plus each system's
//! health/log) to a metor-panel — which serves a metor-db on `127.0.0.1:2240` — in
//! metor-proto's wire format. Start metor-panel first, then `cargo run -p adcs-fsw2`,
//! and watch the spacecraft converge in the UI. Ctrl-C to stop.
//!
//! `Coordinator::run_for` is a complete `init → run → shutdown` lifecycle (it spawns the
//! telemetry sender + opens the downlink on init and tears them down on shutdown), so it
//! is called **once** for a single continuous mission — looping it would re-create the
//! sender and reconnect every chunk. The per-second convergence print therefore runs on a
//! side task reading the shared `SIM_LOG`, not by chunking the run.
//!
//! If nothing is listening on 2240 the downlink simply fails to connect and the control
//! loop runs unaffected (you'll still see the convergence printout). The fast, headless
//! convergence check lives in `tests/closed_loop.rs`.

use std::net::SocketAddr;
use std::time::Duration;

use adcs_fsw2::{DT, SIM_LOG, build_live, reset_sim_log};

#[stellarator::main]
async fn main() -> anyhow::Result<()> {
    reset_sim_log();

    let addr: SocketAddr = ([127, 0, 0, 1], 2240).into();
    println!("ADCS mission — streaming telemetry to metor-panel at {addr}");
    println!("(start metor-panel first; it serves the db on :2240).  Ctrl-C to stop.\n");

    let mut coord = build_live(addr)?;

    // Side task: print the current attitude error / body rate once a second from the
    // shared convergence log, so the terminal tracks progress while the panel plots the
    // full telemetry. Independent of the coordinator's single continuous run below.
    let _printer = stellarator::spawn(async {
        loop {
            stellarator::sleep(Duration::from_secs(1)).await;
            let log = SIM_LOG.lock().unwrap();
            if let (Some(&err), Some(&rate)) = (log.err_angle.last(), log.rate.last()) {
                let t = log.err_angle.len() as f64 * DT;
                println!(
                    "t={t:6.1}s   attitude error {:6.2}°   body rate {:.4} rad/s",
                    err.to_degrees(),
                    rate
                );
            }
        }
    });

    // One continuous run: init once (sender spawned, downlink opened), then cycle on the
    // wall clock until the process is interrupted.
    coord.run_for(usize::MAX).await;
    Ok(())
}
