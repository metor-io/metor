//! A dl-open integration-test fixture: one ordinary `impl CyclicSystem` made
//! `dlopen`-loadable with a single `export_system!`. The `dl_integration` host test
//! builds this crate as a `cdylib`, `dlopen`s the produced shared object, and drives
//! it through the real C ABI.
//!
//! `DlCounter` consumes a `tick_in` frame and republishes `start + value` as a
//! `tick_out` frame — enough to prove the input view, the output writer, the params
//! blob, and the descriptor all cross a real `.so` boundary. The frame definitions
//! match the host test's byte-for-byte (same name + layout), the compile-time contract
//! `compatible()` enforces.

// The `export_system!`-generated `extern "C" fn fsw_*` exports take raw pointers by
// ABI contract (the host owns their validity); clippy's `not_unsafe_ptr_arg_deref` is
// inherent to that macro surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, MsgOut, NamedMsg, Out, Output, System, SystemInput,
    SystemOutput,
};
use metor_fsw_2::metor_proto::types::Timestamp;
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

/// A self-describing `(PacketId, postcard)` **message** this system also emits — the
/// Postcard-schema port crossing the dl ABI (`fsw_describe` carries it as
/// `PortSchemaMsg::Postcard`; the host wires + taps it like any message channel).
/// The `Msg` impl is the blanket `Serialize + Schema` one; the id hashes the schema
/// name, so the host's byte-identical mirror decodes it with nothing but the id.
#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
pub struct TickEvent {
    pub count: u64,
}

impl NamedMsg for TickEvent {
    const NAME: &'static str = "TickEvent";
}

/// Two params of **differing types** crossing `fsw_create` as postcard bytes (so the
/// KDL≡builder byte-equality gate is a real multi-field, mixed-type check): a `start`
/// offset (`u64`) and a `scale` factor (`f64`) applied to each input tick.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
pub struct CounterParams {
    pub start: u64,
    pub scale: f64,
}

// A dl system needs no KDL coupling at all — it is constructed only via
// `BuildSystem` (below), decoding canonical postcard `Params` bytes in `fsw_create`.
// This fixture proves the `BuildSystem`/`kdl` decoupling.

/// Applies `start + value * scale` to each input tick and republishes the sum.
pub struct DlCounter {
    start: u64,
    scale: f64,
}

#[derive(SystemInput)]
pub struct DlCounterIn {
    tick: Input<TickIn>,
}

#[derive(SystemOutput)]
pub struct DlCounterOut {
    out: Output<TickOut>,
    /// The Postcard port beside the Table port — one bundle, both schemas.
    events: MsgOut<TickEvent>,
}

impl System for DlCounter {
    type Input = DlCounterIn;
    type Output = Out<DlCounterOut>;
    const NAME: &'static str = "dl_counter";
}

impl CyclicSystem for DlCounter {
    fn execute(
        &mut self,
        now: Timestamp,
        input: &mut DlCounterIn,
        output: &mut Out<DlCounterOut>,
    ) {
        let value = match input.tick.latest() {
            Some(t) => t.get().value,
            None => {
                output.health().error("no_tick");
                return;
            }
        };
        let count = self.start + (value as f64 * self.scale).round() as u64;
        let _ = output.out.write(&TickOut {
            timestamp: now,
            count,
        });
        // The message twin: every cycle's count as a self-describing log record.
        let _ = output.events.emit(&TickEvent { count });
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
