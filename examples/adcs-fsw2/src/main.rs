//! Runs the ADCS closed loop wired **code-first** and prints the convergence of the
//! attitude error and body rate from the first cycle to the last.

use adcs_fsw2::{DT, SIM_LOG, build_code_first, reset_sim_log};

#[stellarator::main]
async fn main() -> anyhow::Result<()> {
    reset_sim_log();

    let mut coord = build_code_first()?;
    coord.run_for(4000).await;

    let log = SIM_LOG.lock().unwrap();
    let n = log.err_angle.len();
    let init_err = log.err_angle.first().copied().unwrap_or(f64::NAN);
    let final_err = log.err_angle.last().copied().unwrap_or(f64::NAN);
    let init_rate = log.rate.first().copied().unwrap_or(f64::NAN);
    let final_rate = log.rate.last().copied().unwrap_or(f64::NAN);
    println!("ran {n} cycles ({:.1} s sim time)", n as f64 * DT);
    println!(
        "attitude error: {:.4} rad ({:.1} deg) -> {:.5} rad ({:.2} deg)",
        init_err,
        init_err.to_degrees(),
        final_err,
        final_err.to_degrees()
    );
    println!("body rate:      {:.4} rad/s -> {:.6} rad/s", init_rate, final_rate);
    Ok(())
}
