//! A slots/sequences integration-test fixture with **params**: one `#[sequence]`
//! occupant made `dlopen`-loadable, taking a typed `Params` and publishing its
//! value on a user output frame. The `slot_wiring` host test resolves it through
//! a KDL `slot`'s `allow occupant="gainer" gain=…` (the B1 regression: slot
//! occupant params must reach the occupant) and asserts the frame carries the
//! configured gain.

// The `#[sequence]` macro emits the `fsw_*` C-ABI exports (raw-pointer entry
// points); the raw-pointer deref lint is inherent to that surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use core::time::Duration;

use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::Backing;
use metor_fsw_2::sequence::{progress, wait};
use metor_fsw_2::{Outcome, Output};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The user output frame: the configured gain, republished so the host can see
/// the params were applied.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "gain_out")]
pub struct GainOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub gain: f64,
}

/// The typed params crossing `fsw_create` as postcard bytes (the same contract
/// as any dl system's `Params`).
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct GainerParams {
    pub gain: f64,
}

/// Publish the configured gain, wait ~2 sim-µs, then complete. The write happens
/// on the first poll, so even a short run observes the params-derived value.
#[metor_fsw_2::sequence]
async fn gainer<B: Backing>(params: GainerParams, mut out: Output<GainOut, B>) -> Outcome {
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
