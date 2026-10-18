//! Hand-written `SystemFn` impls over a self-typed state, the shape `#[system]` emits.

use core::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3_ring::{NoWake, Notifier, frame_len};
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
use crate::log::{Log, LogEventExt};
use crate::port::{Input, Output, Sample, ring_capacity};
use crate::tests::utils::{Control, Imu, Nav, NoParams, input, output, pair, ring};
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

impl FnSignature for Doubler {
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

impl FnSignature for Summer {
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

impl FnSignature for Counter {
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

/// A `#[system]` fn's ports under `cx`.
fn def_in<S: SystemFn>(_: S, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    <FnSystem<S> as System>::def(cx)
}

/// A `#[system]` fn's ports under the empty context.
fn def_of<S: SystemFn>(system: S) -> SystemDef {
    def_in(system, &DefCx::empty()).expect("a static definition")
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
    let summer = def_of(Summer);
    assert_eq!(summer.inputs.len(), 2);
    assert_eq!(summer.outputs[0].name, "sum");
    let counter = def_of(Counter::default());
    assert!(counter.inputs.is_empty());
    assert_eq!(counter.outputs.len(), 1);
}

fn gain_table() -> SystemTable {
    let mut table = SystemTable::new();
    let mut register = || -> Result<(), BuildError> {
        table.register("gain", gain(), Gain::new)?;
        table.register("doubler", Doubler, || Doubler)?;
        table.register("summer", Summer, || Summer)?;
        table.register("counter", Counter::default(), Counter::default)?;
        Ok(())
    };
    register().expect("valid records");
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
                    InputConfig::new("a", "one", "imu"),
                    InputConfig::new("b", "ten", "imu"),
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
                inputs: vec![InputConfig::new("imu", "imu", "imu")],
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
        crate::SystemDef::new::<(), (), NoWake>("noisy", cx)
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
    let subscriber = tracing_subscriber::registry().with(crate::log::LogLayer);
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

/// Logs once through each of two `&mut Log` parameters.
#[crate::system]
fn two_logs(first: &mut Log, second: &mut Log) {
    first.info("first");
    second.info("second");
}

#[test]
fn test_two_log_params() {
    let (mut config, mut table, seen) = log_config(0);
    table
        .register("two_logs", two_logs(), Default::default)
        .expect("valid records");
    config.systems[0] = SystemConfig::new("talker", "two_logs");
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    let messages: Vec<_> = seen.borrow().iter().map(|l| l.message.clone()).collect();
    assert_eq!(messages, ["first", "second", "first", "second"]);
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
                inputs: vec![InputConfig::new("log", "padder", "log")],
                ..SystemConfig::new("probe", "log_probe")
            },
        ],
        ..Default::default()
    };
    (config, table, seen)
}

/// Lines of `line` one `log` ring holds at the default ring depth.
fn padded_records(line: &str, now: Timestamp) -> usize {
    let event = LogEvent::line(now, LogLevel::Info, line.to_string(), Vec::new());
    let mut buf = vec![0u8; LogEvent::MAX_LEN];
    let len = event.encode(&mut buf).expect("encodes").len();
    let depth = LogEvent::DEPTH * CoordinatorConfig::default().ring_depth.get();
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
    assert!(records >= LogEvent::DEPTH * CoordinatorConfig::default().ring_depth.get());
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
    let (_imu, mut output, mut input) = pair::<Imu>(8);
    gain()(&mut Gain(3.0), &mut output);
    let sample = input.latest().expect("valid").map(|f| f.sample);
    assert_eq!(sample, Some(3.0));
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

/// An async `#[system]` fn's definition under `cx`.
fn async_def_in<S: AsyncSystemFn>(_: S, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    <FnSystem<S> as AsyncSystem>::def(cx)
}

#[test]
fn test_async_and_cyclic_ports_match() {
    let relay = async_def_in(relay(), &DefCx::empty()).expect("a static definition");
    let cyclic = def_of(relay_cyclic());
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

    let (imu, nav, log) = (ring::<Imu>(8, 1), ring::<Nav>(8, 1), ring::<LogEvent>(8, 1));
    let mut source = output::<Imu>(&imu);
    let mut estimates = input::<Nav>(&nav);
    let mut lines = input::<LogEvent>(&log);

    let mut threads = Threads::default();
    let def = entry.def.clone();
    let mut step = (entry.make)(MakeCx {
        id: "relay",
        adapter: None,
        def: &def,
        params: crate::coordinator::Params(&json!(null)),
        inputs: vec![vec![&imu]],
        outputs: vec![&nav, &log],
        threads: &mut threads,
        seed: Some(0),
        namespace: None,
        records: &Default::default(),
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
    use crate::def::Records;
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
    let records = Records::from_port_defs([Output::<Nav>::def(&DefCx::empty(), "nav")].iter());
    let outputs = [crate::coordinator::OutputConfig::with_record(
        "sink",
        Nav::NAME,
    )];
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
                inputs: vec![InputConfig::new("imu", "count", "imu")],
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
        Err(BuildError::TypeDef {
            source: DefError::SampleOnAsync { .. },
            ..
        })
    ));
}

/// Writes `a` samples on `a` and `b` on `b` each cycle.
struct Flood {
    a: usize,
    b: usize,
}

#[crate::system]
fn flood(#[state] s: &mut Flood, a: &mut Output<Imu>, b: &mut Output<Imu>) {
    for _ in 0..s.a {
        let _ = a.write(&Imu::new(0, 0.0));
    }
    for _ in 0..s.b {
        let _ = b.write(&Imu::new(0, 0.0));
    }
}

/// A ring two `Imu` records fill, which nothing drains.
fn two_record_ring() -> metor_fsw_3_ring::RingBuffer {
    metor_fsw_3_ring::RingBuffer::create_in_memory(metor_fsw_3_ring::Config {
        capacity: 64,
        max_readers: 1,
    })
    .expect("a valid ring config")
}

/// The log lines `flood` writes over `cycles` cycles into two-record rings.
fn flooded(a: usize, b: usize, cycles: i64) -> Vec<LogEvent> {
    use crate::coordinator::MakeCx;
    use crate::thread::Threads;

    let mut table = SystemTable::new();
    table
        .register("flood", flood(), move || Flood { a, b })
        .expect("valid records");
    let entry = table.get("flood").expect("registered");
    let (ring_a, ring_b, log) = (two_record_ring(), two_record_ring(), ring::<LogEvent>(8, 1));
    let _readers = (input::<Imu>(&ring_a), input::<Imu>(&ring_b));
    let mut lines = input::<LogEvent>(&log);
    let mut threads = Threads::default();
    let mut step = (entry.make)(MakeCx {
        id: "flood",
        adapter: None,
        def: &entry.def.clone(),
        params: crate::coordinator::Params(&json!(null)),
        inputs: Vec::new(),
        outputs: vec![&ring_a, &ring_b, &log],
        threads: &mut threads,
        seed: Some(0),
        namespace: None,
        records: &Default::default(),
    })
    .expect("no params");
    let mut seen = Vec::new();
    for cycle in 1..=cycles {
        step.execute(Timestamp(cycle));
        seen.extend(lines.drain().map(|line| line.expect("decodes")));
    }
    seen
}

fn is_publish_dropped(line: &LogEvent) -> bool {
    line.level == LogLevel::Error && line.fields == [("kind".into(), "publish_dropped".into())]
}

#[test]
fn test_full_ring_faults_each_cycle() {
    let lines = flooded(3, 0, 2);
    assert!(lines.iter().all(is_publish_dropped), "{lines:?}");
    let seen: Vec<_> = lines
        .iter()
        .map(|line| (line.timestamp, line.message.as_ref()))
        .collect();
    assert_eq!(
        seen,
        [
            (Timestamp(1), "1 writes dropped"),
            (Timestamp(2), "3 writes dropped")
        ]
    );
}

#[test]
fn test_no_failed_write_logs_nothing() {
    assert!(flooded(1, 1, 2).is_empty());
}

#[test]
fn test_failed_writes_sum_across_outputs() {
    let lines = flooded(3, 4, 1);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(is_publish_dropped(&lines[0]));
    assert_eq!(lines[0].message, "3 writes dropped");
}

/// Two declared outputs, for binding by hand.
struct TwoOutputs;

impl FnSignature for TwoOutputs {
    type State = ();
    type Params = (Output<Imu>, Output<Imu>);
    const NAME: &'static str = "two_outputs";
    const NAMES: &'static [&'static str] = &["a", "b"];
}

#[test]
fn test_outputs_share_one_counter() {
    use crate::system::{OutputBinding, Ports};

    let rings = [ring::<Imu>(8, 1), ring::<Imu>(8, 1), ring::<LogEvent>(8, 1)];
    let defs = OutSet::<TwoOutputs>::defs(&DefCx::empty()).expect("static ports");
    let bindings = defs
        .into_iter()
        .zip(&rings)
        .map(|(def, ring)| OutputBinding {
            def,
            writer: ring.writer(NoWake).expect("free writer"),
        })
        .collect();
    let set = OutSet::<TwoOutputs>::bind(bindings);
    assert_eq!(std::sync::Arc::strong_count(&set.dropped), 3);
}

type Draws = Rc<RefCell<Vec<(String, u64)>>>;

#[derive(Deserialize, schemars::JsonSchema)]
struct Tagged {
    tag: String,
}

struct Drawer {
    tag: String,
    seen: Draws,
}

/// Records one draw a cycle, under its tag.
#[crate::system]
fn drawer(#[state] s: &mut Drawer, rng: &mut crate::Rng) {
    s.seen.borrow_mut().push((s.tag.clone(), rng.next_u64()));
}

#[crate::system]
async fn async_drawer(rng: &mut crate::Rng, stop: Stop) {
    let _ = (rng.next_u64(), stop);
}

fn drawer_table(seen: &Draws) -> SystemTable {
    let mut table = SystemTable::new();
    let seen = seen.clone();
    let make = move |p: Tagged| Drawer {
        tag: p.tag,
        seen: seen.clone(),
    };
    table.register("drawer", drawer(), make).expect("valid");
    table
}

fn drawers(seed: u64, namespace: Option<&str>, ids: &[&str], cycles: i64) -> Vec<(String, u64)> {
    let seen = Draws::default();
    let systems = ids.iter().map(|id| SystemConfig {
        params: json!({ "tag": id }),
        ..SystemConfig::new(*id, "drawer")
    });
    let config = CoordinatorConfig {
        systems: systems.collect(),
        seed,
        namespace: namespace.map(Into::into),
        ..Default::default()
    };
    let mut coordinator = config.build(&drawer_table(&seen)).expect("valid");
    (0..cycles).for_each(|t| coordinator.step(Timestamp(t)));
    seen.take()
}

fn first(draws: &[(String, u64)], tag: &str) -> Vec<u64> {
    let mine = draws.iter().filter(|(t, _)| t == tag);
    mine.map(|(_, d)| *d).collect()
}

#[test]
fn test_rng_draws_from_the_target_seed_and_name() {
    let draws = drawers(7, None, &["a"], 2);
    let mut want = crate::Rng::seeded(7, "a");
    assert_eq!(first(&draws, "a"), [want.next_u64(), want.next_u64()]);
    let named = drawers(7, Some("t"), &["a"], 1);
    assert_eq!(
        first(&named, "a"),
        [crate::Rng::seeded(7, "t.a").next_u64()]
    );
    assert_eq!(drawers(7, None, &["a"], 3), drawers(7, None, &["a"], 3));
}

#[test]
fn test_rng_streams_are_per_instance() {
    let two = drawers(7, None, &["a", "b"], 3);
    assert_ne!(first(&two, "a"), first(&two, "b"));
    let three = drawers(7, None, &["a", "b", "c"], 3);
    assert_eq!(
        first(&two, "a"),
        first(&three, "a"),
        "a new system leaves a alone"
    );
    assert_eq!(first(&two, "b"), first(&three, "b"));
    assert_ne!(first(&two, "a"), first(&drawers(8, None, &["a"], 3), "a"));
}

#[test]
fn test_rng_without_a_seed_fails_the_make() {
    let seen = Draws::default();
    let table = drawer_table(&seen);
    let entry = table.get("drawer").expect("registered");
    let def = entry.def.clone();
    let log = ring::<LogEvent>(4, 1);
    let mut threads = crate::thread::Threads::default();
    let made = (entry.make)(crate::coordinator::MakeCx {
        id: "d",
        adapter: None,
        def: &def,
        params: crate::coordinator::Params(&json!({ "tag": "d" })),
        inputs: Vec::new(),
        outputs: vec![&log],
        threads: &mut threads,
        seed: None,
        namespace: None,
        records: &Default::default(),
    });
    let err = made.err();
    assert!(
        matches!(&err, Some(crate::coordinator::ParamError::GuestRng { system }) if system == "d"),
        "{err:?}"
    );
}

#[test]
fn test_rng_on_an_async_system_is_refused() {
    let mut table = SystemTable::new();
    let err = table.register_async("d", async_drawer(), || ()).err();
    let refused = |e: &BuildError| {
        matches!(
            e,
            BuildError::TypeDef {
                source: DefError::RngOnAsync { .. },
                ..
            }
        )
    };
    assert!(err.as_ref().is_some_and(refused), "{err:?}");
}

#[test]
fn test_seed_defaults_to_zero() {
    let mut json = serde_json::to_value(CoordinatorConfig::default()).expect("encodes");
    json.as_object_mut().expect("an object").remove("seed");
    let decoded: CoordinatorConfig = serde_json::from_value(json).expect("decodes");
    assert_eq!(decoded.seed, 0);
    let seeded = CoordinatorConfig {
        seed: 7,
        ..Default::default()
    };
    let json = serde_json::to_value(&seeded).expect("encodes");
    assert_eq!(json["seed"], 7);
}

type Found = Rc<RefCell<Vec<bool>>>;

/// Records whether the host knows the `imu` and `nav` records.
#[crate::system]
fn lookup(#[state] found: &mut Found, records: &crate::Records) {
    let known = |name| records.port_def("x", name).is_ok();
    found
        .borrow_mut()
        .extend([known(Imu::NAME), known(Nav::NAME)]);
}

/// Writes one `nav` whose estimate says whether the host knows `imu`.
#[crate::system]
async fn async_lookup(records: &crate::Records, nav: &mut Output<Nav>, stop: Stop) {
    let known = records.port_def("x", Imu::NAME).is_ok();
    let _ = nav.write(&Nav {
        timestamp: Timestamp(1),
        estimate: f64::from(u8::from(known)),
    });
    stop.wait().await;
}

#[test]
fn test_records_param_reads_the_host_table() {
    let found = Found::default();
    let mut table = SystemTable::new();
    table
        .register("source", crate::tests::utils::imu_source(), i64::default)
        .expect("valid");
    let seen = found.clone();
    table
        .register("lookup", lookup(), move || seen.clone())
        .expect("valid");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig::new("lookup", "lookup"),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(1));
    assert_eq!(
        *found.borrow(),
        [true, false],
        "imu is registered, nav is not"
    );
}

#[test]
fn test_async_records_param_reads_the_host_table() {
    use crate::coordinator::MakeCx;
    use crate::thread::Threads;

    let mut table = SystemTable::new();
    table
        .register("source", crate::tests::utils::imu_source(), i64::default)
        .expect("valid");
    table
        .register_async("lookup", async_lookup(), Default::default)
        .expect("valid");
    let records = std::sync::Arc::new(table.records());
    let entry = table.get("lookup").expect("registered");
    let (nav, log) = (ring::<Nav>(8, 1), ring::<LogEvent>(8, 1));
    let mut estimates = input::<Nav>(&nav);
    let mut threads = Threads::default();
    let def = entry.def.clone();
    let mut step = (entry.make)(MakeCx {
        id: "lookup",
        adapter: None,
        def: &def,
        params: crate::coordinator::Params(&json!(null)),
        inputs: Vec::new(),
        outputs: vec![&nav, &log],
        threads: &mut threads,
        seed: Some(0),
        namespace: None,
        records: &records,
    })
    .expect("no params");
    let deadline = std::time::Instant::now() + core::time::Duration::from_secs(5);
    let estimate = loop {
        assert!(std::time::Instant::now() < deadline, "no lookup arrived");
        step.execute(Timestamp(1));
        if let Some(nav) = estimates.latest().expect("valid") {
            break nav.estimate;
        }
        std::thread::sleep(core::time::Duration::from_millis(1));
    };
    assert_eq!(estimate, 1.0);
}
