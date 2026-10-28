//! The channel adapter, which puts a cyclic system behind a channel.
//!
//! The adapter wraps the system's step. Each cycle it applies the commands
//! sent to its channel, steps the system if it is running, and reports the
//! channel's status.

use std::sync::Arc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceAccepts, SequenceChannelState, SequenceCommand, SequenceCommandKind,
};

use crate::coordinator::{
    Adapter, ChannelConfig, EndReason, MakeCx, MakeError, MakeFn, Params, RunState, Step, StepCx,
    catch_execute,
};
use crate::def::{DefCx, Records};
use crate::io::IoState;
use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;

use super::apply::{Command, Phase, State, accepts, apply};
use super::{
    ABORT_PORT, AbortRequest, COMMANDS_PORT, ChannelStatus, MESSAGE_LEN, STATUS_PORT, channel_state,
};
use crate::FrameStr;

/// The number of records the private abort ring holds.
const ABORT_DEPTH: usize = 4;

/// Appends the channel's ports to a system's definition.
///
/// # Errors
/// Returns the name of a system port that already uses one of the channel's
/// port names.
pub(crate) fn def(mut inner: SystemDef, cx: &DefCx<'_>) -> Result<SystemDef, String> {
    let reserved = [COMMANDS_PORT, STATUS_PORT];
    let mut taken = inner.inputs.iter().chain(&inner.outputs);
    if let Some(port) = taken.find(|p| reserved.contains(&p.name.as_ref())) {
        return Err(port.name.to_string());
    }
    let [commands, status] = channel_ports(cx);
    inner.inputs.push(commands);
    inner.outputs.push(status);
    Ok(inner)
}

/// Returns the ports every channel adds.
///
/// These are the commands input, then the status output.
pub(crate) fn channel_ports(cx: &DefCx<'_>) -> [PortDef; 2] {
    [
        Input::<SequenceCommand>::def(cx, COMMANDS_PORT),
        Output::<ChannelStatus>::def(cx, STATUS_PORT),
    ]
}

/// Builds a system with `make` and wraps it in `channel`.
///
/// `cx.def` must be the definition [`def`] returned, so the channel's ports
/// are the last input and the last output.
pub(crate) fn make_channel(
    make: &Arc<MakeFn>,
    channel: &ChannelConfig,
    mut cx: MakeCx<'_>,
) -> Result<Box<dyn Step>, MakeError> {
    // PANIC Safety: `def` appended one input and one output.
    let commands = cx.inputs.pop().expect("the commands input");
    let status = cx.outputs.pop().expect("the channel_status output");
    let mut def = cx.def.clone();
    def.inputs.pop();
    def.outputs.truncate(cx.outputs.len());
    let mut inputs: Vec<Vec<RingBuffer>> = cx
        .inputs
        .into_iter()
        .map(|rings| rings.into_iter().cloned().collect())
        .collect();
    let abort = private_abort(&def, &mut inputs);
    let recipe = SystemRecipe {
        make: make.clone(),
        id: cx.id.to_string(),
        adapter: Adapter::Channel(channel.clone()),
        def,
        params: cx.params.0.clone(),
        inputs,
        outputs: cx.outputs.into_iter().cloned().collect(),
        seed: cx.seed,
        namespace: cx.namespace.map(str::to_string),
        records: cx.records.clone(),
    };
    let mut threads = Threads::default();
    let inner = recipe.make(&mut threads, cx.io)?;
    Ok(Box::new(Channel {
        commands: command_input(commands),
        core: Core {
            name: channel.name.to_string(),
            phase: ChannelPhase::Live {
                inner,
                run: channel.start,
            },
            reported_end_reason: None,
            loaded: None,
            recipe,
            threads,
            abort,
            status: writer(status),
            clock: RunClock::default(),
            message: FrameStr::default(),
            message_at: 0,
        },
    }))
}

/// Rebinds the system's `abort` input to a private ring.
///
/// Returns the writer the adapter uses to send aborts, or `None` if the
/// system has no `abort` input.
fn private_abort(def: &SystemDef, inputs: &mut [Vec<RingBuffer>]) -> Option<Output<AbortRequest>> {
    let index = def.inputs.iter().position(|p| p.name == ABORT_PORT)?;
    let ring = abort_ring();
    inputs[index] = vec![ring.clone()];
    Some(writer::<AbortRequest>(&ring))
}

/// Creates a ring that only the adapter writes and only the system reads.
fn abort_ring() -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: a small message record at a small depth fits any host.
        capacity: ring_capacity(AbortRequest::MAX_LEN, ABORT_DEPTH).expect("a small ring"),
        max_readers: 1,
    })
    .expect("a valid ring config")
}

fn command_input(rings: Vec<&RingBuffer>) -> Input<SequenceCommand> {
    // PANIC Safety: the build pass counts one reader slot per edge.
    Input::new(
        rings
            .into_iter()
            .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
            .collect(),
    )
}

fn writer<T: Record>(ring: &RingBuffer) -> Output<T> {
    // PANIC Safety: one ring is allocated per output.
    Output::new(ring.writer(NoWake).expect("one writer per output ring"))
}

/// Everything needed to build the system again over the same rings.
struct SystemRecipe {
    make: Arc<MakeFn>,
    id: String,
    adapter: Adapter,
    def: SystemDef,
    params: serde_json::Value,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
    seed: u64,
    namespace: Option<String>,
    records: Arc<Records>,
}

impl SystemRecipe {
    fn make(&self, threads: &mut Threads, io: &mut IoState) -> Result<Box<dyn Step>, MakeError> {
        (self.make)(MakeCx {
            id: &self.id,
            adapter: Some(&self.adapter),
            def: &self.def,
            params: Params(&self.params),
            inputs: self
                .inputs
                .iter()
                .map(|rings| rings.iter().collect())
                .collect(),
            outputs: self.outputs.iter().collect(),
            threads,
            io,
            seed: self.seed,
            namespace: self.namespace.as_deref(),
            records: &self.records,
        })
    }
}

/// The step for a system behind a channel.
///
/// It applies the commands sent to the channel, then steps the system if it
/// is running.
pub struct Channel {
    commands: Input<SequenceCommand>,
    core: Core,
}

struct Core {
    name: String,
    phase: ChannelPhase,
    /// The end last reported as a message.
    reported_end_reason: Option<EndReason>,
    /// The last name a `Load` accepted. A reset loads it again.
    loaded: Option<String>,
    recipe: SystemRecipe,
    threads: Threads,
    abort: Option<Output<AbortRequest>>,
    status: Output<ChannelStatus>,
    clock: RunClock,
    message: FrameStr<MESSAGE_LEN>,
    message_at: i64,
}

/// The run and abort clocks a channel reports in its status.
///
/// Both count the cycles' time only while the system runs, so a stop pauses
/// them. The run clock holds at the run's length once it ends, and a reset or
/// load starts a fresh one. The abort clock starts at an accepted abort and
/// goes back to zero when the run ends.
#[derive(Default)]
struct RunClock {
    run: Stopwatch,
    abort: Option<Stopwatch>,
}

impl RunClock {
    fn abort(&mut self) {
        self.abort.get_or_insert_default();
    }

    /// Advances to `now` and returns `(elapsed, abort_elapsed)` in seconds.
    fn tick(&mut self, now: Timestamp, running: bool, ended: bool) -> (f64, f64) {
        if ended {
            self.abort = None;
        }
        let elapsed = self.run.advance(now, running);
        let abort = self.abort.as_mut().map_or(0.0, |w| w.advance(now, running));
        (elapsed, abort)
    }
}

/// Seconds counted across the stretches something ran.
#[derive(Default)]
struct Stopwatch {
    /// Seconds from the stretches before the current one.
    banked: f64,
    /// When the current stretch began.
    since: Option<Timestamp>,
}

impl Stopwatch {
    fn advance(&mut self, now: Timestamp, running: bool) -> f64 {
        let seconds = |from: Timestamp| (now.0 - from.0) as f64 / 1e6;
        if running {
            self.since.get_or_insert(now);
        } else if let Some(since) = self.since.take() {
            self.banked += seconds(since);
        }
        self.banked + self.since.map_or(0.0, seconds)
    }
}

/// Where a channel's system is.
enum ChannelPhase {
    /// Made, and stepped while `run` is `Running` and it has not ended.
    Live { inner: Box<dyn Step>, run: RunState },
    /// Gone: its make or load failed, or it panicked.
    Gone(EndReason),
    /// Dropped, and made again once its ops in flight finish.
    Restarting,
    /// Loading `name` once its ops in flight finish.
    Loading { inner: Box<dyn Step>, name: String },
}

impl ChannelPhase {
    fn inner(&self) -> Option<&dyn Step> {
        match self {
            Self::Live { inner, .. } | Self::Loading { inner, .. } => Some(inner.as_ref()),
            Self::Gone(_) | Self::Restarting => None,
        }
    }
}

impl Step for Channel {
    fn execute(&mut self, now: Timestamp, cx: &mut StepCx<'_>) {
        let Self { commands, core } = self;
        for command in commands.drain() {
            match command {
                Ok(command) if command.channel == core.name => {
                    core.command(command.command, now, cx.io);
                }
                _ => {}
            }
        }
        core.step(now, cx);
    }
}

impl Core {
    fn command(&mut self, kind: SequenceCommandKind, now: Timestamp, io: &mut IoState) {
        let message = match apply(self.state(), Command::of(&kind)) {
            Ok(()) => self.transition(kind, io),
            Err(refusal) => refused(refusal),
        };
        self.say(&message, now);
    }

    fn state(&self) -> State {
        let phase = match &self.phase {
            ChannelPhase::Restarting | ChannelPhase::Loading { .. } => Phase::Quiescing,
            _ if self.ended().is_some() => Phase::Ended,
            ChannelPhase::Live {
                run: RunState::Running,
                ..
            } => Phase::Running,
            _ => Phase::Stopped,
        };
        State {
            phase,
            abortable: self.abort.is_some(),
        }
    }

    /// Applies an accepted command and returns its message.
    fn transition(&mut self, kind: SequenceCommandKind, io: &mut IoState) -> String {
        match kind {
            SequenceCommandKind::Start => {
                self.set_run(RunState::Running);
                "started".into()
            }
            SequenceCommandKind::Stop => {
                self.set_run(RunState::Stopped);
                "stopped".into()
            }
            SequenceCommandKind::Abort => self.abort(),
            SequenceCommandKind::Reset => self.reset(io),
            SequenceCommandKind::Load { name } => self.defer_load(name, io),
        }
    }

    fn set_run(&mut self, state: RunState) {
        if let ChannelPhase::Live { run, .. } = &mut self.phase {
            *run = state;
        }
    }

    fn abort(&mut self) -> String {
        let written = self
            .abort
            .as_mut()
            .is_some_and(|abort| abort.write(&AbortRequest {}).is_ok());
        if !written {
            return refused("the abort ring is full");
        }
        self.clock.abort();
        "abort requested".into()
    }

    /// Resets the system. An IO system is dropped now and made again once its
    /// ops in flight finish.
    fn reset(&mut self, io: &mut IoState) -> String {
        if !self.has_io(io) {
            return self.restart_message(io);
        }
        self.phase = ChannelPhase::Restarting;
        io.quiesce_system(&self.recipe.id);
        "quiescing".into()
    }

    fn restart_message(&mut self, io: &mut IoState) -> String {
        match self.restart(io) {
            Ok(()) => match &self.loaded {
                Some(name) => format!("loaded {name}"),
                None => "reset".into(),
            },
            Err(reason) => self.fail(reason),
        }
    }

    /// Loads the last loaded name again, stopped.
    ///
    /// If the system is gone or has nothing loaded, builds it again first.
    fn restart(&mut self, io: &mut IoState) -> Result<(), String> {
        let phase = core::mem::replace(&mut self.phase, ChannelPhase::Restarting);
        let kept = match (phase, &self.loaded) {
            (ChannelPhase::Live { inner, .. }, Some(_)) => Some(inner),
            _ => None,
        };
        let mut inner = match kept {
            Some(inner) => inner,
            None => self.remake(io)?,
        };
        if let Some(name) = &self.loaded {
            crate::panic::catch(|| inner.load(name, &mut StepCx::new(io)))
                .ok_or_else(|| "the load panicked".to_string())?
                .map_err(|e| e.to_string())?;
        }
        self.phase = ChannelPhase::Live {
            inner,
            run: RunState::Stopped,
        };
        self.clock = RunClock::default();
        Ok(())
    }

    /// Loads `name`. On an IO system whose step loads occupants, the load
    /// waits for its ops in flight. Any other step loads, or refuses, at once.
    fn defer_load(&mut self, name: String, io: &mut IoState) -> String {
        let loads = self
            .phase
            .inner()
            .is_some_and(|inner| inner.occupant().is_some());
        let defer = loads && self.has_io(io);
        match core::mem::replace(&mut self.phase, ChannelPhase::Restarting) {
            ChannelPhase::Live { inner, .. } if defer => {
                let message = format!("loading {name}");
                self.phase = ChannelPhase::Loading { inner, name };
                io.quiesce_system(&self.recipe.id);
                message
            }
            ChannelPhase::Live { inner, run } => self.load(inner, run, name, io),
            phase => {
                self.phase = phase;
                refused("the system is gone; reset it first")
            }
        }
    }

    /// Whether the system does IO.
    fn has_io(&self, io: &IoState) -> bool {
        io.index(&self.recipe.id).is_some()
    }

    /// Runs the deferred reset or load once the system is idle.
    fn settle(&mut self, now: Timestamp, io: &mut IoState) {
        let id = &self.recipe.id;
        if !io.is_system_idle(id) {
            return;
        }
        io.end_quiescing_system(id);
        let message = match core::mem::replace(&mut self.phase, ChannelPhase::Restarting) {
            ChannelPhase::Restarting => self.restart_message(io),
            ChannelPhase::Loading { inner, name } => self.load(inner, RunState::Stopped, name, io),
            phase => {
                self.phase = phase;
                return;
            }
        };
        self.say(&message, now);
    }

    /// Loads `name` into `inner`, which was in `run`. A refused load leaves
    /// `inner` as it was.
    fn load(
        &mut self,
        mut inner: Box<dyn Step>,
        run: RunState,
        name: String,
        io: &mut IoState,
    ) -> String {
        match crate::panic::catch(|| inner.load(&name, &mut StepCx::new(io))) {
            Some(Ok(())) => {
                self.phase = ChannelPhase::Live {
                    inner,
                    run: RunState::Stopped,
                };
                let message = format!("loaded {name}");
                self.loaded = Some(name);
                self.clock = RunClock::default();
                message
            }
            Some(Err(error)) => {
                self.phase = ChannelPhase::Live { inner, run };
                refused(error)
            }
            None => {
                crate::panic::catch(|| drop(inner));
                self.fail("the load panicked".into())
            }
        }
    }

    /// Builds the system again over the same rings. The old one must already
    /// be dropped.
    fn remake(&mut self, io: &mut IoState) -> Result<Box<dyn Step>, String> {
        let made = crate::panic::catch(|| self.recipe.make(&mut self.threads, io))
            .ok_or_else(|| "the system's constructor panicked".to_string())?;
        made.map_err(|e| e.to_string())
    }

    /// Ends the channel as failed and returns its message.
    ///
    /// The end is marked as already reported, so the next cycle does not
    /// report it again.
    fn fail(&mut self, reason: String) -> String {
        let reason = clipped(reason);
        let message = format!("failed: {reason}");
        let end = EndReason::Failed(reason.into());
        self.reported_end_reason = Some(end.clone());
        self.phase = ChannelPhase::Gone(end);
        message
    }

    fn step(&mut self, now: Timestamp, cx: &mut StepCx<'_>) {
        if matches!(
            self.phase,
            ChannelPhase::Restarting | ChannelPhase::Loading { .. }
        ) {
            self.settle(now, cx.io);
        } else if let ChannelPhase::Live {
            inner,
            run: RunState::Running,
        } = &mut self.phase
            && catch_execute(inner.as_mut(), now, cx)
        {
            let panicked = ChannelPhase::Gone(EndReason::Panicked);
            let phase = core::mem::replace(&mut self.phase, panicked);
            crate::panic::catch(|| drop(phase));
        }
        self.report_end(now);
        self.write_status(now);
    }

    fn ended(&self) -> Option<&EndReason> {
        match &self.phase {
            ChannelPhase::Gone(end) => Some(end),
            phase => phase.inner().and_then(|inner| inner.ended()),
        }
    }

    /// Reports how the system ended, once per change.
    ///
    /// Raises a message the first cycle the end differs from the last one
    /// reported.
    fn report_end(&mut self, now: Timestamp) {
        let end = self.ended().cloned();
        if end == self.reported_end_reason {
            return;
        }
        if let Some(end) = &end {
            self.say(&ended(end), now);
        }
        self.reported_end_reason = end;
    }

    /// Makes `text` the channel's message, raised at `now`.
    fn say(&mut self, text: &str, now: Timestamp) {
        self.message = FrameStr::truncated(text);
        self.message_at = now.0;
    }

    /// The commands this channel would accept now. `apply` judges by phase
    /// alone, so `Load` is withheld here where the step would refuse it: the
    /// system is gone until a reset, or it never loads anything.
    fn accepts(&self) -> SequenceAccepts {
        let mut accepts = accepts(self.state());
        if !self.phase.inner().is_some_and(|inner| inner.loads()) {
            accepts.0 &= !SequenceAccepts::LOAD;
        }
        accepts
    }

    fn write_status(&mut self, now: Timestamp) {
        let running = matches!(
            self.phase,
            ChannelPhase::Live {
                run: RunState::Running,
                ..
            }
        );
        let ended = self.ended().is_some();
        let (elapsed, abort_elapsed) = self.clock.tick(now, running && !ended, ended);
        let occupant = self.phase.inner().and_then(|inner| inner.occupant());
        let status = ChannelStatus {
            timestamp: now,
            state: self.channel_state().code(),
            loaded: occupant.map_or(u32::MAX, |o| o.index),
            artifact: occupant.map(|o| o.artifact).unwrap_or_default(),
            hash: occupant.map(|o| o.hash).unwrap_or_default(),
            elapsed,
            abort_elapsed,
            accepts: self.accepts().0,
            message: self.message,
            message_at: self.message_at,
        };
        let _ = self.status.write(&status);
    }

    fn channel_state(&self) -> SequenceChannelState {
        match &self.phase {
            ChannelPhase::Restarting | ChannelPhase::Loading { .. } => {
                SequenceChannelState::Quiescing
            }
            ChannelPhase::Live { run, .. } => channel_state(*run, self.ended()),
            ChannelPhase::Gone(end) => channel_state(RunState::Stopped, Some(end)),
        }
    }
}

/// The message for a refused command.
fn refused(reason: impl core::fmt::Display) -> String {
    format!("refused: {reason}")
}

/// The message for how a system ended.
fn ended(end: &EndReason) -> String {
    match end {
        EndReason::Completed => "completed".into(),
        EndReason::Aborted => "aborted".into(),
        EndReason::Failed(reason) => format!("failed: {reason}"),
        EndReason::Panicked => "panicked".into(),
        EndReason::Unloaded => "unloaded".into(),
    }
}

/// The longest failure reason, in bytes, that an [`EndReason`] keeps.
const REASON_CAP: usize = 512;

/// Cuts `text` to at most `REASON_CAP` bytes, on a char boundary.
fn clipped(mut text: String) -> String {
    let mut end = REASON_CAP.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests;
