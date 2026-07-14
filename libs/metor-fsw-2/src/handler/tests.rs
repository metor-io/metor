//! Fn-authored systems end to end: the descriptor computed from parameter
//! types, two pack entries flowing frames through a coordinator with state
//! shared between them, and the create-phase failure modes.

use std::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::{Msg, Timestamp};
use metor_proto_wkt::SequenceCommand;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::frame::Frame;
use crate::pack::{EntryParams, MakeError};
use crate::{
    ClockMode, Coordinator, CoordinatorConfig, HealthPort, Input, MsgIn, Output, Pack, PortId,
    PortRef, SystemKind, system,
};

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "pk_imu")]
struct PkImu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "pk_nav")]
struct PkNav {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
}

fn config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Wall,
        ..CoordinatorConfig::default()
    }
}

/// The descriptor lists ports per direction in signature order, appends the
/// health/log tail, and non-port params (`Timestamp`, `&mut HealthPort`)
/// contribute nothing to the cursor walk.
#[test]
fn descriptor_orders_ports_by_signature() {
    fn probe(
        _s: &mut u64,
        _now: Timestamp,
        _a: &mut Input<PkImu>,
        _cmds: &mut MsgIn<SequenceCommand>,
        _b: &mut Output<PkNav>,
        _h: &mut HealthPort,
    ) {
    }

    let entry = Pack::new()
        .system("probe", system(probe))
        .into_entries()
        .pop()
        .expect("one entry");
    assert_eq!(entry.name(), "probe");
    assert!(entry.reloadable());

    let d = entry.descriptor();
    assert_eq!(d.kind, SystemKind::Cyclic);
    assert!(d.capabilities.is_empty());
    let input_ids: Vec<_> = d.inputs.iter().map(|p| p.id()).collect();
    assert_eq!(
        input_ids,
        vec![
            PortId::Component(PkImu::FRAME_ID),
            PortId::Packet(SequenceCommand::ID),
        ]
    );
    let output_ids: Vec<_> = d.outputs.iter().map(|p| p.id()).collect();
    assert_eq!(
        output_ids,
        vec![
            PortId::Component(PkNav::FRAME_ID),
            PortId::Component(crate::SystemHealth::FRAME_ID),
            PortId::Component(crate::SystemLog::FRAME_ID),
        ]
    );
}

// ---------------------------------------------------------------------------
// The headline: two fn-authored entries in one pack, wired through a
// coordinator, with the consumer's state a shared handle built outside both.
// ---------------------------------------------------------------------------

struct ProdState {
    n: f64,
}

fn produce(s: &mut ProdState, now: Timestamp, imu: &mut Output<PkImu>) {
    s.n += 1.0;
    imu.publish(&PkImu {
        timestamp: now,
        omega: s.n,
    });
}

struct ConsState {
    seen: Rc<RefCell<Vec<f64>>>,
}

fn consume(s: &mut ConsState, imu: &mut Input<PkImu>, health: &mut HealthPort) {
    if let Some(r) = imu.latest() {
        // Field access through the grant's Deref; no `.get()`.
        if s.seen.borrow().last() != Some(&r.omega) {
            s.seen.borrow_mut().push(r.omega);
        }
    } else {
        health.error("no_sample");
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn pack_entries_flow_and_share_state() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut pack = Pack::new()
        .system("prod", system(produce).init(|| ProdState { n: 0.0 }))
        .system("cons", system(consume).state(ConsState { seen: seen.clone() }));

    let mut b = Coordinator::builder(config());
    let prod = b
        .add_pack_entry(
            "prod",
            pack.entry_mut("prod").expect("registered"),
            EntryParams::Postcard(&[]),
        )
        .expect("create prod");
    let cons = b
        .add_pack_entry(
            "cons",
            pack.entry_mut("cons").expect("registered"),
            EntryParams::Postcard(&[]),
        )
        .expect("create cons");
    b.connect(PortRef::new::<PkImu>(prod), PortRef::new::<PkImu>(cons))
        .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(5).await;

    // Producer steps before consumer each cycle (registration order), so the
    // shared handle saw every fresh value.
    assert_eq!(*seen.borrow(), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    assert!(coord.stopped().is_empty());
}

/// A `.state(...)` entry instantiates once; the second create reports
/// [`MakeError::StateTaken`] instead of silently cloning or defaulting.
#[test]
fn prebuilt_state_is_move_once() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut pack = Pack::new().system("cons", system(consume).state(ConsState { seen }));
    let entry = pack.entry_mut("cons").unwrap();
    assert!(!entry.reloadable());

    drop(entry.create(EntryParams::Postcard(&[])).expect("first"));
    assert!(matches!(
        entry.create(EntryParams::Postcard(&[])),
        Err(MakeError::StateTaken)
    ));
}

// ---------------------------------------------------------------------------
// The async form: a task entry owning its ports, polled per cycle under the
// ambient clock — a sequence as an ordinary wired system.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn task_entry_runs_as_wired_sequence() {
    use crate::sequence::{Outcome, now, progress, wait};
    use crate::{ClockMode, Params};
    use std::time::Duration;

    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
    struct SeqParams {
        target: f64,
    }

    async fn point(Params(p): Params<SeqParams>, mut nav: Output<PkNav>) -> Outcome {
        progress("pointing");
        if wait(Duration::from_millis(2)).await.aborted() {
            return Outcome::Aborted;
        }
        nav.publish(&PkNav {
            timestamp: now(),
            angle: p.target,
        });
        Outcome::Completed
    }

    let seen = Rc::new(RefCell::new(Vec::new()));
    struct WatchState {
        seen: Rc<RefCell<Vec<f64>>>,
    }
    fn watch(s: &mut WatchState, nav: &mut Input<PkNav>) {
        if let Some(r) = nav.latest()
            && s.seen.borrow().last() != Some(&r.angle)
        {
            s.seen.borrow_mut().push(r.angle);
        }
    }

    let mut pack = Pack::new()
        .task("point", point)
        .system("watch", system(watch).state(WatchState { seen: seen.clone() }));

    let params = postcard::to_allocvec(&SeqParams { target: 7.0 }).unwrap();
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    });
    let point_h = b
        .add_pack_entry(
            "point",
            pack.entry_mut("point").unwrap(),
            EntryParams::Postcard(&params),
        )
        .expect("create task");
    let watch_h = b
        .add_pack_entry(
            "watch",
            pack.entry_mut("watch").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create watcher");
    b.connect(PortRef::new::<PkNav>(point_h), PortRef::new::<PkNav>(watch_h))
        .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(10).await;

    // The wait elapsed on the simulated clock, the params reached the body,
    // and the completed future stopped publishing (one recorded value).
    assert_eq!(*seen.borrow(), vec![7.0]);
    assert!(coord.stopped().is_empty(), "Done is not an error stop");
}

/// Bad postcard params fail at create, before any registration.
#[test]
fn bad_params_fail_at_create() {
    #[derive(serde::Deserialize, postcard_schema::Schema)]
    struct Gain {
        _gain: f64,
    }
    fn noop(_s: &mut f64, _out: &mut Output<PkNav>) {}

    let mut pack = Pack::new().system("g", system(noop).init(|_p: Gain| 0.0f64));
    let entry = pack.entry_mut("g").unwrap();
    // Empty bytes cannot decode a struct with an f64 field.
    assert!(matches!(
        entry.create(EntryParams::Postcard(&[])),
        Err(MakeError::Postcard(_))
    ));
}

/// A wired task loops on `cycle().await`: state in locals, exactly one
/// publish per coordinator cycle, and drops from the future-owned output
/// (forced by a stalled broad reader) fold into its health — the counter
/// asymmetry the ports-in-future design used to have.
#[cfg(not(miri))]
#[stellarator::test]
async fn task_cycles_every_cycle_and_folds_drops() {
    use crate::sequence::cycle;
    use crate::{ClockMode, SystemHealth};
    use std::time::Duration;

    async fn beat(mut nav: Output<PkNav>) {
        let mut n = 0.0;
        loop {
            let now = cycle().await;
            n += 1.0;
            nav.publish(&PkNav {
                timestamp: now,
                angle: n,
            });
        }
    }

    let seen = Rc::new(RefCell::new(Vec::new()));
    struct WatchState {
        seen: Rc<RefCell<Vec<f64>>>,
    }
    fn watch(s: &mut WatchState, nav: &mut Input<PkNav>) {
        if let Some(r) = nav.latest()
            && s.seen.borrow().last() != Some(&r.angle)
        {
            s.seen.borrow_mut().push(r.angle);
        }
    }

    let mut pack = Pack::new()
        .task("beat", beat)
        .system("watch", system(watch).state(WatchState { seen: seen.clone() }));

    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    });
    let beat_h = b
        .add_pack_entry(
            "beat",
            pack.entry_mut("beat").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create task");
    let watch_h = b
        .add_pack_entry(
            "watch",
            pack.entry_mut("watch").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create watcher");
    b.connect(PortRef::new::<PkNav>(beat_h), PortRef::new::<PkNav>(watch_h))
        .unwrap();
    let mut coord = b.build().unwrap();

    // A broad reader that never drains: its cursor pins the ring at the live
    // edge, so once the depth is exhausted every publish drops.
    let stalled = coord
        .registry()
        .view(metor_proto::types::ComponentId::new("beat.pk_nav"))
        .expect("the task output is registered")
        .expect("reader slot available");
    let mut health_view: Input<SystemHealth> = Input::new(
        coord
            .registry()
            .view(metor_proto::types::ComponentId::new("beat.health"))
            .expect("health is registered")
            .expect("reader slot available"),
    );

    coord.run_for(30).await;

    // One increment per cycle reached the consumer until the ring pinned.
    let seen = seen.borrow();
    assert!(!seen.is_empty(), "the task published");
    for w in seen.windows(2) {
        assert_eq!(w[1] - w[0], 1.0, "exactly one publish per cycle: {seen:?}");
    }
    // The stalled reader forced drops, and they landed on the task's health
    // through the shared cell.
    let health = health_view.latest().expect("health published");
    assert!(
        health.errors > 0,
        "future-owned drops fold into health (errors = {})",
        health.errors
    );
    drop(stalled);
}
