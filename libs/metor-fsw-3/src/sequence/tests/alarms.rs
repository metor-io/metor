//! `alarm!` and `redline!` publishing through a sequence's alarm ports.

use core::num::NonZeroUsize;
use core::time::Duration;

use metor_proto::types::Timestamp;

use crate::coordinator::{
    BuildError, Coordinator, CoordinatorConfig, EndReason, InputConfig, SystemConfig, SystemTable,
};
use crate::port::Sample;
use crate::sequence::{Seq, SeqError};
use crate::tests::alarms::{Seen, Shared, feeder, occ, sink};
use crate::tests::utils::{Imu, ended};
use crate::{AlarmSpec, Msg, Severity, Watch};

const HOUR: Duration = Duration::from_secs(3600);
const PC_HIGH: AlarmSpec = AlarmSpec::warning("pc_high").debounce(2);
const HOT: AlarmSpec = AlarmSpec::warning("hot");
const COLD: AlarmSpec = AlarmSpec::info("cold");

fn above(imu: &Sample<Imu>, v: f64) -> Option<bool> {
    imu.get().map(|i| i.sample > v)
}

/// Raises `pc_high` above 1.0, with the value in its message.
#[crate::sequence]
async fn burn(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::alarm!(seq, &PC_HIGH, || above(imu, 1.0), |m| {
        write!(m, "pc {:.1}", imu.get().map_or(f64::NAN, |i| i.sample))
    });
    seq.sleep(HOUR).await
}

/// `pc_high` with its default message.
#[crate::sequence]
async fn plain(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::alarm!(seq, &PC_HIGH, || above(imu, 1.0));
    seq.sleep(HOUR).await
}

/// Aborts above 2.0.
#[crate::sequence]
async fn guarded(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::redline!(seq, "imu high", || above(imu, 2.0) == Some(true));
    seq.sleep(HOUR).await
}

/// Watches `hot` for two cycles, then sleeps without it.
#[crate::sequence]
async fn scoped(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    {
        crate::alarm!(seq, &HOT, || above(imu, 1.0));
        seq.next_cycle().await;
        seq.next_cycle().await;
    }
    seq.sleep(HOUR).await
}

/// Two alarms and a redline that all trip above 2.0.
#[crate::sequence]
async fn trio(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::alarm!(seq, &HOT, || above(imu, 2.0));
    crate::alarm!(seq, &COLD, || above(imu, 2.0));
    crate::redline!(seq, "imu high", || above(imu, 2.0) == Some(true), |m| {
        write!(m, "imu over")
    });
    seq.sleep(HOUR).await
}

const FLOOD: [AlarmSpec; 20] = {
    const NAMES: [&str; 20] = [
        "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10", "a11", "a12", "a13",
        "a14", "a15", "a16", "a17", "a18", "a19",
    ];
    let mut specs = [AlarmSpec::info("x"); 20];
    let mut i = 0;
    while i < 20 {
        specs[i] = AlarmSpec::info(NAMES[i]);
        i += 1;
    }
    specs
};

/// Twenty alarms with full messages that all raise on the first sample.
#[crate::sequence]
async fn flood(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    let long = |m: &mut Msg| write!(m, "{}", "m".repeat(128));
    let watches: Vec<_> = FLOOD
        .iter()
        .map(|spec| Box::pin(Watch::alarm(spec, || above(imu, 0.0), long)))
        .collect();
    for watch in &watches {
        seq.watch(watch.as_ref());
    }
    seq.sleep(HOUR).await
}

/// Raises `hot` above 1.0, then panics on its third poll.
#[crate::sequence]
async fn smoke(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::alarm!(seq, &HOT, || above(imu, 1.0));
    seq.next_cycle().await;
    seq.next_cycle().await;
    panic!("smoke")
}

/// `cold` above 1.0, a redline above 2.0, then a condition that panics
/// above 2.0.
#[crate::sequence]
async fn faulty(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::alarm!(seq, &COLD, || above(imu, 1.0));
    crate::redline!(seq, "imu high", || above(imu, 2.0) == Some(true));
    crate::alarm!(seq, &HOT, || match above(imu, 2.0) {
        Some(true) => panic!("bad sensor"),
        level => level,
    });
    seq.sleep(HOUR).await
}

/// Two unnamed redlines on different conditions.
#[crate::sequence]
async fn unnamed_pair(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::redline!(seq, || above(imu, 2.0) == Some(true));
    crate::redline!(seq, || above(imu, 2.5) == Some(true));
    seq.sleep(HOUR).await
}

fn hot(imu: &Sample<Imu>, v: f64) -> bool {
    above(imu, v) == Some(true)
}

/// Two long conditions that differ only past the id's cut.
#[crate::sequence]
async fn long(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    crate::redline!(seq, || hot(imu, 1.0)
        && hot(imu, 1.25)
        && hot(imu, 1.5)
        && hot(imu, 1.75)
        && hot(imu, 2.0)
        && hot(imu, 2.25)
        && hot(imu, 2.5)
        && hot(imu, 2.75)
        && hot(imu, 2.8));
    crate::redline!(seq, || hot(imu, 1.0)
        && hot(imu, 1.25)
        && hot(imu, 1.5)
        && hot(imu, 1.75)
        && hot(imu, 2.0)
        && hot(imu, 2.25)
        && hot(imu, 2.5)
        && hot(imu, 2.75)
        && hot(imu, 2.9));
    seq.sleep(HOUR).await
}

fn table(shared: &Shared) -> SystemTable {
    let mut table = SystemTable::new();
    let mut register = || -> Result<(), BuildError> {
        let s = shared.clone();
        table.register("feeder", feeder(), move || s.clone())?;
        let s = shared.clone();
        table.register("sink", sink(), move || s.clone())?;
        table.register_sequence("burn", burn(), Default::default)?;
        table.register_sequence("plain", plain(), Default::default)?;
        table.register_sequence("guarded", guarded(), Default::default)?;
        table.register_sequence("scoped", scoped(), Default::default)?;
        table.register_sequence("trio", trio(), Default::default)?;
        table.register_sequence("flood", flood(), Default::default)?;
        table.register_sequence("smoke", smoke(), Default::default)?;
        table.register_sequence("faulty", faulty(), Default::default)?;
        table.register_sequence("unnamed_pair", unnamed_pair(), Default::default)?;
        table.register_sequence("long", long(), Default::default)?;
        Ok(())
    };
    register().expect("valid");
    table
}

/// `feed -> {ty} as t -> sink`, with rings `ring_depth` cycles deep.
fn build(ty: &str, values: &[f64], ring_depth: usize) -> (Shared, Coordinator) {
    let shared = Shared::scripted(values);
    let config = build_config(ty, ring_depth);
    let coordinator = config.build(&table(&shared)).expect("valid config");
    (shared, coordinator)
}

/// The config `build` runs.
fn build_config(ty: &str, ring_depth: usize) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("feed", "feeder"),
            SystemConfig {
                inputs: vec![InputConfig::new("imu", "feed", "batt")],
                ..SystemConfig::new("t", ty)
            },
            SystemConfig {
                inputs: vec![
                    InputConfig::new("raised", "t", "alarm_raised"),
                    InputConfig::new("cleared", "t", "alarm_cleared"),
                    InputConfig::new("log", "t", "log"),
                ],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ring_depth: NonZeroUsize::new(ring_depth).expect("nonzero"),
        ..Default::default()
    }
}

fn run(coordinator: &mut Coordinator, cycles: core::ops::Range<i64>) {
    cycles.for_each(|t| coordinator.step(Timestamp(t)));
}

fn raised(t: i64, id: &str, at: i64, severity: Severity, msg: &str) -> Seen {
    Seen::Raised(t, id.into(), occ(id, at), severity, msg.into())
}

fn cleared(t: i64, id: &str, at: i64) -> Seen {
    Seen::Cleared(t, id.into(), occ(id, at))
}

const REDLINE: &str = "t.redline.imu high";

#[test]
fn alarm_raises_after_debounce_and_clears_on_recovery() {
    let (shared, mut c) = build("burn", &[1.5, 1.5, 1.5, 0.5, 0.5], 8);
    run(&mut c, 0..5);
    let id = "t.pc_high";
    let want = [
        raised(2, id, 2, Severity::Warning, "pc 1.5"),
        cleared(4, id, 2),
    ];
    assert_eq!(shared.take(), want);
}

#[test]
fn three_argument_alarm_names_its_message() {
    let (shared, mut c) = build("plain", &[1.5, 1.5, 1.5], 8);
    run(&mut c, 0..3);
    let id = "t.pc_high";
    assert_eq!(
        shared.take(),
        [raised(2, id, 2, Severity::Warning, "pc_high")]
    );
}

#[test]
fn redline_raises_and_clears_as_it_aborts() {
    let (shared, mut c) = build("guarded", &[0.0, 3.0], 8);
    run(&mut c, 0..2);
    let want = [
        raised(1, REDLINE, 1, Severity::Critical, "imu high"),
        cleared(1, REDLINE, 1),
    ];
    assert_eq!(shared.take(), want);
    assert_eq!(shared.faults("redline"), ["imu high"]);
    assert_eq!(ended(&c), [("t", &EndReason::Aborted)]);
}

#[test]
fn panicking_body_clears_its_alarms() {
    let (shared, mut c) = build("smoke", &[1.5, 1.5, 1.5, 1.5], 8);
    run(&mut c, 0..4);
    let id = "t.hot";
    let want = [
        raised(1, id, 1, Severity::Warning, "hot"),
        cleared(2, id, 1),
    ];
    assert_eq!(shared.take(), want);
    assert_eq!(ended(&c), [("t", &EndReason::Panicked)]);
}

#[test]
fn panicking_condition_clears_its_alarms() {
    let (shared, mut c) = build("faulty", &[0.0, 1.5, 3.0, 3.0], 8);
    run(&mut c, 0..4);
    let want = [
        raised(1, "t.cold", 1, Severity::Info, "cold"),
        raised(2, REDLINE, 2, Severity::Critical, "imu high"),
        cleared(2, REDLINE, 2),
        cleared(2, "t.cold", 1),
    ];
    assert_eq!(shared.take(), want);
    assert_eq!(ended(&c), [("t", &EndReason::Panicked)]);
}

#[test]
fn alarm_clears_when_its_block_ends() {
    let (shared, mut c) = build("scoped", &[1.5, 1.5, 1.5, 1.5], 8);
    run(&mut c, 0..4);
    let id = "t.hot";
    let want = [
        raised(1, id, 1, Severity::Warning, "hot"),
        cleared(2, id, 1),
    ];
    assert_eq!(shared.take(), want);
    assert!(ended(&c).is_empty(), "a raise never aborts");
}

#[test]
fn two_alarms_and_a_redline_step_in_one_walk() {
    let (shared, mut c) = build("trio", &[3.0, 3.0], 8);
    run(&mut c, 0..3);
    let seen = shared.take();
    let raises = seen.iter().filter(|s| matches!(s, Seen::Raised(1, ..)));
    let clears = seen.iter().filter(|s| matches!(s, Seen::Cleared(1, ..)));
    assert_eq!((raises.count(), clears.count()), (3, 3), "{seen:?}");
    let want = raised(1, REDLINE, 1, Severity::Critical, "imu over");
    assert!(seen.contains(&want));
    assert_eq!(ended(&c), [("t", &EndReason::Aborted)]);
}

#[test]
fn full_alarm_ring_counts_a_publish_dropped_fault() {
    let (shared, mut c) = build("flood", &[1.0, 1.0], 1);
    run(&mut c, 0..2);
    assert!(shared.take().len() < FLOOD.len());
    assert_eq!(shared.faults("publish_dropped").len(), 1);
}

/// The ids and messages of the raises `ty` publishes on `values`.
fn raises(ty: &str, values: &[f64]) -> Vec<(String, String)> {
    let (shared, mut c) = build(ty, values, 8);
    run(&mut c, 0..values.len() as i64);
    let seen = shared.take().into_iter();
    let raised = seen.filter_map(|s| match s {
        Seen::Raised(_, id, _, _, msg) => Some((id, msg)),
        Seen::Cleared(..) => None,
    });
    raised.collect()
}

fn ends_in_hash(id: &str) -> bool {
    let (_, tail) = id.split_at(id.len() - 9);
    tail.starts_with('_') && tail[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[test]
fn unnamed_redlines_get_distinct_ids_and_their_text() {
    let raised = raises("unnamed_pair", &[0.0, 3.0]);
    let text = "|| above(imu, 2.0) == Some(true)";
    assert_eq!(raised.len(), 2);
    assert_eq!(raised[0], (format!("t.redline.{text}"), text.into()));
    assert_ne!(raised[0].0, raised[1].0);
}

#[test]
fn long_conditions_cut_with_a_hash() {
    let raised = raises("long", &[0.0, 3.0]);
    assert_eq!(raised.len(), 2);
    for (id, _) in &raised {
        assert_eq!(id.len(), 96, "{id}");
        assert!(ends_in_hash(id), "{id}");
    }
    assert_ne!(raised[0].0, raised[1].0);
    assert_eq!(raised[0].0[..87], raised[1].0[..87]);
}
