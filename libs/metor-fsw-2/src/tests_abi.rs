//! WP8 Wave 2 acceptance tests (dl-open.md §2/§3): the generated `export_system!`
//! C-ABI exercised **in-process, without dlopen/libloading** (W2b is the host
//! loader). A direct in-crate call of the generated `#[unsafe(no_mangle)]` `fsw_*`
//! functions proves the macro + `RawBinder` + `attach_raw` + the verbatim
//! `CyclicRunner<…, RawBacking>` + the descriptor serialization all compose.
//!
//! The host side is emulated by hand: it allocates `RingBuffer<BoxBacking>`s, hands
//! the system their `(base, len, role)` as `FswRing`s, and reads results back through
//! its own `BoxBacking` writer/view over the *same* regions the system reconstructs
//! over `RawBacking`.

use core::ffi::c_void;
use core::ptr;
use std::slice;

use metor_fsw_ring::{Backing, BoxBacking, Config, NoWake, Overrun, RingBuffer};
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto::vtable::VTable;
use postcard_schema::Schema;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::abi::{
    FswRing, FswStatus, ROLE_INPUT, ROLE_OUTPUT, SystemDescriptorMsg, run_bind_init, run_create,
    run_destroy, run_execute, run_shutdown,
};
use crate::wiring::{FromKdlNode, LoadError, RegisteredSystem};
use crate::{
    CyclicSystem, Frame, Input, Out, Output, System, SystemHealth, SystemInput, SystemKind,
    SystemLog, SystemOutput, buffer_capacity,
};

// ---------------------------------------------------------------------------
// Fixture frames + a tiny cyclic system, exported through the ABI.
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

/// A real `Params` struct: `Serialize + Deserialize + Schema` (the postcard params
/// contract) plus the WP6 `FromKdlNode` the `RegisteredSystem` construction needs.
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
struct CounterParams {
    start: u64,
}

impl FromKdlNode for CounterParams {
    fn from_kdl_node(node: &kdl::KdlNode, src: &str) -> Result<Self, LoadError> {
        let start = crate::wiring::kdl_optional::<u64>(node, "start", "counter", src)?.unwrap_or(0);
        Ok(Self { start })
    }
}

/// Adds `start` to each input tick and republishes the sum — enough to prove the
/// input view, the output writer, and the counters all ride the boundary.
struct Counter {
    start: u64,
}

#[derive(SystemInput)]
struct CounterIn<B: Backing = BoxBacking> {
    tick: Input<TickIn, B>,
}

#[derive(SystemOutput)]
struct CounterOut<B: Backing = BoxBacking> {
    out: Output<TickOut, B>,
}

impl<B: Backing> System<B> for Counter {
    type Input = CounterIn<B>;
    type Output = Out<CounterOut<B>, B>;
    const NAME: &'static str = "counter";
}

impl<B: Backing> CyclicSystem<B> for Counter {
    fn execute(&mut self, now: Timestamp, input: &mut CounterIn<B>, output: &mut Out<CounterOut<B>, B>) {
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

impl RegisteredSystem for Counter {
    type Params = CounterParams;
    fn new(params: Self::Params) -> Self {
        Counter { start: params.start }
    }
}

// The one `#[unsafe(no_mangle)] fsw_*` surface this test crate may carry (no_mangle
// symbols are crate-unique): the `Counter`. `Boom` (below) drives the `run_*` helpers
// directly to avoid a second, clashing export.
crate::export_system!(Counter);

// ---------------------------------------------------------------------------
// Host-emulation helpers.
// ---------------------------------------------------------------------------

fn overwrite_ring<F: Frame>(depth: usize, readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
        overrun: Overrun::Overwrite,
    })
}

/// An `FswRing` over a host ring's region (the same-process handle the host fills).
fn handle(ring: &RingBuffer<BoxBacking>, role: u8) -> FswRing {
    let (base, len) = ring.region();
    FswRing { base, len, role }
}

// ---------------------------------------------------------------------------
// 1+2. Lifecycle end-to-end: create → bind_init → write → execute → read back.
// ---------------------------------------------------------------------------

#[test]
fn abi_lifecycle_end_to_end() {
    // Host-side rings (the coordinator owns these). Descriptor order on the output
    // side is [user `out`, implicit health, implicit log].
    let in_ring = overwrite_ring::<TickIn>(8, 1);
    let out_ring = overwrite_ring::<TickOut>(8, 1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    // The host reads results through its own view; register it before the system
    // writes (a fresh view only sees data committed from now on).
    let mut out_view = Input::<TickOut>::new(out_ring.view(NoWake, NoWake).unwrap());

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // params blob — canonical postcard, exactly what a KDL/builder front-end would emit.
    let params = postcard::to_allocvec(&CounterParams { start: 100 }).unwrap();

    // fsw_create → fsw_bind_init.
    let state = fsw_create(params.as_ptr(), params.len());
    assert!(!state.is_null(), "fsw_create returned state");
    fsw_bind_init(state, inputs.as_ptr(), inputs.len(), outputs.as_ptr(), outputs.len());

    // The host writes one input record through a BoxBacking writer over `in_ring`,
    // which the system reads through its RawBacking view over the same region.
    let mut in_writer = Output::<TickIn>::new(in_ring.writer(NoWake, NoWake));
    in_writer
        .write(&TickIn {
            timestamp: Timestamp(7),
            value: 5,
        })
        .unwrap();

    // One cycle. `now` is the coordinator's raw timestamp tick.
    let status = fsw_execute(state, 1000);
    assert_eq!(status, FswStatus::Running);

    // Read the produced frame back through the host's view.
    let out = out_view.latest().unwrap().expect("system produced an output");
    assert_eq!(out.get().count, 105, "start(100) + value(5)");
    assert_eq!(out.get().timestamp, Timestamp(1000), "stamped with `now`");

    // Teardown: shutdown + destroy inside the `.so` *before* the host rings drop.
    fsw_shutdown(state);
    fsw_destroy(state);

    drop((in_ring, out_ring, health_ring, log_ring, out_view));
}

// ---------------------------------------------------------------------------
// 3. Descriptor round-trip: fsw_describe → postcard → SystemDescriptorMsg.
// ---------------------------------------------------------------------------

extern "C" fn collect_bytes(ctx: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: the test passes a `&mut Vec<u8>` as `ctx`, and `buf`/`len` is the
    // descriptor buffer the export owns for the call.
    let sink = unsafe { &mut *(ctx as *mut Vec<u8>) };
    let bytes = unsafe { slice::from_raw_parts(buf, len) };
    sink.extend_from_slice(bytes);
}

#[test]
fn abi_describe_round_trips() {
    let mut buf: Vec<u8> = Vec::new();
    let rc = fsw_describe(collect_bytes, &mut buf as *mut Vec<u8> as *mut c_void);
    assert_eq!(rc, 0, "fsw_describe succeeded");

    let msg: SystemDescriptorMsg = postcard::from_bytes(&buf).expect("descriptor decodes");
    let desc = <Counter as CyclicSystem>::descriptor();

    assert_eq!(msg.name, "counter");
    assert_eq!(msg.kind, SystemKind::Cyclic);

    // Ports / frame ids match the static descriptor.
    assert_eq!(msg.inputs.len(), desc.inputs.len());
    assert_eq!(msg.inputs.len(), 1);
    assert_eq!(msg.inputs[0].frame_id, TickIn::FRAME_ID);
    assert_eq!(msg.inputs[0].frame_id, desc.inputs[0].frame_id);

    // user `out` + implicit health + implicit log.
    assert_eq!(msg.outputs.len(), desc.outputs.len());
    assert_eq!(msg.outputs.len(), 3);
    assert_eq!(msg.outputs[0].frame_id, TickOut::FRAME_ID);
    for (m, d) in msg.outputs.iter().zip(&desc.outputs) {
        assert_eq!(m.frame_id, d.frame_id);
        assert_eq!(m.frame_name, d.frame_name);
    }

    // params_schema present and correct.
    assert_eq!(
        msg.params_schema,
        OwnedNamedType::from(<CounterParams as Schema>::SCHEMA),
        "Params schema round-trips"
    );

    // The descriptor mirror reconstructs a usable SystemDescriptor (host side).
    let rebuilt = msg.into_descriptor();
    assert_eq!(rebuilt.name, "counter");
    assert_eq!(rebuilt.inputs[0].frame_id, TickIn::FRAME_ID);
}

// ---------------------------------------------------------------------------
// 3b. Telemetry-prefix rewrite (dl-open.md §7, D): a dl output's `announce(prefix)`
//     yields instance-prefixed *vtable* ids, matching a static system's WP7 announce.
// ---------------------------------------------------------------------------

/// Every component id a vtable realizes (registration mode), for asserting which ids
/// are baked into the schema the telemetry tap downlinks.
fn realized_ids(vt: &VTable) -> Vec<ComponentId> {
    vt.realize_fields(None)
        .flatten()
        .map(|f| f.component_id)
        .collect()
}

#[test]
fn dl_announce_prefixes_vtable_ids() {
    // Lower → wire → reconstruct the descriptor exactly as the host loader does.
    let desc = <Counter as CyclicSystem>::descriptor();
    let in_meta: Vec<_> = desc.inputs.iter().map(|p| (p.announce)("").1).collect();
    let out_meta: Vec<_> = desc.outputs.iter().map(|p| (p.announce)("").1).collect();
    let schema = OwnedNamedType::from(<CounterParams as Schema>::SCHEMA);
    let msg = SystemDescriptorMsg::lower(&desc, schema, in_meta, out_meta);
    let rebuilt = msg.into_descriptor();

    // The user output `out` (frame `tick_out`, field `count`) is outputs[0].
    let port = &rebuilt.outputs[0];
    let (vtable, metadata) = (port.announce)("inst");

    // The metadata ids are prefixed (the always-worked half).
    assert!(
        metadata
            .iter()
            .any(|m| m.component_id == ComponentId::new("inst.tick_out.count")),
        "metadata id is instance-prefixed"
    );

    // The vtable's *baked* leaf id is now prefixed too (the deferred W2b rewrite): it
    // matches what a static system bakes via `announce_of::<F>("inst")`, and the
    // unprefixed id no longer appears.
    let ids = realized_ids(&vtable);
    assert!(
        ids.contains(&ComponentId::new("inst.tick_out.count")),
        "vtable leaf id is instance-prefixed: {ids:?}"
    );
    assert!(
        !ids.contains(&ComponentId::new("tick_out.count")),
        "no unprefixed leaf id remains: {ids:?}"
    );

    // The *unprefixed* vtable on the PortDesc (what `compatible()` validates) is left
    // alone, so wiring validation still sees the frame-relative ids.
    let unprefixed = realized_ids(&port.vtable);
    assert!(
        unprefixed.contains(&ComponentId::new("tick_out.count")),
        "PortDesc.vtable stays unprefixed for compatibility: {unprefixed:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Panic containment: a panicking `execute` returns Panicked, never unwinds.
// ---------------------------------------------------------------------------

struct Boom;

#[derive(SystemInput)]
struct BoomIn<B: Backing = BoxBacking> {
    tick: Input<TickIn, B>,
}

#[derive(SystemOutput)]
struct BoomOut<B: Backing = BoxBacking> {
    // Bound by the ABI but never written (Boom panics first); kept so the bundle has
    // a real output port for the descriptor/bind walk.
    #[allow(dead_code)]
    out: Output<TickOut, B>,
}

impl<B: Backing> System<B> for Boom {
    type Input = BoomIn<B>;
    type Output = Out<BoomOut<B>, B>;
    const NAME: &'static str = "boom";
}

impl<B: Backing> CyclicSystem<B> for Boom {
    fn execute(&mut self, _now: Timestamp, _input: &mut BoomIn<B>, _output: &mut Out<BoomOut<B>, B>) {
        panic!("boom: execute panicked across the ABI");
    }
}

impl RegisteredSystem for Boom {
    type Params = ();
    fn new(_params: ()) -> Self {
        Boom
    }
}

/// An `extern "C"` frame over `run_execute` — the exact shape `export_system!`
/// generates — so the test exercises the panic crossing a real C ABI frame.
extern "C" fn boom_execute(state: *mut c_void, now: u64) -> FswStatus {
    // SAFETY: `state` is a live `AbiState<Boom>` from `run_create`.
    unsafe { run_execute::<Boom>(state, now) }
}

#[test]
fn abi_panic_is_contained() {
    let in_ring = overwrite_ring::<TickIn>(8, 1);
    let out_ring = overwrite_ring::<TickOut>(8, 1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // `()` params encode to zero bytes; pass a null/empty blob.
    // SAFETY: null params with len 0 is the documented empty-params case.
    let state = unsafe { run_create::<Boom>(ptr::null(), 0) };
    assert!(!state.is_null());
    // SAFETY: live state + valid handle regions that outlive the runner.
    unsafe {
        run_bind_init::<Boom, _>(state, inputs.as_ptr(), inputs.len(), outputs.as_ptr(), outputs.len())
    };

    // execute panics inside the `.so`; the boundary converts it to `Panicked`.
    let status = boom_execute(state, 1);
    assert_eq!(status, FswStatus::Panicked, "panic converted, not unwound");

    // A subsequent cycle is short-circuited (the slot is poisoned).
    assert_eq!(boom_execute(state, 2), FswStatus::Panicked);

    // SAFETY: live state, used once more then destroyed.
    unsafe {
        run_shutdown::<Boom>(state);
        run_destroy::<Boom>(state);
    }

    drop((in_ring, out_ring, health_ring, log_ring));
}
