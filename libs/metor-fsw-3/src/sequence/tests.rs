//! Sequence steps, driven by hand and by a coordinator.

use core::cell::{Cell, RefCell};
use core::time::Duration;
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use super::*;
use crate::channel::AbortRequest;
use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, Coordinator, CoordinatorConfig, EndReason, InputConfig,
    PortRef, RunState, Step, SystemConfig, SystemTable,
};
use crate::log::Log;
use crate::port::{Output, Sample, ring_capacity};
use crate::system::{InputBinding, OutputBinding};
use crate::tests::utils::{self, Imu, Recorder};

/// The phase a sequence has reached, shared with the test.
#[derive(Clone, Default)]
struct Phase(Rc<Cell<u32>>);

impl Phase {
    fn get(&self) -> u32 {
        self.0.get()
    }
}

/// Marks three phases, one cycle apart.
#[crate::sequence]
async fn phases(#[state] phase: &mut Phase, seq: &Seq) -> Result<(), SeqError> {
    for at in 1..=3 {
        phase.0.set(at);
        if at < 3 {
            seq.next_cycle().await;
        }
    }
    Ok(())
}

/// Sleeps 10 ms, then marks its phase.
#[crate::sequence]
async fn napper(#[state] phase: &mut Phase, seq: &Seq) -> Result<(), SeqError> {
    seq.sleep(Duration::from_millis(10)).await?;
    phase.0.set(1);
    Ok(())
}

/// Fails once the second cycle comes.
#[crate::sequence]
async fn no_fix(seq: &Seq, log: &mut Log) -> Result<(), SeqError> {
    log.info("waiting for a fix");
    seq.next_cycle().await;
    Err(SeqError::Failed("no fix".into()))
}

/// Waits an hour, which only an abort cuts short.
#[crate::sequence]
async fn patient(seq: &Seq) -> Result<(), SeqError> {
    seq.sleep(Duration::from_secs(3600)).await
}

/// Panics on its second poll.
#[crate::sequence]
async fn doomed(seq: &Seq) -> Result<(), SeqError> {
    seq.next_cycle().await;
    panic!("doomed on its second poll")
}

/// Writes one sample, then ends.
#[crate::sequence]
async fn one_sample(seq: &Seq, imu: &mut Output<Imu>) -> Result<(), SeqError> {
    let _ = imu.write(&Imu {
        timestamp: seq.now(),
        sample: 1.0,
    });
    Ok(())
}

/// Asks for an abort from its second cycle on.
#[crate::system]
fn aborter(#[state] cycles: &mut u64, abort: &mut Output<AbortRequest>) {
    *cycles += 1;
    if *cycles >= 2 {
        let _ = abort.write(&AbortRequest {});
    }
}

fn ms(ms: i64) -> Timestamp {
    Timestamp(ms * 1_000)
}

/// A sequence's definition under the empty context.
fn def_of<S: SequenceFn>(_sequence: S) -> crate::system::SystemDef {
    def::<S>(&crate::def::DefCx::empty()).expect("a static definition")
}

/// One step of `S` over fresh rings, with nothing feeding its inputs.
fn step_of<S: SequenceFn>(_sequence: S, state: S::State) -> (SequenceStep, Vec<RingBuffer>) {
    let def = def::<S>(&crate::def::DefCx::empty()).expect("a static definition");
    let inputs = def
        .inputs
        .iter()
        .map(|def| InputBinding {
            def: def.clone(),
            views: Vec::new(),
        })
        .collect();
    let rings: Vec<RingBuffer> = def
        .outputs
        .iter()
        .map(|port| {
            RingBuffer::create_in_memory(Config {
                capacity: ring_capacity(port.max_len, 8).expect("valid capacity"),
                max_readers: 1,
            })
        })
        .collect();
    let outputs = def
        .outputs
        .iter()
        .zip(&rings)
        .map(|(def, ring)| OutputBinding {
            def: def.clone(),
            writer: ring.writer(NoWake).expect("a free writer"),
        })
        .collect();
    (SequenceStep::new::<S>(state, inputs, outputs), rings)
}

fn table(recorder: &Recorder) -> SystemTable {
    let mut table = utils::table(recorder);
    table
        .register_sequence("no_fix", no_fix(), Default::default)
        .expect("valid records");
    table
        .register_sequence("patient", patient(), Default::default)
        .expect("valid records");
    table
        .register_sequence("doomed", doomed(), Default::default)
        .expect("valid records");
    table
        .register_sequence("phases", phases(), Phase::default)
        .expect("valid records");
    table
        .register("aborter", aborter(), u64::default)
        .expect("valid records");
    table
}

/// `id`, of type `ty`, with its log drained into the recorder after it.
fn logged(id: &str, ty: &str) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new(id, ty),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "lines".into(),
                    from: vec![PortRef::new(id, "log")],
                }],
                ..SystemConfig::new("logs", "log_sink")
            },
        ],
        ..Default::default()
    }
}

#[test]
fn test_def_reads_abort_first() {
    let def = def_of(one_sample());
    let inputs: Vec<_> = def.inputs.iter().map(|p| p.name.as_ref()).collect();
    let outputs: Vec<_> = def.outputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(inputs, ["abort"]);
    assert_eq!(outputs, ["imu", "log"]);
    assert_eq!(def.name, "one_sample");
}

#[test]
fn test_sequence_completes() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(phases(), phase.clone());
    step.execute(ms(0));
    assert_eq!((phase.get(), step.ended()), (1, None));
    step.execute(ms(1));
    assert_eq!((phase.get(), step.ended()), (2, None));
    step.execute(ms(2));
    assert_eq!(
        (phase.get(), step.ended()),
        (3, Some(&EndReason::Completed))
    );
}

#[test]
fn test_paused_step_resumes_at_its_await() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(phases(), phase.clone());
    step.execute(ms(0));
    step.execute(ms(1));
    // Cycles 2 to 9 pass with the step not executed, as a stopped channel's.
    step.execute(ms(10));
    assert_eq!(
        (phase.get(), step.ended()),
        (3, Some(&EndReason::Completed))
    );
}

#[test]
fn test_sleep_counts_paused_time() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(napper(), phase.clone());
    step.execute(ms(0));
    step.execute(ms(5));
    assert_eq!(phase.get(), 0);
    step.execute(ms(40));
    assert_eq!(
        (phase.get(), step.ended()),
        (1, Some(&EndReason::Completed))
    );
}

#[test]
fn test_ended_step_polls_nothing() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(napper(), phase.clone());
    step.execute(ms(0));
    step.execute(ms(10));
    phase.0.set(7);
    step.execute(ms(20));
    assert_eq!(
        (phase.get(), step.ended()),
        (7, Some(&EndReason::Completed))
    );
}

#[test]
fn test_failure_ends_with_its_text() {
    let recorder = Recorder::default();
    let mut coordinator = logged("fix", "no_fix").build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("fix", &EndReason::Failed("no fix".into()))]);
    assert_eq!(coordinator.latched().count(), 0);

    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        (lines[0].level, lines[0].message.as_ref()),
        (LogLevel::Info, "waiting for a fix")
    );
    assert_eq!(lines[0].timestamp, ms(1));
    assert_eq!(
        (lines[1].level, lines[1].message.as_ref()),
        (LogLevel::Error, "no fix")
    );
    assert_eq!(lines[1].fields, vec![("kind".into(), "failed".into())]);
}

#[test]
fn test_abort_ends_the_sequence() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("aborter", "aborter"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "abort".into(),
                    from: vec![PortRef::new("aborter", "abort")],
                }],
                ..SystemConfig::new("wait", "patient")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    assert_eq!(coordinator.ended().count(), 0);
    coordinator.step(ms(2));
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("wait", &EndReason::Aborted)]);
}

#[test]
fn test_panic_unwinds_to_the_coordinator() {
    let recorder = Recorder::default();
    let mut coordinator = logged("doom", "doomed").build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    assert_eq!(coordinator.latched().count(), 0);
    coordinator.step(ms(2));
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), ["doom"]);
    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].message, "doomed on its second poll");
    assert_eq!(lines[0].fields, vec![("kind".into(), "panic".into())]);
}

#[test]
fn test_sequence_refuses_a_thread() {
    let config = CoordinatorConfig {
        systems: vec![SystemConfig {
            adapter: Some(Adapter::Thread("gps".into())),
            ..SystemConfig::new("p", "phases")
        }],
        ..Default::default()
    };
    assert!(matches!(
        config.build(&table(&Recorder::default())),
        Err(BuildError::ThreadOnCyclic { .. })
    ));
}

/// The samples a sequence saw, shared with the test.
#[derive(Clone, Default)]
struct Seen(Rc<RefCell<Vec<Option<f64>>>>);

/// Keeps the sample it holds across each of three cycles.
#[crate::sequence]
async fn watcher(#[state] seen: &mut Seen, seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    for _ in 0..3 {
        seen.0.borrow_mut().push(imu.get().map(|imu| imu.sample));
        seq.next_cycle().await;
    }
    Ok(())
}

#[test]
fn test_sample_held_across_cycles() {
    let seen = Seen::default();
    let mut table = table(&Recorder::default());
    let state = seen.clone();
    table
        .register_sequence("watcher", watcher(), move || state.clone())
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "imu".into(),
                    from: vec![PortRef::new("imu", "imu")],
                }],
                ..SystemConfig::new("watch", "watcher")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    for at in 1..=3 {
        coordinator.step(ms(at));
    }
    assert_eq!(*seen.0.borrow(), [Some(1.0), Some(2.0), Some(3.0)]);
}

#[test]
fn test_sequence_registers_its_doc() {
    let table = table(&Recorder::default());
    let entry = table.get("phases").expect("registered");
    assert_eq!(entry.doc, "Marks three phases, one cycle apart.");
    assert_eq!(entry.def.inputs[0].name, "abort");
}

/// The handlers that ran, in order, shared with the test.
#[derive(Clone, Default)]
struct Trace(Rc<RefCell<Vec<&'static str>>>);

impl Trace {
    fn push(&self, name: &'static str) {
        self.0.borrow_mut().push(name);
    }

    fn take(&self) -> Vec<&'static str> {
        self.0.take()
    }
}

/// Holds its valve open every cycle until it ends.
#[crate::sequence]
async fn hold_open(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    loop {
        seq.next_cycle().await;
    }
}

/// Waits a millisecond for a light that never comes, then aborts.
#[crate::sequence]
async fn no_light(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    let _vented = seq.on_abort(|_| trace.push("vented"));
    seq.wait_for(Duration::from_millis(1), || false)
        .await
        .map_err(|_| seq.abort())
}

/// Arms a handler for its first cycle only, then waits an hour.
#[crate::sequence]
async fn staged(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _outer = seq.on_abort(|_| trace.push("outer"));
    {
        let _stage = seq.on_abort(|_| trace.push("stage"));
        seq.next_cycle().await;
    }
    seq.sleep(Duration::from_secs(3600)).await
}

/// Panics on its second poll with a handler armed.
#[crate::sequence]
async fn doomed_valve(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    seq.next_cycle().await;
    panic!("doomed with a valve open")
}

/// Completes on its second cycle with a handler armed.
#[crate::sequence]
async fn clean(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    seq.next_cycle().await;
    Ok(())
}

/// Raises its own abort flag and keeps waiting.
#[crate::sequence]
async fn self_abort(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    let _ = seq.abort();
    loop {
        seq.next_cycle().await;
    }
}

/// The commands a test sends to the `seq` channel.
#[derive(Clone, Default)]
struct Commands(Rc<RefCell<Vec<SequenceCommandKind>>>);

impl Commands {
    fn send(&self, command: SequenceCommandKind) {
        self.0.borrow_mut().push(command);
    }
}

/// Writes the queued commands to the `seq` channel.
#[crate::system]
fn commander(#[state] queue: &mut Commands, commands: &mut Output<SequenceCommand>) {
    for command in queue.0.borrow_mut().drain(..) {
        let _ = commands.write(&SequenceCommand {
            channel: "seq".into(),
            command,
        });
    }
}

/// The test table plus `sequence` as `traced` over `trace`, and a commander.
fn traced<S: SequenceFn<State = Trace>>(
    recorder: &Recorder,
    sequence: S,
    trace: &Trace,
    commands: &Commands,
) -> SystemTable {
    let mut table = table(recorder);
    let state = trace.clone();
    table
        .register_sequence("traced", sequence, move || state.clone())
        .expect("valid records");
    let queue = commands.clone();
    table
        .register("commander", commander(), move || queue.clone())
        .expect("valid records");
    table
}

/// `traced` as `t`, its log drained, aborted by `aborter` from the second cycle.
fn aborted() -> CoordinatorConfig {
    let mut config = logged("t", "traced");
    config.systems[0].inputs.push(InputConfig {
        port: "abort".into(),
        from: vec![PortRef::new("aborter", "abort")],
    });
    config
        .systems
        .insert(0, SystemConfig::new("aborter", "aborter"));
    config
}

/// `traced` as channel `seq`, driven by `commander`.
fn channeled(start: RunState) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "commander"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "commands".into(),
                    from: vec![PortRef::new("cmds", "commands")],
                }],
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: "seq".into(),
                    start,
                })),
                ..SystemConfig::new("t", "traced")
            },
        ],
        ..Default::default()
    }
}

/// Builds `config` over `sequence`, returning what its handlers ran into.
fn run<S: SequenceFn<State = Trace>>(
    sequence: S,
    config: CoordinatorConfig,
) -> (Trace, Commands, Recorder, Coordinator) {
    let (trace, commands, recorder) = Default::default();
    let table = traced(&recorder, sequence, &trace, &commands);
    let coordinator = config.build(&table).expect("a valid config");
    (trace, commands, recorder, coordinator)
}

#[test]
fn test_abort_cancels_a_cycle_loop() {
    let (trace, _, _, mut coordinator) = run(hold_open(), aborted());
    coordinator.step(ms(1));
    assert!(trace.take().is_empty());
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("t", &EndReason::Aborted)]);
}

#[test]
fn test_explicit_abort_after_timeout() {
    let (trace, _, recorder, mut coordinator) = run(no_light(), logged("t", "traced"));
    coordinator.step(ms(1));
    assert!(trace.take().is_empty());
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["vented", "closed"]);
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("t", &EndReason::Aborted)]);
    assert!(recorder.take_logs().is_empty());
}

#[test]
fn test_raised_flag_is_never_lowered() {
    let (trace, _, _, mut coordinator) = run(self_abort(), logged("t", "traced"));
    coordinator.step(ms(1));
    assert!(trace.take().is_empty());
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("t", &EndReason::Aborted)]);
}

#[test]
fn test_finished_block_handler_is_disarmed() {
    let (trace, commands, _, mut coordinator) = run(staged(), channeled(RunState::Running));
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    commands.send(SequenceCommandKind::Abort);
    coordinator.step(ms(3));
    assert_eq!(trace.take(), ["outer"]);
}

#[test]
fn test_panic_runs_handlers() {
    let (trace, _, recorder, mut coordinator) = run(doomed_valve(), logged("t", "traced"));
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), ["t"]);
    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].message, "doomed with a valve open");
    assert_eq!(lines[0].fields, vec![("kind".into(), "panic".into())]);
}

#[test]
fn test_completion_runs_no_handlers() {
    let (trace, _, _, mut coordinator) = run(clean(), logged("t", "traced"));
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    assert!(trace.take().is_empty());
    let ended: Vec<_> = coordinator.ended().collect();
    assert_eq!(ended, [("t", &EndReason::Completed)]);
}

#[test]
fn test_reset_of_stopped_sequence_runs_no_handlers() {
    let (trace, commands, _, mut coordinator) = run(hold_open(), channeled(RunState::Running));
    coordinator.step(ms(1));
    commands.send(SequenceCommandKind::Stop);
    coordinator.step(ms(2));
    commands.send(SequenceCommandKind::Reset);
    coordinator.step(ms(3));
    commands.send(SequenceCommandKind::Abort);
    coordinator.step(ms(4));
    assert!(trace.take().is_empty());
}
