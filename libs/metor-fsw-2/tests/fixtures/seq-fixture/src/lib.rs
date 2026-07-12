//! A fixture crate holding one sequence body, built as a `cdylib` so a host
//! test can `dlopen` the shared object and drive the sequence through a slot's
//! lifecycle commands (load, start, stop, reset, abort).
//!
//! `waiter` declares no ports of its own; it carries only the implicit
//! slot-control input and the status, health, and log outputs every sequence
//! has. It waits two simulated microseconds and then completes. If the slot
//! aborts it before the deadline, the wait returns early and the sequence
//! reports `Aborted`. One sequence therefore covers both the run-to-completion
//! and the cooperative-cancel paths.
//!
//! The pack registers the same body twice, as `waiter` and as `napper`, so a
//! slot can allow two occupants with distinct Load names and identical
//! contracts out of one artifact — the occupant-swap shape the process-slot
//! tests drive.

use core::time::Duration;

use metor_fsw_2::sequence::{progress, wait};
use metor_fsw_2::{Outcome, Pack};

/// Load-time canary for the process-slot isolation tests: when
/// `SEQ_FIXTURE_CANARY` names a file, mapping this object appends the loading
/// process's pid to it, so a host can prove which address spaces ever held
/// the artifact (a process slot's host must never appear — only its describe
/// and run workers do). Without the env var, every in-process test's dlopen,
/// this is a no-op.
#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn canary() {
    if let Ok(path) = std::env::var("SEQ_FIXTURE_CANARY") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}", std::process::id());
        }
    }
}

#[cfg(target_os = "macos")]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static CANARY: extern "C" fn() = canary;

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static CANARY: extern "C" fn() = canary;

/// Waits two simulated microseconds, completing unless aborted first.
async fn waiter() -> Outcome {
    progress("waiting");
    if wait(Duration::from_micros(2)).await.aborted() {
        progress("aborted");
        return Outcome::Aborted;
    }
    progress("done");
    Outcome::Completed
}

/// The crate's pack, referenced by `export_pack!` below.
pub fn pack() -> Pack {
    Pack::new()
        .sequence("waiter", waiter)
        .sequence("napper", waiter)
}

metor_fsw_2::export_pack!(pack);
