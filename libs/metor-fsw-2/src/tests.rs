//! Fn-authored systems end to end: the descriptor computed from parameter
//! types, two pack entries flowing frames through a coordinator with state
//! shared between them, and the create-phase failure modes.

use std::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::{Msg, Timestamp};
use metor_proto_wkt::SequenceCommand;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    ClockMode, CoordinatorConfig, Input, LogEvent, LogLevel, LogPort, MsgIn, Output, Pack, PortId,
    SystemKind, system,
};
use metor_fsw_2_core::Frame;
use metor_fsw_2_core::{EntryParams, MakeError};

use crate::coordinator::PortRef;
use crate::coordinator::init::pending_node;

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
/// log tail, and non-port params (`Timestamp`, `&mut LogPort`)
/// contribute nothing to the cursor walk.
#[test]
fn descriptor_orders_ports_by_signature() {
    fn probe(
        _s: &mut u64,
        _now: Timestamp,
        _a: &mut Input<PkImu>,
        _cmds: &mut MsgIn<SequenceCommand>,
        _b: &mut Output<PkNav>,
        _h: &mut LogPort,
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
            PortId::Packet(metor_proto_wkt::LogEvent::ID),
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

fn consume(s: &mut ConsState, imu: &mut Input<PkImu>, log: &mut LogPort) {
    if let Ok(Some(r)) = imu.latest() {
        // Field access through the grant's Deref; no `.get()`.
        if s.seen.borrow().last() != Some(&r.omega) {
            s.seen.borrow_mut().push(r.omega);
        }
    } else {
        log.fault(LogLevel::Warn, "no_sample", "no imu sample", &[]);
    }
}

struct NavWatch {
    seen: Rc<RefCell<Vec<f64>>>,
}

fn watch_nav(s: &mut NavWatch, nav: &mut Input<PkNav>) {
    if let Ok(Some(r)) = nav.latest()
        && s.seen.borrow().last() != Some(&r.angle)
    {
        s.seen.borrow_mut().push(r.angle);
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn pack_entries_flow_and_share_state() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut pack = Pack::new()
        .system("prod", system(produce).init(|| ProdState { n: 0.0 }))
        .system(
            "cons",
            system(consume).state(ConsState { seen: seen.clone() }),
        );

    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(
        pending_node(
            "prod".into(),
            pack.entry_mut("prod").expect("registered"),
            EntryParams::Postcard(&[]),
        )
        .expect("create prod"),
    );
    let cons = b.push_node(
        pending_node(
            "cons".into(),
            pack.entry_mut("cons").expect("registered"),
            EntryParams::Postcard(&[]),
        )
        .expect("create cons"),
    );
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(PkImu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(PkImu::FRAME_ID),
        },
    );
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
// ambient clock, a sequence as an ordinary wired system.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn task_entry_runs_as_wired_sequence() {
    use crate::{ClockMode, Params};
    use metor_fsw_2_core::sequence::{Outcome, now, progress, wait};
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
    let mut pack = Pack::new().task("point", point).system(
        "watch",
        system(watch_nav).state(NavWatch { seen: seen.clone() }),
    );

    let params = postcard::to_allocvec(&SeqParams { target: 7.0 }).unwrap();
    let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    });
    let point_h = b.push_node(
        pending_node(
            "point".into(),
            pack.entry_mut("point").unwrap(),
            EntryParams::Postcard(&params),
        )
        .expect("create task"),
    );
    let watch_h = b.push_node(
        pending_node(
            "watch".into(),
            pack.entry_mut("watch").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create watcher"),
    );
    b.connect(
        PortRef {
            system: point_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
        PortRef {
            system: watch_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
    );
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

// ---------------------------------------------------------------------------
// Pack-shared state: several entries granted the same `&mut` instance, with
// the state's lifecycle run once across all of them.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    value: f64,
    starts: u32,
    shutdowns: u32,
    report: Option<Rc<RefCell<(f64, u32, u32)>>>,
}

impl crate::SharedLifecycle for Tally {
    fn start(&mut self) {
        self.starts += 1;
    }

    fn shutdown(&mut self) {
        self.shutdowns += 1;
        if let Some(report) = &self.report {
            *report.borrow_mut() = (self.value, self.starts, self.shutdowns);
        }
    }
}

fn bump(s: &mut Tally, _now: Timestamp) {
    s.value += 1.0;
}

fn double(s: &mut Tally, _now: Timestamp) {
    s.value *= 2.0;
}

/// The headline: two entries attached to one shared state see the *same*
/// instance in registration order (`(((0+1)*2+1)*2+1)*2 = 14`), and the
/// lifecycle hooks ran exactly once around the whole run.
#[cfg(not(miri))]
#[stellarator::test]
async fn shared_state_entries_share_one_instance() {
    let report = Rc::new(RefCell::new((0.0, 0, 0)));
    let mut pack = Pack::new();
    let tally = pack.shared_state("Tally", {
        let report = report.clone();
        move |(): ()| {
            Ok::<_, std::convert::Infallible>(Tally {
                report: Some(report.clone()),
                ..Tally::default()
            })
        }
    });
    let mut pack = pack
        .system("bump", system(bump).shared(&tally))
        .system("double", system(double).shared(&tally));

    pack.state_entry_mut("Tally")
        .expect("declared")
        .create(EntryParams::Postcard(&[]))
        .expect("state constructs");

    let mut b = crate::coordinator::init::InitGraph::new(config());
    for name in ["bump", "double"] {
        b.push_node(
            pending_node(
                name.into(),
                pack.entry_mut(name).expect("registered"),
                EntryParams::Postcard(&[]),
            )
            .expect("create"),
        );
    }
    let mut coord = b.build().unwrap();
    coord.run_for(3).await;

    assert_eq!(*report.borrow(), (14.0, 1, 1));
    assert!(coord.stopped().is_empty());
}

/// An attached entry cannot instantiate before the state's own wiring
/// declaration constructed the instance.
#[test]
fn shared_entry_requires_constructed_state() {
    let mut pack = Pack::new();
    let tally = pack.shared_state("Tally", |(): ()| {
        Ok::<_, std::convert::Infallible>(Tally::default())
    });
    let mut pack = pack.system("bump", system(bump).shared(&tally));
    let entry = pack.entry_mut("bump").unwrap();
    assert!(!entry.reloadable());
    assert!(matches!(
        entry.create(EntryParams::Postcard(&[])),
        Err(MakeError::StateNotConstructed { state: "Tally" })
    ));
}

/// An attached entry instantiates once, like a `.state(...)` entry.
#[test]
fn shared_entry_instantiates_once() {
    let mut pack = Pack::new();
    let tally = pack.shared_state("Tally", |(): ()| {
        Ok::<_, std::convert::Infallible>(Tally::default())
    });
    let mut pack = pack.system("bump", system(bump).shared(&tally));
    pack.state_entry_mut("Tally")
        .unwrap()
        .create(EntryParams::Postcard(&[]))
        .unwrap();
    let entry = pack.entry_mut("bump").unwrap();
    drop(entry.create(EntryParams::Postcard(&[])).expect("first"));
    assert!(matches!(
        entry.create(EntryParams::Postcard(&[])),
        Err(MakeError::SharedEntryReinstantiated)
    ));
}

/// A failing shared-state init fn (resource acquisition) surfaces as a
/// create error naming the state, not a panic.
#[test]
fn shared_state_init_failure_reports() {
    let mut pack = Pack::new();
    let _tally: crate::Shared<Tally> =
        pack.shared_state("Tally", |(): ()| Err("address in use".to_string()));
    let err = pack
        .state_entry_mut("Tally")
        .unwrap()
        .create(EntryParams::Postcard(&[]))
        .expect_err("init failed");
    assert!(matches!(
        err,
        MakeError::StateInit { state: "Tally", ref detail } if detail == "address in use"
    ));
}

/// The host publishes a `system_status` record for every slot it steps, and
/// one for itself, once per cycle, carrying the cycle count, the step time,
/// and the slot's run state, with no entry declaring or writing the port.
#[cfg(not(miri))]
#[stellarator::test]
async fn host_publishes_a_status_record_for_every_slot() {
    use crate::{ClockMode, SlotState, SystemStatus};
    use metor_fsw_2_core::sequence::cycle;
    use std::time::Duration;

    async fn beat(mut nav: Output<PkNav>) {
        loop {
            let now = cycle().await;
            nav.publish(&PkNav {
                timestamp: now,
                angle: 1.0,
            });
        }
    }

    let mut pack = Pack::new().task("beat", beat).system(
        "watch",
        system(watch_nav).state(NavWatch {
            seen: Rc::new(RefCell::new(Vec::new())),
        }),
    );
    let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    });
    let beat_h = b.push_node(
        pending_node(
            "beat".into(),
            pack.entry_mut("beat").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create task"),
    );
    let watch_h = b.push_node(
        pending_node(
            "watch".into(),
            pack.entry_mut("watch").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create watcher"),
    );
    b.connect(
        PortRef {
            system: beat_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
        PortRef {
            system: watch_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
    );
    // Neither entry declares the port; the host appended it at registration.
    for h in [beat_h, watch_h] {
        let outputs = &b.descriptor_of(h).outputs;
        let last = outputs.last().expect("outputs");
        assert_eq!(last.id(), PortId::Component(SystemStatus::FRAME_ID));
        assert_eq!(last.conn, crate::PortConn::Host);
    }
    let mut coord = b.build().unwrap();

    let mut views: Vec<Input<SystemStatus>> = ["beat", "watch", "coordinator"]
        .iter()
        .map(|inst| {
            Input::new(
                coord
                    .registry()
                    .view(metor_proto::types::ComponentId::new(&format!(
                        "{inst}.system_status"
                    )))
                    .expect("the host-appended status is registered")
                    .expect("reader slot available"),
            )
        })
        .collect();

    // Fewer cycles than the ring depth, so these undrained views never pin
    // the writer and the newest record is the last cycle's.
    coord.run_for(5).await;

    for view in &mut views {
        let status = view
            .latest()
            .expect("ring readable")
            .expect("status published");
        assert_eq!(status.cycles, 5);
        assert_eq!(status.state, SlotState::Running.code());
    }
}

/// A wired task loops on `cycle().await`: state in locals, exactly one
/// publish per coordinator cycle, drops from the future-owned output (forced
/// by a stalled broad reader) reported on its log, and the host publishing
/// its `system_status` record like any other slot's.
#[cfg(not(miri))]
#[stellarator::test]
async fn task_cycles_every_cycle_and_reports_drops() {
    use crate::{ClockMode, SlotState, SystemStatus};
    use metor_fsw_2_core::sequence::cycle;
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
    let mut pack = Pack::new().task("beat", beat).system(
        "watch",
        system(watch_nav).state(NavWatch { seen: seen.clone() }),
    );

    let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        cycle_rate: 1000.0,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    });
    let beat_h = b.push_node(
        pending_node(
            "beat".into(),
            pack.entry_mut("beat").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create task"),
    );
    let watch_h = b.push_node(
        pending_node(
            "watch".into(),
            pack.entry_mut("watch").unwrap(),
            EntryParams::Postcard(&[]),
        )
        .expect("create watcher"),
    );
    b.connect(
        PortRef {
            system: beat_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
        PortRef {
            system: watch_h,
            port: PortId::Component(PkNav::FRAME_ID),
        },
    );
    let mut coord = b.build().unwrap();

    // A broad reader that never drains: its cursor pins the ring at the live
    // edge, so once the depth is exhausted every publish drops.
    let stalled = coord
        .registry()
        .view(metor_proto::types::ComponentId::new("beat.pk_nav"))
        .expect("the task output is registered")
        .expect("reader slot available");
    let mut log_in: MsgIn<LogEvent> = MsgIn::new(
        coord
            .registry()
            .view(metor_proto::types::ComponentId::new("beat.log"))
            .expect("the log is registered")
            .expect("reader slot available"),
    );
    let mut status_in: Input<SystemStatus> = Input::new(
        coord
            .registry()
            .view(metor_proto::types::ComponentId::new("beat.system_status"))
            .expect("the host-appended status is registered")
            .expect("reader slot available"),
    );

    coord.run_for(30).await;

    // One increment per cycle reached the consumer until the ring pinned.
    let seen = seen.borrow();
    assert!(!seen.is_empty(), "the task published");
    for w in seen.windows(2) {
        assert_eq!(w[1] - w[0], 1.0, "exactly one publish per cycle: {seen:?}");
    }
    // The stalled reader forced drops, and they landed on the task's log
    // through the shared cell.
    let mut faults = Vec::new();
    log_in.drain(|ev| faults.push(ev)).unwrap();
    assert!(
        faults.iter().any(|ev| ev
            .fields
            .iter()
            .any(|(k, v)| k == "kind" && v == "publish_dropped")),
        "future-owned drops are reported on the log: {faults:?}"
    );
    // The host authored the task's status record. This view never drained,
    // so it pinned the ring once it filled: the newest readable record is
    // one of the first depth's worth, not the last cycle's.
    let status = status_in
        .latest()
        .expect("ring readable")
        .expect("status published");
    assert!(
        (1..=30).contains(&status.cycles),
        "cycles = {}",
        status.cycles
    );
    assert_eq!(status.state, SlotState::Running.code());
    drop(stalled);
}

// ---------------------------------------------------------------------------
// A struct-authored attached entry whose construction mints ports: the host
// registers the instance descriptor, not the static one.
// ---------------------------------------------------------------------------

#[derive(crate::SystemInput)]
struct MintIn {}

#[derive(crate::SystemOutput)]
struct MintOut {}

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct MintParams {
    mint: bool,
}

struct MintSys {
    mint: bool,
}

impl crate::BuildSystem for MintSys {
    type Params = MintParams;
    fn new(params: MintParams) -> Self {
        Self { mint: params.mint }
    }
}

impl crate::System for MintSys {
    type Input = MintIn;
    type Output = crate::Out<MintOut>;
    const NAME: &'static str = "mint";
}

impl crate::CyclicSystem for MintSys {
    fn instance_descriptor(&self) -> crate::SystemDescriptor {
        let mut desc = <Self as crate::CyclicSystem>::descriptor();
        if self.mint {
            desc.outputs
                .push(crate::PortDesc::msg_dynamic("cmd", SequenceCommand::ID).untelemetered());
        }
        desc
    }

    fn execute(&mut self, _now: Timestamp, _input: &mut MintIn, _output: &mut Self::Output) {}
}

#[test]
fn system_type_shared_registers_instance_descriptor() {
    use metor_fsw_2_core::AttachTarget;
    use metor_fsw_2_core::MsgTable;

    // A shared entry attaches by name: build the resolved token the resolver
    // would hand its create, and drive create through the value surface.
    fn mint_params<'a>(
        value: &'a serde_json::Value,
        msgs: &'a MsgTable,
        attach: &'a AttachTarget,
    ) -> EntryParams<'a> {
        EntryParams::Value {
            value,
            src: "Mint",
            name: "mint",
            msgs,
            attach: Some(attach),
        }
    }

    fn mint_pack() -> (Pack, AttachTarget) {
        let mut pack = Pack::new();
        let tally = pack.shared_state("Tally", |(): ()| {
            Ok::<_, std::convert::Infallible>(Tally::default())
        });
        let attach = AttachTarget {
            ty: "Tally",
            token: std::rc::Rc::new(tally.clone()),
        };
        let mut pack = pack.system_type_shared::<MintSys, Tally>("Mint", |p, _tally| {
            <MintSys as crate::BuildSystem>::new(p)
        });
        pack.state_entry_mut("Tally")
            .unwrap()
            .create(EntryParams::Postcard(&[]))
            .unwrap();
        (pack, attach)
    }

    let msgs = MsgTable::default();
    let (mut pack, attach) = mint_pack();

    // A non-minting instance stands on the static descriptor.
    let plain = serde_json::json!({ "mint": false });
    let created = pack
        .entry_mut("Mint")
        .unwrap()
        .create(mint_params(&plain, &msgs, &attach))
        .unwrap();
    assert!(created.instance_desc.is_none());

    // A second instantiation is rejected, so re-register for the minting one.
    let (mut pack, attach) = mint_pack();
    let minting = serde_json::json!({ "mint": true });
    let node = pending_node(
        "mint".into(),
        pack.entry_mut("Mint").unwrap(),
        mint_params(&minting, &msgs, &attach),
    )
    .expect("create");
    assert!(
        node.desc
            .outputs
            .iter()
            .any(|p| p.id() == PortId::Packet(SequenceCommand::ID)),
        "the minted port reached the registered descriptor"
    );
}

#[stellarator::test]
async fn overrun_cycles_allow_async_progress() {
    use crate::wiring::{Registry, resolve};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    fn slow(_: &mut ()) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_micros(10) {
            std::hint::spin_loop();
        }
    }
    let mut registry = Registry::new();
    registry.register_pack(Pack::new().system("slow", system(slow)));
    let wiring = crate::WiringBuilder::new()
        .coordinator(1e9, crate::ClockSpec::Wall)
        .system("slow")
        .ty("slow")
        .end()
        .build();
    let mut coordinator = resolve(&wiring, &registry).unwrap();
    let polls = Rc::new(Cell::new(0));
    let observed = polls.clone();
    let task = stellarator::spawn(async move {
        loop {
            observed.set(observed.get() + 1);
            stellarator::yield_now().await;
        }
    })
    .drop_guard();
    stellarator::yield_now().await;
    let before = polls.get();
    coordinator.run_for(10).await;
    assert!(polls.get() > before, "overruns must not starve async tasks");
    drop(task);
}

#[test]
fn invalid_timer_periods_and_ring_sizes_are_build_errors() {
    use crate::coordinator::init::InitGraph;
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e-300, 1e-11, 1e300] {
        let result = InitGraph::new(CoordinatorConfig {
            cycle_rate: rate,
            ..config()
        })
        .build();
        assert!(
            matches!(result, Err(crate::WireError::InvalidCycleRate { .. })),
            "rate {rate}"
        );
    }
    for cfg in [
        CoordinatorConfig {
            default_depth: usize::MAX,
            ..config()
        },
        CoordinatorConfig {
            reader_slack: usize::MAX,
            ..config()
        },
    ] {
        assert!(matches!(
            InitGraph::new(cfg).build(),
            Err(crate::WireError::InvalidRingSize { .. })
        ));
    }
    let simulated = CoordinatorConfig {
        cycle_rate: f64::NAN,
        clock: ClockMode::Simulated {
            dt: std::time::Duration::from_micros(1),
        },
        ..config()
    };
    assert!(InitGraph::new(simulated).build().is_ok());
}
