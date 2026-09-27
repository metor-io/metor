//! Hand-written `SystemFn` impls over a self-typed state, the shape `#[system]` emits.

use core::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, Notifier, RingBuffer, frame_len};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};
use serde::Deserialize;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::async_system::Stop;
use crate::coordinator::{
    BuildError, CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemTable,
};
use crate::log::Log;
use crate::port::{Input, Output, Sample, ring_capacity};
use crate::tests::utils::{
    Control, Imu, Nav, NoParams, static_async_def, static_def, static_inputs, static_outputs,
};
use crate::{Frame, Record};

/// Doubles the newest sample, stamping it with the cycle time.
struct Doubler;

impl Doubler {
    fn execute(&mut self, imu: &mut Input<Imu>, nav: &mut Output<Nav>, now: Timestamp) {
        if let Ok(Some(imu)) = imu.latest() {
            let estimate = imu.sample * 2.0;
            let _ = nav.write(&Nav {
                timestamp: now,
                estimate,
            });
        }
    }
}

impl Ports for Doubler {
    type State = Self;
    type Params = (Input<Imu>, Output<Nav>, Timestamp);
    const NAME: &'static str = "doubler";
    const NAMES: &'static [&'static str] = &["imu", "nav", "now"];
}

impl SystemFn for Doubler {
    fn call(state: &mut Self, (imu, nav, now): <Self::Params as Param>::Item<'_, NoWake>) {
        state.execute(imu, nav, now);
    }
}

/// Adds two `imu` inputs, with the output declared before them.
struct Summer;

impl Summer {
    fn execute(&mut self, sum: &mut Output<Control>, a: &mut Input<Imu>, b: &mut Input<Imu>) {
        let left = a.latest().ok().flatten().map(|f| f.sample).unwrap_or(0.0);
        let right = b.latest().ok().flatten().map(|f| f.sample).unwrap_or(0.0);
        let _ = sum.write(&Control {
            timestamp: Timestamp(0),
            command: left + right,
        });
    }
}

impl Ports for Summer {
    type State = Self;
    type Params = (Output<Control>, Input<Imu>, Input<Imu>);
    const NAME: &'static str = "summer";
    const NAMES: &'static [&'static str] = &["sum", "a", "b"];
}

impl SystemFn for Summer {
    fn call(state: &mut Self, (sum, a, b): <Self::Params as Param>::Item<'_, NoWake>) {
        state.execute(sum, a, b);
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GainParams {
    /// The constant this system publishes.
    k: f64,
}

/// Publishes a constant scaled by its param.
struct Gain(f64);

impl Gain {
    fn new(p: GainParams) -> Self {
        Self(p.k)
    }
}

/// Publishes the gain.
///
/// One sample per cycle.
#[crate::system]
fn gain(#[state] s: &mut Gain, imu: &mut Output<Imu>) {
    let _ = imu.write(&Imu::new(1, s.0));
}

/// Records the newest frame on its one input for the test to read.
struct Probe<T>(Rc<RefCell<Option<T>>>);

#[crate::system]
fn probe<T: Frame + Clone>(#[state] s: &mut Probe<T>, input: &mut Input<T>) {
    if let Ok(Some(frame)) = input.latest() {
        *s.0.borrow_mut() = Some(frame.clone());
    }
}

fn register_probe<T: Frame + Clone + 'static>(
    table: &mut SystemTable,
    ty: &str,
) -> Rc<RefCell<Option<T>>> {
    let seen = Rc::new(RefCell::new(None));
    let state = seen.clone();
    table
        .register(ty, probe::<T>(), move || Probe(state.clone()))
        .expect("valid records");
    seen
}

fn probing(id: &str, ty: &str, from: PortRef) -> SystemConfig {
    SystemConfig {
        inputs: vec![InputConfig {
            port: "input".into(),
            from: vec![from],
        }],
        ..SystemConfig::new(id, ty)
    }
}

/// Counts cycles with no ports at all.
#[derive(Default)]
struct Counter(u64);

impl Counter {
    fn execute(&mut self) {
        self.0 += 1;
    }
}

impl Ports for Counter {
    type State = Self;
    type Params = ();
    const NAME: &'static str = "counter";
    const NAMES: &'static [&'static str] = &[];
}

impl SystemFn for Counter {
    fn call(state: &mut Self, (): ()) {
        state.execute();
    }
}

/// A system's `Ports` consts, read through its value.
fn ports<P: Ports>(_: &P) -> (&'static str, &'static [&'static str], &'static str) {
    (P::NAME, P::NAMES, P::DOC)
}

/// A `#[system]` fn's ports under the empty context.
fn def_of<S: SystemFn>(_: S) -> SystemDef {
    static_def::<FnSystem<S>>()
}

/// A `#[system]` fn's ports under `cx`.
fn def_in<S: SystemFn>(_: S, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    FnSystem::<S>::def(cx)
}

#[test]
fn test_attribute_type_and_ports() {
    assert_eq!(ports(&gain()).0, "gain");
    assert_eq!(ports(&gain()).1, &["imu"]);
    assert_eq!(ports(&probe::<Imu>()).0, "probe");
    assert_eq!(
        def_of(gain()).outputs,
        vec![
            Output::<Imu>::def(&DefCx::empty(), "imu"),
            Output::<LogEvent>::def(&DefCx::empty(), "log")
        ]
    );
}

#[test]
fn test_port_parameter_order() {
    let def = def_of(Doubler);
    assert_eq!(def.name, "doubler");
    assert_eq!(def.inputs, vec![Input::<Imu>::def(&DefCx::empty(), "imu")]);
    assert_eq!(
        def.outputs,
        vec![
            Output::<Nav>::def(&DefCx::empty(), "nav"),
            Output::<LogEvent>::def(&DefCx::empty(), "log")
        ]
    );
    assert_eq!(static_inputs::<InSet<Summer>, _>().len(), 2);
    assert_eq!(static_outputs::<OutSet<Summer>>()[0].name, "sum");
    assert!(static_inputs::<InSet<Counter>, _>().is_empty());
    assert_eq!(static_outputs::<OutSet<Counter>>().len(), 1);
}

fn gain_table() -> SystemTable {
    let mut table = SystemTable::new();
    table
        .register("gain", gain(), Gain::new)
        .expect("valid records");
    table
        .register("doubler", Doubler, || Doubler)
        .expect("valid records");
    table
        .register("summer", Summer, || Summer)
        .expect("valid records");
    table
        .register("counter", Counter::default(), Counter::default)
        .expect("valid records");
    table
}

#[test]
fn test_constructor_receives_params() {
    let mut table = gain_table();
    let seen = register_probe::<Imu>(&mut table, "imu_probe");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig {
                params: json!({ "k": 4.0 }),
                ..SystemConfig::new("g", "gain")
            },
            probing("p", "imu_probe", PortRef::new("g", "imu")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(0));
    assert_eq!(seen.borrow().as_ref().map(|f| f.sample), Some(4.0));
}

#[test]
fn test_param_error_reports_system() {
    let config = CoordinatorConfig {
        systems: vec![SystemConfig {
            params: json!({ "kp": 4.0 }),
            ..SystemConfig::new("g", "gain")
        }],
        ..Default::default()
    };
    assert!(matches!(
        config.build(&gain_table()).err(),
        Some(crate::BuildError::Params { id, .. }) if id == "g"
    ));
}

#[test]
fn test_default_constructors() {
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig {
                params: json!(null),
                ..SystemConfig::new("d", "doubler")
            },
            SystemConfig::new("c", "counter"),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&gain_table()).expect("valid");
    coordinator.step(Timestamp(0));
    assert_eq!(coordinator.cycle(), 1);
}

#[test]
fn test_separate_input_bindings() {
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig {
                params: json!({ "k": 1.0 }),
                ..SystemConfig::new("one", "gain")
            },
            SystemConfig {
                params: json!({ "k": 10.0 }),
                ..SystemConfig::new("ten", "gain")
            },
            SystemConfig {
                inputs: vec![
                    InputConfig {
                        port: "a".into(),
                        from: vec![PortRef::new("one", "imu")],
                    },
                    InputConfig {
                        port: "b".into(),
                        from: vec![PortRef::new("ten", "imu")],
                    },
                ],
                ..SystemConfig::new("sum", "summer")
            },
        ],
        ..Default::default()
    };
    let mut table = gain_table();
    let seen = register_probe::<Control>(&mut table, "control_probe");
    let mut config = config;
    config
        .systems
        .push(probing("p", "control_probe", PortRef::new("sum", "sum")));
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(0));
    assert_eq!(seen.borrow().as_ref().map(|f| f.command), Some(11.0));
}

#[test]
fn test_pipeline_matches_trait() {
    let mut table = gain_table();
    table
        .register_system("nav_params", |p| {
            p.decode::<NoParams>()
                .map(|_| (FnSystem::<Doubler>::default(), Doubler))
        })
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig {
                params: json!({ "k": 1.5 }),
                ..SystemConfig::new("imu", "gain")
            },
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "imu".into(),
                    from: vec![PortRef::new("imu", "imu")],
                }],
                ..SystemConfig::new("nav", "doubler")
            },
        ],
        ..Default::default()
    };
    let seen = register_probe::<Nav>(&mut table, "nav_probe");
    let mut config = config;
    config
        .systems
        .push(probing("p", "nav_probe", PortRef::new("nav", "nav")));
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(7));
    let nav = seen.borrow().expect("nav published");
    assert_eq!((nav.timestamp, nav.estimate), (Timestamp(7), 3.0));
    assert_eq!(Nav::ID, Output::<Nav>::def(&DefCx::empty(), "nav").id);
}

/// Logs one direct line and `lines` traced lines per cycle.
struct Talker {
    lines: usize,
}

#[crate::system]
fn talker(#[state] s: &mut Talker, log: &mut Log) {
    log.info("direct");
    for i in 0..s.lines {
        tracing::info!(i, "traced");
    }
}

/// Emits one traced line from the trait path, where nothing drains it.
struct Noisy;

impl crate::System for Noisy {
    type State = ();
    type Inputs = ();
    type Outputs = ();

    fn def(cx: &crate::DefCx<'_>) -> Result<crate::SystemDef, crate::DefError> {
        crate::SystemDef::new::<(), ()>("noisy", cx)
    }

    fn execute(&self, _now: Timestamp, _state: &mut (), _inputs: &mut (), _outputs: &mut ()) {
        tracing::info!("between systems");
    }
}

/// Drains every log line it is wired to.
struct LogProbe(Rc<RefCell<Vec<LogEvent>>>);

#[crate::system]
fn log_probe(#[state] s: &mut LogProbe, log: &mut Input<LogEvent>) {
    let mut seen = s.0.borrow_mut();
    seen.extend(log.drain().map(|r| r.expect("decodes")));
}

fn log_config(lines: usize) -> (CoordinatorConfig, SystemTable, Rc<RefCell<Vec<LogEvent>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut table = SystemTable::new();
    table
        .register("talker", talker(), move || Talker { lines })
        .expect("valid records");
    table
        .register_system("noisy", |_| Ok((Noisy, ())))
        .expect("valid records");
    let sink = seen.clone();
    table
        .register("log_probe", log_probe(), move || LogProbe(sink.clone()))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("talker", "talker"),
            SystemConfig::new("noisy", "noisy"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "log".into(),
                    from: vec![PortRef::new("talker", "log"), PortRef::new("probe", "log")],
                }],
                ..SystemConfig::new("probe", "log_probe")
            },
        ],
        ..Default::default()
    };
    (config, table, seen)
}

fn with_layer(f: impl FnOnce()) {
    let subscriber = tracing_subscriber::registry().with(crate::log::layer());
    tracing::subscriber::with_default(subscriber, f);
}

#[test]
fn test_system_log_order() {
    let (config, table, seen) = log_config(1);
    let mut coordinator = config.build(&table).expect("valid");
    with_layer(|| coordinator.step(Timestamp(3)));
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].message, "direct");
    assert_eq!(seen[1].message, "traced");
    assert!(seen.iter().all(|l| l.timestamp == Timestamp(3)));
    assert_eq!(seen[1].target, module_path!());
}

#[test]
fn test_log_between_systems() {
    let (config, table, seen) = log_config(0);
    let mut coordinator = config.build(&table).expect("valid");
    with_layer(|| coordinator.step(Timestamp(0)));
    assert!(seen.borrow().iter().all(|l| l.message != "between systems"));
}

/// Writes `lines` identical padded lines, on its first cycle only.
struct Padder {
    line: String,
    lines: usize,
}

#[crate::system]
fn padder(#[state] s: &mut Padder, log: &mut Log) {
    for _ in 0..core::mem::take(&mut s.lines) {
        log.info(s.line.clone());
    }
}

fn pad_config(
    lines: usize,
    line: &str,
) -> (CoordinatorConfig, SystemTable, Rc<RefCell<Vec<LogEvent>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut table = SystemTable::new();
    let (line, sink) = (line.to_string(), seen.clone());
    table
        .register("padder", padder(), move || Padder {
            line: line.clone(),
            lines,
        })
        .expect("valid records");
    table
        .register("log_probe", log_probe(), move || LogProbe(sink.clone()))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("padder", "padder"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "log".into(),
                    from: vec![PortRef::new("padder", "log")],
                }],
                ..SystemConfig::new("probe", "log_probe")
            },
        ],
        ..Default::default()
    };
    (config, table, seen)
}

/// Lines of `line` one `log` ring holds at the default ring depth.
fn padded_records(line: &str, now: Timestamp) -> usize {
    let event = LogEvent {
        timestamp: now,
        level: LogLevel::Info,
        source: "".into(),
        target: "".into(),
        message: line.to_string().into(),
        span: None,
        fields: Vec::new(),
        file: None,
        line: None,
    };
    let mut buf = vec![0u8; LogEvent::MAX_LEN];
    let len = event.encode(&mut buf).expect("encodes").len();
    let depth = LogEvent::DEPTH * CoordinatorConfig::default().ring_depth;
    ring_capacity(LogEvent::MAX_LEN, depth).expect("valid") / frame_len(len)
}

#[test]
fn test_log_overflow_report() {
    let line = "p".repeat(LogEvent::MAX_LEN / 2);
    let records = padded_records(&line, Timestamp(0));
    let (config, table, seen) = pad_config(records + 3, &line);
    let mut coordinator = config.build(&table).expect("valid");
    // The warning waits for the room the drained ring gives back.
    for cycle in 0..4 {
        coordinator.step(Timestamp(cycle));
    }
    let landed = seen.borrow();
    let (warnings, infos): (Vec<_>, Vec<_>) =
        landed.iter().partition(|l| l.level == LogLevel::Warn);
    assert_eq!(infos.len(), records);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].fields, vec![("dropped".into(), "3".into())]);
}

#[test]
fn test_log_ring_capacity() {
    let line = "p".repeat(LogEvent::MAX_LEN / 2);
    let records = padded_records(&line, Timestamp(0));
    assert!(records >= LogEvent::DEPTH * CoordinatorConfig::default().ring_depth);
    let (config, table, seen) = pad_config(records, &line);
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(0));
    coordinator.step(Timestamp(1));
    let landed = seen.borrow();
    assert_eq!(landed.len(), records);
    assert!(landed.iter().all(|l| l.level == LogLevel::Info));
}

#[test]
fn test_reject_explicit_log_output() {
    #[crate::system]
    fn log_output(log: &mut Output<LogEvent>) {
        let _ = log;
    }

    let mut table = SystemTable::new();
    table
        .register("log_output", log_output(), Default::default)
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![SystemConfig::new("logger", "log_output")],
        ..Default::default()
    };
    assert_eq!(
        config.build(&table).err(),
        Some(BuildError::DuplicateOutput {
            system: "logger".into(),
            port: "log".into(),
        })
    );
}

/// The name of a system's state type.
fn state_of<P: Ports>(_: &P) -> &'static str {
    core::any::type_name::<P::State>()
}

#[test]
fn test_ports_consts() {
    assert_eq!(ports(&gain()), ("gain", &["imu"][..], ports(&gain()).2));
    assert_eq!(state_of(&gain()), core::any::type_name::<Gain>());
    assert_eq!(ports(&relay()).0, "relay");
    assert_eq!(state_of(&relay()), "()");
    assert_eq!(ports(&probe::<Nav>()), ("probe", &["input"][..], ""));
    assert_eq!(
        state_of(&probe::<Nav>()),
        core::any::type_name::<Probe<Nav>>()
    );
    assert_eq!(ports(&relay_cyclic()).1, &["imu", "nav", "log"]);
    assert_eq!(state_of(&relay_cyclic()), "()");
}

/// Publishes one fixed sample per cycle, with no state.
#[crate::system]
fn unit(imu: &mut Output<Imu>) {
    let _ = imu.write(&Imu::new(1, 9.0));
}

#[test]
fn test_stateless_default_registration() {
    let mut table = SystemTable::new();
    table
        .register("unit", unit(), Default::default)
        .expect("valid records");
    let seen = register_probe::<Imu>(&mut table, "imu_probe");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("u", "unit"),
            probing("p", "imu_probe", PortRef::new("u", "imu")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(0));
    assert_eq!(seen.borrow().as_ref().map(|f| f.sample), Some(9.0));
}

#[test]
fn test_direct_call_writes_output() {
    let imu = ring::<Imu>();
    let mut output = Output::<Imu>::new(imu.writer(NoWake).expect("free writer"));
    let mut input = Input::<Imu>::new(vec![imu.view(NoWake).expect("free slot")]);
    gain()(&mut Gain(3.0), &mut output);
    let sample = input.latest().expect("valid").map(|f| f.sample);
    assert_eq!(sample, Some(3.0));
}

#[test]
fn test_generic_system_two_records() {
    let mut table = SystemTable::new();
    let _imu = register_probe::<Imu>(&mut table, "imu_probe");
    let _nav = register_probe::<Nav>(&mut table, "nav_probe");
    let inputs = |ty| table.get(ty).expect("registered").def.inputs.clone();
    assert_eq!(
        inputs("imu_probe"),
        vec![Input::<Imu>::def(&DefCx::empty(), "input")]
    );
    assert_eq!(
        inputs("nav_probe"),
        vec![Input::<Nav>::def(&DefCx::empty(), "input")]
    );
}

#[test]
fn test_execute_doc_comment() {
    assert_eq!(
        ports(&gain()).2,
        "Publishes the gain.\n\nOne sample per cycle."
    );
    assert_eq!(Doubler::DOC, "");
    assert_eq!(ports(&probe::<Imu>()).2, "");
}

#[test]
fn test_constructor_param_schema() {
    let schema = <fn(GainParams) -> Gain as Ctor<Gain, (GainParams,)>>::schema()
        .expect("a params ctor has a schema");
    assert!(
        schema
            .get()
            .contains(r#""description":"The constant this system publishes.""#)
    );
    assert!(schema.get().contains(r#""k""#));
}

fn ring<R: Record + ?Sized>() -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(R::MAX_LEN, 8).expect("valid capacity"),
        max_readers: 2,
    })
}

/// Forwards records as they arrive.
#[crate::system]
async fn relay(imu: &mut Input<Imu, Notifier>, nav: &mut Output<Nav>, log: &mut Log, stop: Stop) {
    log.info("relaying");
    loop {
        let waited = futures_lite::future::or(async { imu.next().await.ok().copied() }, async {
            stop.wait().await;
            None
        });
        let Some(sample) = waited.await else { return };
        let _ = nav.write(&Nav {
            timestamp: sample.timestamp,
            estimate: sample.sample * 2.0,
        });
    }
}

/// Forwards records as they arrive.
#[crate::system]
fn relay_cyclic(imu: &mut Input<Imu>, nav: &mut Output<Nav>, log: &mut Log) {
    let _ = (imu, nav, log);
}

/// An async `#[system]` fn's ports under the empty context.
fn async_def_of<S: AsyncSystemFn>(_: S) -> SystemDef {
    static_async_def::<FnAsyncSystem<S>>()
}

#[test]
fn test_async_and_cyclic_ports_match() {
    let (_, names, doc) = ports(&relay());
    assert_eq!(names, &["imu", "nav", "log"]);
    assert_eq!(doc, "Forwards records as they arrive.");
    let (relay, cyclic) = (async_def_of(relay()), def_of(relay_cyclic()));
    assert_eq!(relay.inputs, cyclic.inputs);
    assert_eq!(relay.outputs, cyclic.outputs);
    assert_eq!(relay.name, "relay");
}

/// A registered async system builds on its own thread and relays through the
/// adapter the cycle thread steps.
#[test]
fn test_async_adapter_relay() {
    use crate::coordinator::MakeCx;
    use crate::thread::Threads;

    let mut table = SystemTable::new();
    table
        .register_async("relay", relay(), Default::default)
        .expect("valid records");
    let entry = table.get("relay").expect("registered");
    assert_eq!(
        entry.def.inputs,
        vec![Input::<Imu>::def(&DefCx::empty(), "imu")]
    );

    let (imu, nav, log) = (ring::<Imu>(), ring::<Nav>(), ring::<LogEvent>());
    let source = Output::<Imu, _>::new(imu.writer(NoWake).expect("free writer"));
    let mut estimates = Input::<Nav>::new(vec![nav.view(NoWake).expect("free slot")]);
    let mut lines = Input::<LogEvent>::new(vec![log.view(NoWake).expect("free slot")]);

    let mut threads = Threads::new();
    let def = entry.def.clone();
    let mut step = (entry.make)(MakeCx {
        id: "relay",
        adapter: None,
        def: &def,
        params: crate::coordinator::Params(&json!(null)),
        inputs: vec![vec![&imu]],
        outputs: vec![&nav, &log],
        threads: &mut threads,
    })
    .expect("no params");

    source.write(&Imu::new(3, 2.0)).expect("ring has room");
    let deadline = std::time::Instant::now() + core::time::Duration::from_secs(5);
    let mut seen = Vec::new();
    let estimate = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the thread never relayed"
        );
        step.execute(Timestamp(1));
        seen.extend(lines.drain().map(|l| l.expect("decodes").message));
        if let Some(nav) = estimates.latest().expect("valid") {
            break nav.estimate;
        }
        std::thread::sleep(core::time::Duration::from_millis(1));
    };
    assert_eq!(estimate, 4.0);
    assert!(seen.contains(&"relaying".into()), "{seen:?}");
}

/// A dynamic parameter takes the ports the config named, wherever it sits.
#[test]
fn test_dynamic_parameter_port_order() {
    use crate::def::{DefCx, Records};
    use crate::port::{DynInputs, DynOutputs};

    #[crate::system]
    fn link(taps: &mut DynInputs, imu: &mut Input<Imu>, sinks: &mut DynOutputs) {
        let _ = (taps, imu, sinks);
    }

    let def = def_of(link());
    assert_eq!(def.inputs, vec![Input::<Imu>::def(&DefCx::empty(), "imu")]);

    let producer = Output::<Nav>::def(&DefCx::empty(), "nav");
    let declared = Output::<Imu>::def(&DefCx::empty(), "imu");
    let edges = [("nav.nav", &producer), ("imu", &declared)];
    let records = Records::of([Output::<Nav>::def(&DefCx::empty(), "nav")].iter());
    let outputs = [crate::coordinator::OutputConfig::record("sink", Nav::NAME)];
    let cx = DefCx {
        inputs: &edges,
        outputs: &outputs,
        records: &records,
        ..DefCx::empty()
    };
    let def = def_in(link(), &cx).expect("a definition");
    let names: Vec<_> = def.inputs.iter().map(|port| port.name.as_ref()).collect();
    assert_eq!(names, vec!["nav.nav", "imu"]);
    let names: Vec<_> = def.outputs.iter().map(|port| port.name.as_ref()).collect();
    assert_eq!(names, vec!["sink", "log"]);
}

/// Two producers on one undeclared port have no one definition to take.
#[test]
fn test_reject_multiple_dynamic_producers() {
    use crate::def::{DefCx, DefError};
    use crate::port::DynInputs;

    #[crate::system]
    fn tap(taps: &mut DynInputs) {
        let _ = taps;
    }

    let first = Output::<Imu>::def(&DefCx::empty(), "imu");
    let second = Output::<Imu>::def(&DefCx::empty(), "imu");
    let edges = [("plant.imu", &first), ("plant.imu", &second)];
    assert_eq!(
        def_in(tap(), &DefCx::of_inputs(&edges)),
        Err(DefError::FanIn {
            port: "plant.imu".into()
        })
    );
}

/// Publishes the cycle count as a sample.
#[crate::system]
fn counting(#[state] tick: &mut i64, imu: &mut Output<Imu>) {
    *tick += 1;
    let _ = imu.write(&Imu::new(*tick, *tick as f64));
}

/// Keeps each cycle's sample for the test.
struct Samples(Rc<RefCell<Vec<Option<f64>>>>);

#[crate::system]
fn sampling(#[state] s: &mut Samples, imu: &Sample<Imu>) {
    s.0.borrow_mut().push(imu.get().map(|imu| imu.sample));
}

/// A `Sample` sees the record its producer wrote earlier in the cycle.
#[test]
fn test_sample_sees_this_cycle() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let state = seen.clone();
    let mut table = SystemTable::new();
    table
        .register("counting", counting(), i64::default)
        .expect("valid records");
    table
        .register("sampling", sampling(), move || Samples(state.clone()))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("count", "counting"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "imu".into(),
                    from: vec![PortRef::new("count", "imu")],
                }],
                ..SystemConfig::new("sample", "sampling")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    assert_eq!(*seen.borrow(), [Some(1.0), Some(2.0)]);
    assert_eq!(
        def_of(sampling()).inputs,
        vec![Input::<Imu>::def(&DefCx::empty(), "imu")]
    );
}

/// Waits for `stop`, holding a sample no step would take.
#[crate::system]
async fn sampling_async(imu: &Sample<Imu>, stop: Stop) {
    let _ = imu;
    stop.wait().await;
}

/// An async `#[system]` fn's definition under `cx`.
fn async_def_in<S: AsyncSystemFn>(_: S, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    FnAsyncSystem::<S>::def(cx)
}

#[test]
fn test_async_sample_refused() {
    let refused = DefError::SampleOnAsync {
        system: "sampling_async".into(),
    };
    assert_eq!(
        async_def_in(sampling_async(), &DefCx::empty()),
        Err(refused)
    );
    let mut table = SystemTable::new();
    assert!(matches!(
        table.register_async("sampling_async", sampling_async(), Default::default),
        Err(BuildError::Def {
            source: DefError::SampleOnAsync { .. },
            ..
        })
    ));
}
