//! Running a system in its own OS process.
//!
//! A **process system** is a dlopen system whose `dlopen` happens in a worker
//! process: the same cdylib artifact, the same `fsw_*` ABI, the same
//! positional ring binding — but every lifecycle call executes outside the
//! coordinator's address space, and the rings that cross the boundary are
//! mmap-backed files both sides attach. The worker is stepped in lockstep
//! with the cycle through a small shared **control block** ([`ctl`]): the
//! host rings a doorbell carrying the cycle timestamp, the worker runs one
//! `fsw_execute` and acks, and the host bounds its wait with a deadline so a
//! hung worker costs a telemetered timeout, not a stalled loop.
//!
//! The design and its rationale live in `docs/process-systems.md`.

pub(crate) mod ctl;
pub(crate) mod worker;

pub use ctl::{CtlError, CtlHost, CtlWorker, StepOutcome, WorkerCmd, WorkerState};
pub use worker::{WORKER_ENV, WorkerManifest, worker_entry};

#[cfg(test)]
mod tests;
