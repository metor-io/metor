//! Running a system in its own OS process.
//!
//! A **process system** is a dlopen system whose `dlopen` happens in a worker
//! process: the same cdylib artifact, the same `fsw_*` ABI, and the same
//! positional ring binding, but every lifecycle call executes outside the
//! coordinator's address space. The rings that cross the boundary are
//! mmap-backed files both sides attach, and the worker is stepped in
//! lockstep with the cycle through the control block in the `ctl` submodule,
//! whose module doc has the full lifecycle protocol.
//!
//! The mechanism needs a cross-process futex, so process systems are
//! supported on Linux and macOS 14.4+ only; everywhere else this module
//! reduces to the no-op [`worker_entry`] stub, and `build()` rejects a
//! process registration.

pub(crate) mod session;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod ctl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod host;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod worker;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use ctl::StepOutcome;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use host::{ProcError, describe_via_worker};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use worker::worker_entry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use worker::worker_guard_installed;

/// On targets without process-system support the guard is a no-op, so an
/// application's unconditional `worker_entry()` call stays portable.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn worker_entry() {}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
