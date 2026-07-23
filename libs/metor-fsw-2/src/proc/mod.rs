//! Running a system in its own OS process.
//!
//! A **process system** is a dlopen system whose `dlopen` happens in a worker
//! process: the same cdylib artifact, the same `fsw_*` ABI, the same
//! positional ring binding — but every lifecycle call executes outside the
//! coordinator's address space, and the rings that cross the boundary are
//! mmap-backed files both sides attach. The worker is stepped in lockstep
//! with the cycle through a small shared control block: the
//! host rings a doorbell carrying the cycle timestamp, the worker runs one
//! `fsw_pack_execute` and acks, and the host bounds its wait with a deadline so a
//! hung worker costs a telemetered timeout, not a stalled loop.
//!
//! The mechanism needs a cross-process futex, so process systems are
//! supported on Linux and macOS 14.4+ only; everywhere else this module
//! reduces to the no-op [`worker_entry`] stub and `build()` rejects a
//! process registration. The design and its rationale live in
//! `docs/process-systems.md`.

pub(crate) mod session;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod ctl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod host;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod worker;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use ctl::{CtlError, CtlHost, CtlWorker, StepOutcome, WorkerCmd, WorkerState};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use host::{ProcError, describe_via_worker};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use worker::worker_guard_installed;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use worker::{RunMode, WORKER_ENV, WorkerManifest, worker_entry};

/// On targets without process-system support the guard is a no-op, so an
/// application's unconditional `worker_entry()` call stays portable.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn worker_entry() {}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
