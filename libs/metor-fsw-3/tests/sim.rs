//! A GPS driver on a sim resource, served by a model through the io steps.

#[path = "fixtures/echo-pack/src/gps.rs"]
#[allow(dead_code)]
mod gps;

use std::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3::coordinator::{
    Adapter, ChannelConfig, CoordinatorConfig, EndReason, InputConfig, RunState, SystemConfig,
    SystemTable,
};
use metor_fsw_3::io::{Cap, ResourceStatus, Uart};
use metor_fsw_3::metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
use metor_fsw_3::{ChannelState, ChannelStatus, Coordinator, Input, Output, Timestamp, system};

use gps::{Gps, GpsState, UartScript, gps, uart_script};

type Log<T> = Rc<RefCell<Vec<(i64, T)>>>;

/// Keeps the cycle of each fix.
#[system]
fn fixes(#[state] seen: &mut Log<()>, now: Timestamp, fix: &mut Input<Gps>) {
    let n = fix.drain().flatten().count();
    seen.borrow_mut().extend((0..n).map(|_| (now.0, ())));
}

#[system]
fn statuses(
    #[state] seen: &mut Log<ResourceStatus>,
    now: Timestamp,
    st: &mut Input<ResourceStatus>,
) {
    let got = st.drain().flatten().map(|s| (now.0, *s));
    seen.borrow_mut().extend(got);
}

#[system]
fn states(#[state] seen: &mut Log<u32>, now: Timestamp, st: &mut Input<ChannelStatus>) {
    let got = st.drain().flatten().map(|s| (now.0, s.state));
    seen.borrow_mut().extend(got);
}

/// Counts the cycles it runs.
#[system]
fn ticks(#[state] n: &mut Rc<RefCell<u32>>) {
    *n.borrow_mut() += 1;
}

/// Sends the commands the test queues.
#[system]
fn cmds(
    #[state] queue: &mut Rc<RefCell<Vec<SequenceCommandKind>>>,
    commands: &mut Output<SequenceCommand>,
) {
    for command in queue.borrow_mut().drain(..) {
        let channel = "gps".into();
        let _ = commands.write(&SequenceCommand { channel, command });
    }
}

/// A model that never writes and panics on its fifth cycle.
#[system]
fn boom(#[state] n: &mut u32, uart: &Cap<Uart>) {
    let _ = uart;
    *n += 1;
    assert!(*n < 5, "the model fails");
}

#[derive(Default)]
struct Seen {
    fixes: Log<()>,
    statuses: Log<ResourceStatus>,
    states: Log<u32>,
    ticks: Rc<RefCell<u32>>,
    queue: Rc<RefCell<Vec<SequenceCommandKind>>>,
}

fn table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
    table.register("echo.gps", gps(), GpsState::new).unwrap();
    table
        .register("echo.uart_script", uart_script(), UartScript::new)
        .unwrap();
    let s = seen.fixes.clone();
    table.register("fixes", fixes(), move || s.clone()).unwrap();
    let s = seen.statuses.clone();
    table
        .register("statuses", statuses(), move || s.clone())
        .unwrap();
    let s = seen.states.clone();
    table
        .register("states", states(), move || s.clone())
        .unwrap();
    let s = seen.ticks.clone();
    table.register("ticks", ticks(), move || s.clone()).unwrap();
    let s = seen.queue.clone();
    table.register("cmds", cmds(), move || s.clone()).unwrap();
    table.register("boom", boom(), u32::default).unwrap();
    table
}

/// The golden target emitted by `tests/golden/sim.py`.
fn golden() -> CoordinatorConfig {
    let json = include_str!("golden/sim.json");
    serde_json::from_str(json).unwrap()
}

/// The golden target with probes on the fix and `io_status` appended.
fn probed(mut config: CoordinatorConfig) -> CoordinatorConfig {
    config.systems.extend([
        SystemConfig {
            inputs: vec![InputConfig::new("fix", "gps", "fix")],
            ..SystemConfig::new("fixes", "fixes")
        },
        SystemConfig {
            inputs: vec![InputConfig::new("st", "io_complete", "io_status")],
            ..SystemConfig::new("statuses", "statuses")
        },
        SystemConfig::new("ticks", "ticks"),
    ]);
    config
}

fn run(seen: &Seen, config: CoordinatorConfig, cycles: i64) -> Coordinator {
    let mut c = config.build(&table(seen)).unwrap();
    steps(&mut c, 1, cycles + 1);
    c
}

fn steps(c: &mut Coordinator, from: i64, to: i64) {
    for t in from..to {
        c.step(Timestamp(t));
    }
}

fn first_fix(seen: &Seen) -> Option<i64> {
    seen.fixes.borrow().first().map(|(t, _)| *t)
}

#[test]
fn model_between_the_io_steps_answers_the_next_cycle() {
    let seen = Seen::default();
    let _c = run(&seen, probed(golden()), 5);
    assert_eq!(first_fix(&seen), Some(2), "read in cycle 1, fix in cycle 2");
    let statuses = seen.statuses.borrow();
    let (_, last) = statuses.last().unwrap();
    assert!(last.completed >= 2 && last.bytes_in > 0, "{last:?}");
    assert_eq!(last.failed, 0);
}

/// Placed before `io_submit`, the model's SQ goes to `io_submit` and its CQ
/// wraps around from `io_complete`, like a driver's.
#[test]
fn model_before_io_submit_is_wired_like_a_driver() {
    let mut config = golden();
    let model = config.systems.remove(2);
    assert_eq!(model.id, "receiver");
    config.systems.insert(1, model);
    for _ in 0..2 {
        let seen = Seen::default();
        let _c = run(&seen, probed(config.clone()), 6);
        assert_eq!(first_fix(&seen), Some(2));
    }
}

#[test]
fn reset_cancels_a_read_parked_on_the_wire() {
    let mut config = golden();
    config.systems[2].params = serde_json::json!({ "script": [] });
    let driver = &mut config.systems[0];
    driver.adapter = Some(Adapter::Channel(ChannelConfig {
        name: metor_fsw_3::ChannelName::try_from("gps".to_string()).expect("a short name"),
        start: RunState::Running,
    }));
    driver
        .inputs
        .push(InputConfig::new("commands", "cmds", "commands"));
    config.systems.insert(0, SystemConfig::new("cmds", "cmds"));
    config.systems.push(SystemConfig {
        inputs: vec![InputConfig::new("st", "gps", "channel_status")],
        ..SystemConfig::new("states", "states")
    });
    let seen = Seen::default();
    let mut c = run(&seen, config, 3);
    seen.queue
        .borrow_mut()
        .extend([SequenceCommandKind::Stop, SequenceCommandKind::Reset]);
    steps(&mut c, 4, 10);
    let states = seen.states.borrow();
    let quiescing = ChannelState::Quiescing as u32;
    let waited: Vec<i64> = states
        .iter()
        .filter(|(_, s)| *s == quiescing)
        .map(|(t, _)| *t)
        .collect();
    assert!(!waited.is_empty(), "the reset waits: {states:?}");
    let after = states.iter().find(|(t, _)| *t > waited[0]).unwrap();
    let stopped = (waited[0] + 1, ChannelState::Stopped as u32);
    assert_eq!(*after, stopped, "remade the cycle after: {states:?}");
    assert!(
        c.systems()
            .all(|(_, end)| end != Some(&EndReason::Panicked))
    );
}

#[test]
fn panicking_model_ends_alone() {
    let mut config = probed(golden());
    config.systems[2].ty = "boom".into();
    config.systems[2].params = serde_json::Value::Null;
    let seen = Seen::default();
    let c = run(&seen, config, 20);
    let ended: Vec<_> = c.systems().filter(|(_, end)| end.is_some()).collect();
    assert!(
        matches!(ended.as_slice(), [("receiver", Some(EndReason::Panicked))]),
        "{ended:?}"
    );
    assert_eq!(first_fix(&seen), None, "the read never completes");
    assert_eq!(*seen.ticks.borrow(), 20, "the target keeps cycling");
}
