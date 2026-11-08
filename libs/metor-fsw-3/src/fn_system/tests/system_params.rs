//! `#[derive(SystemParams)]` structs: names, binding, and items.

use core::cell::RefCell;
use core::marker::PhantomData;
use core::time::Duration;
use std::rc::Rc;
use std::time::Instant;

use metor_fsw_3_ring::{NoWake, Notifier};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;
use serde_json::json;

use crate::Rng;
use crate::SystemParams;
use crate::async_system::Stop;
use crate::coordinator::{
    BuildError, CoordinatorConfig, InputConfig, MakeCx, Params, StepCx, SystemConfig, SystemTable,
};
use crate::def::{DefCx, DefError};
use crate::io::{Cap, Uart};
use crate::port::{Input, Output, Sample};
use crate::tests::utils::{Control, Imu, Nav, input, output, ring};
use crate::thread::Threads;

use super::{async_def_in, def_in, def_of};

fn names(ports: &[crate::PortDef]) -> Vec<String> {
    ports.iter().map(|port| port.name.to_string()).collect()
}

/// Read only for its port names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Sensors<'a> {
    fss: [&'a mut Input<Imu>; 8],
    gyro: &'a Sample<Imu>,
    mag: &'a mut Input<Imu>,
}

/// Read only for its port names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct WheelIo<'a> {
    speed: &'a mut Input<Imu>,
    cmd: &'a mut Output<Control>,
}

/// Read only for its port names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Actuators<'a> {
    wheels: [WheelIo<'a>; 4],
    mtq: &'a mut Output<Control>,
}

#[crate::system]
fn adcs(now: Timestamp, sensors: Sensors<'_>, act: Actuators<'_>) {
    let _ = (now, sensors, act);
}

#[test]
fn test_ports_follow_the_design_example() {
    let def = def_of(adcs());
    let fss = (0..8).map(|i| format!("fss.{i}"));
    let speeds = (0..4).map(|i| format!("wheels.{i}.speed"));
    let inputs: Vec<_> = fss
        .chain(["gyro".into(), "mag".into()])
        .chain(speeds)
        .collect();
    assert_eq!(names(&def.inputs), inputs);
    let cmds = (0..4).map(|i| format!("wheels.{i}.cmd"));
    let outputs: Vec<_> = cmds.chain(["mtq".into(), "log".into()]).collect();
    assert_eq!(names(&def.outputs), outputs);
}

/// Read only for its port names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Tagged<'a> {
    #[fsw(rename = "rate")]
    gyro: &'a mut Input<Imu>,
    #[fsw(flatten)]
    io: WheelIo<'a>,
    _note: PhantomData<&'a ()>,
    #[fsw(skip = false)]
    _kept: &'a mut Output<Nav>,
}

#[crate::system]
fn tagged(tags: [Tagged<'_>; 2], #[state] s: &mut (), more: Tagged<'_>) {
    let _ = (tags, s, more);
}

#[test]
fn test_rename_flatten_and_skip() {
    let def = def_of(tagged());
    let inputs = [
        "tags.0.rate",
        "tags.0.speed",
        "tags.1.rate",
        "tags.1.speed",
        "rate",
        "speed",
    ];
    assert_eq!(names(&def.inputs), inputs);
    let outputs = [
        "tags.0.cmd",
        "tags.0._kept",
        "tags.1.cmd",
        "tags.1._kept",
        "cmd",
        "_kept",
    ];
    assert_eq!(names(&def.outputs), [&outputs[..], &["log"]].concat());
}

#[crate::system]
fn twice(a: WheelIo<'_>, b: WheelIo<'_>) {
    let _ = (a, b);
}

#[test]
fn test_flattened_arguments_that_share_a_port_fail_to_build() {
    let mut table = SystemTable::new();
    table.register("twice", twice(), || ()).expect("valid");
    let config = CoordinatorConfig {
        systems: vec![SystemConfig::new("t", "twice")],
        ..Default::default()
    };
    let err = config.build(&table).err();
    let shared = matches!(&err, Some(BuildError::DuplicateInput { port, .. }) if port == "speed");
    assert!(shared, "{err:?}");
}

/// Writes its value on `imu` every cycle.
#[crate::system]
fn constant(#[state] value: &mut f64, imu: &mut Output<Imu>, now: Timestamp) {
    let _ = imu.write(&Imu::new(now.0, *value));
}

#[derive(SystemParams)]
struct Pair<'a> {
    fresh: &'a mut Input<Imu>,
    held: &'a Sample<Imu>,
    out: &'a mut Output<Nav>,
}

type Seen = Rc<RefCell<Vec<(Option<f64>, Option<f64>)>>>;

/// Records each pair's newest input and sample, and writes their sum.
#[crate::system]
fn pairs(#[state] seen: &mut Seen, pairs: [Pair<'_>; 2]) {
    for pair in pairs {
        let fresh = pair.fresh.latest().ok().flatten().map(|imu| imu.sample);
        let held = pair.held.get().map(|imu| imu.sample);
        seen.borrow_mut().push((fresh, held));
        let estimate = fresh.unwrap_or(0.0) + held.unwrap_or(0.0);
        let _ = pair.out.write(&Nav {
            timestamp: Timestamp(0),
            estimate,
        });
    }
}

#[test]
fn test_struct_fields_reach_their_ports() {
    let seen = Seen::default();
    let mut table = SystemTable::new();
    table.register("one", constant(), || 1.0).expect("valid");
    table.register("two", constant(), || 2.0).expect("valid");
    let state = seen.clone();
    table
        .register("pairs", pairs(), move || state.clone())
        .expect("valid");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("one", "one"),
            SystemConfig::new("two", "two"),
            SystemConfig {
                inputs: vec![
                    InputConfig::new("pairs.0.fresh", "one", "imu"),
                    InputConfig::new("pairs.1.held", "two", "imu"),
                ],
                ..SystemConfig::new("p", "pairs")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    coordinator.step(Timestamp(1));
    assert_eq!(seen.take(), [(Some(1.0), None), (None, Some(2.0))]);
}

#[derive(SystemParams)]
#[system_params(async)]
struct Feeds<'a> {
    fss: [&'a mut Input<Imu, Notifier>; 2],
    est: &'a mut Output<Nav>,
}

/// Forwards each record on `fss.1` as it arrives.
#[crate::system]
async fn fuse(feeds: Feeds<'_>, stop: Stop) {
    let Feeds { fss: [_, fss], est } = feeds;
    loop {
        let next = async { fss.next().await.ok().copied() };
        let stopped = async {
            stop.wait().await;
            None
        };
        let Some(sample) = futures_lite::future::or(next, stopped).await else {
            return;
        };
        let _ = est.write(&Nav {
            timestamp: sample.timestamp,
            estimate: sample.sample * 2.0,
        });
    }
}

#[test]
fn test_async_struct_ports() {
    let def = async_def_in(fuse(), &DefCx::empty()).expect("a static definition");
    assert_eq!(names(&def.inputs), ["fss.0", "fss.1"]);
    assert_eq!(names(&def.outputs), ["est", "log"]);
}

#[test]
fn test_async_struct_wakes_on_a_record() {
    let mut table = SystemTable::new();
    table
        .register_async("fuse", fuse(), Default::default)
        .expect("valid records");
    let entry = table.get("fuse").expect("registered");
    let (unwired, imu) = (ring::<Imu>(8, 1), ring::<Imu>(8, 1));
    let (nav, log) = (ring::<Nav>(8, 1), ring::<LogEvent>(8, 1));
    let mut source = output::<Imu>(&imu);
    let mut estimates = input::<Nav>(&nav);
    let mut threads = Threads::default();
    let def = entry.def.clone();
    let mut step = (entry.make)(MakeCx {
        id: "fuse",
        adapter: None,
        def: &def,
        params: Params(&json!(null)),
        inputs: vec![vec![&unwired], vec![&imu]],
        outputs: vec![&nav, &log],
        threads: &mut threads,
        io: &mut crate::io::IoState::default(),
        seed: 0,
        namespace: None,
        records: &Default::default(),
    })
    .expect("no params");
    source.write(&Imu::new(3, 2.0)).expect("ring has room");
    let deadline = Instant::now() + Duration::from_secs(5);
    let estimate = loop {
        assert!(Instant::now() < deadline, "the thread never relayed");
        step.execute(Timestamp(1), &mut StepCx::new(&mut Default::default()));
        if let Some(nav) = estimates.latest().expect("valid") {
            break nav.estimate;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(estimate, 4.0);
}

type ImuIn = Input<Imu>;

/// Flattens a leaf the derive cannot see through.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Unnamed<'a> {
    #[fsw(flatten)]
    imu: &'a mut ImuIn,
}

#[crate::system]
fn unnamed(u: Unnamed<'_>) {
    let _ = u;
}

#[test]
fn test_flattened_leaf_is_refused_at_def() {
    let err = def_in(unnamed(), &DefCx::empty()).err();
    assert!(matches!(err, Some(DefError::EmptyName)), "{err:?}");
}

/// Names one param of two.
struct ShortNames;

impl crate::FnSignature for ShortNames {
    type State = ();
    type Wake = NoWake;
    type Params = (Input<Imu>, Output<Nav>);
    const NAME: &'static str = "short";
    const NAMES: &'static [&'static str] = &["imu"];
}

impl crate::SystemFn for ShortNames {
    fn call(_: &mut (), _: <Self::Params as crate::Param<NoWake>>::Item<'_>) {}
}

#[test]
fn test_unnamed_argument_is_refused_at_def() {
    let err = def_in(ShortNames, &DefCx::empty()).err();
    assert!(matches!(err, Some(DefError::EmptyName)), "{err:?}");
}

/// Read only for its params' names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Noise<'a> {
    rng: &'a mut Rng,
}

/// Read only for its params' names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct Port<'a> {
    uart: &'a Cap<Uart>,
}

#[crate::system]
fn two_rngs(a: Noise<'_>, b: Noise<'_>) {
    let _ = (a, b);
}

#[crate::system]
fn two_caps(a: Port<'_>, b: Port<'_>) {
    let _ = (a, b);
}

#[crate::system]
fn named_apart(a: [Noise<'_>; 2], b: [Port<'_>; 2]) {
    let _ = (a, b);
}

#[test]
fn test_flattened_params_of_one_name_are_refused() {
    let err = def_in(two_rngs(), &DefCx::empty()).err();
    let rng = matches!(&err, Some(DefError::DuplicateParam { kind, name }) if kind == "Rng" && name == "rng");
    assert!(rng, "{err:?}");
    let err = def_in(two_caps(), &DefCx::empty()).err();
    let cap = matches!(&err, Some(DefError::DuplicateParam { kind, name }) if kind == "Cap" && name == "uart");
    assert!(cap, "{err:?}");
}

#[test]
fn test_params_in_arrays_are_named_apart() {
    let def = def_in(named_apart(), &DefCx::empty()).expect("distinct names");
    let grants: Vec<_> = def.grants.iter().map(|g| g.param.to_string()).collect();
    assert_eq!(grants, ["b.0.uart", "b.1.uart"]);
}
