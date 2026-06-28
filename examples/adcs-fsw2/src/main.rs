//! Runs the ADCS closed loop **live** against a running metor-panel.
//!
//! The loop is paced on a wall clock at 120 Hz and streams every output frame
//! (`sensors` / `attitude_estimate` / `torque_cmd` / `truth` plus each system's
//! health/log) to a metor-panel — which serves a metor-db on `127.0.0.1:2240` — in
//! metor-proto's wire format. Start metor-panel first, then `cargo run -p adcs-fsw2`,
//! and watch the spacecraft converge in the UI. Ctrl-C to stop.
//!
//! If nothing is listening on 2240 the downlink simply fails to connect and the control
//! loop runs unaffected (you'll still see the convergence printout below).
//!
//! The fast, headless convergence check lives in `tests/closed_loop.rs`.

use std::net::SocketAddr;

use adcs_fsw2::{DT, SIM_LOG, build_live, reset_sim_log};

#[stellarator::main]
async fn main() -> anyhow::Result<()> {
    reset_sim_log();

    let addr: SocketAddr = ([127, 0, 0, 1], 2240).into();
    println!("ADCS mission — streaming telemetry to metor-panel at {addr}");
    println!("(start metor-panel first; it serves the db on :2240).  Ctrl-C to stop.\n");

    let mut coord = build_live(addr)?;

    // Wall-clock paced at 120 Hz: run ~1 s of mission per chunk, then print the current
    // attitude error / body rate so the terminal tracks convergence while the panel plots
    // the full telemetry. Runs until interrupted.
    loop {
        coord.run_for(120).await;
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
}
