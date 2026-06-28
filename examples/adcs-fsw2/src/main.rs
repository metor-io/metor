//! The `adcs-fsw2` mission binary — a one-line delegate to the framework runner
//! (cli-runner.md). All build/package/run logic lives in [`metor_fsw_2::cli`], so this
//! mission is driven entirely by `mission.kdl` + CLI flags:
//!
//! ```text
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build            # headless sim
//! cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build --wall \
//!     --telemetry 127.0.0.1:2240                                                  # live → metor-panel
//! ```
//!
//! `metor-fsw run examples/adcs-fsw2/mission.kdl …` (the standalone framework binary)
//! behaves identically. The headless convergence check lives in `tests/closed_loop.rs`.

fn main() {
    metor_fsw_2::cli::main()
}
