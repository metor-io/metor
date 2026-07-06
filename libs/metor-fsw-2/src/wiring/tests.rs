//! Wiring acceptance tests (wiring.md "Tests"): an end-to-end KDL load + run wiring a
//! params-bearing cyclic chain into an async logger, instance-name disambiguation
//! of two instances of one system type, the span-carrying error cases, and the
//! serde-over-KDL params round-trip. Systems are tiny but real `CyclicSystem`/
//! `AsyncSystem` impls registered in a [`Registry`].

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_fsw_ring::Notifier;
use metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::wiring::{
    ClockSpec, LoadError, ParamSource, Registry, SlotInitState, WiringBuilder, load, parse,
    resolve,
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

/// Same frame *name* (hence the same `frame_id`) as [`Imu`], but a strictly larger
/// component set — a consumer of this is **incompatible** with an [`Imu`] producer.
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
// A params-bearing cyclic producer: writes an incrementing `Imu` every cycle.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
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
// A params-bearing cyclic filter: consumes `imu`, produces `nav` = omega * gain.
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
        if let Some(imu) = input.imu.latest() {
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
// An async, no-params logger: consumes `nav` over a private copy-in buffer and
// records the latest angle to module statics (observed by the e2e test).
// ---------------------------------------------------------------------------

static LOG_COUNT: AtomicU64 = AtomicU64::new(0);
static LOG_LAST_ANGLE_BITS: AtomicU64 = AtomicU64::new(0);

struct NavLogger;

#[derive(SystemInput)]
struct LogIn {
    nav: Input<Nav, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct LogNoOut {}

impl System for NavLogger {
    type Input = LogIn;
    type Output = Out<LogNoOut, Notifier, Notifier>;
    const NAME: &'static str = "nav_logger";
}

impl AsyncSystem for NavLogger {
    async fn run(&mut self, input: &mut Self::Input, _o: &mut Self::Output) {
        // Corrupt (the only recv error) is unreachable in practice; skip the wake.
        if let Ok(nav) = input.nav.recv().await {
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
// A picky consumer requiring the larger `ImuBig` (same frame_id, bigger set).
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
// A loop-closer: consumes `nav`, produces `imu`. With `NavFilter` (imu -> nav)
// this forms a 2-system feedback cycle that exercises the KDL `delayed=` edge.
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
            Some(nav) => nav.get().angle,
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
// Two paramless cyclic systems (for the builder ≡ KDL equivalence test): a `Src`
// producing `imu`, a `Sink` consuming it. Config-less, so both front-ends produce
// byte-equal `SystemSpec`s (empty params).
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

/// The app's registry: the visible, auditable list of loadable systems.
fn registry() -> Registry {
    // Seeded from the built-ins (the alarm engine), as an app registry would be.
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
    r
}

// ---------------------------------------------------------------------------
// End-to-end: load a worked KDL document, run it, assert data flows.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn end_to_end_load_and_run() {
    LOG_COUNT.store(0, Relaxed);
    LOG_LAST_ANGLE_BITS.store(0, Relaxed);

    let kdl = r#"
coordinator cycle_rate=1000.0

system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "nav" type="NavFilter" gain=2.0
system "log" type="NavLogger"

connect "imu" -> "nav" frame="imu"
connect "nav" -> "log" frame="nav"
"#;

    let mut coord = load(kdl, &registry()).expect("load succeeds");
    coord.run_for(60).await;

    // The async logger received `nav` records flowing imu -> nav -> log, and the
    // last angle is omega * gain (gain = 2.0), proving the params reached the
    // filter and data crossed every edge.
    assert!(LOG_COUNT.load(Relaxed) >= 1, "logger received nav via copy-in");
    let last = f64::from_bits(LOG_LAST_ANGLE_BITS.load(Relaxed));
    assert!(last > 0.0, "received a real angle: {last}");
    assert_eq!(last % 2.0, 0.0, "angle = omega * gain(2.0): {last}");
}

// ---------------------------------------------------------------------------
// Alarms from KDL: the built-in `type="Alarms"` node loads, defers behind the
// other systems (F1), wires an uplink AlarmAck edge, and raises on the wire.
// ---------------------------------------------------------------------------

/// The full `docs/alarms.md` §3 surface through `load`: the alarms node is
/// deliberately declared FIRST — without the resolver's deferred-ReceiveAll pass
/// this graph fails `build()` with `ReceiveAllNotLast` (`docs/alarms.md` §7 F1) —
/// and the `AlarmAck` edge from a (never-connecting) uplink resolves like any
/// message edge. The raise lands on the `alarms.AlarmRaised` registry ring.
#[cfg(not(miri))]
#[stellarator::test]
async fn alarms_node_loads_defers_and_raises() {
    let kdl = r#"
coordinator cycle_rate=1000.0

system "alarms" type="Alarms" {
    alarm id="OMEGA_HIGH" name="Omega High" {
        target component="imu.imu.omega"
        warning above=2.5
        critical above=4.5
    }
}
system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0

uplink { transport "tcp" addr="127.0.0.1:2299" }
connect "uplink" -> "alarms" msg="AlarmAck"
"#;

    let coord = load(kdl, &registry()).expect("load succeeds (alarms deferred)");
    let registry = coord.registry();
    let mut raised: crate::MsgIn<metor_proto_wkt::AlarmRaised> = crate::MsgIn::new(
        registry
            .get(ComponentId::new("alarms.AlarmRaised"))
            .expect("alarm raise channel registered")
            .view()
            .expect("reader slot"),
    );

    let mut coord = coord;
    // omega increments 1.0/cycle: warning at 3.0 (cycle 3), critical at 5.0 (cycle 5).
    coord.run_for(5).await;

    let mut got = Vec::new();
    raised.drain(|r| got.push(r));
    assert_eq!(got.len(), 2, "warning then escalation: {got:?}");
    assert_eq!(got[0].def_id, "OMEGA_HIGH");
    assert_eq!(got[0].severity, metor_proto_wkt::Severity::Warning);
    assert_eq!(got[1].severity, metor_proto_wkt::Severity::Critical);
    assert_eq!(got[1].occurrence, got[0].occurrence);
}

/// A semantically-bad alarm spec surfaces as a spanned `InvalidParam` through the
/// full `load` path (the `try_from` refinement inside the factory's deserialize).
#[test]
fn err_alarm_misconfig_is_invalid_param() {
    let err = load_err(
        r#"
coordinator cycle_rate=100.0
system "alarms" type="Alarms" {
    alarm id="X" name="X" {
        target component="a.b.c"
    }
}
"#,
    );
    assert!(
        matches!(err, LoadError::InvalidParam { ref property, .. } if property == "alarm"),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Instance-naming: two instances of one type → distinct handles + recorded names.
// ---------------------------------------------------------------------------

#[test]
fn instance_name_disambiguation() {
    let kdl = r#"
coordinator cycle_rate=100.0

system "imu_left"  type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "imu_right" type="ImuDriver" i2c_bus=2 sample_hz=200.0
system "nav"       type="NavFilter" gain=1.0

connect "imu_left" -> "nav" frame="imu"
"#;
    // imu_right is unconnected as a producer (no inputs of its own), so the graph
    // builds: only inputs must be connected, and imu_right has none.
    let coord = load(kdl, &registry()).expect("load succeeds");

    // Both `imu` output buffers exist under their distinct instance names, despite
    // sharing the same `frame_id` (the headline collision, now disambiguated).
    let imu_id = ComponentId::new("imu");
    let outs = coord.output_instances();
    let imu_instances: Vec<&str> = outs
        .iter()
        .filter(|(_, fid)| *fid == imu_id)
        .map(|(name, _)| *name)
        .collect();
    assert!(imu_instances.contains(&"imu_left"), "{imu_instances:?}");
    assert!(imu_instances.contains(&"imu_right"), "{imu_instances:?}");
}

// ---------------------------------------------------------------------------
// Error cases, each with a source span (LoadError is a miette Diagnostic).
// ---------------------------------------------------------------------------

/// `load` the document expecting failure (`Coordinator` is not `Debug`, so we
/// cannot use `unwrap_err`).
fn load_err(kdl: &str) -> LoadError {
    match load(kdl, &registry()) {
        Ok(_) => panic!("expected load to fail, but it succeeded"),
        Err(e) => e,
    }
}

#[test]
fn err_unknown_type() {
    let kdl = r#"
coordinator cycle_rate=100.0
system "x" type="Nonexistent"
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::UnknownType { .. }), "{err:?}");
}

#[test]
fn err_missing_param() {
    // NavFilter requires `gain` — omit it.
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav" type="NavFilter"
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::MissingParam { .. }), "{err:?}");
}

#[test]
fn err_invalid_param() {
    // `gain` wants a number; give it a string.
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav" type="NavFilter" gain="fast"
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
}

#[test]
fn err_unknown_frame() {
    // `bogus` is not a port of either system.
    let kdl = r#"
coordinator cycle_rate=100.0
system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "nav" type="NavFilter" gain=1.0
connect "imu" -> "nav" frame="bogus"
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::UnknownFrame { .. }), "{err:?}");
}

#[test]
fn err_incompatible_edge_surfaced_as_wire() {
    // ImuDriver produces `imu` (omega); PickyConsumer requires the larger `imu`
    // (omega + extra) under the same frame_id → Incompatible at build().
    let kdl = r#"
coordinator cycle_rate=100.0
system "imu"   type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "picky" type="PickyConsumer"
connect "imu" -> "picky" frame="imu"
"#;
    let err = load_err(kdl);
    assert!(
        matches!(err, LoadError::Wire { source: crate::WireError::Incompatible { .. }, .. }),
        "{err:?}"
    );
}

#[test]
fn err_duplicate_instance() {
    let kdl = r#"
coordinator cycle_rate=100.0
system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "imu" type="ImuDriver" i2c_bus=2 sample_hz=200.0
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::DuplicateInstance { .. }), "{err:?}");
}

#[test]
fn err_missing_coordinator() {
    let kdl = r#"system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::MissingCoordinator), "{err:?}");
}

#[test]
fn err_multiple_coordinators() {
    let kdl = r#"
coordinator cycle_rate=100.0
coordinator cycle_rate=200.0
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::MultipleCoordinators { .. }), "{err:?}");
}

// ---------------------------------------------------------------------------
// Feedback edges: an unbroken KDL cycle surfaces as a Wire(FeedbackCycle); a
// `delayed=#true` edge breaks it so the document loads.
// ---------------------------------------------------------------------------

#[test]
fn err_unbroken_feedback_cycle() {
    // nav -> closer (nav) and closer -> nav (imu) with both plain → a cycle.
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav"    type="NavFilter" gain=1.0
system "closer" type="Closer"
connect "closer" -> "nav"    frame="imu"
connect "nav"    -> "closer" frame="nav"
"#;
    let err = load_err(kdl);
    assert!(
        matches!(err, LoadError::Wire { source: crate::WireError::FeedbackCycle { .. }, .. }),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn delayed_kdl_edge_breaks_cycle_and_runs() {
    // The same loop, but the `closer -> nav` back-edge (backward in registration
    // order: nav registers first, so it reads closer one cycle late) is
    // `delayed=#true`. Also uses a `sim_dt` (free-running simulated clock) — both
    // new KDL surfaces at once.
    let kdl = r#"
coordinator cycle_rate=100.0 sim_dt=0.00833
system "nav"    type="NavFilter" gain=1.0
system "closer" type="Closer"
connect "closer" -> "nav"    frame="imu" delayed=#true
connect "nav"    -> "closer" frame="nav"
"#;
    let mut coord = load(kdl, &registry()).expect("delayed edge breaks the cycle; doc loads");
    coord.run_for(5).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// Telemetry: a `telemetry` node loads (registered last) and runs. The TCP endpoint
// is closed, so the sender just fails to connect and stops downlinking — the control
// cycle is unaffected and the run completes (telemetry.md §7/§8).
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn telemetry_node_loads_and_runs() {
    let kdl = r#"
coordinator cycle_rate=1000.0 sim_dt=0.001

system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "nav" type="NavFilter" gain=2.0

connect "imu" -> "nav" frame="imu"

telemetry {
    transport "tcp" addr="127.0.0.1:59421"
    mode "all"
}
"#;
    let mut coord = load(kdl, &registry()).expect("telemetry node loads");
    coord.run_for(10).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// The serde-over-KDL params deserializer round-trips scalars, a string, an
// optional, and a `#[serde(default)]` (the old derive's `#[kdl(default)]`).
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

fn first_node(src: &str) -> kdl::KdlNode {
    src.parse::<kdl::KdlDocument>().unwrap().nodes()[0].clone()
}

/// Deserialize a params struct off a bare (reserved-key-less, arg-less) node —
/// the derive-replacement entry the registry factory uses.
fn round_trip(src: &str) -> Result<RoundTrip, LoadError> {
    let node = first_node(src);
    super::de::from_kdl_node::<RoundTrip>(&node, src, "params", &[], 0)
}

#[test]
fn from_kdl_node_round_trip_defaults() {
    let src = r#"params count=3 rate=1.5 label="hi""#;
    let got = round_trip(src).unwrap();
    assert_eq!(
        got,
        RoundTrip {
            count: 3,
            rate: 1.5,
            label: "hi".to_string(),
            offset: None,
            depth: 4, // the #[serde(default = "default_depth")] fallback
        }
    );
}

#[test]
fn from_kdl_node_round_trip_explicit() {
    let src = r#"params count=7 rate=2 label="full" offset=0.25 depth=9"#;
    let got = round_trip(src).unwrap();
    assert_eq!(
        got,
        RoundTrip {
            count: 7,
            rate: 2.0, // integer literal accepted where a float is wanted
            label: "full".to_string(),
            offset: Some(0.25),
            depth: 9,
        }
    );
}

// ---------------------------------------------------------------------------
// Params-surface errors through the full load path: unknown params (static path,
// previously silently ignored), entry-precise spans, duplicate keys, and the
// E5 surface fixes (unknown top-level nodes, the `lib=` -> `artifact=` rename,
// `type=` optional on dl systems).
// ---------------------------------------------------------------------------

#[test]
fn err_unknown_param_on_static_system() {
    // A typo'd property on a static system is a spanned `UnknownParam` naming the
    // key (the old `FromKdlNode` walk silently ignored it).
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav" type="NavFilter" gain=0.8 gian=0.5
"#;
    let err = load_err(kdl);
    match err {
        LoadError::UnknownParam { property, system, span, .. } => {
            assert_eq!(property, "gian");
            assert_eq!(system, "nav");
            // The span points at the `gian=0.5` entry (within the carried node
            // text), not at offset 0 (the whole node).
            assert!(span.offset() > 0, "entry-precise span, got {span:?}");
        }
        other => panic!("expected UnknownParam, got {other:?}"),
    }
}

#[test]
fn err_invalid_param_span_points_at_the_entry() {
    // `gain` wants a number; the span points inside the `gain="fast"` entry, not
    // at the node start.
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav" type="NavFilter" gain="fast"
"#;
    let err = load_err(kdl);
    match err {
        LoadError::InvalidParam { property, src, span, .. } => {
            assert_eq!(property, "gain");
            let at = src.find("gain=").expect("the carried text holds the entry");
            assert!(
                span.offset() >= at,
                "span {span:?} points at the entry (at {at}) in {src:?}"
            );
        }
        other => panic!("expected InvalidParam, got {other:?}"),
    }
}

#[test]
fn err_unit_params_reject_stray_properties() {
    // `Src` is paramless (`type Params = ()`): a stray property is an UnknownParam,
    // not a silent no-op.
    let kdl = r#"
coordinator cycle_rate=100.0
system "src" type="Src" bogus=1
"#;
    let err = load_err(kdl);
    assert!(
        matches!(err, LoadError::UnknownParam { ref property, .. } if property == "bogus"),
        "{err:?}"
    );
}

#[test]
fn err_repeated_property_is_rejected() {
    // KDL's last-wins for a repeated property is NOT honored for params — explicit
    // is better than a silently-dropped first value.
    let kdl = r#"
coordinator cycle_rate=100.0
system "nav" type="NavFilter" gain=1.0 gain=2.0
"#;
    let err = load_err(kdl);
    assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
}

#[test]
fn err_unknown_coordinator_property() {
    // The coordinator props ride the same deserializer: a misspelled knob is a
    // spanned UnknownParam rather than a silent skip.
    let kdl = r#"
coordinator cycle_rate=100.0 default_dept=8
system "src" type="Src"
"#;
    let err = load_err(kdl);
    assert!(
        matches!(err, LoadError::UnknownParam { ref property, .. } if property == "default_dept"),
        "{err:?}"
    );
}

#[test]
fn err_unknown_top_level_node() {
    let kdl = r#"
coordinator cycle_rate=100.0
frobnicate "x"
"#;
    let err = load_err(kdl);
    assert!(
        matches!(err, LoadError::UnknownTopLevelNode { ref node, .. } if node == "frobnicate"),
        "{err:?}"
    );
}

#[test]
fn err_lib_on_system_is_a_rename_guidance_error() {
    // The pre-rename `lib=` spelling on a `system` node gets a dedicated spanned
    // error pointing at the entry (`artifact` nodes keep `lib=` for the stem).
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "plant" crate="adcs-plant" lib="adcs_plant" type="Plant"
system "plant" type="Plant" lib="plant"
"#;
    let err = load_err(kdl);
    match err {
        LoadError::SystemLibRenamed { src, span } => {
            let at = src.rfind("lib=\"plant\"").expect("the system's lib= entry");
            assert!(span.offset() >= at, "span {span:?} at the system's lib= (at {at})");
        }
        other => panic!("expected SystemLibRenamed, got {other:?}"),
    }
}

#[test]
fn type_is_optional_when_artifact_is_given() {
    // A dl system may omit `type=` (the artifact's `system_type` is authoritative,
    // filled at resolve); a static system still requires it.
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "plant" crate="adcs-plant" lib="adcs_plant" type="Plant"
system "plant" artifact="plant"
"#;
    let wiring = parse(kdl).expect("type= is optional with artifact=");
    assert_eq!(wiring.systems[0].ty, None);
    assert_eq!(wiring.systems[0].artifact.as_deref(), Some("plant"));

    let err = match parse(
        r#"
coordinator cycle_rate=100.0
system "nav"
"#,
    ) {
        Ok(_) => panic!("a static system without type= must not parse"),
        Err(e) => e,
    };
    assert!(matches!(err, LoadError::MissingType { .. }), "{err:?}");
}

#[test]
fn allow_line_property_params_are_carried() {
    // B1 (parse half): params as line properties on an `allow` node — not just a
    // child block — are carried as `ParamSource::Kdl` for the resolve-time encoder.
    let kdl = r#"
coordinator cycle_rate=100.0
slot "adcs" {
    allow occupant="commissioning" gain=0.8
    allow occupant="safe_mode"
}
"#;
    let wiring = parse(kdl).expect("parse a slot with line-property allow params");
    match &wiring.slots[0].allow[0].params {
        ParamSource::Kdl(text) => {
            assert!(text.contains("gain=0.8"), "carried allow text: {text}")
        }
        other => panic!("expected ParamSource::Kdl, got {other:?}"),
    }
    assert_eq!(wiring.slots[0].allow[1].params, ParamSource::None);
}

// ---------------------------------------------------------------------------
// The Rust `WiringBuilder` and the KDL `parse` are two front-ends onto the same
// `Wiring` data model — they produce byte-equal `Wiring` for a (config-less, static)
// graph, and resolve to an equivalent running coordinator.
// ---------------------------------------------------------------------------

/// The same graph expressed in Rust, used by both equivalence tests below.
fn equiv_builder() -> WiringBuilder {
    WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("a")
        .ty("Src")
        .from_static()
        .end()
        .system("b")
        .ty("Sink")
        .from_static()
        .end()
        .connect("a", "imu", "b", "imu")
}

#[test]
fn builder_and_kdl_produce_equal_wiring() {
    let kdl = r#"
coordinator cycle_rate=100.0
system "a" type="Src"
system "b" type="Sink"
connect "a" -> "b" frame="imu"
"#;
    let from_kdl = parse(kdl).expect("parse onto the Wiring data model");
    let from_builder = equiv_builder().build();
    assert_eq!(from_kdl, from_builder, "the two front-ends produce equal Wiring");
}

#[cfg(not(miri))]
#[stellarator::test]
async fn builder_wiring_resolves_and_runs() {
    // The builder-origin Wiring (no text at all) resolves through the one shared
    // resolver and runs without any system hard-stopping.
    let wiring = equiv_builder().build();
    let mut coord = resolve(&wiring, &registry()).expect("resolve the builder Wiring");
    coord.run_for(5).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// A dl system declared in KDL carries its `system`-node config as `ParamSource::Kdl`
// (schema-encoded at resolve). Resolution of the `.so` itself + the headline
// KDL≡builder byte-equality is exercised by `tests_abi`/`tests/wiring_resolve.rs`
// (a real dlopen).
// ---------------------------------------------------------------------------

#[test]
fn dl_kdl_params_are_carried_as_kdl_source() {
    // KDL params on an `artifact=` system are carried as the node source text for
    // the resolve-time schema-guided encoder.
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "plant" crate="adcs-plant" lib="adcs_plant" type="Plant"
system "plant" type="Plant" artifact="plant" gain=5.0
"#;
    let wiring = parse(kdl).expect("KDL params on a dl system parse");
    match &wiring.systems[0].params {
        ParamSource::Kdl(text) => assert!(text.contains("gain=5.0"), "carried node text: {text}"),
        other => panic!("expected ParamSource::Kdl, got {other:?}"),
    }
}

#[test]
fn dl_system_in_kdl_parses_into_an_artifact_ref() {
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "plant" crate="adcs-plant" lib="adcs_plant" type="Plant"
system "plant" type="Plant" artifact="plant"
"#;
    let wiring = parse(kdl).expect("parse a dl system + artifact");
    assert_eq!(wiring.artifacts.len(), 1);
    assert_eq!(wiring.artifacts[0].id, "plant");
    assert_eq!(wiring.artifacts[0].crate_name, "adcs-plant");
    // `lib=` is a stem; the framework decorates it to this platform's produced file name.
    assert_eq!(
        wiring.artifacts[0].cdylib,
        super::cdylib_file_name("adcs_plant")
    );
    assert_eq!(wiring.artifacts[0].system_type, "Plant");
    assert_eq!(wiring.systems.len(), 1);
    assert_eq!(wiring.systems[0].artifact.as_deref(), Some("plant"));
    assert_eq!(wiring.systems[0].params, ParamSource::None, "no params (dl, no config)");
}

// ---------------------------------------------------------------------------
// Slots: a `slot` KDL node round-trips to a `SlotSpec` (name, inputs, outputs,
// allow list with per-occupant params, initial), and shares the instance namespace.
// ---------------------------------------------------------------------------

#[test]
fn slot_node_round_trips_to_slot_spec() {
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "commissioning" crate="adcs-seqs" lib="adcs_commissioning" type="commissioning"
artifact "safe_mode"     crate="adcs-seqs" lib="adcs_safe_mode"     type="safe_mode"

slot "adcs" {
    input  frame="sensors"
    output frame="mode"
    allow occupant="commissioning" {
        gain 0.8
    }
    allow occupant="safe_mode"
    initial occupant="commissioning" state="running"
}
"#;
    let wiring = parse(kdl).expect("parse a slot node onto Wiring");
    assert_eq!(wiring.slots.len(), 1);
    let slot = &wiring.slots[0];
    assert_eq!(slot.name, "adcs");
    assert_eq!(slot.inputs, vec!["sensors".to_string()]);
    assert_eq!(slot.outputs, vec!["mode".to_string()]);
    assert_eq!(slot.allow.len(), 2);
    assert_eq!(slot.allow[0].occupant, "commissioning");
    // The first `allow` carries a params child block ⇒ `ParamSource::Kdl`.
    match &slot.allow[0].params {
        ParamSource::Kdl(text) => assert!(text.contains("gain"), "carried allow text: {text}"),
        other => panic!("expected ParamSource::Kdl, got {other:?}"),
    }
    // The second `allow` has no params block ⇒ `ParamSource::None`.
    assert_eq!(slot.allow[1].occupant, "safe_mode");
    assert_eq!(slot.allow[1].params, ParamSource::None);
    let initial = slot.initial.as_ref().expect("an initial occupant");
    assert_eq!(initial.occupant, "commissioning");
    assert_eq!(initial.state, SlotInitState::Running);
}

#[test]
fn slot_state_defaults_to_loaded_and_rejects_garbage() {
    // No `state=` ⇒ Loaded (the conservative default).
    let ok = parse(
        r#"
coordinator cycle_rate=100.0
slot "s" {
    allow occupant="x"
    initial occupant="x"
}
"#,
    )
    .expect("parse a slot with a state-less initial");
    assert_eq!(ok.slots[0].initial.as_ref().unwrap().state, SlotInitState::Loaded);

    // A bogus `state=` is a clean `BadSlotState`.
    let err = parse(
        r#"
coordinator cycle_rate=100.0
slot "s" {
    allow occupant="x"
    initial occupant="x" state="frobnicate"
}
"#,
    )
    .expect_err("a bad state is rejected");
    assert!(matches!(err, LoadError::BadSlotState { .. }), "{err:?}");
}

#[test]
fn slot_unknown_child_is_a_clean_error() {
    let err = parse(
        r#"
coordinator cycle_rate=100.0
slot "s" {
    allow occupant="x"
    bogus thing="here"
}
"#,
    )
    .expect_err("an unknown slot child is rejected");
    assert!(matches!(err, LoadError::UnknownSlotChild { .. }), "{err:?}");
}

#[test]
fn slot_name_collides_with_a_system_as_duplicate_instance() {
    // A slot and a system sharing a name is a duplicate-instance error (shared namespace).
    let err = parse(
        r#"
coordinator cycle_rate=100.0
system "adcs" type="Src"
slot "adcs" {
    allow occupant="x"
}
"#,
    )
    .expect_err("a slot/system name collision is rejected");
    assert!(matches!(err, LoadError::DuplicateInstance { .. }), "{err:?}");
}

#[test]
fn slot_builder_mirrors_kdl() {
    // The Rust front-end expresses the same slot the KDL `slot` node does.
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .artifact("commissioning", "adcs-seqs", "adcs_commissioning", "commissioning")
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
fn unknown_artifact_ref_is_a_clean_error() {
    // A dl system referencing an undeclared artifact resolves to UnknownArtifact.
    let kdl = r#"
coordinator cycle_rate=100.0
system "plant" type="Plant" artifact="missing"
"#;
    let wiring = parse(kdl).expect("parses (resolution is deferred)");
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected an UnknownArtifact error"),
        Err(e) => e,
    };
    assert!(matches!(err, LoadError::UnknownArtifact { .. }), "{err:?}");
}

#[test]
fn err_static_system_with_typed_params() {
    // Typed builder params on a *static* system have no decode path (the Registry
    // factory deserializes KDL, not postcard) — rejected at resolve, never
    // silently dropped onto defaults.
    #[derive(serde::Serialize)]
    struct Gain {
        gain: f64,
    }
    let wiring = WiringBuilder::new()
        .coordinator(100.0, ClockSpec::Wall)
        .system("nav")
        .ty("NavFilter")
        .from_static()
        .params(Gain { gain: 2.0 })
        .end()
        .build();
    let err = match resolve(&wiring, &registry()) {
        Ok(_) => panic!("expected a StaticPostcardParams error"),
        Err(e) => e,
    };
    assert!(matches!(err, LoadError::StaticPostcardParams { .. }), "{err:?}");
}

#[test]
fn err_unknown_initial_occupant() {
    // A typo'd `initial occupant=` is a resolve-time error naming the bad occupant and
    // the allowed set — not a slot that silently boots `Empty`. Pure spec validation,
    // so it fires before any artifact is built or opened.
    let kdl = r#"
coordinator cycle_rate=100.0
artifact "waiter" crate="seqs" lib="seq_waiter" type="waiter"
slot "adcs" {
    allow occupant="waiter"
    initial occupant="waiterr" state="running"
}
"#;
    let err = load_err(kdl);
    match err {
        LoadError::UnknownInitialOccupant {
            slot,
            occupant,
            allowed,
            ..
        } => {
            assert_eq!(slot, "adcs");
            assert_eq!(occupant, "waiterr");
            assert_eq!(allowed, "waiter");
        }
        other => panic!("expected UnknownInitialOccupant, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Message edges through the wiring front-ends (docs/message-wiring.md §3.4):
// KDL `msg=` and `WiringBuilder::connect_msg`.
// ---------------------------------------------------------------------------

static WIRE_SINK_COUNT: AtomicU64 = AtomicU64::new(0);
static WIRE_SINK_LAST: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct WireEvent {
    seq: u64,
}

// A wired message port needs the explicit, stable name token (A10).
impl crate::NamedMsg for WireEvent {
    const NAME: &'static str = "WireEvent";
}

// A cyclic message producer (no params): emits one `WireEvent` per cycle.
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

// A cyclic message consumer (no params): records each drained `WireEvent` to statics.
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
        input.events.drain(|e| {
            WIRE_SINK_COUNT.fetch_add(1, Relaxed);
            WIRE_SINK_LAST.store(e.seq, Relaxed);
        });
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
async fn msg_edge_kdl_round_trip() {
    WIRE_SINK_COUNT.store(0, Relaxed);
    WIRE_SINK_LAST.store(0, Relaxed);

    let kdl = r#"
coordinator cycle_rate=1000.0

system "src"  type="MsgSrc"
system "sink" type="MsgSink"

connect "src" -> "sink" msg="WireEvent"
"#;

    // The `msg=` shorthand parses to an `EdgeKind::Msg` `EdgeSpec`...
    let wiring = parse(kdl).expect("parse succeeds");
    assert_eq!(wiring.edges.len(), 1);
    assert_eq!(wiring.edges[0].kind, crate::wiring::EdgeKind::Msg);
    assert_eq!(wiring.edges[0].out, "WireEvent");

    // ...and resolves + builds + runs, delivering the messages.
    let mut coord = load(kdl, &registry()).expect("load succeeds");
    coord.run_for(4).await;
    assert!(WIRE_SINK_COUNT.load(Relaxed) >= 4, "sink drained events");
    assert_eq!(WIRE_SINK_LAST.load(Relaxed), 4, "last seq = cycle 4");
}

#[test]
fn msg_edge_delayed_is_rejected_at_build() {
    // `delayed=#true` on a `msg=` edge parses (no parse-time schema knowledge) but
    // surfaces `WireError::DelayedLogEdge` at build — previously it was silently
    // meaningless (A7).
    let kdl = r#"
coordinator cycle_rate=1000.0

system "src"  type="MsgSrc"
system "sink" type="MsgSink"

connect "src" -> "sink" msg="WireEvent" delayed=#true
"#;
    let err = match load(kdl, &registry()) {
        Err(e) => e,
        Ok(_) => panic!("expected DelayedLogEdge for delayed msg edge"),
    };
    assert!(
        matches!(
            &err,
            LoadError::Wire {
                source: crate::WireError::DelayedLogEdge { .. },
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn msg_edge_unknown_type_is_a_clean_error() {
    let kdl = r#"
coordinator cycle_rate=100.0

system "src"  type="MsgSrc"
system "sink" type="MsgSink"

connect "src" -> "sink" msg="Nope"
"#;
    // `Coordinator` is not `Debug`, so match rather than `unwrap_err`.
    let err = match load(kdl, &registry()) {
        Err(e) => e,
        Ok(_) => panic!("expected UnknownMsg for a misspelled message type"),
    };
    assert!(
        matches!(err, LoadError::UnknownMsg { .. }),
        "unexpected error: {err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_edge_builder_matches_kdl() {
    WIRE_SINK_COUNT.store(0, Relaxed);

    // The fluent builder's `connect_msg` produces the same `EdgeKind::Msg` edge as KDL.
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

// ---------------------------------------------------------------------------
// The command plane through the KDL front-end (A2): the reserved "coordinator"
// instance, the `uplink { … }` node, and msg-edge resolution by packet id when the
// producer's display name is overridden (`coordinator.commands`).
// ---------------------------------------------------------------------------

static CMD_SINK_COUNT: AtomicU64 = AtomicU64::new(0);

// A cyclic consumer of the wkt `SequenceCommand` (no params): counts drained commands.
struct CmdSink;

#[derive(SystemInput)]
struct CmdSinkIn {
    commands: MsgIn<metor_proto_wkt::SequenceCommand>,
}

impl System for CmdSink {
    type Input = CmdSinkIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "cmd_sink";
}

impl CyclicSystem for CmdSink {
    fn execute(&mut self, _now: Timestamp, input: &mut CmdSinkIn, _o: &mut Self::Output) {
        input.commands.drain(|_| {
            CMD_SINK_COUNT.fetch_add(1, Relaxed);
        });
    }
}

impl BuildSystem for CmdSink {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        CmdSink
    }
}

fn cmd_registry() -> Registry {
    let mut r = Registry::new();
    r.register::<CmdSink, _>("CmdSink");
    r
}

/// `uplink { transport "tcp" addr=… }` parses onto `Wiring.uplink` (the design's
/// KDL surface for the instance name "uplink").
#[test]
fn uplink_node_parses() {
    let kdl = r#"
coordinator cycle_rate=100.0
uplink { transport "tcp" addr="127.0.0.1:2241" }
"#;
    let wiring = parse(kdl).expect("parse succeeds");
    assert_eq!(
        wiring.uplink,
        Some(crate::wiring::UplinkSpec {
            addr: "127.0.0.1:2241".parse().unwrap()
        })
    );
}

/// `"coordinator"` is a reserved instance name: a user system claiming it is a
/// clean `DuplicateInstance`, not a silent registry-key collision.
#[test]
fn user_system_named_coordinator_is_rejected() {
    let kdl = r#"
coordinator cycle_rate=100.0
system "coordinator" type="CmdSink"
"#;
    let err = match load(kdl, &cmd_registry()) {
        Err(e) => e,
        Ok(_) => panic!("expected DuplicateInstance for the reserved name"),
    };
    assert!(
        matches!(err, LoadError::DuplicateInstance { ref name, .. } if name == "coordinator"),
        "unexpected error: {err:?}"
    );
}

/// The operator command edge resolves through KDL: the `msg="SequenceCommand"` token
/// names the message TYPE; the coordinator's producer port (display name
/// `"commands"`, registry key `coordinator.commands`) is matched by packet id via the
/// consumer the token resolved on — and the in-proc `control_handle` emit reaches the
/// consumer over that edge.
#[cfg(not(miri))]
#[stellarator::test]
async fn coordinator_command_edge_resolves_and_delivers() {
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
    CMD_SINK_COUNT.store(0, Relaxed);

    let kdl = r#"
coordinator cycle_rate=1000.0

system "sink" type="CmdSink"

connect "coordinator" -> "sink" msg="SequenceCommand"
"#;
    let mut coord = load(kdl, &cmd_registry()).expect("load succeeds");
    let mut control = coord.control_handle().expect("taken once");
    control
        .emit(&SequenceCommand {
            channel: "anything".to_string(),
            command: SequenceCommandKind::Start,
        })
        .expect("emit over the declared operator channel");
    coord.run_for(2).await;
    assert_eq!(CMD_SINK_COUNT.load(Relaxed), 1, "the edged command arrived");
}

/// Without the edge the operator channel is inert (the A2 opt-in, KDL spelling).
/// Asserted through the SLOT-FREE consumer's own drain: the sink's `MsgIn` binds
/// zero producer views, so the emit cannot arrive. (Reads the sink's fan-in shape
/// off the built graph rather than a shared counter — the counter tests run in
/// parallel.)
#[cfg(not(miri))]
#[stellarator::test]
async fn coordinator_commands_without_edge_are_inert() {
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

    let kdl = r#"
coordinator cycle_rate=1000.0

system "isolated_sink" type="CmdSinkInert"
"#;
    let mut r = Registry::new();
    r.register::<CmdSinkInert, _>("CmdSinkInert");
    let mut coord = load(kdl, &r).expect("load succeeds");
    let mut control = coord.control_handle().expect("taken once");
    control
        .emit(&SequenceCommand {
            channel: "anything".to_string(),
            command: SequenceCommandKind::Start,
        })
        .expect("emit over the declared operator channel");
    coord.run_for(2).await;
    assert_eq!(
        CMD_INERT_COUNT.load(Relaxed),
        0,
        "no edge, no delivery — the handle is inert by wiring"
    );
}

static CMD_INERT_COUNT: AtomicU64 = AtomicU64::new(0);

/// The inert-path twin of [`CmdSink`], with its own counter (tests run in parallel).
struct CmdSinkInert;

#[derive(SystemInput)]
struct CmdSinkInertIn {
    commands: MsgIn<metor_proto_wkt::SequenceCommand>,
}

impl System for CmdSinkInert {
    type Input = CmdSinkInertIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "cmd_sink_inert";
}

impl CyclicSystem for CmdSinkInert {
    fn execute(&mut self, _now: Timestamp, input: &mut CmdSinkInertIn, _o: &mut Self::Output) {
        input.commands.drain(|_| {
            CMD_INERT_COUNT.fetch_add(1, Relaxed);
        });
    }
}

impl BuildSystem for CmdSinkInert {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        CmdSinkInert
    }
}
