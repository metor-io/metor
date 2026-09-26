use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};

use crate::channel::{Abort, ChannelState, ChannelStatus};
use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, Coordinator, CoordinatorConfig, EndReason, InputConfig,
    LoadError, Occupant, PortRef, RunState, Step, SystemConfig, SystemTable, TableEntry,
};
use crate::def::DefCx;
use crate::port::{Input, Output};
use crate::sequence::{Seq, SeqError};
use crate::system::SystemDef;
use crate::tests::utils::{Imu, ImuSource};
use crate::{FrameBytes, FrameStr, system};

/// What the test systems saw, shared with the test.
#[derive(Default)]
struct Seen {
    queue: VecDeque<SequenceCommand>,
    counts: Vec<u64>,
    makes: u64,
    aborts: u64,
    /// The count a counter panics on.
    panic_at: Option<u64>,
    /// Whether the next `Counter::new` refuses.
    make_fails: bool,
    /// The end the fake slot's occupant reports.
    slot_end: Option<EndReason>,
    events: Vec<SequenceChannelEvent>,
    states: Vec<u32>,
    status: Option<ChannelStatus>,
}

#[derive(Clone, Default)]
struct Shared(Rc<RefCell<Seen>>);

impl Shared {
    fn send(&self, channel: &str, command: SequenceCommandKind) {
        self.0.borrow_mut().queue.push_back(SequenceCommand {
            channel: channel.into(),
            command,
        });
    }

    fn events(&self) -> Vec<SequenceEventKind> {
        let events = std::mem::take(&mut self.0.borrow_mut().events);
        events.into_iter().map(|event| event.kind).collect()
    }

    fn counts(&self) -> Vec<u64> {
        std::mem::take(&mut self.0.borrow_mut().counts)
    }

    fn state(&self) -> u32 {
        *self.0.borrow().states.last().expect("a status")
    }
}

struct Source(Shared);

#[system]
fn source(#[state] s: &mut Source, commands: &mut Output<SequenceCommand>) {
    while let Some(command) = s.0.0.borrow_mut().queue.pop_front() {
        let _ = commands.write(&command);
    }
}

struct Counter {
    shared: Shared,
    count: u64,
}

impl Counter {
    fn new(shared: Shared) -> Self {
        shared.0.borrow_mut().makes += 1;
        Self { shared, count: 0 }
    }

    fn try_new(shared: Shared, _: NoParams) -> Result<Self, crate::coordinator::ParamError> {
        if shared.0.borrow().make_fails {
            return Err(crate::coordinator::ParamError::Decode(
                "no counter today".into(),
            ));
        }
        Ok(Self::new(shared))
    }

    fn tick(&mut self) {
        self.count += 1;
        let mut seen = self.shared.0.borrow_mut();
        seen.counts.push(self.count);
        let panics = seen.panic_at == Some(self.count);
        drop(seen);
        assert!(!panics, "counter panics at {}", self.count);
    }
}

/// Counts cycles and aborts, reading the imu so a remake needs a reader slot.
#[system]
fn counter(#[state] c: &mut Counter, imu: &mut Input<Imu>, abort: &mut Abort) {
    let _ = imu.latest();
    if abort.requested() {
        c.shared.0.borrow_mut().aborts += 1;
    }
    c.tick();
}

/// Counts cycles, with no abort port.
#[system]
fn plain(#[state] c: &mut Counter) {
    c.tick();
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoParams {}

/// Counts cycles and never reads its abort.
#[system]
fn deaf(#[state] c: &mut Counter, _abort: &mut Abort) {
    c.tick();
}

/// Ticks once a cycle until aborted.
#[crate::sequence]
async fn ticker(#[state] c: &mut Counter, seq: &Seq) -> Result<(), SeqError> {
    loop {
        c.tick();
        seq.next_cycle().await;
        if seq.aborted() {
            return Err(SeqError::Aborted);
        }
    }
}

struct Watch(Shared);

#[system]
fn watch(
    #[state] w: &mut Watch,
    events: &mut Input<SequenceChannelEvent>,
    status: &mut Input<ChannelStatus>,
) {
    let mut seen = w.0.0.borrow_mut();
    seen.events.extend(events.drain().filter_map(Result::ok));
    if let Ok(Some(status)) = status.latest() {
        seen.states.push(status.state);
        seen.status = Some(*status);
    }
}

/// A step that loads `a` or `b` and reports which; `none` empties it.
struct Slot {
    shared: Shared,
    occupant: Option<Occupant>,
    end: Option<EndReason>,
}

impl Step for Slot {
    fn execute(&mut self, _now: Timestamp) {
        self.end = self.shared.0.borrow().slot_end.clone();
    }

    fn ended(&self) -> Option<&EndReason> {
        match self.occupant {
            None => Some(&EndReason::Unloaded),
            Some(_) => self.end.as_ref(),
        }
    }

    fn load(&mut self, name: &str) -> Result<(), LoadError> {
        if name == "none" {
            self.occupant = None;
            return Ok(());
        }
        let index = ["a", "b"]
            .iter()
            .position(|known| *known == name)
            .ok_or_else(|| LoadError::Unknown(name.into()))?;
        self.occupant = Some(Occupant {
            index: index as u32,
            artifact: FrameStr::new(&format!("lib{name}.so")).expect("short"),
            hash: FrameBytes([index as u8 + 1; 32]),
        });
        Ok(())
    }

    fn occupant(&self) -> Option<&Occupant> {
        self.occupant.as_ref()
    }
}

#[allow(clippy::arc_with_non_send_sync)]
fn slot_entry(shared: &Shared) -> TableEntry {
    let shared = shared.clone();
    let def = SystemDef {
        name: "slot".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    TableEntry {
        def: def.clone(),
        def_fn: Box::new(move |_| Ok(def.clone())),
        doc: "".into(),
        schema: None,
        make: Arc::new(move |_| {
            Ok(Box::new(Slot {
                shared: shared.clone(),
                occupant: None,
                end: None,
            }))
        }),
    }
}

fn table(shared: &Shared) -> SystemTable {
    let mut table = SystemTable::new();
    let s = shared.clone();
    table
        .register("source", source(), move || Source(s.clone()))
        .expect("valid");
    let s = shared.clone();
    table
        .register("counter", counter(), move || Counter::new(s.clone()))
        .expect("valid");
    let s = shared.clone();
    table
        .register("plain", plain(), move || Counter::new(s.clone()))
        .expect("valid");
    let s = shared.clone();
    table
        .register("deaf", deaf(), move |p| Counter::try_new(s.clone(), p))
        .expect("valid");
    let s = shared.clone();
    table
        .register("watch", watch(), move || Watch(s.clone()))
        .expect("valid");
    table
        .register_system("imu", |_| Ok((ImuSource, 0)))
        .expect("valid");
    let s = shared.clone();
    table
        .register_sequence("ticker", ticker(), move || Counter::new(s.clone()))
        .expect("valid");
    table.insert("slot", slot_entry(shared)).expect("valid");
    table
}

fn channel(name: &str, start: RunState) -> Option<Adapter> {
    Some(Adapter::Channel(ChannelConfig {
        name: name.into(),
        start,
    }))
}

fn input(port: &str, system: &str, from: &str) -> InputConfig {
    InputConfig {
        port: port.into(),
        from: vec![PortRef::new(system, from)],
    }
}

/// `cmds -> imu -> {ty} as channel "ctr" -> watch`.
fn config(ty: &str, start: RunState) -> CoordinatorConfig {
    let mut inputs = vec![input("commands", "cmds", "commands")];
    if ty == "counter" {
        inputs.push(input("imu", "imu", "imu"));
    }
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "source"),
            SystemConfig::new("imu", "imu"),
            SystemConfig {
                inputs,
                adapter: channel("ctr", start),
                ..SystemConfig::new("ctr", ty)
            },
            SystemConfig {
                inputs: vec![
                    input("events", "ctr", "events"),
                    input("status", "ctr", "channel_status"),
                ],
                ..SystemConfig::new("watch", "watch")
            },
        ],
        ..Default::default()
    }
}

fn build(ty: &str, start: RunState) -> (Shared, Coordinator) {
    let shared = Shared::default();
    let coordinator = config(ty, start)
        .build(&table(&shared))
        .expect("a valid config");
    (shared, coordinator)
}

#[test]
fn test_stop_skips_execution() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.counts(), [1]);
    assert_eq!(shared.state(), ChannelState::Running.code());

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    assert!(shared.counts().is_empty());
    assert_eq!(shared.events(), [SequenceEventKind::Stopped]);
    assert_eq!(shared.state(), ChannelState::Stopped.code());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert_eq!(shared.counts(), [2]);
    assert_eq!(shared.events(), [SequenceEventKind::Started]);
}

#[test]
fn test_start_at_boot_stopped() {
    let (shared, mut coordinator) = build("counter", RunState::Stopped);
    coordinator.step(Timestamp(1));
    assert!(shared.counts().is_empty());
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_reset_makes_fresh_step() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    assert_eq!(shared.counts(), [1, 2]);

    shared.send("ctr", SequenceCommandKind::Stop);
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(Timestamp(3));
    assert_eq!(shared.0.borrow().makes, 2);
    let loaded = SequenceEventKind::Loaded { name: "ctr".into() };
    assert_eq!(shared.events(), [SequenceEventKind::Stopped, loaded]);
    assert_eq!(shared.state(), ChannelState::Stopped.code());

    // The imu ring has one reader slot, so the new step's view took the old one's.
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_inner_panic_keeps_channel() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.0.borrow_mut().panic_at = Some(2);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    assert_eq!(coordinator.latched().count(), 0);
    let panicked = SequenceEventKind::Failed {
        reason: "panicked".into(),
    };
    assert_eq!(shared.events(), [panicked]);
    assert_eq!(shared.state(), ChannelState::Panicked.code());
    assert_eq!(shared.counts(), [1, 2]);

    shared.0.borrow_mut().panic_at = None;
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert!(matches!(
        shared.events().as_slice(),
        [SequenceEventKind::Refused { .. }]
    ));

    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(5));
    assert_eq!(shared.counts(), [1]);
    assert_eq!(shared.state(), ChannelState::Running.code());
}

#[test]
fn test_abort_reaches_system() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.0.borrow().aborts, 1);
    assert!(matches!(
        shared.events().as_slice(),
        [SequenceEventKind::Progress { .. }]
    ));
}

#[test]
fn test_abort_without_port_refused() {
    let (shared, mut coordinator) = build("plain", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(1));
    let events = shared.events();
    let [SequenceEventKind::Refused { reason }] = events.as_slice() else {
        panic!("one refusal")
    };
    assert!(reason.contains("abort"), "{reason}");
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_other_channel_ignored() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("elsewhere", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(1));
    assert!(shared.events().is_empty());
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_load_refused_without_slot() {
    let (shared, mut coordinator) = build("counter", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    let events = shared.events();
    let [SequenceEventKind::Refused { reason }] = events.as_slice() else {
        panic!("one refusal")
    };
    assert!(reason.contains("nothing can be loaded"), "{reason}");
}

#[test]
fn test_load_reports_occupant() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.events(), [SequenceEventKind::Unloaded]);
    assert_eq!(shared.state(), ChannelState::Unloaded.code());
    let status = shared.0.borrow().status.expect("a status");
    assert_eq!(status.loaded, u32::MAX);
    assert_eq!(status.artifact, FrameStr::EMPTY);

    shared.send("ctr", SequenceCommandKind::Start);
    shared.send("ctr", SequenceCommandKind::Load { name: "b".into() });
    coordinator.step(Timestamp(2));
    let events = shared.events();
    assert!(matches!(events[0], SequenceEventKind::Refused { .. }));
    assert_eq!(events[1], SequenceEventKind::Loaded { name: "b".into() });
    let status = shared.0.borrow().status.expect("a status");
    assert_eq!(status.state, ChannelState::Stopped.code());
    assert_eq!(status.loaded, 1);
    assert_eq!(status.artifact.to_str(), Ok("libb.so"));
    assert_eq!(status.hash, FrameBytes([2; 32]));
}

#[test]
fn test_reset_reapplies_load() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "b".into() });
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(Timestamp(1));
    let events = shared.events();
    assert_eq!(
        events.last(),
        Some(&SequenceEventKind::Loaded { name: "b".into() })
    );
    let status = shared.0.borrow().status.expect("a status");
    assert_eq!(status.loaded, 1);
}

#[test]
fn test_unknown_load_keeps_occupant() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    shared.send("ctr", SequenceCommandKind::Load { name: "z".into() });
    coordinator.step(Timestamp(1));
    let events = shared.events();
    assert_eq!(events[0], SequenceEventKind::Loaded { name: "a".into() });
    assert!(matches!(&events[1], SequenceEventKind::Refused { reason } if reason.contains("`z`")));
    assert_eq!(shared.0.borrow().status.expect("a status").loaded, 0);
}

#[test]
fn test_reserved_ports_refused() {
    let shared = Shared::default();
    let mut into_abort = config("counter", RunState::Running);
    into_abort.systems[2]
        .inputs
        .push(input("abort", "cmds", "commands"));
    assert!(matches!(
        into_abort.build(&table(&shared)),
        Err(BuildError::ReservedChannelPort { port, .. }) if port == "abort"
    ));

    // `watch` declares an input named `events`, which a channel takes.
    let mut watched = config("counter", RunState::Running);
    watched.systems[3].adapter = channel("w", RunState::Running);
    assert!(matches!(
        watched.build(&table(&shared)),
        Err(BuildError::ReservedChannelPort { port, .. }) if port == "events"
    ));
}

#[test]
fn test_channel_def_appends_ports() {
    let inner = SystemDef {
        name: "x".into(),
        inputs: vec![Input::<Imu>::def(&DefCx::empty(), "imu")],
        outputs: Vec::new(),
    };
    let def = crate::channel::def(inner, &DefCx::empty()).expect("no reserved ports");
    let inputs: Vec<_> = def.inputs.iter().map(|p| p.name.as_ref()).collect();
    let outputs: Vec<_> = def.outputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(inputs, ["imu", "commands"]);
    assert_eq!(outputs, ["events", "channel_status"]);
}

#[test]
fn test_sequence_resumes_at_its_await() {
    let (shared, mut coordinator) = build("ticker", RunState::Running);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    assert_eq!(shared.counts(), [1, 2]);

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(3));
    coordinator.step(Timestamp(4));
    assert!(shared.counts().is_empty());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(5));
    assert_eq!(shared.counts(), [3]);
    assert_eq!(shared.0.borrow().makes, 1);
}

#[test]
fn test_abort_ends_sequence_channel() {
    let (shared, mut coordinator) = build("ticker", RunState::Running);
    coordinator.step(Timestamp(1));
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    let events = shared.events();
    assert!(
        matches!(
            events.as_slice(),
            [
                SequenceEventKind::Progress { .. },
                SequenceEventKind::Aborted
            ]
        ),
        "{events:?}"
    );
    assert_eq!(shared.state(), ChannelState::Aborted.code());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert!(matches!(
        shared.events().as_slice(),
        [SequenceEventKind::Refused { .. }]
    ));
}

/// A `Load` on a channel whose step is gone is refused and changes nothing.
#[test]
fn test_load_on_gone_step_changes_nothing() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.0.borrow_mut().panic_at = Some(1);
    coordinator.step(Timestamp(1));
    shared.events();
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(2));
    let events = shared.events();
    assert!(
        matches!(&events[..], [SequenceEventKind::Refused { reason }] if reason.contains("reset"))
    );
    assert_eq!(shared.state(), ChannelState::Panicked.code());
    assert_eq!(shared.0.borrow().makes, 1);
}

/// A `Reset` whose make fails ends the channel as failed; the next one works.
#[test]
fn test_failed_remake_can_be_retried() {
    let (shared, mut coordinator) = build("deaf", RunState::Stopped);
    shared.0.borrow_mut().make_fails = true;
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(Timestamp(1));
    let events = shared.events();
    assert!(
        matches!(&events[..], [SequenceEventKind::Failed { reason }] if reason.contains("no counter"))
    );
    assert_eq!(shared.state(), ChannelState::Failed.code());
    coordinator.step(Timestamp(2));
    assert!(shared.events().is_empty(), "an end is reported once");

    shared.0.borrow_mut().make_fails = false;
    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(3));
    assert_eq!(shared.state(), ChannelState::Running.code());
    assert_eq!(shared.counts(), [1]);
}

/// An abort the ring cannot take is refused, not reported as requested.
#[test]
fn test_abort_ring_full_refused() {
    let (shared, mut coordinator) = build("deaf", RunState::Running);
    for cycle in 1..=4 {
        shared.send("ctr", SequenceCommandKind::Abort);
        coordinator.step(Timestamp(cycle));
        assert!(matches!(
            shared.events()[..],
            [SequenceEventKind::Progress { .. }]
        ));
    }
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(5));
    let events = shared.events();
    assert!(
        matches!(&events[..], [SequenceEventKind::Refused { reason }] if reason.contains("full"))
    );
}

/// A reason longer than the cap is cut on a char boundary.
#[test]
fn test_long_reason_is_clipped() {
    assert_eq!(super::clipped("short".into()), "short");
    let long = "é".repeat(400);
    let cut = super::clipped(long);
    assert!(cut.len() <= super::REASON_CAP && cut.len() > super::REASON_CAP - 2);
    assert!(cut.chars().all(|c| c == 'é'));
}

/// An occupant that completes and is then unloaded reports both ends.
#[test]
fn test_completed_then_unloaded_reported() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    shared.0.borrow_mut().slot_end = Some(EndReason::Completed);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    let events = shared.events();
    assert_eq!(events[events.len() - 1], SequenceEventKind::Completed);

    shared.0.borrow_mut().slot_end = None;
    shared.send(
        "ctr",
        SequenceCommandKind::Load {
            name: "none".into(),
        },
    );
    coordinator.step(Timestamp(3));
    let events = shared.events();
    assert_eq!(
        events.last(),
        Some(&SequenceEventKind::Unloaded),
        "{events:?}"
    );
}
