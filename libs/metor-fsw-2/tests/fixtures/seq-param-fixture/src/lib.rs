//! A test fixture exercising sequence params. The crate builds as a loadable
//! shared library containing one `#[sequence]`, `gainer`, which takes a typed
//! [`GainerParams`] and republishes the configured gain on its output frame.
//! A host that loads the library and passes params can read the frame back to
//! confirm the values reached the running sequence.

// The `#[sequence]` macro generates raw-pointer C entry points, and this lint
// fires on that generated code rather than anything written here.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use core::time::Duration;

use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::sequence::{progress, wait};
use metor_fsw_2::{Outcome, Output};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The output frame carrying the gain the sequence was configured with.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "gain_out")]
pub struct GainOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub gain: f64,
}

/// Typed params handed to the sequence at creation, encoded as postcard bytes.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct GainerParams {
    pub gain: f64,
}

/// Publishes the configured gain, waits two simulated microseconds, and
/// completes. The write lands on the first poll, so even a very short run
/// observes the params-derived value.
#[metor_fsw_2::sequence]
async fn gainer(params: GainerParams, mut out: Output<GainOut>) -> Outcome {
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
