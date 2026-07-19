//! A two-entry pack compiled as a shared library and driven over the C ABI.
//!
//! This crate builds as a `cdylib` whose only export surface is the
//! [`export_pack!`](metor_fsw_2::export_pack) invocation at the bottom. A
//! host process `dlopen`s the produced library, reads the pack manifest from
//! `fsw_pack_describe`, constructs an entry through `fsw_pack_create` from
//! postcard-encoded params, and steps it cycle by cycle.
//!
//! The pack exports two cyclic entries so hosts can exercise multi-entry
//! selection:
//!
//! * `DlCounter` reads a `tick_in` frame each cycle and republishes
//!   `start + round(value * scale)` as a `tick_out` frame, plus a
//!   [`TickEvent`] message. That covers every kind of data crossing the
//!   library boundary: a frame input, a frame output, a message output, and
//!   the params blob.
//! * `DlEcho` is paramless and republishes each `tick_in` value verbatim on
//!   its own `echo_out` frame, giving the pack a second entry with a
//!   distinct descriptor.
//!
//! The host declares byte-identical twins of the frame types, and the layout
//! check in the wiring step verifies that the two sides agree.

use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, MsgOut, NamedMsg, Out, Output, Pack, System, SystemInput,
    SystemOutput,
};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Input frame the host writes each cycle.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
pub struct TickIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

/// Output frame [`DlCounter`] publishes each cycle.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
pub struct TickOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub count: u64,
}

/// Message emitted once per cycle with the published count.
///
/// Unlike the frames, this crosses the boundary as postcard bytes tagged with
/// an id hashed from the schema name, so a peer holding the same definition
/// can decode it from the id alone.
#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
pub struct TickEvent {
    pub count: u64,
}

impl NamedMsg for TickEvent {
    const NAME: &'static str = "TickEvent";
}

/// Construction parameters, decoded from postcard bytes in `fsw_pack_create`.
///
/// The mix of an integer and a float keeps parameter round-trip checks honest
/// beyond a single-value encoding.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq, metor_fsw_2::ParamsDocs)]
pub struct CounterParams {
    pub start: u64,
    pub scale: f64,
}

/// Republishes each input tick as `start + round(value * scale)`.
///
/// Deliberately has no KDL support; the host constructs it only through
/// [`BuildSystem`], proving the two construction paths are independent.
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
    events: MsgOut<TickEvent>,
}

impl System for DlCounter {
    type Input = DlCounterIn;
    type Output = Out<DlCounterOut>;
    const NAME: &'static str = "dl_counter";
}

impl CyclicSystem for DlCounter {
    fn execute(&mut self, now: Timestamp, input: &mut DlCounterIn, output: &mut Out<DlCounterOut>) {
        let value = match input.tick.latest() {
            Ok(Some(t)) => t.get().value,
            Ok(None) => {
                output.health().error("no_tick");
                return;
            }
            Err(_) => {
                output.health().error("tick_corrupt");
                return;
            }
        };
        let count = self.start + (value as f64 * self.scale).round() as u64;
        // Exercises the pack-side tracing pipeline: the export shim installed
        // a per-dylib subscriber, so this lands on the instance's log port.
        tracing::info!(count, "tick counted");
        let _ = output.out.write(&TickOut {
            timestamp: now,
            count,
        });
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

/// Output frame [`DlEcho`] publishes each cycle, named apart from `tick_out`
/// so the pack's two entries have distinct output components.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "echo_out")]
pub struct EchoOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

/// The pack's second entry: paramless, republishing each tick verbatim.
pub struct DlEcho;

#[derive(SystemInput)]
pub struct DlEchoIn {
    tick: Input<TickIn>,
}

#[derive(SystemOutput)]
pub struct DlEchoOut {
    out: Output<EchoOut>,
}

impl System for DlEcho {
    type Input = DlEchoIn;
    type Output = Out<DlEchoOut>;
    const NAME: &'static str = "dl_echo";
}

impl CyclicSystem for DlEcho {
    fn execute(&mut self, now: Timestamp, input: &mut DlEchoIn, output: &mut Out<DlEchoOut>) {
        if let Ok(Some(t)) = input.tick.latest() {
            let _ = output.out.write(&EchoOut {
                timestamp: now,
                value: t.get().value,
            });
        }
    }
}

impl BuildSystem for DlEcho {
    type Params = ();
    fn new(_params: ()) -> Self {
        DlEcho
    }
}

/// The crate's pack, referenced by `export_pack!` below.
pub fn pack() -> Pack {
    Pack::new()
        .system_type::<DlCounter, _>("DlCounter")
        .system_type::<DlEcho, _>("DlEcho")
}

// Exports the `fsw_*` C symbols the host resolves after `dlopen`.
metor_fsw_2::export_pack!(pack);
