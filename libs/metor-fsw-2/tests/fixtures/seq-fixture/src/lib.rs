//! A slots/sequences integration-test fixture: one `#[sequence]` occupant made
//! `dlopen`-loadable. The `slot_integration` host test builds this crate as a `cdylib`,
//! `dlopen`s the produced shared object through [`DlSystem`], and drives it through a
//! runtime **slot** (Load/Start/Stop/Reset/Abort).
//!
//! `waiter` has **no user ports** — just the implicit `SlotControlIn` input (the slot's
//! cancel) and the `SequenceStatus` + health/log output tail. It `wait`s ~2 sim-µs then
//! returns `Completed`; if it is aborted before the deadline the `wait` short-circuits
//! and it returns `Aborted`. One sequence thus exercises both the run-to-completion and
//! the cooperative-cancel paths the slot test drives.

use core::time::Duration;

use metor_fsw_2::Outcome;
use metor_fsw_2::ring::Backing;
use metor_fsw_2::sequence::{progress, wait};

/// Wait ~2 sim-µs, then complete — unless aborted first, in which case bail out with
/// `Aborted` (the cooperative cancel the slot's `Abort` command raises). `B: Backing` is
/// the macro's port backing (unused here — `waiter` has no user ports).
#[metor_fsw_2::sequence]
async fn waiter<B: Backing>() -> Outcome {
    progress("waiting");
    if wait(Duration::from_micros(2)).await.aborted() {
        progress("aborted");
        return Outcome::Aborted;
    }
    progress("done");
    Outcome::Completed
}
