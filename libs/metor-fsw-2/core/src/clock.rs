//! The ambient FSW clock.
//!
//! Every record a system writes is stamped with the `now` its `execute` was
//! handed, so ports never need this module. It exists for the code that runs
//! *outside* a port handoff but still wants FSW time: the tracing forward
//! layer stamping [`LogEvent`](crate::LogEvent)s at the instant they fire, or
//! an async system publishing an out-of-cycle health record. Under a
//! [`Simulated`](crate::ClockMode::Simulated) clock wall time drifts freely
//! from the cycle timeline, so stamping such records with `Timestamp::now()`
//! would scatter them off the FSW's time axis.
//!
//! The coordinator publishes each cycle's timestamp here before stepping the
//! graph. A pack dylib links its own copy of this static, so the ABI shim
//! republishes inside the shared object from the `now` word that already
//! crosses `fsw_pack_execute` — packs and process workers read the same
//! clock with no extra plumbing. Before the first cycle reaches a linkage
//! unit (build, bind, init) the clock is unset and readers fall back to wall
//! time. A simulated run chooses its epoch when `run_for` starts.

use core::sync::atomic::{AtomicI64, Ordering::Relaxed};

use metor_proto::types::Timestamp;

/// Sentinel for "no cycle has been published in this linkage unit yet".
const UNSET: i64 = i64::MIN;

static NOW: AtomicI64 = AtomicI64::new(UNSET);

/// Publish the current cycle's timestamp: the coordinator at the top of each
/// cycle, and the ABI shim before each `fsw_pack_execute` step.
pub fn set_now(now: Timestamp) {
    NOW.store(now.0, Relaxed);
}

/// The current cycle timestamp, falling back to wall time before the first
/// cycle reaches this linkage unit.
pub fn now_or_wall() -> Timestamp {
    match NOW.load(Relaxed) {
        UNSET => None,
        v => Some(Timestamp(v)),
    }
    .unwrap_or_else(Timestamp::now)
}

/// A stopwatch for the per-execute timing that feeds
/// [`SystemStatus`](crate::SystemStatus)'s `last_execute_us`.
///
/// `wasm32-unknown-unknown` has no monotonic clock of its own — `Instant::now`
/// is unsupported there and panics — so a guest reads the host's through the
/// `fsw.monotonic_us` import instead, which the wasm host links in beside the
/// module.
pub(crate) struct ExecTimer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
    #[cfg(target_arch = "wasm32")]
    start_us: u64,
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "fsw")]
unsafe extern "C" {
    /// Microseconds on the host's monotonic clock; the epoch is arbitrary.
    fn monotonic_us() -> u64;
}

impl ExecTimer {
    /// Start timing an execute.
    pub(crate) fn start() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
            // SAFETY: a plain host import with no arguments or pointers.
            #[cfg(target_arch = "wasm32")]
            start_us: unsafe { monotonic_us() },
        }
    }

    /// Microseconds since [`start`](Self::start).
    pub(crate) fn elapsed_micros(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.start.elapsed().as_micros() as u64
        }
        #[cfg(target_arch = "wasm32")]
        {
            // SAFETY: as above.
            unsafe { monotonic_us() }.saturating_sub(self.start_us)
        }
    }
}
