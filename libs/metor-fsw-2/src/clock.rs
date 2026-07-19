//! The ambient mission clock.
//!
//! Every record a system writes is stamped with the `now` its `execute` was
//! handed, so ports never need this module. It exists for the code that runs
//! *outside* a port handoff but still wants mission time: the tracing forward
//! layer stamping [`LogEvent`](crate::LogEvent)s at the instant they fire, or
//! an async system publishing an out-of-cycle health record. Under a
//! [`Simulated`](crate::ClockMode::Simulated) clock wall time drifts freely
//! from the cycle timeline, so stamping such records with `Timestamp::now()`
//! would scatter them off the mission's time axis.
//!
//! The coordinator publishes each cycle's timestamp here before stepping the
//! graph. A pack dylib links its own copy of this static, so the ABI shim
//! republishes inside the shared object from the `now` word that already
//! crosses `fsw_pack_execute` — packs and process workers read the same
//! clock with no extra plumbing. Before the first cycle reaches a linkage
//! unit (build, bind, init) the clock is unset and readers fall back to wall
//! time, which coincides with the mission epoch at boot.

use core::sync::atomic::{AtomicI64, Ordering::Relaxed};

use metor_proto::types::Timestamp;

/// Sentinel for "no cycle has been published in this linkage unit yet".
const UNSET: i64 = i64::MIN;

static MISSION_NOW: AtomicI64 = AtomicI64::new(UNSET);

/// Publish the current cycle's timestamp: the coordinator at the top of each
/// cycle, and the ABI shim before each `fsw_pack_execute` step.
pub(crate) fn set_mission_now(now: Timestamp) {
    MISSION_NOW.store(now.0, Relaxed);
}

/// The current cycle's timestamp, or `None` before the first cycle reaches
/// this linkage unit.
pub fn mission_now() -> Option<Timestamp> {
    match MISSION_NOW.load(Relaxed) {
        UNSET => None,
        v => Some(Timestamp(v)),
    }
}

/// [`mission_now`], falling back to wall time before the first cycle.
pub fn mission_now_or_wall() -> Timestamp {
    mission_now().unwrap_or_else(Timestamp::now)
}
