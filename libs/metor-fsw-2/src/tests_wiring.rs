//! WP6 acceptance tests (wiring.md "Tests"): an end-to-end KDL load + run wiring a
//! params-bearing cyclic chain into an async logger, instance-name disambiguation
//! of two instances of one system type, the span-carrying error cases, and the
//! `FromKdlNode` derive round-trip. Systems are tiny but real `CyclicSystem`/
//! `AsyncSystem` impls registered in a [`Registry`].

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_fsw_ring::{BoxBacking, Notifier};
use metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::wiring::{FromKdlNode, LoadError, Registry, RegisteredSystem, load};
use crate::{
    AsyncSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
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

#[derive(FromKdlNode)]
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

impl RegisteredSystem for ImuDriver {
    type Params = ImuParams;
    fn new(params: Self::Params) -> Self {
        Self { params, n: 0.0 }
    }
}

// ---------------------------------------------------------------------------
// A params-bearing cyclic filter: consumes `imu`, produces `nav` = omega * gain.
// ---------------------------------------------------------------------------

#[derive(FromKdlNode)]
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

impl RegisteredSystem for NavFilter {
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
    nav: Input<Nav, BoxBacking, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct LogNoOut {}

impl System for NavLogger {
    type Input = LogIn;
    type Output = Out<LogNoOut, BoxBacking, Notifier, Notifier>;
    const NAME: &'static str = "nav_logger";
}

impl AsyncSystem for NavLogger {
    async fn run(&mut self, input: &mut Self::Input, _o: &mut Self::Output) {
        match input.nav.recv().await {
            Ok(nav) => {
                LOG_COUNT.fetch_add(1, Relaxed);
                LOG_LAST_ANGLE_BITS.store(nav.get().angle.to_bits(), Relaxed);
            }
            Err(_) => input.nav.resync(),
        }
    }
}

impl RegisteredSystem for NavLogger {
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

impl RegisteredSystem for PickyConsumer {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        PickyConsumer
    }
}

// ---------------------------------------------------------------------------
// A loop-closer: consumes `nav`, produces `imu`. With `NavFilter` (imu -> nav)
// this forms a 2-system feedback cycle, used to exercise the KDL `delayed=` edge.
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

impl RegisteredSystem for Closer {
    type Params = ();
    fn new(_params: Self::Params) -> Self {
        Closer
    }
}

/// The app's registry: the visible, auditable list of loadable systems.
fn registry() -> Registry {
    let mut r = Registry::new();
    crate::register_system!(&mut r, ImuDriver => "ImuDriver");
    r.register::<NavFilter, _>("NavFilter");
    r.register::<NavLogger, _>("NavLogger");
    r.register::<PickyConsumer, _>("PickyConsumer");
    r.register::<Closer, _>("Closer");
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
    // The same loop, but the `nav -> closer` back-edge is `delayed=#true`. Also uses
    // a `sim_dt` (free-running simulated clock) — both new KDL surfaces at once.
    let kdl = r#"
coordinator cycle_rate=100.0 sim_dt=0.00833
system "nav"    type="NavFilter" gain=1.0
system "closer" type="Closer"
connect "closer" -> "nav"    frame="imu"
connect "nav"    -> "closer" frame="nav" delayed=#true
"#;
    let mut coord = load(kdl, &registry()).expect("delayed edge breaks the cycle; doc loads");
    coord.run_for(5).await;
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// FromKdlNode derive round-trips scalars, a string, an optional, and a default.
// ---------------------------------------------------------------------------

#[derive(FromKdlNode, Debug, PartialEq)]
struct RoundTrip {
    count: i64,
    rate: f64,
    label: String,
    offset: Option<f64>,
    #[kdl(default = 4)]
    depth: i64,
}

fn first_node(src: &str) -> kdl::KdlNode {
    src.parse::<kdl::KdlDocument>().unwrap().nodes()[0].clone()
}

#[test]
fn from_kdl_node_round_trip_defaults() {
    let src = r#"params count=3 rate=1.5 label="hi""#;
    let node = first_node(src);
    let got = RoundTrip::from_kdl_node(&node, src).unwrap();
    assert_eq!(
        got,
        RoundTrip {
            count: 3,
            rate: 1.5,
            label: "hi".to_string(),
            offset: None,
            depth: 4, // the #[kdl(default = 4)] fallback
        }
    );
}

#[test]
fn from_kdl_node_round_trip_explicit() {
    let src = r#"params count=7 rate=2 label="full" offset=0.25 depth=9"#;
    let node = first_node(src);
    let got = RoundTrip::from_kdl_node(&node, src).unwrap();
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
