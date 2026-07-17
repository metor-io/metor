//! End-to-end tests for the wiring front-end. The fluent [`WiringBuilder`]
//! produces a `Wiring` that resolves into a running coordinator, value-tree
//! params reach each system's `new`, and bundles round-trip. The systems here
//! are tiny but real `CyclicSystem`/`AsyncSystem` impls registered in a
//! [`Registry`].

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_fsw_ring::Notifier;
use metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::wiring::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, IR_VERSION, InitialOccupantSpec,
    LoadErrorKind, ParamSource, Registry, SlotInitState, SlotSpec, SystemSpec, Wiring,
    WiringBuilder, resolve,
};
use crate::{
    AsyncSystem, BuildSystem, CyclicSystem, Input, MsgIn, MsgOut, Out, Output, System, SystemInput,
    SystemOutput,
};

// ---------------------------------------------------------------------------
// Frames under test.
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct Nav {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
}

/// Shares the frame name (and so the `frame_id`) with [`Imu`], but has a
/// strictly larger component set, so a consumer of this cannot accept an
/// [`Imu`] producer.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuBig {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    extra: f64,
}

#[derive(SystemInput)]
struct NoIn {}

#[derive(SystemOutput)]
struct NoOut {}

// ---------------------------------------------------------------------------
// ImuDriver: a params-bearing cyclic producer of an incrementing `Imu`.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, postcard_schema::Schema)]
struct ImuParams {
    #[allow(dead_code)]
    i2c_bus: i64,
    #[allow(dead_code)]
    sample_hz: f64,
}

struct ImuDriver {
    #[allow(dead_code)]
    params: ImuParams,
    n: f64,
}

#[derive(SystemOutput)]
struct ImuOut {
    imu: Output<Imu>,
}

impl System for ImuDriver {
    type Input = NoIn;
    type Output = Out<ImuOut>;
    const NAME: &'static str = "imu_driver";
}

impl CyclicSystem for ImuDriver {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        self.n += 1.0;
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: self.n,
        });
    }
}

impl BuildSystem for ImuDriver {
    type Params = ImuParams;
    fn new(params: Self::Params) -> Self {
        Self { params, n: 0.0 }
    }
}

// ---------------------------------------------------------------------------
// NavFilter: a params-bearing cyclic filter, `nav.angle = imu.omega * gain`.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct NavParams {
    gain: f64,
}

struct NavFilter {
    gain: f64,
}

#[derive(SystemInput)]
struct NavIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct NavOut {
    nav: Output<Nav>,
}

impl System for NavFilter {
    type Input = NavIn;
    type Output = Out<NavOut>;
    const NAME: &'static str = "nav_filter";
}

impl CyclicSystem for NavFilter {
    fn execute(&mut self, now: Timestamp, input: &mut NavIn, o: &mut Self::Output) {
        if let Ok(Some(imu)) = input.imu.latest() {
            let omega = imu.get().omega;
            let _ = o.nav.write(&Nav {
                timestamp: now,
                angle: omega * self.gain,
            });
        }
    }
}

impl BuildSystem for NavFilter {
    type Params = NavParams;
    fn new(params: Self::Params) -> Self {
        Self { gain: params.gain }
    }
}

// ---------------------------------------------------------------------------
// NavLogger: an async, paramless consumer of `nav` that records the latest
// angle to module statics for the end-to-end test to observe.
// ---------------------------------------------------------------------------

static LOG_COUNT: AtomicU64 = AtomicU64::new(0);
static LOG_LAST_ANGLE_BITS: AtomicU64 = AtomicU64::new(0);

struct NavLogger;

#[derive(SystemInput)]
struct LogIn {
    nav: Input<Nav, Notifier>,
}

#[derive(SystemOutput)]
struct LogNoOut {}

impl System for NavLogger {
    type Input = LogIn;
    type Output = Out<LogNoOut, Notifier>;
    const NAME: &'static str = "nav_logger";
}

impl AsyncSystem for NavLogger {
    async fn run(
        &mut self,
        context: &crate::AsyncContext,
        input: &mut Self::Input,
        _o: &mut Self::Output,
    ) {
        while let Some(Ok(nav)) = context.until_cancelled(input.nav.recv()).await {
            LOG_COUNT.fetch_add(1, Relaxed);
            LOG_LAST_ANGLE_BITS.store(nav.get().angle.to_bits(), Relaxed);
        }
    }
}

impl BuildSystem for NavLogger {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        NavLogger
    }
}

// ---------------------------------------------------------------------------
// PickyConsumer: requires the larger `ImuBig` under the same frame_id.
// ---------------------------------------------------------------------------

struct PickyConsumer;

#[derive(SystemInput)]
struct PickyIn {
    imu: Input<ImuBig>,
}

impl System for PickyConsumer {
    type Input = PickyIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "picky";
}

impl CyclicSystem for PickyConsumer {
    fn execute(&mut self, _now: Timestamp, input: &mut PickyIn, _o: &mut Self::Output) {
        let _ = input.imu.latest();
    }
}

impl BuildSystem for PickyConsumer {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        PickyConsumer
    }
}

// ---------------------------------------------------------------------------
// Closer: consumes `nav`, produces `imu`. Paired with NavFilter it forms a
// two-system feedback loop for the `delayed=` edge tests.
// ---------------------------------------------------------------------------

struct Closer;

#[derive(SystemInput)]
struct CloserIn {
    nav: Input<Nav>,
}

#[derive(SystemOutput)]
struct CloserOut {
    imu: Output<Imu>,
}

impl System for Closer {
    type Input = CloserIn;
    type Output = Out<CloserOut>;
    const NAME: &'static str = "closer";
}

impl CyclicSystem for Closer {
    fn execute(&mut self, now: Timestamp, input: &mut CloserIn, o: &mut Self::Output) {
        let omega = match input.nav.latest() {
            Ok(Some(nav)) => nav.get().angle,
            _ => 0.0,
        };
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega,
        });
    }
}

impl BuildSystem for Closer {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        Closer
    }
}

// ---------------------------------------------------------------------------
// Src and Sink: two paramless cyclic systems used by the builder equivalence
// and IR-version tests.
// ---------------------------------------------------------------------------

struct Src;

#[derive(SystemOutput)]
struct SrcOut {
    imu: Output<Imu>,
}

impl System for Src {
    type Input = NoIn;
    type Output = Out<SrcOut>;
    const NAME: &'static str = "src";
}

impl CyclicSystem for Src {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: 1.0,
        });
    }
}

impl BuildSystem for Src {
    type Params = ();
    fn new(_params: ()) -> Self {
        Src
    }
}

struct Sink;

#[derive(SystemInput)]
struct SinkIn {
    imu: Input<Imu>,
}

impl System for Sink {
    type Input = SinkIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "sink";
}

impl CyclicSystem for Sink {
    fn execute(&mut self, _now: Timestamp, input: &mut SinkIn, _o: &mut Self::Output) {
        let _ = input.imu.latest();
    }
}

impl BuildSystem for Sink {
    type Params = ();
    fn new(_params: ()) -> Self {
        Sink
    }
}

/// The registry every test resolves against, seeded with the built-ins as a
/// real app registry would be.
fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    crate::register_system!(&mut r, ImuDriver => "ImuDriver");
    r.register::<NavFilter, _>("NavFilter");
    r.register::<NavLogger, _>("NavLogger");
    r.register::<PickyConsumer, _>("PickyConsumer");
    r.register::<Closer, _>("Closer");
    r.register::<Src, _>("Src");
    r.register::<Sink, _>("Sink");
    r.register::<MsgSrc, _>("MsgSrc");
    r.register::<MsgSink, _>("MsgSink");
    r.register::<ValueProbe, _>("ValueProbe");
    r
}

// ---------------------------------------------------------------------------
// A params struct with scalars, a string, an optional, and a
// `#[serde(default)]`, shared by the value-tree decode test below.
// ---------------------------------------------------------------------------

fn default_depth() -> i64 {
    4
}

#[derive(serde::Deserialize, Debug, PartialEq)]
struct RoundTrip {
    count: i64,
    rate: f64,
    label: String,
    offset: Option<f64>,
    #[serde(default = "default_depth")]
    depth: i64,
}

// ---------------------------------------------------------------------------
// A shared builder graph for the resolve, manifest, and IR-version tests: two
// paramless static systems joined by one frame edge.
// ---------------------------------------------------------------------------

/// The graph expressed in Rust, used by the tests below.
fn equiv_builder() -> WiringBuilder {
    WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("a")
        .ty("Src")
        .end()
        .system("b")
        .ty("Sink")
        .end()
        .connect("a", "imu", "b", "imu")
}

#[cfg(not(miri))]
#[stellarator::test]
async fn builder_wiring_resolves_and_runs() {
    // The builder-origin Wiring carries no source text at all, yet resolves
    // through the one shared resolver and runs without any system stopping.
    let wiring = equiv_builder().build();
    let mut coord = resolve(&wiring, &registry()).expect("resolve the builder Wiring");
    coord.run_for(5).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

#[cfg(not(miri))]
#[stellarator::test]
async fn resolve_broadcasts_the_wiring_manifest() {
    // resolve serializes the path-stripped IR onto the coordinator's `wiring`
    // channel; the boot record decodes back to exactly that Wiring, the
    // contract the panel graph tile consumes off recorded telemetry.
    use metor_proto::types::Msg as _;
    use metor_proto_wkt::WiringManifest;

    let wiring = equiv_builder().build();
    let mut coord = resolve(&wiring, &registry()).expect("resolve the builder Wiring");
    let mut view = coord
        .registry()
        .get(ComponentId::new("coordinator.wiring"))
        .expect("the wiring channel is registered")
        .view()
        .expect("reader slot");

    coord.run_for(2).await;

    let mut buf = Vec::new();
    assert!(
        view.try_read_into(&mut buf).expect("readable ring"),
        "boot manifest present"
    );
    let (id, payload) = crate::message::split_record(&buf).expect("id-prefixed record");
    assert_eq!(id, WiringManifest::ID);
    let manifest: WiringManifest = postcard::from_bytes(payload).expect("decode manifest");
    assert_eq!(manifest.ir_version, wiring.ir_version);
    let round_tripped: Wiring =
        serde_json::from_str(&manifest.ir_json).expect("the payload is the JSON IR");
    assert_eq!(
        round_tripped,
        wiring.path_stripped(),
        "the manifest carries the resolved, path-stripped Wiring"
    );
}

// ---------------------------------------------------------------------------
// Slots: the builder expresses a slot (name, inputs, outputs, allow list,
// initial) onto the same SlotSpec the resolver consumes.
// ---------------------------------------------------------------------------

#[test]
fn slot_builder_mirrors_kdl() {
    // The Rust front-end expresses a slot with inputs, outputs, an allow list,
    // and an initial occupant.
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .artifact("commissioning", "adcs-seqs", "adcs_commissioning")
        .slot("adcs")
        .input("sensors")
        .output("mode")
        .allow("commissioning")
        .initial("commissioning", SlotInitState::Running)
        .end()
        .build();
    assert_eq!(wiring.slots.len(), 1);
    let slot = &wiring.slots[0];
    assert_eq!(slot.name, "adcs");
    assert_eq!(slot.inputs, vec!["sensors".to_string()]);
    assert_eq!(slot.outputs, vec!["mode".to_string()]);
    assert_eq!(slot.allow.len(), 1);
    assert_eq!(slot.allow[0].occupant, "commissioning");
    assert_eq!(slot.initial.as_ref().unwrap().state, SlotInitState::Running);
}

#[test]
fn err_static_system_with_typed_params() {
    // Typed builder (postcard) params on a static system have no decode path
    // (a static system's factory takes a value tree, not postcard bytes), so
    // resolve rejects them instead of silently dropping onto defaults. The
    // builder makes this unrepresentable (postcard `params` live on the
    // artifact path), so the invalid `Wiring` is built literally to drive the
    // serde-path `validate` gate.
    #[derive(serde::Serialize)]
    struct Gain {
        gain: f64,
    }
    let wiring = Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
        },
        artifacts: Vec::new(),
        systems: vec![SystemSpec {
            name: "nav".into(),
            ty: Some("NavFilter".into()),
            artifact: None,
            params: ParamSource::Postcard(postcard::to_allocvec(&Gain { gain: 2.0 }).unwrap()),
            process: false,
            src: None,
            scope: None,
        }],
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
    };
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a StaticPostcardParams error"),
        Err(e) => e,
    };
    assert!(
        matches!(err.kind, LoadErrorKind::StaticPostcardParams { .. }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Message edges through the builder: `WiringBuilder::connect_msg`.
// ---------------------------------------------------------------------------

static WIRE_SINK_COUNT: AtomicU64 = AtomicU64::new(0);
static WIRE_SINK_LAST: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct WireEvent {
    seq: u64,
}

// A wired message port resolves by this explicit, stable name token.
impl crate::NamedMsg for WireEvent {
    const NAME: &'static str = "WireEvent";
}

// A cyclic message producer (no params) emitting one `WireEvent` per cycle.
struct MsgSrc {
    n: u64,
}

#[derive(SystemOutput)]
struct MsgSrcOut {
    events: MsgOut<WireEvent>,
}

impl System for MsgSrc {
    type Input = NoIn;
    type Output = Out<MsgSrcOut>;
    const NAME: &'static str = "msg_src";
}

impl CyclicSystem for MsgSrc {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        self.n += 1;
        let _ = o.events.emit(&WireEvent { seq: self.n });
    }
}

impl BuildSystem for MsgSrc {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        Self { n: 0 }
    }
}

// A cyclic message consumer (no params) recording each drained `WireEvent`.
struct MsgSink;

#[derive(SystemInput)]
struct MsgSinkIn {
    events: MsgIn<WireEvent>,
}

impl System for MsgSink {
    type Input = MsgSinkIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "msg_sink";
}

impl CyclicSystem for MsgSink {
    fn execute(&mut self, _now: Timestamp, input: &mut MsgSinkIn, _o: &mut Self::Output) {
        input
            .events
            .drain(|e| {
                WIRE_SINK_COUNT.fetch_add(1, Relaxed);
                WIRE_SINK_LAST.store(e.seq, Relaxed);
            })
            .unwrap();
    }
}

impl BuildSystem for MsgSink {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        MsgSink
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_edge_builder_matches_kdl() {
    WIRE_SINK_COUNT.store(0, Relaxed);

    // The fluent builder's connect_msg produces an EdgeKind::Msg edge that
    // resolves, builds, and runs, delivering the messages.
    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Wall)
        .system("src")
        .ty("MsgSrc")
        .end()
        .system("sink")
        .ty("MsgSink")
        .end()
        .connect_msg("src", "sink", "WireEvent")
        .build();
    assert_eq!(wiring.edges[0].kind, crate::wiring::EdgeKind::Msg);

    let mut coord = resolve(&wiring, &registry()).expect("resolve succeeds");
    coord.run_for(3).await;
    assert!(WIRE_SINK_COUNT.load(Relaxed) >= 3);
}

/// A serialized document from before the field existed deserializes with
/// `process=false` (the serde default), so stored wirings stay readable.
#[test]
fn slot_spec_process_serde_defaults_off() {
    let json = r#"{
        "name": "adcs",
        "inputs": [],
        "outputs": [],
        "allow": [],
        "initial": null
    }"#;
    let spec: super::SlotSpec = serde_json::from_str(json).expect("old document deserializes");
    assert!(!spec.process, "an omitted `process` field defaults off");
}

// ---------------------------------------------------------------------------
// ParamSource::Value: the value-tree params surface. Static systems serde-
// deserialize it (field defaults honored, unknown keys rejected); pack
// entries take it through the same EntryParams seam, defaults overlay
// included.
// ---------------------------------------------------------------------------

static VALUE_PROBE_BITS: AtomicU64 = AtomicU64::new(0);

/// A params struct with a required and a serde-defaulted field, to observe
/// that both semantics survive the `Value` path end to end.
#[derive(serde::Deserialize)]
struct ValueProbeParams {
    gain: f64,
    #[serde(default)]
    bias: f64,
}

struct ValueProbe {
    gain: f64,
    bias: f64,
}

impl System for ValueProbe {
    type Input = NoIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "value_probe";
}

impl CyclicSystem for ValueProbe {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, _o: &mut Self::Output) {
        VALUE_PROBE_BITS.store((self.gain + self.bias).to_bits(), Relaxed);
    }
}

impl BuildSystem for ValueProbe {
    type Params = ValueProbeParams;
    fn new(params: Self::Params) -> Self {
        Self {
            gain: params.gain,
            bias: params.bias,
        }
    }
}

/// A builder-origin `Value` params tree reaches a static system's `new`,
/// with the omitted `#[serde(default)]` field on its default.
#[cfg(not(miri))]
#[stellarator::test]
async fn static_system_resolves_and_runs_from_value_params() {
    VALUE_PROBE_BITS.store(0, Relaxed);
    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Wall)
        .system("probe")
        .ty("ValueProbe")
        .params_value(serde_json::json!({ "gain": 2.25 }))
        .end()
        .build();
    let mut coord = resolve(&wiring, &registry()).expect("value params resolve");
    coord.run_for(2).await;
    assert_eq!(
        f64::from_bits(VALUE_PROBE_BITS.load(Relaxed)),
        2.25,
        "gain reached new(); bias took its serde default"
    );
}

/// An unknown key in a `Value` tree is an `UnknownParam` (via serde_ignored),
/// matching the typo guard, not a silent skip.
#[test]
fn err_value_params_unknown_key() {
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("nav")
        .ty("NavFilter")
        .params_value(serde_json::json!({ "gain": 0.8, "gian": 0.5 }))
        .end()
        .build();
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected an UnknownParam error"),
        Err(e) => e,
    };
    assert!(
        matches!(
            err.kind,
            LoadErrorKind::UnknownParam { ref property, ref system, .. }
                if property == "gian" && system == "nav"
        ),
        "{err:?}"
    );
}

/// A missing required field on the `Value` path is a `ValueParams` decode
/// error carrying serde's own message.
#[test]
fn err_value_params_missing_required() {
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("nav")
        .ty("NavFilter")
        .params_value(serde_json::json!({}))
        .end()
        .build();
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a ValueParams error"),
        Err(e) => e,
    };
    match err.kind {
        LoadErrorKind::ValueParams { system, reason, .. } => {
            assert_eq!(system, "nav");
            assert!(reason.contains("gain"), "names the missing field: {reason}");
        }
        other => panic!("expected ValueParams, got {other:?}"),
    }
}

/// An integer outside the field's width fails the static serde path loudly.
/// (The dl path range-checks in `conform_value`; this pins the parity.)
#[test]
fn err_value_params_out_of_range_int() {
    // ImuParams.i2c_bus is i64; u64::MAX cannot narrow into it.
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("imu")
        .ty("ImuDriver")
        .params_value(serde_json::json!({ "i2c_bus": u64::MAX, "sample_hz": 200.0 }))
        .end()
        .build();
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a ValueParams error"),
        Err(e) => e,
    };
    assert!(
        matches!(err.kind, LoadErrorKind::ValueParams { ref system, .. } if system == "imu"),
        "{err:?}"
    );
}

/// The `Value` decode is plain serde: `#[serde(default = ...)]` fills an
/// absent field and an absent `Option` reads as `None`.
#[test]
fn value_params_honor_serde_field_defaults() {
    let value = serde_json::json!({ "count": 3, "rate": 1.5, "label": "hi" });
    let got: RoundTrip = super::decode_value_params(&value, "params", "snippet").unwrap();
    assert_eq!(
        got,
        RoundTrip {
            count: 3,
            rate: 1.5,
            label: "hi".to_string(),
            offset: None,
            depth: 4,
        }
    );
}

/// A pack entry with declared defaults takes `Value` overrides through the
/// same top-level overlay as config: the spelled key replaces the default,
/// everything else stays.
#[cfg(not(miri))]
#[stellarator::test]
async fn pack_entry_defaults_overlay_value_overrides() {
    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
    struct TrimParams {
        gain: f64,
        bias: f64,
    }

    static LAST: AtomicU64 = AtomicU64::new(0);
    struct St {
        gain: f64,
        bias: f64,
    }
    fn emit(s: &mut St, now: Timestamp, out: &mut crate::Output<Imu>) {
        LAST.store((s.gain + s.bias).to_bits(), Relaxed);
        out.publish(&Imu {
            timestamp: now,
            omega: s.gain + s.bias,
        });
    }

    let mut r = Registry::new();
    r.register_pack(
        crate::Pack::new().system(
            "TrimV",
            crate::system(emit)
                .init(|p: TrimParams| St {
                    gain: p.gain,
                    bias: p.bias,
                })
                .defaults(TrimParams {
                    gain: 10.0,
                    bias: 0.5,
                }),
        ),
    );

    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Wall)
        .system("trim")
        .ty("TrimV")
        .params_value(serde_json::json!({ "gain": 2.0 }))
        .end()
        .build();
    let mut coord = resolve(&wiring, &r).expect("value overrides overlay the defaults");
    coord.run_for(2).await;
    assert_eq!(
        f64::from_bits(LAST.load(Relaxed)),
        2.5,
        "override (2.0) + defaulted bias (0.5)"
    );
}

/// The TCP built-ins' specs carry their `addr` as a `Value` tree, and the
/// variant survives a serde round-trip of the spec.
#[test]
fn builtin_link_specs_carry_value_params() {
    let spec =
        crate::wiring::SystemSpec::tcp_downlink("telemetry", "127.0.0.1:2240".parse().unwrap());
    match &spec.params {
        ParamSource::Value(v) => {
            assert_eq!(v, &serde_json::json!({ "addr": "127.0.0.1:2240" }))
        }
        other => panic!("expected ParamSource::Value, got {other:?}"),
    }
    let json = serde_json::to_string(&spec).expect("serialize");
    let back: crate::wiring::SystemSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, spec, "Value params round-trip");
}

/// Both built-ins resolve and run from their `Value` params: the
/// `SocketAddr` reads from the JSON string and the subset/msgs fields fall
/// back to their serde defaults.
#[cfg(not(miri))]
#[stellarator::test]
async fn builder_telemetry_and_uplink_resolve_from_value_params() {
    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Simulated { dt_secs: 0.001 })
        .system("imu")
        .ty("ImuDriver")
        .params_value(serde_json::json!({ "i2c_bus": 1, "sample_hz": 200.0 }))
        .end()
        .telemetry("127.0.0.1:59422".parse().unwrap())
        .uplink("127.0.0.1:59423".parse().unwrap())
        .build();
    let mut coord = resolve(&wiring, &registry()).expect("builtins resolve from Value params");
    coord.run_for(5).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// The versioned IR: the builder stamps IR_VERSION, serde carries it as a
// required field, and resolve refuses any other value.
// ---------------------------------------------------------------------------

/// A serialized `Wiring` with no `ir_version` fails to deserialize; an
/// unversioned document must never pass as current.
#[test]
fn wiring_json_without_ir_version_is_rejected() {
    let mut json = serde_json::to_value(equiv_builder().build()).expect("serialize");
    json.as_object_mut().unwrap().remove("ir_version");
    assert!(
        serde_json::from_value::<Wiring>(json).is_err(),
        "a missing version is an error, not a default"
    );
}

#[test]
fn resolve_rejects_ir_version_mismatch() {
    let mut wiring = equiv_builder().build();
    wiring.ir_version += 1;
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected an IrVersionMismatch error"),
        Err(e) => e,
    };
    match err.kind {
        LoadErrorKind::IrVersionMismatch { found, expected } => {
            assert_eq!(found, IR_VERSION + 1);
            assert_eq!(expected, IR_VERSION);
        }
        other => panic!("expected IrVersionMismatch, got {other:?}"),
    }
}

#[test]
fn resolve_rejects_invalid_simulated_steps_without_panicking() {
    for dt_secs in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
        let wiring = WiringBuilder::new()
            .coordinator(100.0, ClockSpec::Simulated { dt_secs })
            .build();
        let err = match resolve(&wiring, &Registry::new()) {
            Ok(_) => panic!("invalid dt must fail: {dt_secs:?}"),
            Err(err) => err,
        };
        assert!(
            matches!(err.kind, LoadErrorKind::InvalidSimulatedStep { .. }),
            "{dt_secs:?}: {err:?}"
        );
    }
}

/// A bundle whose artifact records a `manifest_hash` verifies it against the
/// copied sidecar at load: a matching bundle loads, a tampered sidecar fails
/// with [`BundleError::ManifestHashMismatch`] before any dlopen. Built over the
/// dl fixture pack so a real sidecar exists.
#[cfg(not(miri))]
#[test]
fn bundle_manifest_hash_checked_at_load() {
    use crate::wiring::{BundleError, PackageOptions, load_bundle, write_bundle};

    let _guard = crate::dl::FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wiring = WiringBuilder::new()
        .artifact(
            "fixture",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
        .build();
    if let Err(e) = crate::wiring::provision_artifacts(&mut wiring, &crate::BuildOptions::default())
    {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return;
    }
    let so = wiring.artifacts[0].path.clone().expect("path filled");
    let sidecar_bytes = std::fs::read(crate::dl::manifest_sidecar_path(&so))
        .expect("build driver wrote the sidecar");
    // Record the true hash, as a generated stub module would.
    wiring.artifacts[0].manifest_hash = Some(super::stubgen::manifest_hash(&sidecar_bytes));

    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().join("fixture.bundle");
    write_bundle(&wiring, &PackageOptions::default(), &dir).expect("write the bundle");
    load_bundle(&dir).expect("a matching bundle loads (hash verified)");

    // meta.json records the flight provenance: source kind, the exact copied
    // bytes' sha256, and the recorded manifest hash.
    let meta: crate::wiring::BundleMeta =
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap())
            .expect("meta.json parses");
    assert_eq!(meta.packs.len(), 1);
    let pack = &meta.packs[0];
    assert_eq!(pack.artifact_id, "fixture");
    assert_eq!(pack.source, crate::wiring::PackSourceKind::CrateBuilt);
    assert_eq!(pack.manifest_hash, wiring.artifacts[0].manifest_hash);
    let member = crate::wiring::cdylib_file_name(&wiring.artifacts[0].lib);
    let copied = std::fs::read(dir.join(member)).unwrap();
    assert_eq!(
        pack.cdylib_sha256,
        super::stubgen::manifest_hash(&copied),
        "provenance digests the bytes actually in the bundle"
    );

    let wiring_path = dir.join(crate::wiring::WIRING_FILE_NAME);
    let wiring_bytes = std::fs::read(&wiring_path).unwrap();
    let mut changed = wiring_bytes.clone();
    changed.push(b' ');
    std::fs::write(&wiring_path, changed).unwrap();
    assert!(matches!(
        load_bundle(&dir),
        Err(BundleError::IrHashMismatch)
    ));
    std::fs::write(&wiring_path, wiring_bytes).unwrap();

    // Tamper the copied sidecar: the load-time hash check now refuses it.
    let member = crate::wiring::cdylib_file_name(&wiring.artifacts[0].lib);
    let copied_sidecar = crate::dl::manifest_sidecar_path(&dir.join(member));
    std::fs::write(&copied_sidecar, b"tampered").expect("overwrite the sidecar");
    let err = load_bundle(&dir).expect_err("a tampered sidecar is refused");
    assert!(
        matches!(&err, BundleError::ManifestHashMismatch { artifact } if artifact == "fixture"),
        "{err:?}"
    );

    std::fs::remove_file(&copied_sidecar).expect("remove the sidecar");
    let err = load_bundle(&dir).expect_err("a missing required sidecar is refused");
    assert!(
        matches!(&err, BundleError::MissingManifest { artifact, .. } if artifact == "fixture"),
        "{err:?}"
    );
}

/// The single-file `.metor` form: a pack round-trips through the tar (loads
/// back with artifact paths filled from the unpacked temp dir), and two packs
/// of identical inputs are byte-identical (stable order, zeroed timestamps,
/// pinned `built_at`).
#[cfg(not(miri))]
#[test]
fn metor_archive_round_trips_and_is_reproducible() {
    use crate::wiring::{PackageOptions, load_bundle, write_bundle};

    let _guard = crate::dl::FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wiring = WiringBuilder::new()
        .artifact(
            "fixture",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
        .build();
    if let Err(e) = crate::wiring::provision_artifacts(&mut wiring, &crate::BuildOptions::default())
    {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return;
    }

    let tmp = tempfile::tempdir().expect("temp dir");
    // Pin built_at so the archive is byte-reproducible.
    let opts = PackageOptions {
        built_at_unix: Some(1_700_000_000),
        ..PackageOptions::default()
    };
    let a = tmp.path().join("a.metor");
    let b = tmp.path().join("b.metor");
    write_bundle(&wiring, &opts, &a).expect("write archive a");
    write_bundle(&wiring, &opts, &b).expect("write archive b");
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "identical inputs produce byte-identical archives"
    );

    // Unpack and load: artifact paths point into the unpacked temp dir.
    let loaded = load_bundle(&a).expect("the .metor bundle loads");
    assert_eq!(loaded.artifacts.len(), 1);
    let path = loaded.artifacts[0]
        .path
        .as_deref()
        .expect("artifact path filled");
    assert!(path.exists(), "the unpacked .so is a real file for dlopen");
    assert!(
        path.file_name().and_then(|n| n.to_str())
            == Some(crate::wiring::cdylib_file_name(&wiring.artifacts[0].lib).as_str()),
        "unpacked under the cdylib name"
    );
}

/// A pre-provenance `meta.json` (no `packs` field) still deserializes — old
/// bundles keep loading.
#[test]
fn bundle_meta_without_packs_deserializes() {
    let meta: crate::wiring::BundleMeta = serde_json::from_str(
        r#"{ "abi_version": 8, "target": null, "profile": "debug",
             "built_at_unix": 0, "ir_sha256": "sha256:00" }"#,
    )
    .expect("old meta.json deserializes");
    assert!(meta.packs.is_empty());
}

// ---------------------------------------------------------------------------
// SourceRef provenance: serde defaults keep old documents (no `src`/`scope`/
// `scopes` fields) readable.
// ---------------------------------------------------------------------------

/// A `Wiring` serialized before provenance existed (no `src`/`scope`/`scopes`
/// fields) deserializes with the defaults.
#[test]
fn wiring_json_without_provenance_fields_deserializes() {
    let json = format!(
        r#"{{
        "ir_version": {IR_VERSION},
        "coordinator": {{ "cycle_rate": 100.0, "default_depth": null, "clock": "Wall" }},
        "artifacts": [{{ "id": "plant", "crate_name": "adcs-plant", "lib": "adcs_plant", "path": null }}],
        "systems": [{{ "name": "a", "ty": "Src", "artifact": null, "params": "None" }}],
        "slots": [{{ "name": "adcs", "inputs": [], "outputs": [],
                     "allow": [{{ "occupant": "ctrl", "params": "None" }}], "initial": null }}],
        "edges": [{{ "from": "a", "out": "imu", "to": "adcs", "in_": "imu", "delayed": false }}]
    }}"#
    );
    let wiring: Wiring = serde_json::from_str(&json).expect("old document deserializes");
    assert!(wiring.scopes.is_empty());
    assert_eq!(wiring.artifacts[0].src, None);
    assert_eq!(wiring.systems[0].src, None);
    assert_eq!(wiring.systems[0].scope, None);
    assert_eq!(wiring.slots[0].src, None);
    assert_eq!(wiring.slots[0].scope, None);
    assert_eq!(wiring.slots[0].allow[0].src, None);
    assert_eq!(wiring.edges[0].src, None);
}

// ---------------------------------------------------------------------------
// Occupant resolution: the reloadable requirement is per-mount, not blanket.
// ---------------------------------------------------------------------------

/// A wired dl system may be non-reloadable (it is instantiated once); a slot
/// occupant may not (it is re-instantiated on every load).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reloadable_is_required_for_slot_occupants_only() {
    let meta = crate::abi::PackEntryDesc {
        descriptor: crate::descriptor::SystemDescriptor {
            name: "Stateful".into(),
            kind: crate::SystemKind::Cyclic,
            inputs: Vec::new(),
            outputs: Vec::new(),
            capabilities: Vec::new(),
        },
        params_schema: postcard_schema::schema::owned::OwnedNamedType::from(
            <() as postcard_schema::Schema>::SCHEMA,
        ),
        params_docs: Vec::new(),
        reloadable: false,
        params_default: None,
    };
    let entries = [meta];
    let source = super::resolve::EntrySource::Described {
        entries: &entries,
        artifact: "art",
    };
    let resolve_one = |owner: &str, require_reloadable: bool| {
        super::resolve::resolve_occupant(
            &source,
            Some("Stateful"),
            &ParamSource::None,
            owner,
            require_reloadable,
            "snippet",
            (0, 0).into(),
        )
    };
    assert!(
        resolve_one("wired", false).is_ok(),
        "a wired dl system does not require a reloadable entry"
    );
    let err = match resolve_one("adcs", true) {
        Ok(_) => panic!("expected OccupantNotReloadable"),
        Err(e) => e,
    };
    assert!(
        matches!(
            err.kind,
            LoadErrorKind::OccupantNotReloadable { ref slot, ref occupant, .. }
                if slot == "adcs" && occupant == "Stateful"
        ),
        "{err:?}"
    );
}

/// A scope index outside the table is refused before any system is built,
/// for spec `scope` fields and the table's own `parent` links alike.
#[test]
fn resolve_rejects_out_of_range_scope_refs() {
    let mut wiring = equiv_builder().build();
    wiring.systems[0].scope = Some(0);
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a BadScopeRef error"),
        Err(e) => e,
    };
    match err.kind {
        LoadErrorKind::BadScopeRef { owner, index, len } => {
            assert_eq!(owner, "system `a`");
            assert_eq!((index, len), (0, 0));
        }
        other => panic!("expected BadScopeRef, got {other:?}"),
    }

    let mut wiring = equiv_builder().build();
    wiring.scopes.push(crate::wiring::ScopeSpec {
        path: "thrusters".to_string(),
        parent: Some(3),
        src: None,
    });
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a BadScopeRef error"),
        Err(e) => e,
    };
    assert!(
        matches!(
            err.kind,
            LoadErrorKind::BadScopeRef {
                index: 3,
                len: 1,
                ..
            }
        ),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// The structural gate: `validate` runs on every ingested Wiring, so the checks
// the builder makes unrepresentable or panics on are still enforced for the
// serde path. Driven by literal Wirings the builder would refuse to construct.
// ---------------------------------------------------------------------------

fn bare_wiring() -> Wiring {
    Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
        },
        artifacts: Vec::new(),
        systems: Vec::new(),
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
    }
}

fn static_system(name: &str, ty: Option<&str>) -> SystemSpec {
    SystemSpec {
        name: name.into(),
        ty: ty.map(Into::into),
        artifact: None,
        params: ParamSource::None,
        process: false,
        src: None,
        scope: None,
    }
}

#[test]
fn validate_accepts_a_well_formed_wiring() {
    super::validate::validate(&equiv_builder().build()).expect("a builder-origin Wiring is valid");
}

#[test]
fn validate_rejects_duplicate_instance_names() {
    let mut w = bare_wiring();
    w.systems = vec![
        static_system("dup", Some("Src")),
        static_system("dup", Some("Sink")),
    ];
    let err = super::validate::validate(&w).expect_err("duplicate names are rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::DuplicateInstance { ref name } if name == "dup"),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_reserved_coordinator_name() {
    let mut w = bare_wiring();
    w.systems = vec![static_system("coordinator", Some("Src"))];
    let err = super::validate::validate(&w).expect_err("the reserved name is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::DuplicateInstance { ref name } if name == "coordinator"),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_a_system_and_slot_name_collision() {
    let mut w = bare_wiring();
    w.systems = vec![static_system("adcs", Some("Src"))];
    w.slots = vec![SlotSpec {
        name: "adcs".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: vec![AllowedOccupantSpec {
            occupant: "ctrl".into(),
            artifact: None,
            params: ParamSource::None,
            src: None,
        }],
        initial: None,
        process: false,
        src: None,
        scope: None,
    }];
    let err = super::validate::validate(&w).expect_err("a system/slot collision is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::DuplicateInstance { .. }),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_duplicate_artifact_ids() {
    let mut w = bare_wiring();
    let art = |id: &str| Artifact {
        id: id.into(),
        crate_name: "c".into(),
        lib: "c".into(),
        path: None,
        prebuilt_dir: None,
        dist: None,
        manifest_hash: None,
        src: None,
    };
    w.artifacts = vec![art("dup"), art("dup")];
    let err = super::validate::validate(&w).expect_err("duplicate artifact ids are rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::DuplicateArtifact { ref id } if id == "dup"),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_a_static_system_without_a_type() {
    let mut w = bare_wiring();
    w.systems = vec![static_system("nav", None)];
    let err = super::validate::validate(&w).expect_err("a typeless static system is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::MissingType { ref name } if name == "nav"),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_process_without_an_artifact() {
    let mut w = bare_wiring();
    let mut spec = static_system("proc", Some("Src"));
    spec.process = true;
    w.systems = vec![spec];
    let err = super::validate::validate(&w).expect_err("process without artifact is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::ProcessNeedsArtifact { ref name } if name == "proc"),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_an_unknown_artifact_ref() {
    let mut w = bare_wiring();
    let mut spec = static_system("dl", Some("Counter"));
    spec.artifact = Some("nope".into());
    w.systems = vec![spec];
    let err = super::validate::validate(&w).expect_err("a dangling artifact ref is rejected");
    assert!(
        matches!(
            err.kind,
            LoadErrorKind::UnknownArtifact { ref artifact, .. } if artifact == "nope"
        ),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_an_empty_slot() {
    let mut w = bare_wiring();
    w.slots = vec![SlotSpec {
        name: "adcs".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: Vec::new(),
        initial: None,
        process: false,
        src: None,
        scope: None,
    }];
    let err = super::validate::validate(&w).expect_err("an empty slot is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::EmptySlot { .. }),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_an_initial_outside_the_allow_set() {
    let mut w = bare_wiring();
    w.slots = vec![SlotSpec {
        name: "adcs".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: vec![AllowedOccupantSpec {
            occupant: "ctrl".into(),
            artifact: None,
            params: ParamSource::None,
            src: None,
        }],
        initial: Some(InitialOccupantSpec {
            occupant: "nonesuch".into(),
            state: SlotInitState::Loaded,
        }),
        process: false,
        src: None,
        scope: None,
    }];
    let err = super::validate::validate(&w).expect_err("an initial outside allow is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::UnknownInitialOccupant { .. }),
        "{err:?}"
    );
}

#[test]
fn validate_rejects_an_unknown_allow_artifact_ref() {
    let mut w = bare_wiring();
    w.slots = vec![SlotSpec {
        name: "adcs".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: vec![AllowedOccupantSpec {
            occupant: "ctrl".into(),
            artifact: Some("nope".into()),
            params: ParamSource::None,
            src: None,
        }],
        initial: None,
        process: false,
        src: None,
        scope: None,
    }];
    let err = super::validate::validate(&w).expect_err("a dangling allow ref is rejected");
    assert!(
        matches!(
            err.kind,
            LoadErrorKind::UnknownArtifact { ref artifact, .. } if artifact == "nope"
        ),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Builder insert-time panics: each invalid IDL fails at the offending call, so
// a builder-produced Wiring is valid by construction.
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "duplicate instance name `a`")]
fn builder_panics_on_duplicate_instance_name() {
    WiringBuilder::new()
        .system("a")
        .ty("Src")
        .end()
        .system("a")
        .ty("Sink")
        .end();
}

#[test]
#[should_panic(expected = "duplicate instance name `coordinator`")]
fn builder_panics_on_reserved_instance_name() {
    WiringBuilder::new().system("coordinator").ty("Src").end();
}

#[test]
#[should_panic(expected = "duplicate artifact id `x`")]
fn builder_panics_on_duplicate_artifact_id() {
    WiringBuilder::new()
        .artifact("x", "c", "c")
        .artifact("x", "c2", "c2");
}

#[test]
#[should_panic(expected = "unknown artifact `nope`")]
fn builder_panics_on_from_artifact_undeclared() {
    WiringBuilder::new().system("s").from_artifact("nope");
}

#[test]
#[should_panic(expected = "unknown artifact `nope`")]
fn builder_panics_on_allow_from_undeclared() {
    WiringBuilder::new().slot("s").allow_from("occ", "nope");
}

#[test]
#[should_panic(expected = "is missing its `type`")]
fn builder_panics_on_static_system_without_type() {
    WiringBuilder::new().system("s").end();
}

#[test]
#[should_panic(expected = "has no `allow` occupant")]
fn builder_panics_on_empty_slot() {
    WiringBuilder::new().slot("s").end();
}

#[test]
#[should_panic(expected = "not in the allowed set")]
fn builder_panics_on_initial_outside_allow() {
    WiringBuilder::new()
        .slot("s")
        .allow("a")
        .initial("b", SlotInitState::Loaded)
        .end();
}
