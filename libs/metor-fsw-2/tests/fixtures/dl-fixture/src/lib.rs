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

use metor_fsw_2::wiring::{FromKdlNode, LoadError, RegisteredSystem, kdl_optional};
use metor_fsw_2::{
    CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
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

/// The `start` offset added to each tick — crossing `fsw_create` as postcard bytes.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct CounterParams {
    pub start: u64,
}

// Only needed to satisfy `RegisteredSystem::Params: FromKdlNode`; the dl path encodes
// params as canonical postcard bytes and never calls this (dl-open.md §6.3).
impl FromKdlNode for CounterParams {
    fn from_kdl_node(node: &metor_fsw_2::kdl::KdlNode, src: &str) -> Result<Self, LoadError> {
        let start = kdl_optional::<u64>(node, "start", "dl_counter", src)?.unwrap_or(0);
        Ok(Self { start })
    }
}

/// Adds `start` to each input tick and republishes the sum.
pub struct DlCounter {
    start: u64,
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
            count: self.start + value,
        });
    }
}

impl RegisteredSystem for DlCounter {
    type Params = CounterParams;
    fn new(params: Self::Params) -> Self {
        DlCounter { start: params.start }
    }
}

// The one C-ABI surface this cdylib exports (the `fsw_*` symbols the host resolves).
metor_fsw_2::export_system!(DlCounter);
