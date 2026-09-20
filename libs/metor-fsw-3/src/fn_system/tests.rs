//! Hand-written `SystemFn` impls, the shape `#[system]` emits.

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
use crate::log::Log;
use crate::port::{Input, Output, ring_capacity};
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
    type Params = (Input<Imu>, Output<Nav>, Timestamp);
    const NAME: &'static str = "doubler";
    const NAMES: &'static [&'static str] = &["imu", "nav", "now"];
}

impl SystemFn for Doubler {
    fn call(&mut self, (imu, nav, now): <Self::Params as Param>::Item<'_, NoWake>) {
        self.execute(imu, nav, now);
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
    type Params = (Output<Control>, Input<Imu>, Input<Imu>);
    const NAME: &'static str = "summer";
    const NAMES: &'static [&'static str] = &["sum", "a", "b"];
}

impl SystemFn for Summer {
    fn call(&mut self, (sum, a, b): <Self::Params as Param>::Item<'_, NoWake>) {
        self.execute(sum, a, b);
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

#[crate::system]
impl Gain {
    /// Publishes the gain.
    ///
    /// One sample per cycle.
    fn execute(&mut self, imu: &mut Output<Imu>) {
        let _ = imu.write(&Imu::new(1, self.0));
    }
}

/// Records the newest frame on its one input for the test to read.
struct Probe<T>(Rc<RefCell<Option<T>>>);

#[crate::system]
impl<T: Frame + Clone + 'static> Probe<T> {
    fn execute(&mut self, input: &mut Input<T>) {
        if let Ok(Some(frame)) = input.latest() {
            *self.0.borrow_mut() = Some(frame.clone());
        }
    }
}

fn probe<T: Frame + Clone + 'static>(table: &mut SystemTable, ty: &str) -> Rc<RefCell<Option<T>>> {
    let seen = Rc::new(RefCell::new(None));
    let state = seen.clone();
    table
        .register(ty, move || Probe(state.clone()))
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
    type Params = ();
    const NAME: &'static str = "counter";
    const NAMES: &'static [&'static str] = &[];
}

impl SystemFn for Counter {
    fn call(&mut self, (): ()) {
        self.execute();
    }
}

#[test]
fn the_attribute_names_the_type_and_its_ports() {
    assert_eq!(Gain::NAME, "gain");
    assert_eq!(Gain::NAMES, &["imu"]);
    assert_eq!(<Probe<Imu>>::NAME, "probe");
    assert_eq!(
        static_def::<FnSystem<Gain>>().outputs,
        vec![Output::<Imu>::def("imu"), Output::<LogEvent>::def("log")]
    );
}

#[test]
fn defs_follow_parameter_order_and_skip_timestamp() {
    let def = static_def::<FnSystem<Doubler>>();
    assert_eq!(def.name, "doubler");
    assert_eq!(def.inputs, vec![Input::<Imu>::def("imu")]);
    assert_eq!(
        def.outputs,
        vec![Output::<Nav>::def("nav"), Output::<LogEvent>::def("log")]
    );
    assert_eq!(static_inputs::<InSet<Summer>, _>().len(), 2);
    assert_eq!(static_outputs::<OutSet<Summer>>()[0].name, "sum");
    assert!(static_inputs::<InSet<Counter>, _>().is_empty());
    assert_eq!(static_outputs::<OutSet<Counter>>().len(), 1);
}

fn gain_table() -> SystemTable {
    let mut table = SystemTable::new();
    table.register("gain", Gain::new).expect("valid records");
    table
        .register("doubler", || Doubler)
        .expect("valid records");
    table.register("summer", || Summer).expect("valid records");
    table
        .register("counter", Counter::default)
        .expect("valid records");
    table
}

#[test]
fn a_params_ctor_receives_the_decoded_params() {
    let mut table = gain_table();
    let seen = probe::<Imu>(&mut table, "imu_probe");
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
fn a_bad_param_fails_the_build_by_system_id() {
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
fn plain_ctors_ignore_params_and_unit_structs_use_default() {
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
fn two_inputs_of_one_record_bind_to_two_rings() {
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
    let seen = probe::<Control>(&mut table, "control_probe");
    let mut config = config;
    config
        .systems
        .push(probing("p", "control_probe", PortRef::new("sum", "sum")));
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(0));
    assert_eq!(seen.borrow().as_ref().map(|f| f.command), Some(11.0));
}

#[test]
fn a_fn_pipeline_matches_the_trait_pipeline() {
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
    let seen = probe::<Nav>(&mut table, "nav_probe");
    let mut config = config;
    config
        .systems
        .push(probing("p", "nav_probe", PortRef::new("nav", "nav")));
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(7));
    let nav = seen.borrow().expect("nav published");
    assert_eq!((nav.timestamp, nav.estimate), (Timestamp(7), 3.0));
    assert_eq!(Nav::ID, Output::<Nav>::def("nav").id);
}

/// Logs one direct line and `lines` traced lines per cycle.
struct Talker {
    lines: usize,
}

#[crate::system]
impl Talker {
    fn execute(&mut self, log: &mut Log) {
        log.info("direct");
        for i in 0..self.lines {
            tracing::info!(i, "traced");
        }
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
impl LogProbe {
    fn execute(&mut self, log: &mut Input<LogEvent>) {
        let mut seen = self.0.borrow_mut();
        seen.extend(log.drain().map(|r| r.expect("decodes")));
    }
}

fn log_config(lines: usize) -> (CoordinatorConfig, SystemTable, Rc<RefCell<Vec<LogEvent>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut table = SystemTable::new();
    table
        .register("talker", move || Talker { lines })
        .expect("valid records");
    table
        .register_system("noisy", |_| Ok((Noisy, ())))
        .expect("valid records");
    let sink = seen.clone();
    table
        .register("log_probe", move || LogProbe(sink.clone()))
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
fn direct_and_traced_lines_land_on_the_system_log_in_order() {
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
fn a_line_between_systems_reaches_no_ring() {
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
impl Padder {
    fn execute(&mut self, log: &mut Log) {
        for _ in 0..core::mem::take(&mut self.lines) {
            log.info(self.line.clone());
        }
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
        .register("padder", move || Padder {
            line: line.clone(),
            lines,
        })
        .expect("valid records");
    table
        .register("log_probe", move || LogProbe(sink.clone()))
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
fn lines_past_the_rings_capacity_are_dropped_and_reported_once() {
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
fn a_log_ring_holds_depth_times_ring_depth_full_length_lines() {
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
fn an_explicit_log_output_cannot_hide_the_implicit_output() {
    struct LogOutput;

    #[crate::system]
    impl LogOutput {
        fn execute(&mut self, log: &mut Output<LogEvent>) {
            let _ = log;
        }
    }

    let mut table = SystemTable::new();
    table
        .register("log_output", || LogOutput)
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

#[test]
fn the_doc_comment_on_execute_becomes_doc() {
    assert_eq!(Gain::DOC, "Publishes the gain.\n\nOne sample per cycle.");
    assert_eq!(Doubler::DOC, "");
    assert_eq!(<Probe<Imu> as Ports>::DOC, "");
}

#[test]
fn a_params_ctor_carries_its_schema() {
    let schema = <fn(GainParams) -> Gain as Ctor<Gain, (GainParams,)>>::schema()
        .expect("a params ctor has a schema");
    assert!(
        schema
            .get()
            .contains(r#""description":"The constant this system publishes.""#)
    );
    assert!(schema.get().contains(r#""k""#));
}

#[test]
fn a_unit_ctor_has_no_schema() {
    assert!(<fn() -> Summer as Ctor<Summer, ()>>::schema().is_none());
}

/// Doubles every sample as it arrives, until stop.
struct Relay;

#[crate::system]
impl Relay {
    /// Forwards records as they arrive.
    async fn run(
        &mut self,
        imu: &mut Input<Imu, Notifier>,
        nav: &mut Output<Nav>,
        log: &mut Log,
        stop: Stop,
    ) {
        log.info("relaying");
        loop {
            let waited =
                futures_lite::future::or(async { imu.next().await.ok().copied() }, async {
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
}

/// The cyclic twin of [`Relay`], to compare definitions.
struct RelayCyclic;

#[crate::system]
impl RelayCyclic {
    /// Forwards records as they arrive.
    fn execute(&mut self, imu: &mut Input<Imu>, nav: &mut Output<Nav>, log: &mut Log) {
        let _ = (imu, nav, log);
    }
}

#[test]
fn an_async_block_declares_the_same_ports_as_its_cyclic_twin() {
    assert_eq!(Relay::NAMES, &["imu", "nav", "log"]);
    assert_eq!(Relay::DOC, "Forwards records as they arrive.");
    let (relay, cyclic) = (
        static_async_def::<FnAsyncSystem<Relay>>(),
        static_def::<FnSystem<RelayCyclic>>(),
    );
    assert_eq!(relay.inputs, cyclic.inputs);
    assert_eq!(relay.outputs, cyclic.outputs);
    assert_eq!(relay.name, "relay");
}

/// A registered async system builds on its own thread and relays through the
/// adapter the cycle thread steps.
#[test]
fn a_registered_async_system_relays_through_its_adapter() {
    use crate::coordinator::MakeCx;
    use crate::thread::{DEFAULT_THREAD, Threads};
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};

    fn ring<R: Record + ?Sized>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(R::MAX_LEN, 8).expect("valid capacity"),
            max_readers: 2,
        })
    }

    let mut table = SystemTable::new();
    table
        .register_async("relay", || Relay)
        .expect("valid records");
    let entry = table.get("relay").expect("registered");
    assert_eq!(entry.def.inputs, vec![Input::<Imu>::def("imu")]);

    let (imu, nav, log) = (ring::<Imu>(), ring::<Nav>(), ring::<LogEvent>());
    let mut source = Output::<Imu, _>::try_new(imu.writer(NoWake).expect("free writer"))
        .expect("supported alignment");
    let mut estimates = Input::<Nav>::try_new(vec![nav.view(NoWake).expect("free slot")])
        .expect("supported alignment");
    let mut lines = Input::<LogEvent>::try_new(vec![log.view(NoWake).expect("free slot")])
        .expect("supported alignment");

    let mut threads = Threads::new();
    let def = entry.def.clone();
    let mut step = (entry.make)(MakeCx {
        id: "relay",
        thread: DEFAULT_THREAD,
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
fn a_dynamic_parameter_takes_the_config_ports_around_a_declared_one() {
    use crate::def::{DefCx, Records};
    use crate::port::{DynInputs, DynOutputs};

    struct Link;

    #[crate::system]
    impl Link {
        fn execute(&mut self, taps: &mut DynInputs, imu: &mut Input<Imu>, sinks: &mut DynOutputs) {
            let _ = (taps, imu, sinks);
        }
    }

    let def = static_def::<FnSystem<Link>>();
    assert_eq!(def.inputs, vec![Input::<Imu>::def("imu")]);

    let producer = Output::<Nav>::def("nav");
    let declared = Output::<Imu>::def("imu");
    let edges = [("nav.nav", &producer), ("imu", &declared)];
    let records = Records::of([Output::<Nav>::def("nav")].iter());
    let outputs = [crate::coordinator::OutputConfig {
        port: "sink".into(),
        record: Nav::NAME.into(),
    }];
    let cx = DefCx {
        inputs: &edges,
        outputs: &outputs,
        records: &records,
    };
    let def = FnSystem::<Link>::def(&cx).expect("a definition");
    let names: Vec<_> = def.inputs.iter().map(|port| port.name.as_ref()).collect();
    assert_eq!(names, vec!["nav.nav", "imu"]);
    let names: Vec<_> = def.outputs.iter().map(|port| port.name.as_ref()).collect();
    assert_eq!(names, vec!["sink", "log"]);
}

/// Two producers on one undeclared port have no one definition to take.
#[test]
fn a_dynamic_input_refuses_a_second_producer() {
    use crate::def::{DefCx, DefError};
    use crate::port::DynInputs;

    struct Tap;

    #[crate::system]
    impl Tap {
        fn execute(&mut self, taps: &mut DynInputs) {
            let _ = taps;
        }
    }

    let first = Output::<Imu>::def("imu");
    let second = Output::<Imu>::def("imu");
    let edges = [("plant.imu", &first), ("plant.imu", &second)];
    assert_eq!(
        FnSystem::<Tap>::def(&DefCx::of_inputs(&edges)),
        Err(DefError::FanIn {
            port: "plant.imu".into()
        })
    );
}
