//! A GPS driver on a sim resource, served by a model through the io steps.

#[path = "fixtures/echo-pack/src/gps.rs"]
#[allow(dead_code)]
mod gps;

use std::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3::coordinator::{
    CoordinatorConfig, EndReason, InputConfig, SystemConfig, SystemTable,
};
use metor_fsw_3::io::{Cap, IO_CQ, IO_SQ, ResourceStatus, Uart};
use metor_fsw_3::{Coordinator, Input, Timestamp, system};

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

/// Counts the cycles it runs.
#[system]
fn ticks(#[state] n: &mut Rc<RefCell<u32>>) {
    *n.borrow_mut() += 1;
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
    ticks: Rc<RefCell<u32>>,
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
    let s = seen.ticks.clone();
    table.register("ticks", ticks(), move || s.clone()).unwrap();
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
    let mut model = config.systems.remove(2);
    assert_eq!(model.id, "receiver");
    model.inputs = vec![InputConfig::new(IO_CQ, "io_complete", "receiver")];
    config.systems.insert(1, model);
    let [_, _, submit, complete] = &mut config.systems[..] else {
        panic!("four systems");
    };
    submit
        .inputs
        .push(InputConfig::new("receiver", "receiver", IO_SQ));
    complete.inputs.clear();
    complete.outputs.append(&mut submit.outputs);
    for _ in 0..2 {
        let seen = Seen::default();
        let _c = run(&seen, probed(config.clone()), 6);
        assert_eq!(first_fix(&seen), Some(2));
    }
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
