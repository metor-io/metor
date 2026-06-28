//! A WP8 dl-open integration-test fixture (dl-open.md §3, §8): one ordinary
//! `impl CyclicSystem` made `dlopen`-loadable with a single `export_system!`. The
//! `dl_integration` host test builds this crate as a `cdylib`, `dlopen`s the produced
//! shared object, and drives it through the real C ABI.
//!
//! `DlCounter` consumes a `tick_in` frame and republishes `start + value` as a
//! `tick_out` frame — enough to prove the input view, the output writer, the params
//! blob, and the descriptor all cross a real `.so` boundary. The frame definitions
//! match the host test's byte-for-byte (same name + layout), the compile-time contract
//! `compatible()` enforces (dl-open.md §8).

// The `export_system!`-generated `extern "C" fn fsw_*` exports take raw pointers by
// ABI contract (the host owns their validity, dl-open.md §2.5); clippy's
// `not_unsafe_ptr_arg_deref` is inherent to that macro surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The input frame the host producer writes (`tick_in`).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
pub struct TickIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

/// The output frame this system produces (`tick_out`).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
pub struct TickOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub count: u64,
}

/// Two params of **differing types** crossing `fsw_create` as postcard bytes (so the
/// Wave 3b KDL≡builder byte-equality gate is a real multi-field, mixed-type check,
/// dl-open.md §6.3): a `start` offset (`u64`) and a `scale` factor (`f64`) applied to
/// each input tick.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct CounterParams {
    pub start: u64,
    pub scale: f64,
}

// Wave 3a (dl-open.md §3.0/§6.3): a dl system needs **no** `FromKdlNode` impl — it is
// constructed only via `BuildSystem` (below), decoding canonical postcard `Params`
// bytes in `fsw_create`. This fixture proves the `RegisteredSystem`/`kdl` decoupling.

/// Applies `start + value * scale` to each input tick and republishes the sum.
pub struct DlCounter {
    start: u64,
    scale: f64,
}

#[derive(SystemInput)]
pub struct DlCounterIn<B: Backing = BoxBacking> {
    tick: Input<TickIn, B>,
}

#[derive(SystemOutput)]
pub struct DlCounterOut<B: Backing = BoxBacking> {
    out: Output<TickOut, B>,
}

impl<B: Backing> System<B> for DlCounter {
    type Input = DlCounterIn<B>;
    type Output = Out<DlCounterOut<B>, B>;
    const NAME: &'static str = "dl_counter";
}

impl<B: Backing> CyclicSystem<B> for DlCounter {
    fn execute(
        &mut self,
        now: Timestamp,
        input: &mut DlCounterIn<B>,
        output: &mut Out<DlCounterOut<B>, B>,
    ) {
        let value = match input.tick.latest() {
            Ok(Some(t)) => t.get().value,
            _ => {
                output.health().error("no_tick");
                return;
            }
        };
        let _ = output.out.write(&TickOut {
            timestamp: now,
            count: self.start + (value as f64 * self.scale).round() as u64,
        });
    }
}

impl BuildSystem for DlCounter {
    type Params = CounterParams;
    fn new(params: Self::Params) -> Self {
        DlCounter {
            start: params.start,
            scale: params.scale,
        }
    }
}

// The one C-ABI surface this cdylib exports (the `fsw_*` symbols the host resolves).
metor_fsw_2::export_system!(DlCounter);
