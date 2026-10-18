//! Sequence steps, driven by hand and by a coordinator.

use core::cell::{Cell, RefCell};
use core::time::Duration;
use std::rc::Rc;

use metor_fsw_3_ring::{NoWake, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use super::*;
use crate::channel::AbortRequest;
use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
use zerocopy::FromBytes;

use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, Coordinator, CoordinatorConfig, EndReason, InputConfig,
    RunState, Step, SystemConfig, SystemTable,
};
use crate::log::Log;
use crate::port::{Output, Sample};
use crate::system::{InputBinding, OutputBinding};
use crate::tests::utils::{self, Imu, Recorder};

/// The phase a sequence has reached, shared with the test.
#[derive(Clone, Default)]
struct Phase(Rc<Cell<u32>>);

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
    let _ = imu.write(&Imu::new(seq.now().0, 1.0));
    Ok(())
}

/// Writes more samples in one poll than its ring holds, then ends.
#[crate::sequence]
async fn flood(seq: &Seq, imu: &mut Output<Imu>) -> Result<(), SeqError> {
    for _ in 0..64 {
        let _ = imu.write(&Imu::new(seq.now().0, 1.0));
    }
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

pub(super) fn ms(ms: i64) -> Timestamp {
    Timestamp(ms * 1_000)
}

/// A sequence's definition under the empty context.
fn def_of<S: SequenceFn>(_sequence: S) -> crate::system::SystemDef {
    def::<S>(&crate::def::DefCx::empty()).expect("a static definition")
}

/// One step of `S` over fresh rings, with nothing feeding its inputs.
fn step_of<S: SequenceFn>(sequence: S, state: S::State) -> (SequenceStep, Vec<RingBuffer>) {
    let def = def_of(sequence);
    let inputs = def
        .inputs
        .iter()
        .map(|def| InputBinding {
            def: def.clone(),
            views: Vec::new(),
        })
        .collect();
    step_over::<S>(&def, state, inputs)
}

/// One step of `S`, defined by `def`, reading `inputs` and writing to fresh rings.
fn step_over<S: SequenceFn>(
    def: &crate::system::SystemDef,
    state: S::State,
    inputs: Vec<InputBinding>,
) -> (SequenceStep, Vec<RingBuffer>) {
    let rings: Vec<RingBuffer> = def
        .outputs
        .iter()
        .map(|port| utils::ring_of(port.max_len, 8, 1))
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
    let mut inputs = inputs.into_iter();
    let abort = crate::port::Input::new(inputs.next().expect("the abort binding").views);
    let io = crate::io::SharedIo::default();
    let instance = crate::fn_system::Instance {
        io: &io,
        id: "seq",
        namespace: None,
        seed: Some(0),
        records: &Default::default(),
    };
    let (ins, outs, _io) =
        crate::fn_system::bind::<S>(inputs.collect(), outputs, instance).expect("binds");
    (SequenceStep::new::<S>(state, abort, ins, outs), rings)
}

fn table(recorder: &Recorder) -> SystemTable {
    let mut table = utils::table(recorder);
    let mut register = || -> Result<(), BuildError> {
        table.register_sequence("no_fix", no_fix(), Default::default)?;
        table.register_sequence("patient", patient(), Default::default)?;
        table.register_sequence("doomed", doomed(), Default::default)?;
        table.register_sequence("phases", phases(), Phase::default)?;
        table.register("aborter", aborter(), u64::default)?;
        Ok(())
    };
    register().expect("valid records");
    table
}

/// A config with system `id` of type `ty`, followed by a `log_sink` that
/// drains its log into the recorder.
fn logged(id: &str, ty: &str) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new(id, ty),
            SystemConfig {
                inputs: vec![InputConfig::new("lines", id, "log")],
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
fn test_failed_writes_fault_after_the_poll() {
    let (mut step, rings) = step_of(flood(), ());
    let mut samples = utils::input::<Imu>(&rings[0]);
    let mut lines = utils::input::<metor_proto_wkt::LogEvent>(&rings[1]);
    step.execute(ms(1));
    assert_eq!(step.ended(), Some(&EndReason::Completed));
    let landed = samples.drain().count();
    let lines: Vec<_> = lines.drain().map(|line| line.expect("decodes")).collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].level, LogLevel::Error);
    assert_eq!(
        lines[0].fields,
        vec![("kind".into(), "publish_dropped".into())]
    );
    assert_eq!(lines[0].message, format!("{} writes dropped", 64 - landed));
}

#[test]
fn test_sequence_completes() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(phases(), phase.clone());
    step.execute(ms(0));
    assert_eq!((phase.0.get(), step.ended()), (1, None));
    step.execute(ms(1));
    assert_eq!((phase.0.get(), step.ended()), (2, None));
    step.execute(ms(2));
    assert_eq!(
        (phase.0.get(), step.ended()),
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
        (phase.0.get(), step.ended()),
        (3, Some(&EndReason::Completed))
    );
}

#[test]
fn test_sleep_counts_paused_time() {
    let phase = Phase::default();
    let (mut step, _rings) = step_of(napper(), phase.clone());
    step.execute(ms(0));
    step.execute(ms(5));
    assert_eq!(phase.0.get(), 0);
    step.execute(ms(40));
    assert_eq!(
        (phase.0.get(), step.ended()),
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
        (phase.0.get(), step.ended()),
        (7, Some(&EndReason::Completed))
    );
}

#[test]
fn test_failure_ends_with_its_text() {
    let recorder = Recorder::default();
    let mut coordinator = logged("fix", "no_fix").build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
    assert_eq!(ended, [("fix", &EndReason::Failed("no fix".into()))]);
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked).len(),
        0
    );

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
                inputs: vec![InputConfig::new("abort", "aborter", "abort")],
                ..SystemConfig::new("wait", "patient")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    assert_eq!(crate::tests::utils::ended(&coordinator).len(), 0);
    coordinator.step(ms(2));
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
    assert_eq!(ended, [("wait", &EndReason::Aborted)]);
}

#[test]
fn test_panic_unwinds_to_the_coordinator() {
    let recorder = Recorder::default();
    let mut coordinator = logged("doom", "doomed").build(&table(&recorder)).unwrap();
    coordinator.step(ms(1));
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked).len(),
        0
    );
    coordinator.step(ms(2));
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked),
        ["doom"]
    );
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
        Err(BuildError::UnsupportedAdapter { .. })
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

/// What `sequence` saw over three cycles, its `imu` fed by the test imu.
fn imu_into<S: SequenceFn<State = Seen>>(sequence: S) -> Vec<Option<f64>> {
    let seen = Seen::default();
    let mut table = table(&Recorder::default());
    let state = seen.clone();
    table
        .register_sequence("s", sequence, move || state.clone())
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            SystemConfig {
                inputs: vec![InputConfig::new("imu", "imu", "imu")],
                ..SystemConfig::new("s", "s")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    for at in 1..=3 {
        coordinator.step(ms(at));
    }
    seen.0.take()
}

#[test]
fn test_sample_held_across_cycles() {
    assert_eq!(imu_into(watcher()), [Some(1.0), Some(2.0), Some(3.0)]);
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
pub(super) struct Trace(Rc<RefCell<Vec<&'static str>>>);

impl Trace {
    pub(super) fn push(&self, name: &'static str) {
        self.0.borrow_mut().push(name);
    }

    pub(super) fn take(&self) -> Vec<&'static str> {
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

/// The test table, plus `sequence` registered as `traced` with `trace` as
/// its state, and a `commander` that sends `commands`.
fn traced<S: SequenceFn<State = Trace>>(
    recorder: &Recorder,
    sequence: S,
    trace: &Trace,
    commands: &Commands,
) -> SystemTable {
    let mut table = table(recorder);
    let register = || -> Result<(), BuildError> {
        let state = trace.clone();
        table.register_sequence("traced", sequence, move || state.clone())?;
        let queue = commands.clone();
        table.register("commander", commander(), move || queue.clone())?;
        Ok(())
    };
    register().expect("valid records");
    table
}

/// Runs `traced` as `t` with its log drained. An `aborter` sends it an
/// abort from the second cycle on.
fn aborted() -> CoordinatorConfig {
    let mut config = logged("t", "traced");
    config.systems[0]
        .inputs
        .push(InputConfig::new("abort", "aborter", "abort"));
    config
        .systems
        .insert(0, SystemConfig::new("aborter", "aborter"));
    config
}

/// Runs `traced` as `t` behind a channel called `seq`, which `commander`
/// sends commands to.
fn channeled(start: RunState) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "commander"),
            SystemConfig {
                inputs: vec![InputConfig::new("commands", "cmds", "commands")],
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: crate::ChannelName::try_from("seq".to_string()).expect("a short name"),
                    start,
                })),
                ..SystemConfig::new("t", "traced")
            },
        ],
        ..Default::default()
    }
}

/// Builds `config` with `sequence` and runs it, returning the trace its
/// handlers wrote.
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
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
    assert_eq!(ended, [("t", &EndReason::Aborted)]);
}

#[test]
fn test_explicit_abort_after_timeout() {
    let (trace, _, recorder, mut coordinator) = run(no_light(), logged("t", "traced"));
    coordinator.step(ms(1));
    assert!(trace.take().is_empty());
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["vented", "closed"]);
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
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
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
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
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked),
        ["t"]
    );
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
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
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

/// Opens its valve until the imu reads 2 or more, closing it on abort.
#[crate::sequence]
async fn guarded(
    #[state] trace: &mut Trace,
    seq: &Seq,
    imu: &Sample<Imu>,
    valve_cmd: &mut Output<Imu>,
) -> Result<(), SeqError> {
    let mut valve_cmd = seq.on_abort_with(valve_cmd, |seq, valve_cmd| {
        trace.push("closed");
        let _ = valve_cmd.write(&Imu::new(seq.now().0, 0.0));
    });
    crate::redline!(seq, "imu high", || imu
        .get()
        .is_some_and(|i| i.sample >= 2.0));
    let _ = valve_cmd.write(&Imu::new(seq.now().0, 1.0));
    loop {
        seq.next_cycle().await;
    }
}

/// An unnamed redline that holds once the imu reads 2 or more.
#[crate::sequence]
async fn unnamed(#[state] trace: &mut Trace, seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    crate::redline!(seq, || imu.get().is_some_and(|i| i.sample >= 2.0));
    seq.sleep(Duration::from_secs(3600)).await
}

/// Guards only its first cycle against an imu reading of 3 or more.
#[crate::sequence]
async fn block_guard(
    #[state] trace: &mut Trace,
    seq: &Seq,
    imu: &Sample<Imu>,
) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    {
        crate::redline!(seq, "early", || imu.get().is_some_and(|i| i.sample >= 3.0));
        seq.next_cycle().await;
    }
    seq.sleep(Duration::from_secs(3600)).await
}

/// Links a redline that already holds, then waits a cycle.
#[crate::sequence]
async fn late_line(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    crate::redline!(seq, "at once", || true);
    trace.push("linked");
    seq.next_cycle().await;
    trace.push("resumed");
    Ok(())
}

/// Records what its redline and its body read, every cycle.
#[crate::sequence]
async fn twin_reads(
    #[state] seen: &mut Seen,
    seq: &Seq,
    imu: &Sample<Imu>,
) -> Result<(), SeqError> {
    crate::redline!(seq, "never", || {
        seen.0.borrow_mut().push(imu.get().map(|i| i.sample));
        false
    });
    loop {
        seen.0.borrow_mut().push(imu.get().map(|i| i.sample));
        seq.next_cycle().await;
    }
}

/// [`channeled`] with an `imu` feeding `t`, and `t`'s log drained.
fn sampled() -> CoordinatorConfig {
    let mut config = channeled(RunState::Running);
    config.systems.insert(0, SystemConfig::new("imu", "imu"));
    config.systems[2]
        .inputs
        .push(InputConfig::new("imu", "imu", "imu"));
    config.systems.push(SystemConfig {
        inputs: vec![InputConfig::new("lines", "t", "log")],
        ..SystemConfig::new("logs", "log_sink")
    });
    config
}

/// [`sampled`], plus a `tap` that records `t`'s valve commands.
fn tapped() -> CoordinatorConfig {
    let mut config = sampled();
    config.systems.push(SystemConfig {
        inputs: vec![InputConfig::new("valve", "t", "valve_cmd")],
        ..SystemConfig::new("taps", "tap")
    });
    config
}

/// The valve positions `taps` recorded.
fn valve_positions(recorder: &Recorder) -> Vec<f64> {
    recorder
        .take_taps()
        .iter()
        .map(|(_, bytes)| Imu::read_from_bytes(bytes).expect("an imu frame").sample)
        .collect()
}

#[test]
fn test_redline_trips_and_safes() {
    let (trace, _, recorder, mut coordinator) = run(guarded(), tapped());
    coordinator.step(ms(1));
    assert!(trace.take().is_empty());
    assert_eq!(valve_positions(&recorder), [1.0]);
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    assert_eq!(valve_positions(&recorder), [0.0]);
    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].message, "imu high");
    assert_eq!(lines[0].fields, vec![("kind".into(), "redline".into())]);
}

#[test]
fn test_unnamed_redline_fault() {
    let (trace, _, recorder, mut coordinator) = run(unnamed(), sampled());
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].message, "redline");
}

#[test]
fn test_block_redline_ends_with_its_block() {
    let (trace, _, recorder, mut coordinator) = run(block_guard(), sampled());
    for at in 1..=4 {
        coordinator.step(ms(at));
    }
    assert!(trace.take().is_empty());
    assert!(recorder.take_logs().is_empty());
}

#[test]
fn test_stopped_sequence_does_not_trip() {
    let (trace, commands, recorder, mut coordinator) = run(guarded(), tapped());
    coordinator.step(ms(1));
    commands.send(SequenceCommandKind::Stop);
    coordinator.step(ms(2));
    coordinator.step(ms(3));
    assert!(trace.take().is_empty());
    assert!(recorder.take_logs().is_empty());
    commands.send(SequenceCommandKind::Start);
    coordinator.step(ms(4));
    assert_eq!(trace.take(), ["closed"]);
}

#[test]
fn test_redline_is_checked_before_the_next_poll() {
    let (trace, _, recorder, mut coordinator) = run(late_line(), logged("t", "traced"));
    coordinator.step(ms(1));
    assert_eq!(trace.take(), ["linked"]);
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    let ended: Vec<_> = crate::tests::utils::ended(&coordinator);
    assert_eq!(ended, [("t", &EndReason::Aborted)]);
    assert_eq!(recorder.take_logs()[0].message, "at once");
}

/// A redline whose condition panics when first checked.
#[crate::sequence]
async fn cut_wire(#[state] trace: &mut Trace, seq: &Seq) -> Result<(), SeqError> {
    let _closed = seq.on_abort(|_| trace.push("closed"));
    crate::redline!(seq, "wire", || panic!("sensor wire cut"));
    seq.sleep(Duration::from_secs(3600)).await
}

#[test]
fn test_panicking_condition_runs_handlers() {
    let (trace, _, recorder, mut coordinator) = run(cut_wire(), logged("t", "traced"));
    coordinator.step(ms(1));
    coordinator.step(ms(2));
    assert_eq!(trace.take(), ["closed"]);
    assert_eq!(
        crate::tests::utils::ended_with(&coordinator, &EndReason::Panicked),
        ["t"]
    );
    let lines = recorder.take_logs();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].message, "sensor wire cut");
    assert_eq!(lines[0].fields, vec![("kind".into(), "panic".into())]);
}

#[test]
fn test_redline_and_body_read_one_sample() {
    let twice = |v| [Some(v), Some(v)];
    let expected: Vec<_> = [Some(1.0)]
        .into_iter()
        .chain(twice(2.0))
        .chain(twice(3.0))
        .collect();
    assert_eq!(imu_into(twin_reads()), expected);
}

/// Glances at its sample once, then ends.
#[crate::sequence]
async fn glance(imu: &Sample<Imu>) -> Result<(), SeqError> {
    let _ = imu.get();
    Ok(())
}

/// One step of `S`, whose one port `imu` reads the returned output.
fn step_on_imu<S: SequenceFn<State = ()>>(
    sequence: S,
) -> (SequenceStep, Output<Imu>, Vec<RingBuffer>) {
    let def = def_of(sequence);
    let ring = utils::ring_of(def.inputs[1].max_len, 2, 1);
    let inputs = vec![
        InputBinding {
            def: def.inputs[0].clone(),
            views: Vec::new(),
        },
        InputBinding {
            def: def.inputs[1].clone(),
            views: vec![ring.view(NoWake).expect("a free slot")],
        },
    ];
    let (step, mut rings) = step_over::<S>(&def, (), inputs);
    let imu = utils::output(&ring);
    rings.push(ring);
    (step, imu, rings)
}

#[test]
fn test_ended_sequence_releases_its_samples() {
    let (mut step, mut imu, _rings) = step_on_imu(glance());
    step.execute(ms(0));
    assert_eq!(step.ended(), Some(&EndReason::Completed));
    for at in 0..16 {
        assert!(
            imu.write(&Imu::new(ms(at).0, 1.0)).is_ok(),
            "write {at} blocked"
        );
    }
}

/// An allocation leaked on purpose.
struct Leaked {
    _ptr: *mut u8,
}

// SAFETY: never read through; it only keeps its allocation reachable.
unsafe impl Send for Leaked {}

static LEAKED: std::sync::Mutex<Vec<Leaked>> = std::sync::Mutex::new(Vec::new());

/// Leaks `boxed` as `mem::forget` does, keeping it reachable so Miri's leak
/// check passes.
fn leak<T: ?Sized>(boxed: core::pin::Pin<Box<T>>) {
    // SAFETY: the box is never moved out of or freed.
    let raw = Box::into_raw(unsafe { core::pin::Pin::into_inner_unchecked(boxed) });
    let mut leaked = LEAKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    leaked.push(Leaked { _ptr: raw.cast() });
}

/// Links a redline over its imu inside a future it polls once and leaks,
/// then ends two cycles later.
#[crate::sequence]
async fn leaks_a_future(seq: &Seq, imu: &Sample<Imu>) -> Result<(), SeqError> {
    let mut nested = Box::pin(async {
        crate::redline!(seq, "imu high", || imu
            .get()
            .is_some_and(|i| i.sample >= 2.0));
        seq.next_cycle().await;
    });
    core::future::poll_fn(|cx| {
        let _ = nested.as_mut().poll(cx);
        core::task::Poll::Ready(())
    })
    .await;
    leak(nested);
    seq.next_cycle().await;
    seq.next_cycle().await;
    Ok(())
}

/// Links a boxed redline over its imu and leaks it, then ends two cycles later.
#[crate::sequence]
async fn leaks_a_redline(seq: &Seq<'_>, imu: &Sample<Imu>) -> Result<(), SeqError> {
    let redline = Box::pin(Redline::new("imu high", || {
        imu.get().is_some_and(|i| i.sample >= 2.0)
    }));
    seq.redline(redline.as_ref());
    leak(redline);
    seq.next_cycle().await;
    seq.next_cycle().await;
    Ok(())
}

/// Runs `S` reading `sample` each cycle, past its end, then drops it.
fn run_past_its_end<S: SequenceFn<State = ()>>(sequence: fn() -> S, sample: f64) -> EndReason {
    let (mut step, mut imu, _rings) = step_on_imu(sequence());
    for at in 0..6 {
        let _ = imu.write(&Imu::new(ms(at).0, sample));
        step.execute(ms(at));
    }
    step.ended().cloned().expect("an ended sequence")
}

#[test]
fn test_redline_leaked_in_a_future_is_checked_until_the_end() {
    assert_eq!(run_past_its_end(leaks_a_future, 1.0), EndReason::Completed);
    assert_eq!(run_past_its_end(leaks_a_future, 2.0), EndReason::Aborted);
}

#[test]
fn test_leaked_redline_is_checked_until_the_end() {
    assert_eq!(run_past_its_end(leaks_a_redline, 1.0), EndReason::Completed);
    assert_eq!(run_past_its_end(leaks_a_redline, 2.0), EndReason::Aborted);
}
