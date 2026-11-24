//! Opt-in frame-rate counters, for measuring how often the UI and plots
//! actually redraw. `METOR_FRAME_STATS=1` logs the rates every two seconds;
//! otherwise every counter is a single relaxed load.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Window draws (the root view renders once per draw).
pub(crate) static DRAWS: AtomicU64 = AtomicU64::new(0);
/// Plots recorded into a frame (a render that reached the GPU).
pub(crate) static PLOT_RENDERS: AtomicU64 = AtomicU64::new(0);
/// Render requests skipped because nothing they draw from changed.
pub(crate) static PLOT_SKIPS: AtomicU64 = AtomicU64::new(0);
/// Frames submitted to the GPU (one per flush with work).
pub(crate) static SUBMITS: AtomicU64 = AtomicU64::new(0);

const PERIOD: Duration = Duration::from_secs(2);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("METOR_FRAME_STATS").is_some_and(|v| v != "0"))
}

pub(crate) fn bump(counter: &AtomicU64) {
    if enabled() {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Start the logger once, if enabled.
pub(crate) fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if !enabled() {
        return;
    }
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("frame-stats".into())
            .spawn(|| {
                let mut last = Instant::now();
                loop {
                    std::thread::sleep(PERIOD);
                    let secs = last.elapsed().as_secs_f64();
                    last = Instant::now();
                    let rate = |c: &AtomicU64| c.swap(0, Ordering::Relaxed) as f64 / secs;
                    tracing::info!(
                        draws = format!("{:.1}", rate(&DRAWS)),
                        plot_renders = format!("{:.1}", rate(&PLOT_RENDERS)),
                        plot_skips = format!("{:.1}", rate(&PLOT_SKIPS)),
                        submits = format!("{:.1}", rate(&SUBMITS)),
                        "frame stats (per second)"
                    );
                }
            })
            .ok();
    });
}
