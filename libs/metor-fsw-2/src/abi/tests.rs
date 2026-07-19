//! In-process tests for the exported C ABI.
//!
//! The `run_pack_*` helpers behind `export_pack!` live in this crate, so the
//! tests call them directly instead of loading a shared library. A plain
//! function call still exercises everything a loaded artifact would use, from
//! the pack manifest through `RawBinder`, `attach_raw`, the entry drivers,
//! and manifest serialization.
//!
//! The tests also play the host's role. They allocate heap-backed
//! `RingBuffer`s, hand the entry `(base, len, role)` handles, and read
//! results back through their own writers and views over the same regions the
//! entry attaches to without owning.

use core::ffi::c_void;
use core::ptr;
use std::slice;

use metor_fsw_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto::vtable::VTable;
use postcard_schema::Schema;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::abi::{
    FswRing, FswStatus, PackManifest, ROLE_INPUT, ROLE_OUTPUT, run_pack_bind_init, run_pack_close,
    run_pack_create, run_pack_describe, run_pack_destroy, run_pack_execute, run_pack_open,
    run_pack_shutdown,
};
use crate::sequence::{Outcome, SequenceStatus, SlotControlIn, wait};
use crate::{
    BuildSystem, CyclicSystem, Frame, Input, Out, Output, Pack, System, SystemHealth, SystemInput,
    SystemKind, SystemOutput, buffer_capacity,
};

// ---------------------------------------------------------------------------
// Fixture frames and a small cyclic system exported through the ABI.
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
struct TickIn {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    value: u64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
struct TickOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    count: u64,
}

/// Configuration for [`Counter`], carrying the full postcard contract
/// (`Serialize + Deserialize + Schema`).
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
struct CounterParams {
    #[serde(default)]
    start: u64,
}

/// A minimal [`CyclicSystem`] that adds `start` to each input tick and
/// republishes the sum.
struct Counter {
    start: u64,
}

#[derive(SystemInput)]
struct CounterIn {
    tick: Input<TickIn>,
}

#[derive(SystemOutput)]
struct CounterOut {
    out: Output<TickOut>,
}

impl System for Counter {
    type Input = CounterIn;
    type Output = Out<CounterOut>;
    const NAME: &'static str = "counter";
}

impl CyclicSystem for Counter {
    fn execute(&mut self, now: Timestamp, input: &mut CounterIn, output: &mut Out<CounterOut>) {
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

impl BuildSystem for Counter {
    type Params = CounterParams;
    fn new(params: Self::Params) -> Self {
        Counter {
            start: params.start,
        }
    }
}

/// Params with a non-trivial `Default`, so [`Trim`]'s `#[system]` expansion
/// declares the entry defaults blob the pack manifest reports.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
struct TrimParams {
    gain: f64,
}

impl Default for TrimParams {
    fn default() -> Self {
        Self { gain: 2.5 }
    }
}

/// A `#[system]`-authored entry, so the manifest tests cover the macro's
/// automatic `params_default_blob` path next to [`Counter`]'s hand-written
/// `BuildSystem` (which declares no defaults).
struct Trim {
    gain: f64,
}

#[crate::system(name = "trim")]
impl Trim {
    fn new(p: TrimParams) -> Self {
        Self { gain: p.gain }
    }

    fn execute(&mut self, now: Timestamp, out: &mut Output<TickOut>) {
        let _ = out.write(&TickOut {
            timestamp: now,
            count: self.gain as u64,
        });
    }
}

/// The test pack: the struct systems (hand-written and `#[system]`-authored),
/// the panicking system, and a sequence entry — the entry shapes the ABI
/// drives. `run_pack_open` takes this fn exactly as `export_pack!` would
/// reference it.
fn test_pack() -> Pack {
    Pack::new()
        .system_type::<Counter>("counter")
        .system_type::<Boom>("boom")
        .task("wait_seq", wait_seq)
        .system_type::<Trim>("trim")
}

/// Entry indices in [`test_pack`]'s manifest order.
const COUNTER: u32 = 0;
const BOOM: u32 = 1;
const WAIT_SEQ: u32 = 2;
const TRIM: u32 = 3;

/// A sequence body that waits about two sim-microseconds and completes.
async fn wait_seq() -> Outcome {
    wait(std::time::Duration::from_micros(2)).await;
    Outcome::Completed
}

/// An opened test pack that closes itself on drop, so every test tears down
/// even on assertion failure.
struct OpenPack(*mut c_void);

impl OpenPack {
    fn new() -> Self {
        let pack = run_pack_open(test_pack);
        assert!(!pack.is_null(), "fsw_pack_open returned a pack");
        Self(pack)
    }
}

impl Drop for OpenPack {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `run_pack_open` and every state was
        // destroyed by the test body before this drop.
        unsafe { run_pack_close(self.0) };
    }
}

// ---------------------------------------------------------------------------
// Host-emulation helpers.
// ---------------------------------------------------------------------------

fn ring_for<F: Frame>(depth: usize, readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
    })
}

/// A byte ring for the implicit `LogEvent` message tail.
fn log_ring_for(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: crate::capacity_for(crate::MAX_MSG_BYTES, 16),
        max_readers: readers,
    })
}

/// Builds the boundary handle for a host-owned ring's region.
fn handle(ring: &RingBuffer, role: u8) -> FswRing {
    let (base, len) = ring.region();
    FswRing { base, len, role }
}

// ---------------------------------------------------------------------------
// Lifecycle end-to-end: create, bind, write, execute, read back.
// ---------------------------------------------------------------------------

#[test]
fn abi_lifecycle_end_to_end() {
    // The host owns the rings. Output descriptor order is the user `out`
    // followed by the implicit health and log ports.
    let in_ring = ring_for::<TickIn>(8, 1);
    let out_ring = ring_for::<TickOut>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = log_ring_for(1);

    // Register the host's view before the system writes; a fresh view only
    // sees data committed after it exists.
    let mut out_view = Input::<TickOut>::new(out_ring.view(NoWake).unwrap());

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // The params blob is canonical postcard, the same bytes any front-end
    // emits.
    let params = postcard::to_allocvec(&CounterParams { start: 100 }).unwrap();

    let pack = OpenPack::new();
    // SAFETY: live pack; the params range is readable.
    let state = unsafe { run_pack_create(pack.0, COUNTER, 0, params.as_ptr(), params.len()) };
    assert!(!state.is_null(), "fsw_pack_create returned state");
    // SAFETY: live state; the handle regions outlive the driver.
    unsafe {
        run_pack_bind_init(
            state,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_ptr(),
            outputs.len(),
            b"inst".as_ptr(),
            4,
        )
    };

    // The host writes one record through its own writer over `in_ring`; the
    // system reads it through its non-owning view over the same region.
    let mut in_writer = Output::<TickIn>::new(in_ring.writer(NoWake).unwrap());
    in_writer
        .write(&TickIn {
            timestamp: Timestamp(7),
            value: 5,
        })
        .unwrap();

    // One cycle, with `now` as the host's raw timestamp tick.
    // SAFETY: live, bound state.
    let status = unsafe { run_pack_execute(state, 1000) };
    assert_eq!(status, FswStatus::Running);

    // The grant borrows the view, so read inside a scope that ends before the
    // final drop.
    {
        let out = out_view
            .latest()
            .expect("ring readable")
            .expect("system produced an output");
        assert_eq!(out.get().count, 105, "start(100) + value(5)");
        assert_eq!(out.get().timestamp, Timestamp(1000), "stamped with `now`");
    }

    // Tear the instance down before the pack closes and the rings drop.
    // SAFETY: live state, destroyed exactly once, before pack close.
    unsafe {
        run_pack_shutdown(state);
        run_pack_destroy(state);
    }

    drop(pack);
    drop((in_ring, out_ring, health_ring, log_ring, out_view));
}

/// An out-of-range entry index is a null state, never a crash; so is a
/// second create of the same reloadable entry succeeding (two instances).
#[test]
fn pack_create_bounds_and_reuse() {
    let pack = OpenPack::new();
    // SAFETY: live pack; empty params.
    let oob = unsafe { run_pack_create(pack.0, 99, 0, ptr::null(), 0) };
    assert!(oob.is_null(), "an out-of-range index returns null");

    let params = postcard::to_allocvec(&CounterParams { start: 1 }).unwrap();
    // SAFETY: live pack; readable params ranges.
    let a = unsafe { run_pack_create(pack.0, COUNTER, 0, params.as_ptr(), params.len()) };
    let b = unsafe { run_pack_create(pack.0, COUNTER, 0, params.as_ptr(), params.len()) };
    assert!(!a.is_null() && !b.is_null(), "one entry, two instances");
    // SAFETY: live states, destroyed exactly once each, before pack close.
    unsafe {
        run_pack_destroy(a);
        run_pack_destroy(b);
    }
}

// ---------------------------------------------------------------------------
// Manifest round-trip through fsw_pack_describe and postcard.
// ---------------------------------------------------------------------------

extern "C" fn collect_bytes(ctx: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: the test passes a `&mut Vec<u8>` as `ctx`, and `buf`/`len` is
    // the descriptor buffer the export owns for the duration of the call.
    let sink = unsafe { &mut *(ctx as *mut Vec<u8>) };
    let bytes = unsafe { slice::from_raw_parts(buf, len) };
    sink.extend_from_slice(bytes);
}

#[test]
fn abi_describe_round_trips() {
    let pack = OpenPack::new();
    let mut buf: Vec<u8> = Vec::new();
    // SAFETY: live pack; the sink/ctx pair matches.
    let rc = unsafe {
        run_pack_describe(
            pack.0,
            collect_bytes,
            &mut buf as *mut Vec<u8> as *mut c_void,
        )
    };
    assert_eq!(rc, 0, "fsw_pack_describe succeeded");

    let manifest: PackManifest = postcard::from_bytes(&buf).expect("manifest decodes");
    assert_eq!(manifest.systems.len(), 4, "one entry per pack registration");
    assert!(manifest.systems.iter().all(|s| s.reloadable));

    // Only the `#[system]`-authored entry with `Params: Default` declares an
    // entry defaults blob; a hand-written `BuildSystem` (Counter, despite its
    // params deriving `Default`) and unit-params entries declare none.
    let defaults = |i: u32| manifest.systems[i as usize].params_default.as_deref();
    assert_eq!(defaults(COUNTER), None);
    assert_eq!(defaults(BOOM), None);
    assert_eq!(defaults(WAIT_SEQ), None);
    assert_eq!(
        defaults(TRIM),
        Some(
            postcard::to_allocvec(&TrimParams::default())
                .unwrap()
                .as_slice()
        ),
        "the macro's defaults probe rides the manifest"
    );

    let entry = &manifest.systems[COUNTER as usize];
    let got = &entry.descriptor;
    let desc = {
        let mut d = <Counter as CyclicSystem>::descriptor();
        d.name = "counter".into();
        d
    };

    assert_eq!(got.name, "counter");
    assert_eq!(got.kind, SystemKind::Cyclic);

    // Ports and frame ids match the static descriptor.
    assert_eq!(got.inputs.len(), desc.inputs.len());
    assert_eq!(got.inputs.len(), 1);
    assert_eq!(
        got.inputs[0].id().component().expect("table port"),
        TickIn::FRAME_ID
    );
    assert_eq!(got.inputs[0].id(), desc.inputs[0].id());

    // The user `out` plus the implicit health and log ports.
    assert_eq!(got.outputs.len(), desc.outputs.len());
    assert_eq!(got.outputs.len(), 3);
    assert_eq!(
        got.outputs[0].id().component().expect("table port"),
        TickOut::FRAME_ID
    );
    for (m, d) in got.outputs.iter().zip(&desc.outputs) {
        assert_eq!(m.id(), d.id());
        assert_eq!(m.name, d.name);
        // The axes and the telemetry flag ride the wire verbatim.
        assert_eq!(m.delivery, d.delivery);
        assert_eq!(m.fan_in, d.fan_in);
        assert_eq!(m.telemetered, d.telemetered);
    }

    // A static system declares no host capabilities.
    assert!(got.capabilities.is_empty());

    assert_eq!(
        entry.params_schema,
        OwnedNamedType::from(<CounterParams as Schema>::SCHEMA),
        "Params schema round-trips"
    );
}

// ---------------------------------------------------------------------------
// Telemetry-prefix rewrite on a reconstructed descriptor's outputs.
// ---------------------------------------------------------------------------

/// Every component id a vtable realizes in registration mode, so a test can
/// assert exactly which ids the schema bakes in.
fn realized_ids(vt: &VTable) -> Vec<ComponentId> {
    vt.realize_fields(None)
        .flatten()
        .map(|f| f.component_id)
        .collect()
}

#[test]
fn dl_announce_prefixes_vtable_ids() {
    // Round-trip the descriptor through postcard, exactly as a host loading
    // it from a manifest would.
    let desc = <Counter as CyclicSystem>::descriptor();
    let bytes = postcard::to_allocvec(&desc).expect("descriptor encodes");
    let rebuilt: crate::SystemDescriptor = postcard::from_bytes(&bytes).expect("decodes");

    // The user output `out` (frame `tick_out`, field `count`) is outputs[0].
    let port = &rebuilt.outputs[0];
    let (vtable, metadata) = port.announce("inst").expect("table port");

    assert!(
        metadata
            .iter()
            .any(|m| m.component_id == ComponentId::new("inst.tick_out.count")),
        "metadata id is instance-prefixed"
    );

    // The vtable's baked leaf id is prefixed too, matching what a static
    // system bakes via `announce_of::<F>("inst")`, and the unprefixed id is
    // gone.
    let ids = realized_ids(&vtable);
    assert!(
        ids.contains(&ComponentId::new("inst.tick_out.count")),
        "vtable leaf id is instance-prefixed: {ids:?}"
    );
    assert!(
        !ids.contains(&ComponentId::new("tick_out.count")),
        "no unprefixed leaf id remains: {ids:?}"
    );

    // The vtable carried on the port itself stays unprefixed; `compatible()`
    // validates wiring against the frame-relative ids.
    let unprefixed = realized_ids(port.vtable().expect("table port"));
    assert!(
        unprefixed.contains(&ComponentId::new("tick_out.count")),
        "PortDesc.vtable stays unprefixed for compatibility: {unprefixed:?}"
    );
}

// ---------------------------------------------------------------------------
// Panic containment: a panicking execute returns Panicked, never unwinds.
// ---------------------------------------------------------------------------

struct Boom;

#[derive(SystemInput)]
struct BoomIn {
    // Bound by the ABI but never drained; Boom panics first.
    #[allow(dead_code)]
    tick: Input<TickIn>,
}

#[derive(SystemOutput)]
struct BoomOut {
    // Bound by the ABI but never written; kept so the bundle has a real
    // output port for the descriptor and bind walk.
    #[allow(dead_code)]
    out: Output<TickOut>,
}

impl System for Boom {
    type Input = BoomIn;
    type Output = Out<BoomOut>;
    const NAME: &'static str = "boom";
}

impl CyclicSystem for Boom {
    fn execute(&mut self, _now: Timestamp, _input: &mut BoomIn, _output: &mut Out<BoomOut>) {
        panic!("boom: execute panicked across the ABI");
    }
}

impl BuildSystem for Boom {
    type Params = ();
    fn new(_params: ()) -> Self {
        Boom
    }
}

/// An `extern "C"` frame over `run_pack_execute`, the exact shape
/// `export_pack!` generates, so the panic crosses a real C ABI frame.
extern "C" fn boom_execute(state: *mut c_void, now: u64) -> FswStatus {
    // SAFETY: `state` is a live `PackAbiState` from `run_pack_create`.
    unsafe { run_pack_execute(state, now) }
}

#[test]
fn abi_panic_is_contained() {
    let in_ring = ring_for::<TickIn>(8, 1);
    let out_ring = ring_for::<TickOut>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = log_ring_for(1);

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    let pack = OpenPack::new();
    // `()` params encode to zero bytes.
    // SAFETY: live pack; null params with len 0 is the documented empty case.
    let state = unsafe { run_pack_create(pack.0, BOOM, 0, ptr::null(), 0) };
    assert!(!state.is_null());
    // SAFETY: live state, and the handle regions outlive the driver.
    unsafe {
        run_pack_bind_init(
            state,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_ptr(),
            outputs.len(),
            b"inst".as_ptr(),
            4,
        )
    };

    // The panic is caught at the boundary and reported as a status.
    let status = boom_execute(state, 1);
    assert_eq!(status, FswStatus::Panicked, "panic converted, not unwound");

    // A later cycle is short-circuited because the slot is poisoned.
    assert_eq!(boom_execute(state, 2), FswStatus::Panicked);

    // SAFETY: live state, used once more then destroyed before pack close.
    unsafe {
        run_pack_shutdown(state);
        run_pack_destroy(state);
    }

    drop(pack);
    drop((in_ring, out_ring, health_ring, log_ring));
}

/// [`FswStatus::from_raw`] sits on the trust boundary. The three declared
/// discriminants round-trip; any other word folds to `Panicked` rather than
/// being treated as a valid `repr(u32)` value.
#[test]
fn from_raw_folds_out_of_range_to_panicked() {
    assert_eq!(FswStatus::from_raw(0), FswStatus::Running);
    assert_eq!(FswStatus::from_raw(1), FswStatus::Panicked);
    assert_eq!(FswStatus::from_raw(2), FswStatus::Done);
    // A well-behaved export never sends these, but a stale build, a
    // hand-rolled exporter, or memory corruption could hand back any word.
    assert_eq!(FswStatus::from_raw(3), FswStatus::Panicked);
    assert_eq!(FswStatus::from_raw(255), FswStatus::Panicked);
    assert_eq!(FswStatus::from_raw(u32::MAX), FswStatus::Panicked);
}

// ---------------------------------------------------------------------------
// A task entry driven through the pack ABI under the slot-occupant mount:
// the framework appends the SlotControlIn input after the entry's inputs
// (none here) and the SequenceStatus output after its health/log tail.
// ---------------------------------------------------------------------------

#[test]
fn seq_abi_runs_to_done() {
    // Rings in occupant-mount order: input [control]; outputs [health, log,
    // status].
    let control_ring = ring_for::<SlotControlIn>(8, 1);
    let status_ring = ring_for::<SequenceStatus>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = log_ring_for(1);

    // Register the host's view before the occupant writes.
    let mut status_view = Input::<SequenceStatus>::new(status_ring.view(NoWake).unwrap());

    let inputs = [handle(&control_ring, ROLE_INPUT)];
    let outputs = [
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
        handle(&status_ring, ROLE_OUTPUT),
    ];

    let pack = OpenPack::new();
    // `()` params encode to zero bytes; mount word 1 = slot occupant.
    // SAFETY: live pack; null params with len 0 is the documented empty case.
    let state = unsafe { run_pack_create(pack.0, WAIT_SEQ, 1, std::ptr::null(), 0) };
    assert!(!state.is_null(), "fsw_pack_create returned state");
    // SAFETY: live state, and the handle regions outlive the future.
    unsafe {
        run_pack_bind_init(
            state,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_ptr(),
            outputs.len(),
            b"inst".as_ptr(),
            4,
        )
    };

    // At t=0 the future suspends on `wait`, so the cycle reports Running.
    // SAFETY: live, bound state.
    assert_eq!(unsafe { run_pack_execute(state, 0) }, FswStatus::Running);
    // At t=2 the deadline has elapsed and the future completes.
    // SAFETY: live, bound state.
    assert_eq!(unsafe { run_pack_execute(state, 2) }, FswStatus::Done);
    // The terminal is latched; a re-poll re-serves it without touching the
    // finished future.
    assert_eq!(unsafe { run_pack_execute(state, 3) }, FswStatus::Done);

    // A terminal SequenceStatus was published on the tail.
    {
        let rec = status_view
            .latest()
            .expect("status ring readable")
            .expect("a SequenceStatus record was written");
        assert_eq!(rec.run_state, Outcome::Completed.run_state());
        assert_eq!(rec.timestamp, Timestamp(2));
    }

    // SAFETY: live state, torn down once before the pack closes and the
    // host rings drop.
    unsafe {
        run_pack_shutdown(state);
        run_pack_destroy(state);
    }
    drop(pack);
    drop((
        control_ring,
        status_ring,
        health_ring,
        log_ring,
        status_view,
    ));
}

// ---------------------------------------------------------------------------
// Announce equivalence: the data path (carried unprefixed vtable + metadata,
// re-prefixed on the host) realizes identically to the static announce path.
// This is the invariant the merged descriptor family rests on. The contract is
// realized identity — same component ids, types, shapes, frames, and dynamic
// paths — not byte identity; every consumer realizes the vtable rather than
// comparing its serialized bytes.
// ---------------------------------------------------------------------------

/// The static announce of `F` under `prefix`: what a statically linked system
/// bakes into its output vtable and metadata.
fn static_announce<F: Frame>(prefix: &str) -> (VTable, Vec<metor_proto_wkt::ComponentMetadata>) {
    crate::descriptor::announce_of::<F>(prefix)
}

/// The data announce of `F` under `prefix`: the port carries the unprefixed
/// vtable and metadata (round-tripped through postcard, exactly as a loaded
/// pack's port crosses the boundary), and the prefixed form is re-derived
/// with no static frame type.
fn data_announce<F: Frame>(prefix: &str) -> (VTable, Vec<metor_proto_wkt::ComponentMetadata>) {
    let desc = crate::PortDesc::of::<F>();
    let bytes = postcard::to_allocvec(&desc).expect("port desc encodes");
    let rd: crate::PortDesc = postcard::from_bytes(&bytes).expect("port desc decodes");
    rd.announce(prefix).expect("a Table port announces")
}

/// A frame with a nested dynamic member (`FrameMap`) beside its scalars, so
/// the announce carries both plain leaf ids and a dynamic-member template.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "probe_dyn")]
struct ProbeDyn {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    scalar: f64,
    counts: crate::dynamic::FrameMap<u64, 4>,
}

/// A named-axes 3-vector field, standing in for the nox spatial types: its
/// metadata carries `element_names`, which the announce path must not drop.
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone, Copy)]
#[repr(transparent)]
struct Axes([f64; 3]);

impl crate::AsVTable for Axes {
    fn vtable_fields(
        path: impl crate::path::ComponentPath,
    ) -> impl Iterator<Item = metor_proto::vtable::builder::FieldBuilder> {
        use metor_proto::types::PrimType;
        use metor_proto::vtable::builder::{component, raw_field, schema};
        core::iter::once(raw_field(
            0,
            (3 * size_of::<f64>()) as u32,
            schema(PrimType::F64, &[3], component(path.to_component_id())),
        ))
    }
}

impl crate::Metadatatize for Axes {
    fn metadata(
        prefix: impl crate::path::ComponentPath,
    ) -> impl Iterator<Item = metor_proto_wkt::ComponentMetadata> {
        core::iter::once(prefix.to_metadata().with_element_names(["x", "y", "z"]))
    }
}

impl metor_proto::com_de::AsComponentView for Axes {
    fn as_component_view(&self) -> metor_proto::types::ComponentView<'_> {
        self.0.as_component_view()
    }
}

impl metor_proto::com_de::FromComponentView for Axes {
    fn from_component_view(
        view: metor_proto::types::ComponentView<'_>,
    ) -> Result<Self, metor_proto::error::Error> {
        <[f64; 3]>::from_component_view(view).map(Axes)
    }
}

/// A frame with an element-named vector field beside a scalar.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "probe_axes")]
struct ProbeAxes {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega_b: Axes,
}

/// Everything registration-mode realization surfaces about one field: the
/// component identity, its schema, its frame tag, and whether it came through
/// a dynamic terminal.
#[derive(Debug, PartialEq)]
struct RealizedShape {
    component_id: ComponentId,
    ty: metor_proto::types::PrimType,
    shape: Vec<usize>,
    frame: Option<ComponentId>,
    dynamic: bool,
}

fn realized_shapes(vt: &VTable) -> Vec<RealizedShape> {
    vt.realize_fields(None)
        .map(|f| {
            let f = f.expect("announce vtable realizes");
            RealizedShape {
                component_id: f.component_id,
                ty: f.ty,
                shape: f.shape.to_vec(),
                frame: f.frame,
                dynamic: f.dynamic,
            }
        })
        .collect()
}

/// The resolved name strings of every dynamic terminal (`Op::List`/`Op::Map`),
/// in op order — the strings the prefix rewrite must carry the instance name
/// onto for top-level dynamics and leave element-relative for nested ones.
fn dynamic_terminal_names(vt: &VTable) -> Vec<String> {
    use metor_proto::vtable::Op;
    vt.ops
        .iter()
        .filter_map(|op| {
            let name = match op {
                Op::List { name, .. } | Op::Map { name, .. } => *name,
                _ => return None,
            };
            let Ok(Op::Data { offset, len }) = vt.get_op(name) else {
                return None;
            };
            let bytes = &vt.data.as_slice()[offset.to_index()..offset.to_index() + *len as usize];
            Some(String::from_utf8(bytes.to_vec()).expect("dynamic name is UTF-8"))
        })
        .collect()
}

/// Assert the data announce realizes identically to the static announce of
/// `F`: the full realized field set, the metadata vector, and the resolved
/// dynamic path strings.
fn assert_announce_eq<F: Frame>(prefix: &str) {
    let (svt, smeta) = static_announce::<F>(prefix);
    let (dvt, dmeta) = data_announce::<F>(prefix);
    assert_eq!(
        realized_shapes(&svt),
        realized_shapes(&dvt),
        "realized field sets match for prefix {prefix:?}"
    );
    assert_eq!(smeta, dmeta, "metadata matches for prefix {prefix:?}");
    assert_eq!(
        dynamic_terminal_names(&svt),
        dynamic_terminal_names(&dvt),
        "dynamic paths match for prefix {prefix:?}"
    );
}

/// For a frame of plain scalar fields, the metadata-driven leaf-id rewrite the
/// data path performs realizes identically to the static announce.
#[test]
fn announce_data_path_matches_static_scalar() {
    for prefix in ["inst", "a.b"] {
        assert_announce_eq::<TickIn>(prefix);
        assert_announce_eq::<TickOut>(prefix);
        assert_announce_eq::<ProbeAxes>(prefix);
    }
}

/// The announce metadata keeps each component's metadata map — element names,
/// enum variants — under the instance prefix, exactly as the static
/// `metadata(prefix)` path emits it.
#[test]
fn announce_preserves_element_names() {
    let d = crate::PortDesc::of::<ProbeAxes>();
    let (_, meta) = d.announce("inst").expect("table port announces");
    let m = meta
        .iter()
        .find(|m| m.name == "inst.probe_axes.omega_b")
        .expect("the axes component is announced");
    assert_eq!(m.element_names(), "x,y,z");
}

/// Frames with dynamic members (`FrameList`/`FrameMap`, including every
/// system's implicit `health` frame) realize identically too: the
/// prefix rewrite carries the instance prefix onto the top-level dynamic path
/// strings, so `inst.health.error_counts.<kind>` ids match the static path and
/// two instances of one system never collide on their dynamic paths.
#[test]
fn announce_data_path_matches_static_dynamic() {
    for prefix in ["inst", "a.b"] {
        assert_announce_eq::<ProbeDyn>(prefix);
        assert_announce_eq::<crate::SystemHealth>(prefix);
    }
}

// ---------------------------------------------------------------------------
// PortDesc: both schema arms round-trip through postcard with their axes.
// ---------------------------------------------------------------------------

#[test]
fn port_desc_round_trips_both_arms() {
    use metor_proto::types::Msg;
    use metor_proto_wkt::SequenceCommand;

    use crate::{Delivery, FanIn, PortDesc, PortId, PortSchema};

    // Postcard arm, with a non-default telemetry flag so the override rides
    // the wire too.
    let d = PortDesc::msg::<SequenceCommand>().untelemetered();
    let bytes = postcard::to_allocvec(&d).expect("encodes");
    let rd: PortDesc = postcard::from_bytes(&bytes).expect("decodes");
    assert_eq!(rd.id(), PortId::Packet(SequenceCommand::ID));
    assert_eq!(rd.name, "SequenceCommand");
    assert_eq!(rd.max_size, d.max_size);
    assert!(matches!(rd.schema, PortSchema::Postcard { .. }));
    assert!(rd.vtable().is_none(), "no vtable on a Postcard port");
    assert_eq!(rd.delivery, Delivery::Log);
    assert_eq!(rd.fan_in, FanIn::Many);
    assert!(!rd.telemetered, "the opt-out survived the wire");
    // The reconstructed desc satisfies an edge against the static twin.
    assert!(crate::descriptor::compatible(
        &rd,
        &PortDesc::msg::<SequenceCommand>()
    ));

    // Table arm, axes at their frame defaults.
    let d = PortDesc::of::<TickOut>();
    let bytes = postcard::to_allocvec(&d).expect("encodes");
    let rd: PortDesc = postcard::from_bytes(&bytes).expect("decodes");
    assert_eq!(rd.id(), PortId::Component(TickOut::FRAME_ID));
    assert_eq!(rd.name, "tick_out");
    assert_eq!(rd.delivery, Delivery::Snapshot);
    assert_eq!(rd.fan_in, FanIn::One);
    assert!(rd.telemetered);
    // Compatibility runs over the carried unprefixed vtable in both
    // directions.
    assert!(crate::descriptor::compatible(
        &PortDesc::of::<TickOut>(),
        &rd
    ));
    assert!(crate::descriptor::compatible(
        &rd,
        &PortDesc::of::<TickOut>()
    ));
}
