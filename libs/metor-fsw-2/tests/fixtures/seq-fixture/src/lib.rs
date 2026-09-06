//! A fixture crate holding one sequence body, built as a `cdylib` so a host
//! test can `dlopen` the shared object and drive the sequence through a slot's
//! lifecycle commands (load, start, stop, reset, abort).
//!
//! `waiter` declares no ports of its own; it carries only the implicit
//! slot-control input and the status and log outputs every sequence
//! has. It waits two simulated microseconds and then completes. If the slot
//! aborts it before the deadline, the wait returns early and the sequence
//! reports `Aborted`. One sequence therefore covers both the run-to-completion
//! and the cooperative-cancel paths.
//!
//! The pack registers the same body twice, as `waiter` and as `napper`, so a
//! slot can allow two occupants with distinct Load names and identical
//! contracts out of one artifact — the occupant-swap shape the process-slot
//! tests drive. A third entry, `beater`, is an ordinary cyclic system (fn
//! style, no ports), proving a slot occupant need not be a sequence at all:
//! the occupant tail is a mount property, so any entry can occupy a slot.
//! A fourth, `gainer`, takes typed params and republishes the configured
//! gain on an output frame, so a host can confirm `allow` params reach the
//! running occupant.

use core::time::Duration;

use metor_fsw_2_core::metor_proto::types::Timestamp;
use metor_fsw_2_core::sequence::{progress, wait};
use metor_fsw_2_core::{Outcome, Output, Pack, Params, system};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

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

/// A plain cyclic system with no ports: state in the leading `&mut u64`,
/// stepped forever until the slot cancels it.
fn beat(count: &mut u64, _now: Timestamp) {
    *count += 1;
}

/// The output frame carrying the gain the sequence was configured with.
#[derive(metor_fsw_2_core::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "gain_out")]
pub struct GainOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub gain: f64,
}

/// Typed params handed to `gainer` at creation.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct GainerParams {
    pub gain: f64,
}

/// Publishes the configured gain on the first poll, waits two simulated
/// microseconds, and completes.
async fn gainer(Params(params): Params<GainerParams>, mut out: Output<GainOut>) -> Outcome {
    progress("publishing gain");
    out.write(&GainOut {
        timestamp: Timestamp(0),
        gain: params.gain,
    })
    .ok();
    if wait(Duration::from_micros(2)).await.aborted() {
        return Outcome::Aborted;
    }
    progress("done");
    Outcome::Completed
}

/// The crate's pack, referenced by `export_pack!` below.
pub fn pack() -> Pack {
    Pack::new()
        .task("waiter", waiter)
        .task("napper", waiter)
        .system("beater", system(beat))
        .task("gainer", gainer)
}

metor_fsw_2_core::export_pack!(pack);
