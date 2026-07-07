//! A fixture crate holding one `#[sequence]`, built as a `cdylib` so a host
//! test can `dlopen` the shared object and drive the sequence through a slot's
//! lifecycle commands (load, start, stop, reset, abort).
//!
//! `waiter` declares no ports of its own; it carries only the implicit
//! slot-control input and the status, health, and log outputs every sequence
//! has. It waits two simulated microseconds and then completes. If the slot
//! aborts it before the deadline, the wait returns early and the sequence
//! reports `Aborted`. One sequence therefore covers both the run-to-completion
//! and the cooperative-cancel paths.

use core::time::Duration;

use metor_fsw_2::Outcome;
use metor_fsw_2::sequence::{progress, wait};

/// Waits two simulated microseconds, completing unless aborted first.
#[metor_fsw_2::sequence]
async fn waiter() -> Outcome {
    progress("waiting");
    if wait(Duration::from_micros(2)).await.aborted() {
        progress("aborted");
        return Outcome::Aborted;
    }
    progress("done");
    Outcome::Completed
}
