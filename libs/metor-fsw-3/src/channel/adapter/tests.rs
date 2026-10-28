use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceAccepts, SequenceChannelState, SequenceCommand, SequenceCommandKind,
};

use crate::channel::{Abort, ChannelMessage, ChannelStatus};
use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, Coordinator, CoordinatorConfig, EndReason, InputConfig,
    LoadError, Occupant, RunState, Step, SystemConfig, SystemKind, SystemTable, TableEntry,
};
use crate::def::DefCx;
use crate::port::{Input, Output};
use crate::sequence::{Seq, SeqError};
use crate::system::SystemDef;
use crate::tests::utils::{Imu, NoParams, imu_source};
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
    status: Option<ChannelStatus>,
    /// Each message `watch` saw, with when it was raised.
    messages: Vec<(String, i64)>,
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

    /// The messages raised since the last call.
    fn messages(&self) -> Vec<String> {
        self.stamped().into_iter().map(|(text, _)| text).collect()
    }

    /// The messages raised since the last call, each with its timestamp.
    fn stamped(&self) -> Vec<(String, i64)> {
        std::mem::take(&mut self.0.borrow_mut().messages)
    }

    /// The one message raised since the last call.
    fn message(&self) -> String {
        let messages = self.messages();
        let [message] = messages.as_slice() else {
            panic!("one message, got {messages:?}")
        };
        message.clone()
    }

    fn status(&self) -> ChannelStatus {
        self.0.borrow().status.expect("a status")
    }

    fn counts(&self) -> Vec<u64> {
        std::mem::take(&mut self.0.borrow_mut().counts)
    }

    fn state(&self) -> u32 {
        self.status().state
    }

    fn accepts(&self) -> u32 {
        self.status().accepts
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

    fn try_new(shared: Shared, _: NoParams) -> Result<Self, crate::coordinator::MakeError> {
        if shared.0.borrow().make_fails {
            return Err(crate::coordinator::ParamError::Decode("no counter today".into()).into());
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

/// Records the channel's latest status and every message.
#[system]
fn watch(
    #[state] w: &mut Watch,
    channel_status: &mut Input<ChannelStatus>,
    channel_message: &mut Input<ChannelMessage>,
) {
    let mut seen = w.0.0.borrow_mut();
    for message in channel_message.drain() {
        let message = message.expect("a message");
        seen.messages
            .push((message.as_str().to_string(), message.timestamp.0));
    }
    if let Ok(Some(status)) = channel_status.latest() {
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
    fn execute(&mut self, _now: Timestamp, _cx: &mut crate::coordinator::StepCx<'_>) {
        self.end = self.shared.0.borrow().slot_end.clone();
    }

    fn ended(&self) -> Option<&EndReason> {
        match self.occupant {
            None => Some(&EndReason::Unloaded),
            Some(_) => self.end.as_ref(),
        }
    }

    fn load(
        &mut self,
        name: &str,
        _cx: &mut crate::coordinator::StepCx<'_>,
    ) -> Result<(), LoadError> {
        self.end = None;
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

    fn loads(&self) -> bool {
        true
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
        log: None,
        grants: Vec::new(),
        io: None,
    };
    TableEntry {
        kind: SystemKind::Cyclic,
        adapters: SystemKind::Cyclic.adapters(),
        def: def.clone(),
        takes_inputs: false,
        takes_outputs: false,
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
    let mut register = || -> Result<(), BuildError> {
        let s = shared.clone();
        table.register("source", source(), move || Source(s.clone()))?;
        let s = shared.clone();
        table.register("counter", counter(), move || Counter::new(s.clone()))?;
        let s = shared.clone();
        table.register("plain", plain(), move || Counter::new(s.clone()))?;
        let s = shared.clone();
        table.register("deaf", deaf(), move |p| Counter::try_new(s.clone(), p))?;
        let s = shared.clone();
        table.register("watch", watch(), move || Watch(s.clone()))?;
        table.register("imu", imu_source(), i64::default)?;
        let s = shared.clone();
        table.register_sequence("ticker", ticker(), move || Counter::new(s.clone()))?;
        table.insert("slot", slot_entry(shared))?;
        Ok(())
    };
    register().expect("valid");
    table
}

fn channel(name: &str, start: RunState) -> Option<Adapter> {
    Some(Adapter::Channel(ChannelConfig {
        name: crate::ChannelName::try_from(name.to_string()).expect("a short name"),
        start,
    }))
}

/// A config where `cmds` commands a `ty` system behind channel `ctr`, and
/// `watch` records the channel's status and messages.
fn config(ty: &str, start: RunState) -> CoordinatorConfig {
    let mut inputs = vec![InputConfig::new("commands", "cmds", "commands")];
    if ty == "counter" {
        inputs.push(InputConfig::new("imu", "imu", "imu"));
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
                    InputConfig::new("channel_status", "ctr", "channel_status"),
                    InputConfig::new("channel_message", "ctr", "channel_message"),
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
fn test_boot_running_reports_running() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(Timestamp(1));
    let status = shared.status();
    assert_eq!(status.state, SequenceChannelState::Running.code());
    assert_eq!(
        status.accepts,
        SequenceAccepts::ABORT | SequenceAccepts::STOP
    );
    assert!(shared.messages().is_empty(), "booting raises no message");
}

#[test]
fn test_stop_skips_execution() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.counts(), [1]);
    assert_eq!(shared.state(), SequenceChannelState::Running.code());

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    assert!(shared.counts().is_empty());
    assert_eq!(
        shared.stamped(),
        [("stopped".to_string(), 2)],
        "stamped with its cycle"
    );
    assert_eq!(shared.state(), SequenceChannelState::Stopped.code());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert_eq!(shared.counts(), [2]);
    assert_eq!(shared.message(), "started");
}

#[test]
fn test_start_at_boot_stopped() {
    let (shared, mut coordinator) = build("counter", RunState::Stopped);
    coordinator.step(Timestamp(1));
    assert!(shared.counts().is_empty());
    assert_eq!(shared.state(), SequenceChannelState::Stopped.code());
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_accepts_follows_state() {
    let (shared, mut coordinator) = build("counter", RunState::Stopped);
    coordinator.step(Timestamp(1));
    // A counter loads nothing, so `Load` is never offered.
    let stopped = SequenceAccepts::START | SequenceAccepts::RESET;
    assert_eq!(shared.accepts(), stopped);

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert_eq!(
        shared.accepts(),
        SequenceAccepts::ABORT | SequenceAccepts::STOP
    );
}

#[test]
fn test_accepts_omits_abort_without_port() {
    let (shared, mut coordinator) = build("plain", RunState::Running);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.accepts(), SequenceAccepts::STOP);
}

#[test]
fn test_accepts_after_end() {
    let (shared, mut coordinator) = build("ticker", RunState::Running);
    coordinator.step(Timestamp(1));
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(2));
    assert_eq!(shared.state(), SequenceChannelState::Aborted.code());
    assert_eq!(shared.accepts(), SequenceAccepts::RESET);
}

#[test]
fn test_accepts_offers_load_on_a_slot() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    coordinator.step(Timestamp(1));
    assert_eq!(
        shared.accepts() & SequenceAccepts::LOAD,
        SequenceAccepts::LOAD,
        "a slot loads even while it holds nothing"
    );
}

#[test]
fn test_accepts_only_reset_once_the_system_is_gone() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.0.borrow_mut().panic_at = Some(1);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.state(), SequenceChannelState::Panicked.code());
    assert_eq!(shared.accepts(), SequenceAccepts::RESET);
}

#[test]
fn test_reset_makes_fresh_step() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    assert_eq!(shared.counts(), [1, 2]);

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(3));
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(Timestamp(4));
    assert_eq!(shared.0.borrow().makes, 2);
    assert_eq!(shared.messages(), ["stopped", "reset"]);
    assert_eq!(shared.state(), SequenceChannelState::Stopped.code());

    // The imu ring has one reader slot, so the new step's view took the old one's.
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(5));
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_inner_panic_keeps_channel() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.0.borrow_mut().panic_at = Some(2);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked).len(),
        0
    );
    assert_eq!(shared.messages(), ["panicked"]);
    assert_eq!(shared.state(), SequenceChannelState::Panicked.code());
    assert_eq!(shared.counts(), [1, 2]);

    shared.0.borrow_mut().panic_at = None;
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert!(shared.message().starts_with("refused: "));

    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(5));
    assert_eq!(shared.counts(), [1]);
    assert_eq!(shared.state(), SequenceChannelState::Running.code());
    assert_eq!(shared.messages(), ["reset", "started"]);
}

/// Microseconds in `n` seconds.
fn secs(n: i64) -> Timestamp {
    Timestamp(n * 1_000_000)
}

/// The run and abort clocks from the latest status.
fn clocks(shared: &Shared) -> (f64, f64) {
    let status = shared.status();
    (status.elapsed, status.abort_elapsed)
}

#[test]
fn test_run_clock_pauses_while_stopped_and_resets() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(secs(1));
    assert_eq!(clocks(&shared), (0.0, 0.0));
    coordinator.step(secs(3));
    assert_eq!(clocks(&shared), (2.0, 0.0));

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(secs(4));
    coordinator.step(secs(10));
    assert_eq!(clocks(&shared), (3.0, 0.0), "a stop pauses the clock");

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(secs(11));
    coordinator.step(secs(12));
    assert_eq!(clocks(&shared), (4.0, 0.0), "a start resumes it");

    shared.send("ctr", SequenceCommandKind::Stop);
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(secs(13));
    assert_eq!(clocks(&shared), (0.0, 0.0), "a reset starts a fresh run");
}

#[test]
fn test_abort_clock_counts_while_winding_down() {
    // The counter hears the abort but keeps running, as a sequence winding
    // down gracefully does.
    let (shared, mut coordinator) = build("counter", RunState::Running);
    coordinator.step(secs(1));
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(secs(3));
    assert_eq!(
        clocks(&shared),
        (2.0, 0.0),
        "the abort clock starts at the abort"
    );
    coordinator.step(secs(5));
    assert_eq!(clocks(&shared), (4.0, 2.0));

    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(secs(6));
    coordinator.step(secs(9));
    assert_eq!(clocks(&shared), (5.0, 3.0), "a stop pauses both clocks");
}

#[test]
fn test_an_ended_run_holds_its_clock_and_clears_the_abort() {
    let (shared, mut coordinator) = build("ticker", RunState::Running);
    coordinator.step(secs(1));
    coordinator.step(secs(2));
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(secs(3));
    assert_eq!(shared.state(), SequenceChannelState::Aborted.code());
    assert_eq!(clocks(&shared), (2.0, 0.0));
    coordinator.step(secs(20));
    assert_eq!(
        clocks(&shared),
        (2.0, 0.0),
        "the run clock holds once ended"
    );
}

#[test]
fn test_abort_reaches_system() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.0.borrow().aborts, 1);
    assert_eq!(shared.message(), "abort requested");
}

#[test]
fn test_abort_without_port_refused() {
    let (shared, mut coordinator) = build("plain", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(1));
    assert_eq!(
        shared.stamped(),
        [("refused: the system takes no abort".to_string(), 1)]
    );
    assert_eq!(shared.state(), SequenceChannelState::Running.code());
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_repeated_refusal_is_a_new_message() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(1));
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    let refused = "refused: the channel is running".to_string();
    assert_eq!(shared.stamped(), [(refused.clone(), 1), (refused, 2)]);
}

#[test]
fn test_repeated_message_in_one_cycle_is_two_records() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Start);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(1));
    let refused = "refused: the channel is running".to_string();
    assert_eq!(shared.stamped(), [(refused.clone(), 1), (refused, 1)]);
}

#[test]
fn test_each_message_in_a_cycle_is_written() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(1));
    assert_eq!(
        shared.stamped(),
        [
            ("refused: the channel is running".to_string(), 1),
            ("stopped".to_string(), 1),
        ]
    );
}

#[test]
fn test_message_written_only_when_raised() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("ctr", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    coordinator.step(Timestamp(3));
    assert_eq!(shared.stamped(), [("stopped".to_string(), 1)]);
}

#[test]
fn test_other_channel_ignored() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.send("elsewhere", SequenceCommandKind::Stop);
    coordinator.step(Timestamp(1));
    assert!(shared.messages().is_empty());
    assert_eq!(shared.counts(), [1]);
}

#[test]
fn test_load_refused_without_slot() {
    let (shared, mut coordinator) = build("counter", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    let message = shared.message();
    assert!(message.contains("nothing can be loaded"), "{message}");
    assert_eq!(shared.state(), SequenceChannelState::Stopped.code());
}

#[test]
fn test_load_reports_occupant() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    coordinator.step(Timestamp(1));
    assert_eq!(shared.message(), "unloaded");
    assert_eq!(shared.state(), SequenceChannelState::Unloaded.code());
    let status = shared.status();
    assert_eq!(status.loaded, u32::MAX);
    assert_eq!(status.artifact, FrameStr::default());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert!(shared.message().starts_with("refused: "));
    assert_eq!(shared.state(), SequenceChannelState::Unloaded.code());

    shared.send("ctr", SequenceCommandKind::Load { name: "b".into() });
    coordinator.step(Timestamp(3));
    assert_eq!(shared.message(), "loaded b");
    let status = shared.status();
    assert_eq!(status.state, SequenceChannelState::Stopped.code());
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
    assert_eq!(shared.messages(), ["loaded b", "loaded b"]);
    assert_eq!(shared.status().loaded, 1);
}

#[test]
fn test_unknown_load_keeps_occupant() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    assert_eq!(shared.message(), "loaded a");
    shared.send("ctr", SequenceCommandKind::Load { name: "z".into() });
    coordinator.step(Timestamp(2));
    let message = shared.message();
    assert!(
        message.starts_with("refused: ") && message.contains("`z`"),
        "{message}"
    );
    assert_eq!(shared.status().loaded, 0);
}

/// A message longer than a record holds is cut on a char boundary.
#[test]
fn test_long_message_is_clipped() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    coordinator.step(Timestamp(1));
    shared.messages();
    let name = "é".repeat(100);
    shared.send("ctr", SequenceCommandKind::Load { name });
    coordinator.step(Timestamp(2));
    let message = shared.message();
    let cap = crate::channel::MESSAGE_LEN - 1;
    assert!(message.len() <= cap && message.len() > cap - 2, "{message}");
    assert!(message.starts_with("refused: "));
    assert!(message.ends_with('é'));
}

#[test]
fn test_reserved_ports_refused() {
    let shared = Shared::default();
    let mut into_abort = config("counter", RunState::Running);
    into_abort.systems[2]
        .inputs
        .push(InputConfig::new("abort", "cmds", "commands"));
    assert!(matches!(
        into_abort.build(&table(&shared)),
        Err(BuildError::ReservedChannelPort { port, .. }) if port == "abort"
    ));

    // `watch` declares an input named `channel_status`, which a channel takes.
    let mut watched = config("counter", RunState::Running);
    watched.systems[3].adapter = channel("w", RunState::Running);
    assert!(matches!(
        watched.build(&table(&shared)),
        Err(BuildError::ReservedChannelPort { port, .. }) if port == "channel_status"
    ));
}

#[test]
fn test_channel_def_appends_ports() {
    let inner = SystemDef {
        name: "x".into(),
        inputs: vec![Input::<Imu>::def(&DefCx::empty(), "imu")],
        outputs: Vec::new(),
        log: None,
        grants: Vec::new(),
        io: None,
    };
    let def = crate::channel::def(inner, &DefCx::empty()).expect("no reserved ports");
    let inputs: Vec<_> = def.inputs.iter().map(|p| p.name.as_ref()).collect();
    let outputs: Vec<_> = def.outputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(inputs, ["imu", "commands"]);
    assert_eq!(outputs, ["channel_status", "channel_message"]);
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
    let messages = shared.messages();
    assert_eq!(messages.last().map(String::as_str), Some("aborted"));
    assert_eq!(shared.state(), SequenceChannelState::Aborted.code());

    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(4));
    assert!(shared.message().starts_with("refused: "));
    assert_eq!(shared.state(), SequenceChannelState::Aborted.code());
}

/// A `Load` on a channel whose step is gone is refused and changes nothing.
#[test]
fn test_load_on_gone_step_changes_nothing() {
    let (shared, mut coordinator) = build("counter", RunState::Running);
    shared.0.borrow_mut().panic_at = Some(1);
    coordinator.step(Timestamp(1));
    shared.messages();
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(2));
    let message = shared.message();
    assert!(
        message.starts_with("refused: ") && message.contains("reset"),
        "{message}"
    );
    assert_eq!(shared.state(), SequenceChannelState::Panicked.code());
    assert_eq!(shared.0.borrow().makes, 1);
}

/// A `Reset` whose make fails ends the channel as failed; the next one works.
#[test]
fn test_failed_remake_can_be_retried() {
    let (shared, mut coordinator) = build("deaf", RunState::Stopped);
    shared.0.borrow_mut().make_fails = true;
    shared.send("ctr", SequenceCommandKind::Reset);
    coordinator.step(Timestamp(1));
    let message = shared.message();
    assert!(
        message.starts_with("failed: ") && message.contains("no counter"),
        "{message}"
    );
    assert_eq!(shared.state(), SequenceChannelState::Failed.code());
    coordinator.step(Timestamp(2));
    assert!(shared.messages().is_empty(), "an end is reported once");

    shared.0.borrow_mut().make_fails = false;
    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(3));
    assert_eq!(shared.state(), SequenceChannelState::Running.code());
    assert_eq!(shared.counts(), [1]);
}

/// An abort the ring cannot take is refused, not reported as requested.
#[test]
fn test_abort_ring_full_refused() {
    let (shared, mut coordinator) = build("deaf", RunState::Running);
    for cycle in 1..=4 {
        shared.send("ctr", SequenceCommandKind::Abort);
        coordinator.step(Timestamp(cycle));
        assert_eq!(shared.message(), "abort requested");
    }
    shared.send("ctr", SequenceCommandKind::Abort);
    coordinator.step(Timestamp(5));
    assert_eq!(shared.message(), "refused: the abort ring is full");
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
    assert_eq!(
        shared.messages().last().map(String::as_str),
        Some("completed")
    );
    assert_eq!(shared.state(), SequenceChannelState::Completed.code());

    shared.0.borrow_mut().slot_end = None;
    shared.send(
        "ctr",
        SequenceCommandKind::Load {
            name: "none".into(),
        },
    );
    coordinator.step(Timestamp(3));
    assert_eq!(shared.messages(), ["loaded none", "unloaded"]);
    assert_eq!(shared.state(), SequenceChannelState::Unloaded.code());
}

/// A fresh run that ends as the last one did still reports its end.
#[test]
fn test_same_end_after_reset_reported_again() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    shared.0.borrow_mut().slot_end = Some(EndReason::Completed);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert_eq!(
        shared.messages().last().map(String::as_str),
        Some("completed")
    );

    shared.send("ctr", SequenceCommandKind::Reset);
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(3));
    assert_eq!(shared.messages(), ["loaded a", "started", "completed"]);
    assert_eq!(shared.state(), SequenceChannelState::Completed.code());
}

/// A run that fails reports why.
#[test]
fn test_failed_run_reports_its_reason() {
    let (shared, mut coordinator) = build("slot", RunState::Stopped);
    shared.send("ctr", SequenceCommandKind::Load { name: "a".into() });
    coordinator.step(Timestamp(1));
    shared.messages();
    shared.0.borrow_mut().slot_end = Some(EndReason::Failed("sensor lost".into()));
    shared.send("ctr", SequenceCommandKind::Start);
    coordinator.step(Timestamp(2));
    assert_eq!(
        shared.stamped(),
        [
            ("started".to_string(), 2),
            ("failed: sensor lost".to_string(), 2),
        ]
    );
    assert_eq!(shared.state(), SequenceChannelState::Failed.code());
}
