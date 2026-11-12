//! The `Alarms` parameter, run in a coordinator.

use core::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, Coordinator, CoordinatorConfig, InputConfig, RunState,
    SystemConfig, SystemTable,
};
use crate::port::{Output, Sample};
use crate::tests::alarms::{Seen, Shared, feeder, occ, sink};
use crate::tests::utils::Imu;
use crate::{AlarmCell, AlarmSpec, Alarms, Severity};

use super::def_of;

const BATT_HOT: AlarmSpec = AlarmSpec::critical("batt_hot").debounce(3);
const BATT_WARM: AlarmSpec = AlarmSpec::warning("batt_warm");

/// The commands `commander` sends to channel `pwr`.
type Commands = Rc<RefCell<VecDeque<SequenceCommandKind>>>;

/// The shared feeder and sink state, and the channel's command queue.
#[derive(Clone, Default)]
struct Rig {
    shared: Shared,
    commands: Commands,
}

impl Rig {
    fn take(&self) -> Vec<Seen> {
        self.shared.take()
    }

    fn send(&self, command: SequenceCommandKind) {
        self.commands.borrow_mut().push_back(command);
    }
}

/// Sends the queued channel commands.
#[crate::system]
fn commander(#[state] s: &mut Commands, commands: &mut Output<SequenceCommand>) {
    while let Some(command) = s.borrow_mut().pop_front() {
        let channel = "pwr".into();
        let _ = commands.write(&SequenceCommand { channel, command });
    }
}

#[derive(Default)]
struct Power {
    batt_hot: AlarmCell,
    batt_warm: AlarmCell,
}

/// Checks `batt_hot`, skipping cycles whose temperature is negative.
#[crate::system]
fn power(#[state] s: &mut Power, batt: &Sample<Imu>, alarms: &mut Alarms) {
    let b = batt.get();
    if b.is_some_and(|b| b.sample < 0.0) {
        return;
    }
    let temp = b.map_or(f64::NAN, |b| b.sample);
    let hot = b.map(|b| b.sample > 60.0);
    alarms.check_with(&mut s.batt_hot, &BATT_HOT, hot, |m| {
        write!(m, "batt {temp:.1} C")
    });
}

/// Checks two specs on one temperature.
#[crate::system]
fn twin(#[state] s: &mut Power, batt: &Sample<Imu>, alarms: &mut Alarms) {
    let b = batt.get();
    alarms.check(&mut s.batt_hot, &BATT_HOT, b.map(|b| b.sample > 60.0));
    let warm = b.map(|b| b.sample > 40.0);
    alarms.check(&mut s.batt_warm, &BATT_WARM, warm);
}

/// Raises `batt_warm` above 40, and panics above 100.
#[crate::system]
fn fragile(#[state] s: &mut Power, batt: &Sample<Imu>, alarms: &mut Alarms) {
    let b = batt.get();
    alarms.check(&mut s.batt_warm, &BATT_WARM, b.map(|b| b.sample > 40.0));
    assert!(b.is_none_or(|b| b.sample <= 100.0), "overheated");
}

fn table(rig: &Rig) -> SystemTable {
    let mut table = SystemTable::new();
    let mut register = || -> Result<(), BuildError> {
        let s = rig.shared.clone();
        table.register("feeder", feeder(), move || s.clone())?;
        let c = rig.commands.clone();
        table.register("commander", commander(), move || c.clone())?;
        let s = rig.shared.clone();
        table.register("sink", sink(), move || s.clone())?;
        table.register("power", power(), Power::default)?;
        table.register("twin", twin(), Power::default)?;
        table.register("fragile", fragile(), Power::default)?;
        Ok(())
    };
    register().expect("valid");
    table
}

/// `feed -> {ty} as pwr -> sink`, behind channel `pwr` commanded by `cmds`.
fn build(ty: &str, temps: &[f64]) -> (Rig, Coordinator) {
    let rig = Rig {
        shared: Shared::scripted(temps),
        commands: Commands::default(),
    };
    let coordinator = config(ty).build(&table(&rig)).expect("valid config");
    (rig, coordinator)
}

fn config(ty: &str) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "commander"),
            SystemConfig::new("feed", "feeder"),
            SystemConfig {
                inputs: vec![
                    InputConfig::new("batt", "feed", "batt"),
                    InputConfig::new("commands", "cmds", "commands"),
                ],
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: crate::ChannelName::try_from("pwr".to_string()).expect("short"),
                    start: RunState::Running,
                })),
                ..SystemConfig::new("pwr", ty)
            },
            SystemConfig {
                inputs: vec![
                    InputConfig::new("raised", "pwr", "alarm_raised"),
                    InputConfig::new("cleared", "pwr", "alarm_cleared"),
                    InputConfig::new("log", "pwr", "log"),
                ],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    }
}

fn run(coordinator: &mut Coordinator, cycles: core::ops::Range<i64>) {
    cycles.for_each(|t| coordinator.step(Timestamp(t)));
}

/// Stops, then resets, the `pwr` channel at cycles `t` and `t + 1`.
fn reset(rig: &Rig, coordinator: &mut Coordinator, t: i64) {
    rig.send(SequenceCommandKind::Stop);
    coordinator.step(Timestamp(t));
    rig.send(SequenceCommandKind::Reset);
    coordinator.step(Timestamp(t + 1));
}

#[test]
fn def_lists_alarm_ports_before_log() {
    let def = def_of(power());
    let names: Vec<_> = def.outputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(names, ["alarm_raised", "alarm_cleared", "log"]);
}

#[test]
fn raises_after_three_hot_cycles_and_clears_after_three_cool() {
    let (shared, mut c) = build("power", &[70.0, 70.0, 70.0, 20.0, 20.0, 20.0]);
    run(&mut c, 0..6);
    let id = "pwr.batt_hot";
    let want = [
        Seen::Raised(
            2,
            id.into(),
            occ(id, 2),
            Severity::Critical,
            "batt 70.0 C".into(),
        ),
        Seen::Cleared(5, id.into(), occ(id, 2)),
    ];
    assert_eq!(shared.take(), want);
}

#[test]
fn reset_while_active_clears_before_the_rebuilt_cycle() {
    let (shared, mut c) = build("power", &[70.0, 70.0, 70.0]);
    run(&mut c, 0..3);
    assert_eq!(shared.take().len(), 1);
    reset(&shared, &mut c, 3);
    let id = "pwr.batt_hot";
    assert_eq!(shared.take(), [Seen::Cleared(4, id.into(), occ(id, 2))]);
    shared.send(SequenceCommandKind::Start);
    run(&mut c, 5..8);
    assert_eq!(shared.take(), [], "the rebuilt cell starts clear");
}

#[test]
fn unchecked_cycle_leaves_the_cell() {
    let (shared, mut c) = build("power", &[70.0, 70.0, -1.0, 70.0]);
    run(&mut c, 0..4);
    let seen = shared.take();
    assert!(matches!(seen.as_slice(), [Seen::Raised(3, ..)]), "{seen:?}");
}

#[test]
fn two_specs_publish_independently_with_their_ids_as_messages() {
    let (shared, mut c) = build("twin", &[50.0, 50.0, 50.0, 30.0]);
    run(&mut c, 0..4);
    let id = "pwr.batt_warm";
    let want = [
        Seen::Raised(
            0,
            id.into(),
            occ(id, 0),
            Severity::Warning,
            "batt_warm".into(),
        ),
        Seen::Cleared(3, id.into(), occ(id, 0)),
    ];
    assert_eq!(shared.take(), want);
}

#[test]
fn reset_with_nothing_active_writes_nothing() {
    let (shared, mut c) = build("power", &[20.0]);
    run(&mut c, 0..1);
    reset(&shared, &mut c, 1);
    assert_eq!(shared.take(), []);
}

#[test]
fn cleared_alarm_is_not_cleared_again_on_drop() {
    let (shared, mut c) = build("twin", &[50.0, 30.0]);
    run(&mut c, 0..2);
    assert_eq!(shared.take().len(), 2);
    reset(&shared, &mut c, 2);
    assert_eq!(shared.take(), []);
}

#[test]
fn panic_clears_active_alarms() {
    let (shared, mut c) = build("fragile", &[50.0, 150.0, 20.0]);
    run(&mut c, 0..3);
    let id = "pwr.batt_warm";
    let seen = shared.take();
    assert!(
        matches!(seen.as_slice(), [Seen::Raised(0, ..), _]),
        "{seen:?}"
    );
    assert_eq!(seen[1], Seen::Cleared(1, id.into(), occ(id, 0)));
    assert_eq!(shared.shared.faults("panic").len(), 1);
}
