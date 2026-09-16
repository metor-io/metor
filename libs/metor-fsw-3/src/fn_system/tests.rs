//! Hand-written `SystemFn` impls, the shape `#[system]` emits.

use core::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};
use serde::Deserialize;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::coordinator::{CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemTable};
use crate::log::Log;
use crate::port::{Input, Output};
use crate::tests::utils::{Control, Imu, Nav, NoParams};
use crate::{Frame, Record, SystemInputs, SystemOutputs};

/// Doubles the newest sample, stamping it with the cycle time.
struct Doubler;

impl Doubler {
    fn execute(&mut self, imu: &mut Input<Imu>, nav: &mut Output<Nav>, now: Timestamp) {
        if let Ok(Some(imu)) = imu.latest() {
            let estimate = imu.sample * 2.0;
            drop(imu);
            let _ = nav.write(&Nav {
                timestamp: now,
                estimate,
            });
        }
    }
}

impl SystemFn for Doubler {
    type Params = (Input<Imu>, Output<Nav>, Timestamp);
    const NAME: &'static str = "doubler";
    const NAMES: &'static [&'static str] = &["imu", "nav", "now"];

    fn call(&mut self, (imu, nav, now): <Self::Params as Param>::Item<'_>) {
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

impl SystemFn for Summer {
    type Params = (Output<Control>, Input<Imu>, Input<Imu>);
    const NAME: &'static str = "summer";
    const NAMES: &'static [&'static str] = &["sum", "a", "b"];

    fn call(&mut self, (sum, a, b): <Self::Params as Param>::Item<'_>) {
        self.execute(sum, a, b);
    }
}

#[derive(Deserialize)]
struct GainParams {
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
    table.register(ty, move || Probe(state.clone()));
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

impl SystemFn for Counter {
    type Params = ();
    const NAME: &'static str = "counter";
    const NAMES: &'static [&'static str] = &[];

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
        FnSystem::<Gain>::def().outputs,
        vec![Output::<Imu>::def("imu"), Output::<LogEvent>::def("log")]
    );
}

#[test]
fn defs_follow_parameter_order_and_skip_timestamp() {
    let def = FnSystem::<Doubler>::def();
    assert_eq!(def.name, "doubler");
    assert_eq!(def.inputs, vec![Input::<Imu>::def("imu")]);
    assert_eq!(
        def.outputs,
        vec![Output::<Nav>::def("nav"), Output::<LogEvent>::def("log")]
    );
    assert_eq!(InSet::<Summer>::defs().len(), 2);
    assert_eq!(OutSet::<Summer>::defs()[0].name, "sum");
    assert!(InSet::<Counter>::defs().is_empty());
    assert_eq!(OutSet::<Counter>::defs().len(), 1);
}

fn gain_table() -> SystemTable {
    let mut table = SystemTable::new();
    table.register("gain", Gain::new);
    table.register("doubler", || Doubler);
    table.register("summer", || Summer);
    table.register("counter", Counter::default);
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
    table.register_system("nav_params", |p| {
        p.decode::<NoParams>()
            .map(|_| (FnSystem::<Doubler>::default(), Doubler))
    });
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

    fn def() -> crate::SystemDef {
        crate::SystemDef::new::<(), ()>("noisy")
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
    table.register("talker", move || Talker { lines });
    table.register_system("noisy", |_| Ok((Noisy, ())));
    let sink = seen.clone();
    table.register("log_probe", move || LogProbe(sink.clone()));
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

#[test]
fn the_sixty_fifth_line_is_dropped_and_reported_once() {
    let (config, table, seen) = log_config(crate::log::MAX_LINES + 1);
    let mut coordinator = config.build(&table).expect("valid");
    with_layer(|| coordinator.step(Timestamp(0)));
    let seen = seen.borrow();
    let traced = seen.iter().filter(|l| l.message == "traced").count();
    assert_eq!(traced, crate::log::MAX_LINES);
    let warnings: Vec<_> = seen.iter().filter(|l| l.level == LogLevel::Warn).collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].fields,
        vec![("dropped".to_string(), "1".to_string())]
    );
}

#[test]
fn a_log_ring_holds_depth_times_ring_depth() {
    // 63 traced lines plus the direct line fill 8 * 8 records; nothing is lost.
    let (config, table, seen) = log_config(63);
    let mut coordinator = config.build(&table).expect("valid");
    with_layer(|| coordinator.step(Timestamp(0)));
    assert_eq!(seen.borrow().len(), 64);
}
