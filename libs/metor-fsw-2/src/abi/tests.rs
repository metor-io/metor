//! In-process tests for the exported C ABI.
//!
//! The `fsw_*` symbols that `export_system!` generates live in this crate, so
//! the tests call them directly instead of loading a shared library. A plain
//! function call still exercises everything a loaded artifact would use, from
//! the macro expansion through `RawBinder`, `attach_raw`, the `CyclicRunner`,
//! and descriptor serialization.
//!
//! The tests also play the host's role. They allocate heap-backed
//! `RingBuffer`s, hand the system `(base, len, role)` handles, and read
//! results back through their own writers and views over the same regions the
//! system attaches to without owning.

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
    FswRing, FswStatus, PortDescMsg, PortSchemaMsg, ROLE_INPUT, ROLE_OUTPUT, RawBinder,
    SystemDescriptorMsg, run_bind_init, run_create, run_destroy, run_execute, run_seq_bind_init,
    run_seq_create, run_seq_destroy, run_seq_execute, run_seq_shutdown, run_shutdown,
};
use crate::binder::BindPorts;
use crate::sequence::{
    Outcome, SeqBound, SeqClock, SeqStatusOut, SeqSystem, SequenceStatus, SlotControlIn, wait,
};
use crate::wiring::LoadError;
use crate::{
    BuildSystem, CyclicSystem, Frame, Input, Out, Output, System, SystemHealth, SystemInput,
    SystemKind, SystemLog, SystemOutput, buffer_capacity,
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

/// Params carrying the full postcard contract (`Serialize + Deserialize +
/// Schema`).
#[derive(Serialize, Deserialize, Schema, Clone, Default, Debug, PartialEq)]
struct CounterParams {
    #[serde(default)]
    start: u64,
}

/// Adds `start` to each input tick and republishes the sum.
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
            Some(t) => t.get().value,
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

// The `no_mangle` symbols are crate-unique, so this crate can export exactly
// one system. `Counter` takes the slot; `Boom` (below) drives the `run_*`
// helpers directly to avoid a second, clashing export.
crate::export_system!(Counter);

// ---------------------------------------------------------------------------
// Host-emulation helpers.
// ---------------------------------------------------------------------------

fn ring_for<F: Frame>(depth: usize, readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
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
    let log_ring = ring_for::<SystemLog>(8, 1);

    // Register the host's view before the system writes; a fresh view only
    // sees data committed after it exists.
    let mut out_view = Input::<TickOut>::new(out_ring.view(NoWake, NoWake).unwrap());

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // The params blob is canonical postcard, the same bytes any front-end
    // emits.
    let params = postcard::to_allocvec(&CounterParams { start: 100 }).unwrap();

    let state = fsw_create(params.as_ptr(), params.len());
    assert!(!state.is_null(), "fsw_create returned state");
    fsw_bind_init(
        state,
        inputs.as_ptr(),
        inputs.len(),
        outputs.as_ptr(),
        outputs.len(),
    );

    // The host writes one record through its own writer over `in_ring`; the
    // system reads it through its non-owning view over the same region.
    let mut in_writer = Output::<TickIn>::new(in_ring.writer(NoWake, NoWake).unwrap());
    in_writer
        .write(&TickIn {
            timestamp: Timestamp(7),
            value: 5,
        })
        .unwrap();

    // One cycle, with `now` as the host's raw timestamp tick.
    let status = fsw_execute(state, 1000);
    assert_eq!(status, FswStatus::Running);

    // The grant borrows the view, so read inside a scope that ends before the
    // final drop.
    {
        let out = out_view.latest().expect("system produced an output");
        assert_eq!(out.get().count, 105, "start(100) + value(5)");
        assert_eq!(out.get().timestamp, Timestamp(1000), "stamped with `now`");
    }

    // Tear the system down before the host rings drop.
    fsw_shutdown(state);
    fsw_destroy(state);

    drop((in_ring, out_ring, health_ring, log_ring, out_view));
}

// ---------------------------------------------------------------------------
// Descriptor round-trip through fsw_describe and postcard.
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
    let mut buf: Vec<u8> = Vec::new();
    let rc = fsw_describe(collect_bytes, &mut buf as *mut Vec<u8> as *mut c_void);
    assert_eq!(rc, 0, "fsw_describe succeeded");

    let msg: SystemDescriptorMsg = postcard::from_bytes(&buf).expect("descriptor decodes");
    let desc = <Counter as CyclicSystem>::descriptor();

    assert_eq!(msg.name, "counter");
    assert_eq!(msg.kind, SystemKind::Cyclic);

    // Ports and frame ids match the static descriptor.
    assert_eq!(msg.inputs.len(), desc.inputs.len());
    assert_eq!(msg.inputs.len(), 1);
    let table_frame_id = |m: &PortDescMsg| match &m.schema {
        PortSchemaMsg::Table { frame_id, .. } => *frame_id,
        PortSchemaMsg::Postcard { .. } => panic!("expected a Table port"),
    };
    assert_eq!(table_frame_id(&msg.inputs[0]), TickIn::FRAME_ID);
    assert_eq!(
        table_frame_id(&msg.inputs[0]),
        desc.inputs[0].id.component().expect("table port")
    );

    // The user `out` plus the implicit health and log ports.
    assert_eq!(msg.outputs.len(), desc.outputs.len());
    assert_eq!(msg.outputs.len(), 3);
    assert_eq!(table_frame_id(&msg.outputs[0]), TickOut::FRAME_ID);
    for (m, d) in msg.outputs.iter().zip(&desc.outputs) {
        assert_eq!(table_frame_id(m), d.id.component().expect("table port"));
        assert_eq!(m.name, d.name);
        // The axes and the telemetry flag ride the wire verbatim.
        assert_eq!(m.delivery, d.delivery);
        assert_eq!(m.fan_in, d.fan_in);
        assert_eq!(m.telemetered, d.telemetered);
    }

    // A static system declares no host capabilities.
    assert!(msg.capabilities.is_empty());

    assert_eq!(
        msg.params_schema,
        OwnedNamedType::from(<CounterParams as Schema>::SCHEMA),
        "Params schema round-trips"
    );

    // The message reconstructs a usable descriptor on the host side.
    let rebuilt = msg.into_descriptor();
    assert_eq!(rebuilt.name, "counter");
    assert_eq!(
        rebuilt.inputs[0].id.component().expect("table port"),
        TickIn::FRAME_ID
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
    // Lower to the wire and reconstruct, exactly as a host loading the
    // descriptor would.
    let desc = <Counter as CyclicSystem>::descriptor();
    let schema = OwnedNamedType::from(<CounterParams as Schema>::SCHEMA);
    let msg = SystemDescriptorMsg::lower(&desc, schema);
    let rebuilt = msg.into_descriptor();

    // The user output `out` (frame `tick_out`, field `count`) is outputs[0].
    let port = &rebuilt.outputs[0];
    let (vtable, metadata) = (port.announce().expect("table port"))("inst");

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

/// An `extern "C"` frame over `run_execute`, the exact shape `export_system!`
/// generates, so the panic crosses a real C ABI frame.
extern "C" fn boom_execute(state: *mut c_void, now: u64) -> FswStatus {
    // SAFETY: `state` is a live `AbiState<Boom>` from `run_create`.
    unsafe { run_execute::<Boom>(state, now) }
}

#[test]
fn abi_panic_is_contained() {
    let in_ring = ring_for::<TickIn>(8, 1);
    let out_ring = ring_for::<TickOut>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

    let inputs = [handle(&in_ring, ROLE_INPUT)];
    let outputs = [
        handle(&out_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // `()` params encode to zero bytes.
    // SAFETY: null params with len 0 is the documented empty-params case.
    let state = unsafe { run_create::<Boom>(ptr::null(), 0) };
    assert!(!state.is_null());
    // SAFETY: live state, and the handle regions outlive the runner.
    unsafe {
        run_bind_init::<Boom, _>(
            state,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_ptr(),
            outputs.len(),
        )
    };

    // The panic is caught at the boundary and reported as a status.
    let status = boom_execute(state, 1);
    assert_eq!(status, FswStatus::Panicked, "panic converted, not unwound");

    // A later cycle is short-circuited because the slot is poisoned.
    assert_eq!(boom_execute(state, 2), FswStatus::Panicked);

    // SAFETY: live state, used once more then destroyed.
    unsafe {
        run_shutdown::<Boom>(state);
        run_destroy::<Boom>(state);
    }

    drop((in_ring, out_ring, health_ring, log_ring));
}

/// `FswStatus::from_raw` sits on the trust boundary. The three declared
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
// Schema-guided KDL params encoding.
// The invariant under test is byte equality: `encode_kdl_params`, driven only
// by an owned schema, must produce exactly the postcard bytes the typed Rust
// value encodes to, so a host can encode config without linking the system's
// `Params` type. The remaining tests cover the span-aware error cases.
// ---------------------------------------------------------------------------

use crate::wiring::encode_kdl_params;

/// `encode_kdl_params` with the `system`-node surface: `type=` and
/// `artifact=` reserved, one leading instance-name argument.
fn encode_system_params(
    node_text: &str,
    schema: &OwnedNamedType,
    system: &str,
) -> Result<Vec<u8>, LoadError> {
    encode_kdl_params(node_text, schema, system, &["type", "artifact"], 1)
}

/// Params spanning several field types, so byte equality is checked across
/// more than one postcard encoding shape.
#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
struct MultiParams {
    start: u64,
    scale: f64,
    enabled: bool,
    label: String,
    bias: i64,
    trim: Option<f64>,
}

/// The schema-guided encoder produces byte-identical postcard to the typed
/// value's own encoding, across every field type.
#[test]
fn kdl_schema_encode_byte_equals_typed_builder() {
    let schema = OwnedNamedType::from(<MultiParams as Schema>::SCHEMA);
    let node_text = r#"system "x" type="T" artifact="a" start=7 scale=2.5 enabled=#true label="hi" bias=-9 trim=0.25"#;

    let kdl_bytes =
        encode_system_params(node_text, &schema, "x").expect("schema-encode KDL params");
    let typed_bytes = postcard::to_allocvec(&MultiParams {
        start: 7,
        scale: 2.5,
        enabled: true,
        label: "hi".into(),
        bias: -9,
        trim: Some(0.25),
    })
    .unwrap();

    assert_eq!(
        kdl_bytes, typed_bytes,
        "KDL schema-encode == typed builder (the wire contract)"
    );

    // The blob decodes back to the typed value on the receiving side.
    let decoded: MultiParams = postcard::from_bytes(&kdl_bytes).unwrap();
    assert_eq!(decoded.start, 7);
    assert_eq!(decoded.trim, Some(0.25));
}

/// An `Option` field with no KDL property encodes as `None`, and an int
/// literal where a float is wanted coerces (`scale=3` becomes 3.0). Both
/// still byte-match.
#[test]
fn kdl_schema_encode_optional_absent_and_int_for_float() {
    let schema = OwnedNamedType::from(<MultiParams as Schema>::SCHEMA);
    let node_text =
        r#"system "x" type="T" artifact="a" start=0 scale=3 enabled=#false label="" bias=0"#;

    let kdl_bytes =
        encode_system_params(node_text, &schema, "x").expect("encode with absent Option");
    let typed_bytes = postcard::to_allocvec(&MultiParams {
        start: 0,
        scale: 3.0,
        enabled: false,
        label: String::new(),
        bias: 0,
        trim: None,
    })
    .unwrap();
    assert_eq!(
        kdl_bytes, typed_bytes,
        "absent Option + int-for-float still byte-match"
    );
}

/// A property whose value type does not match the schema field is a spanned
/// `InvalidParam`, not a panic or a silent divergence.
#[test]
fn kdl_schema_encode_type_mismatch_is_clean_error() {
    let schema = OwnedNamedType::from(<MultiParams as Schema>::SCHEMA);
    // `start` wants a u64 but is given a string.
    let node_text =
        r#"system "x" type="T" artifact="a" start="oops" scale=1.0 enabled=#true label="h" bias=0"#;
    let err = encode_system_params(node_text, &schema, "x").expect_err("type mismatch is an error");
    match err {
        LoadError::InvalidParam {
            system,
            property,
            span,
            ..
        } => {
            assert_eq!(system, "x");
            assert_eq!(property, "start");
            // The span points at the offending `start="oops"` entry.
            let at = node_text.find("start=").unwrap();
            assert!(span.offset() >= at, "entry-precise span, got {span:?}");
        }
        other => panic!("expected InvalidParam, got {other:?}"),
    }
}

/// A required (non-`Option`) schema field with no property is a
/// `MissingParam`.
#[test]
fn kdl_schema_encode_missing_field_is_clean_error() {
    let schema = OwnedNamedType::from(<MultiParams as Schema>::SCHEMA);
    // `bias` (a required i64) is absent.
    let node_text = r#"system "x" type="T" artifact="a" start=1 scale=1.0 enabled=#true label="h""#;
    let err = encode_system_params(node_text, &schema, "x").expect_err("missing field is an error");
    match err {
        LoadError::MissingParam {
            system, property, ..
        } => {
            assert_eq!(system, "x");
            assert_eq!(property, "bias");
        }
        other => panic!("expected MissingParam, got {other:?}"),
    }
}

/// A property not present in the schema is a spanned `UnknownParam`, guarding
/// against typos. The reserved `type=`/`artifact=` are never treated as
/// params.
#[test]
fn kdl_schema_encode_unknown_property_is_clean_error() {
    let schema = OwnedNamedType::from(<MultiParams as Schema>::SCHEMA);
    let node_text = r#"system "x" type="T" artifact="a" start=1 scale=1.0 enabled=#true label="h" bias=0 typo=5"#;
    let err =
        encode_system_params(node_text, &schema, "x").expect_err("extra property is an error");
    match err {
        LoadError::UnknownParam {
            system,
            property,
            span,
            ..
        } => {
            assert_eq!(system, "x");
            assert_eq!(property, "typo");
            let at = node_text.find("typo=5").unwrap();
            assert!(span.offset() >= at, "entry-precise span, got {span:?}");
        }
        other => panic!("expected UnknownParam, got {other:?}"),
    }
}

/// Nested params expressed as children: a nested struct, a `Vec` in both the
/// multi-arg and repeated-children spellings, and an absent `Option`, all
/// byte-equal to the typed encoding.
#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
struct NestedParams {
    pid: PidParams,
    taps: Vec<u64>,
    label: Option<String>,
}

#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
struct PidParams {
    p: f64,
    i: f64,
    d: f64,
}

#[test]
fn kdl_schema_encode_nested_struct_and_vec_byte_equal() {
    let schema = OwnedNamedType::from(<NestedParams as Schema>::SCHEMA);
    let typed_bytes = postcard::to_allocvec(&NestedParams {
        pid: PidParams {
            p: 1.0,
            i: 0.5,
            d: 0.1,
        },
        taps: vec![1, 2, 3],
        label: None,
    })
    .unwrap();

    // Multi-arg child form for the Vec.
    let node_text = r#"system "x" type="T" artifact="a" {
    pid p=1.0 i=0.5 d=0.1
    taps 1 2 3
}"#;
    let kdl_bytes = encode_system_params(node_text, &schema, "x").expect("nested encode");
    assert_eq!(
        kdl_bytes, typed_bytes,
        "nested struct + multi-arg Vec byte-match"
    );

    // The repeated-children form for the Vec produces the same wire.
    let node_text = r#"system "x" type="T" artifact="a" {
    pid p=1.0 i=0.5 d=0.1
    taps 1
    taps 2
    taps 3
}"#;
    let kdl_bytes =
        encode_system_params(node_text, &schema, "x").expect("repeated-children encode");
    assert_eq!(kdl_bytes, typed_bytes, "repeated-children Vec byte-match");

    // A wrong-typed nested leaf errors with the nested field's name.
    let node_text = r#"system "x" type="T" artifact="a" {
    pid p="oops" i=0.5 d=0.1
    taps 1 2 3
}"#;
    let err = encode_system_params(node_text, &schema, "x").expect_err("nested mismatch errors");
    assert!(
        matches!(err, LoadError::InvalidParam { ref property, .. } if property == "p"),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// A hand-written SeqSystem driven through the run_seq_* helpers. It has no
// user ports, just the implicit SlotControlIn input and the SequenceStatus
// plus health/log output tail.
// ---------------------------------------------------------------------------

use std::rc::Rc;
use std::time::Duration;

use crate::SystemDescriptor;

/// A sequence occupant whose future waits about two sim-microseconds and then
/// returns `Completed`.
struct WaitSeq;

impl SeqSystem for WaitSeq {
    type Params = ();

    fn descriptor() -> SystemDescriptor {
        // inputs = [SlotControlIn]; outputs = the Out<SeqStatusOut> tail
        // (SequenceStatus, health, log).
        let inputs = vec![<Input<SlotControlIn>>::descriptor()];
        let outputs = <Out<SeqStatusOut> as SystemOutput>::port_descs();
        SystemDescriptor {
            name: "wait_seq",
            kind: SystemKind::Cyclic,
            inputs,
            outputs,
            capabilities: Vec::new(),
        }
    }

    fn build(_params: (), binder: &mut RawBinder, clock: &Rc<SeqClock>) -> SeqBound {
        let control = <Input<SlotControlIn>>::bind(binder);
        let status = <Out<SeqStatusOut> as BindPorts>::bind(binder);
        let _ = clock;
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = Outcome>>> =
            Box::pin(async {
                // The deadline is 2us past `now` at the first poll; `wait`
                // resolves once `now` reaches it.
                wait(Duration::from_micros(2)).await;
                Outcome::Completed
            });
        SeqBound {
            future,
            status,
            control,
        }
    }
}

#[test]
fn seq_abi_runs_to_done() {
    // Rings in descriptor order: input [control]; outputs [status, health,
    // log].
    let control_ring = ring_for::<SlotControlIn>(8, 1);
    let status_ring = ring_for::<SequenceStatus>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

    // Register the host's view before the occupant writes.
    let mut status_view = Input::<SequenceStatus>::new(status_ring.view(NoWake, NoWake).unwrap());

    let inputs = [handle(&control_ring, ROLE_INPUT)];
    let outputs = [
        handle(&status_ring, ROLE_OUTPUT),
        handle(&health_ring, ROLE_OUTPUT),
        handle(&log_ring, ROLE_OUTPUT),
    ];

    // `()` params encode to zero bytes.
    // SAFETY: null params with len 0 is the documented empty-params case.
    let state = unsafe { run_seq_create::<WaitSeq>(std::ptr::null(), 0) };
    assert!(!state.is_null(), "run_seq_create returned state");
    // SAFETY: live state, and the handle regions outlive the future.
    unsafe {
        run_seq_bind_init::<WaitSeq>(
            state,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_ptr(),
            outputs.len(),
        )
    };

    // At t=0 the future suspends on `wait`, so the cycle reports Running.
    // SAFETY: live, bound state.
    assert_eq!(
        unsafe { run_seq_execute::<WaitSeq>(state, 0) },
        FswStatus::Running
    );
    // At t=2 the deadline has elapsed and the future completes.
    // SAFETY: live, bound state.
    assert_eq!(
        unsafe { run_seq_execute::<WaitSeq>(state, 2) },
        FswStatus::Done
    );

    // A terminal SequenceStatus was published on the tail.
    {
        let rec = status_view
            .latest()
            .expect("a SequenceStatus record was written");
        assert_eq!(rec.get().run_state, Outcome::Completed.run_state());
        assert_eq!(rec.get().timestamp, Timestamp(2));
    }

    // SAFETY: live state, torn down once before the host rings drop.
    unsafe {
        run_seq_shutdown::<WaitSeq>(state);
        run_seq_destroy::<WaitSeq>(state);
    }
    drop((
        control_ring,
        status_ring,
        health_ring,
        log_ring,
        status_view,
    ));
}

// ---------------------------------------------------------------------------
// PortDescMsg: both schema arms round-trip through postcard with their axes.
// ---------------------------------------------------------------------------

#[test]
fn port_desc_msg_round_trips_both_arms() {
    use metor_proto::types::Msg;
    use metor_proto_wkt::SequenceCommand;

    use crate::{Delivery, FanIn, PortDesc, PortId, PortSchema};

    // Postcard arm, with a non-default telemetry flag so the override rides
    // the wire too.
    let d = PortDesc::msg::<SequenceCommand>().untelemetered();
    let m = PortDescMsg::lower(&d);
    let bytes = postcard::to_allocvec(&m).expect("encodes");
    let back: PortDescMsg = postcard::from_bytes(&bytes).expect("decodes");
    let rd = back.into_port_desc();
    assert_eq!(rd.id, PortId::Packet(SequenceCommand::ID));
    assert_eq!(rd.name, "SequenceCommand");
    assert_eq!(rd.max_size, d.max_size);
    assert!(matches!(rd.schema, PortSchema::Postcard));
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
    let m = PortDescMsg::lower(&d);
    let bytes = postcard::to_allocvec(&m).expect("encodes");
    let back: PortDescMsg = postcard::from_bytes(&bytes).expect("decodes");
    let rd = back.into_port_desc();
    assert_eq!(rd.id, PortId::Component(TickOut::FRAME_ID));
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
