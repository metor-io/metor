//! The channel adapter, which puts a cyclic system behind a channel.
//!
//! The adapter wraps the system's step. Each cycle it applies the commands
//! sent to its channel, steps the system if it is running, and reports the
//! channel's events and status.

use std::sync::Arc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};

use crate::coordinator::{
    Adapter, EndReason, MakeCx, MakeFn, ParamError, Params, RunState, Step, Stepped, catch_step,
};
use crate::def::DefCx;
use crate::io::SharedIo;
use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;
use crate::{FrameBytes, FrameStr};

use super::apply::{Command, State, apply};
use super::{
    ABORT_PORT, AbortRequest, COMMANDS_PORT, ChannelState, ChannelStatus, EVENTS_PORT, STATUS_PORT,
};

/// The number of records the private abort ring holds.
const ABORT_DEPTH: usize = 4;

/// Appends the channel's ports to a system's definition.
///
/// # Errors
/// Returns the name of a system port that already uses one of the channel's
/// port names.
pub(crate) fn def(mut inner: SystemDef, cx: &DefCx<'_>) -> Result<SystemDef, String> {
    let reserved = [COMMANDS_PORT, EVENTS_PORT, STATUS_PORT];
    let mut taken = inner.inputs.iter().chain(&inner.outputs);
    if let Some(port) = taken.find(|p| reserved.contains(&p.name.as_ref())) {
        return Err(port.name.to_string());
    }
    let [commands, events, status] = ports(cx);
    inner.inputs.push(commands);
    inner.outputs.extend([events, status]);
    Ok(inner)
}

/// Returns the ports every channel adds.
///
/// These are the commands input, then the events and status outputs.
pub(crate) fn ports(cx: &DefCx<'_>) -> [PortDef; 3] {
    [
        Input::<SequenceCommand>::def(cx, COMMANDS_PORT),
        Output::<SequenceChannelEvent>::def(cx, EVENTS_PORT),
        Output::<ChannelStatus>::def(cx, STATUS_PORT),
    ]
}

/// Builds a system with `make` and wraps it in a channel.
///
/// Without a channel adapter in `cx`, returns the system unwrapped. `cx.def`
/// must be the definition [`def`] returned, so the channel's ports are the
/// last input and the last two outputs.
pub(crate) fn make(make: &Arc<MakeFn>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    let Some(Adapter::Channel(channel)) = cx.adapter else {
        return make(cx);
    };
    let (mut inputs, mut outputs) = (cx.inputs, cx.outputs);
    // PANIC Safety: `def` appended one input and two outputs.
    let commands = inputs.pop().expect("the commands input");
    let status = outputs.pop().expect("the channel_status output");
    let events = outputs.pop().expect("the events output");
    let mut def = cx.def.clone();
    def.inputs.pop();
    def.outputs.truncate(outputs.len());
    let mut inputs: Vec<Vec<RingBuffer>> = inputs
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
        outputs: outputs.into_iter().cloned().collect(),
        io: cx.io.clone(),
        seed: cx.seed,
        namespace: cx.namespace.to_string(),
    };
    let mut threads = Threads::new();
    let inner = recipe.make(&mut threads)?;
    Ok(Box::new(Channel {
        commands: command_input(commands),
        core: Core {
            name: channel.name.clone(),
            run: channel.start,
            inner: Some(inner),
            end_reason: None,
            reported_end_reason: None,
            loaded: None,
            deferred: None,
            recipe,
            threads,
            abort,
            events: writer(events),
            status: writer(status),
        },
    }))
}

/// Rebinds the system's `abort` input to a private ring.
///
/// Returns the writer the adapter uses to send aborts, or `None` if the
/// system has no `abort` input.
fn private_abort(def: &SystemDef, inputs: &mut [Vec<RingBuffer>]) -> Option<Output<AbortRequest>> {
    let at = def.inputs.iter().position(|p| p.name == ABORT_PORT)?;
    let ring = abort_ring();
    inputs[at] = vec![ring.clone()];
    Some(writer::<AbortRequest>(&ring))
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

/// Creates a ring that only the adapter writes and only the system reads.
fn abort_ring() -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: a small message record at a small depth fits any host.
        capacity: ring_capacity(AbortRequest::MAX_LEN, ABORT_DEPTH).expect("a small ring"),
        max_readers: 1,
    })
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
    io: SharedIo,
    seed: Option<u64>,
    namespace: String,
}

impl SystemRecipe {
    fn make(&self, threads: &mut Threads) -> Result<Box<dyn Step>, ParamError> {
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
            io: &self.io,
            seed: self.seed,
            namespace: &self.namespace,
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
    run: RunState,
    inner: Option<Box<dyn Step>>,
    /// An end the adapter set itself: a failed make, or a step that
    /// panicked.
    end_reason: Option<EndReason>,
    /// The end last reported on `events`.
    reported_end_reason: Option<EndReason>,
    /// The last name a `Load` accepted. A reset loads it again.
    loaded: Option<String>,
    /// A reset or load waiting for the system's ops in flight to finish.
    deferred: Option<Deferred>,
    recipe: SystemRecipe,
    threads: Threads,
    abort: Option<Output<AbortRequest>>,
    events: Output<SequenceChannelEvent>,
    status: Output<ChannelStatus>,
}

/// What a quiescing channel does once its system has nothing in flight.
enum Deferred {
    Restart,
    Load(String),
}

/// The longest reason, in bytes, that an event carries.
///
/// This keeps every refusal small enough to fit in its record.
const REASON_CAP: usize = 512;

impl Step for Channel {
    fn execute(&mut self, now: Timestamp) {
        let Self { commands, core } = self;
        for command in commands.drain() {
            match command {
                Ok(command) if command.channel == core.name => core.command(command.command),
                _ => {}
            }
        }
        core.step(now);
    }
}

impl Core {
    fn command(&mut self, kind: SequenceCommandKind) {
        let event = match apply(self.state(), Command::of(&kind)) {
            Ok(()) => self.transition(kind),
            Err(refusal) => refused(refusal.to_string()),
        };
        self.emit(event);
    }

    fn state(&self) -> State {
        State {
            run: self.run,
            ended: self.ended().is_some(),
            abortable: self.abort.is_some(),
            quiescing: self.deferred.is_some(),
        }
    }

    fn transition(&mut self, kind: SequenceCommandKind) -> SequenceEventKind {
        match kind {
            SequenceCommandKind::Start => {
                self.run = RunState::Running;
                SequenceEventKind::Started
            }
            SequenceCommandKind::Stop => {
                self.run = RunState::Stopped;
                SequenceEventKind::Stopped
            }
            SequenceCommandKind::Abort => self.abort(),
            SequenceCommandKind::Reset => self.reset(),
            SequenceCommandKind::Load { name } => self.defer_load(name),
        }
    }

    fn abort(&mut self) -> SequenceEventKind {
        let written = self
            .abort
            .as_mut()
            .is_some_and(|abort| abort.write(&AbortRequest {}).is_ok());
        if !written {
            return refused("the abort ring is full".into());
        }
        SequenceEventKind::Progress {
            detail: "abort requested".into(),
        }
    }

    /// Resets the system. An IO system is dropped now and made again once its
    /// ops in flight finish.
    fn reset(&mut self) -> SequenceEventKind {
        self.run = RunState::Stopped;
        let Some(member) = self.member() else {
            return self.restarted();
        };
        self.inner = None;
        self.end_reason = None;
        self.recipe.io.borrow_mut().begin(&member);
        self.deferred = Some(Deferred::Restart);
        quiescing()
    }

    fn restarted(&mut self) -> SequenceEventKind {
        match self.restart() {
            Ok(()) => SequenceEventKind::Loaded {
                name: self.loaded_name(),
            },
            Err(reason) => self.fail(reason),
        }
    }

    /// Loads the last loaded name again.
    ///
    /// If the system is gone or has nothing loaded, builds it again first.
    fn restart(&mut self) -> Result<(), String> {
        if self.inner.is_none() || self.loaded.is_none() {
            self.remake()?;
        }
        match (self.inner.as_mut(), &self.loaded) {
            (Some(inner), Some(name)) => reload(inner, name),
            _ => Ok(()),
        }
    }

    /// Loads `name`. On an IO system whose step loads occupants, the load
    /// waits for its ops in flight. Any other step loads, or refuses, at once.
    fn defer_load(&mut self, name: String) -> SequenceEventKind {
        let loads = self
            .inner
            .as_ref()
            .is_some_and(|inner| inner.occupant().is_some());
        let Some(member) = self.member().filter(|_| loads) else {
            return self.load(name);
        };
        self.recipe.io.borrow_mut().begin(&member);
        self.deferred = Some(Deferred::Load(name));
        quiescing()
    }

    /// The system's IO member, if it has one.
    fn member(&self) -> Option<std::rc::Rc<crate::io::shared::Member>> {
        self.recipe.io.borrow().find(&self.recipe.id)
    }

    /// Runs the deferred reset or load once the system is idle.
    fn settle(&mut self) {
        if let Some(member) = self.member() {
            if !self.recipe.io.borrow().idle(&member) {
                return;
            }
            self.recipe.io.borrow().end(&member);
        }
        let event = match self.deferred.take() {
            Some(Deferred::Restart) => self.restarted(),
            Some(Deferred::Load(name)) => self.load(name),
            None => return,
        };
        self.emit(event);
    }

    fn load(&mut self, name: String) -> SequenceEventKind {
        let Some(inner) = self.inner.as_mut() else {
            return refused("the system is gone; reset it first".into());
        };
        match crate::panic::catch(|| inner.load(&name)) {
            Some(Ok(())) => {
                self.run = RunState::Stopped;
                self.end_reason = None;
                self.loaded = Some(name.clone());
                SequenceEventKind::Loaded { name }
            }
            Some(Err(error)) => refused(error.to_string()),
            None => {
                self.inner = None;
                self.fail("the load panicked".into())
            }
        }
    }

    /// Drops the system, then builds it again over the same rings.
    fn remake(&mut self) -> Result<(), String> {
        self.inner = None;
        self.end_reason = None;
        let made = crate::panic::catch(|| self.recipe.make(&mut self.threads))
            .ok_or_else(|| "the system's constructor panicked".to_string())?;
        self.inner = Some(made.map_err(|e| e.to_string())?);
        Ok(())
    }

    /// Ends the channel as failed and returns the `Failed` event.
    ///
    /// The end is marked as already reported, so the next cycle does not
    /// report it again.
    fn fail(&mut self, reason: String) -> SequenceEventKind {
        let reason = clipped(reason);
        self.end_reason = Some(EndReason::Failed(reason.clone().into()));
        self.reported_end_reason = self.end_reason.clone();
        SequenceEventKind::Failed { reason }
    }

    fn loaded_name(&self) -> String {
        self.loaded
            .clone()
            .unwrap_or_else(|| self.recipe.id.clone())
    }

    fn step(&mut self, now: Timestamp) {
        if self.deferred.is_some() {
            self.settle();
        } else if self.run == RunState::Running
            && self.end_reason.is_none()
            && catch_step(&mut self.inner, now) == Stepped::Unwound
        {
            self.end_reason = Some(EndReason::Panicked);
        }
        self.report_end();
        self.write_status(now);
    }

    fn ended(&self) -> Option<&EndReason> {
        self.end_reason
            .as_ref()
            .or_else(|| self.inner.as_ref().and_then(|inner| inner.ended()))
    }

    /// Reports how the system ended, once per change.
    ///
    /// Emits an event the first cycle the end differs from the last one
    /// reported.
    fn report_end(&mut self) {
        let end = self.ended().cloned();
        if end == self.reported_end_reason {
            return;
        }
        if let Some(end) = &end {
            self.emit(end_event(end));
        }
        self.reported_end_reason = end;
    }

    fn write_status(&mut self, now: Timestamp) {
        let occupant = self.inner.as_ref().and_then(|inner| inner.occupant());
        let status = ChannelStatus {
            timestamp: now,
            state: self.channel_state().code(),
            loaded: occupant.map_or(u32::MAX, |o| o.index),
            artifact: occupant.map_or(FrameStr::EMPTY, |o| o.artifact),
            hash: occupant.map_or(FrameBytes::ZERO, |o| o.hash),
        };
        let _ = self.status.write(&status);
    }

    fn channel_state(&self) -> ChannelState {
        match self.deferred {
            Some(_) => ChannelState::Quiescing,
            None => ChannelState::of(self.run, self.ended()),
        }
    }

    fn emit(&self, kind: SequenceEventKind) {
        let event = SequenceChannelEvent {
            channel: self.name.clone(),
            kind,
        };
        let _ = self.events.write(&event);
    }
}

/// Loads `name` again.
///
/// This drops the current occupant and creates a new one.
fn reload(inner: &mut Box<dyn Step>, name: &str) -> Result<(), String> {
    crate::panic::catch(|| inner.load(name))
        .ok_or_else(|| "the load panicked".to_string())?
        .map_err(|e| e.to_string())
}

fn quiescing() -> SequenceEventKind {
    SequenceEventKind::Progress {
        detail: "quiescing".into(),
    }
}

fn refused(reason: String) -> SequenceEventKind {
    SequenceEventKind::Refused {
        reason: clipped(reason),
    }
}

/// Cuts `text` to at most `REASON_CAP` bytes, on a char boundary.
fn clipped(mut text: String) -> String {
    let mut end = REASON_CAP.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn end_event(end: &EndReason) -> SequenceEventKind {
    match end {
        EndReason::Completed => SequenceEventKind::Completed,
        EndReason::Aborted => SequenceEventKind::Aborted,
        EndReason::Failed(reason) => SequenceEventKind::Failed {
            reason: clipped(reason.to_string()),
        },
        EndReason::Panicked => SequenceEventKind::Failed {
            reason: "panicked".into(),
        },
        EndReason::Unloaded => SequenceEventKind::Unloaded,
    }
}

#[cfg(test)]
mod tests;
