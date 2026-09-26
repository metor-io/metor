//! The channel adapter: a step that wraps one cyclic system and applies the
//! commands addressed to its channel.

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
use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;
use crate::{FrameBytes, FrameStr};

use super::apply::{Command, State, apply};
use super::{
    ABORT_PORT, AbortRequest, COMMANDS_PORT, ChannelState, ChannelStatus, EVENTS_PORT, STATUS_PORT,
};

/// Records the private abort ring holds.
const ABORT_DEPTH: usize = 4;

/// The system's definition with the channel's ports appended, or the name
/// of a system port that takes one of theirs.
pub(crate) fn def(mut inner: SystemDef, cx: &DefCx<'_>) -> Result<SystemDef, String> {
    let reserved = [COMMANDS_PORT, EVENTS_PORT, STATUS_PORT];
    let ports = inner.inputs.iter().chain(&inner.outputs);
    if let Some(port) = ports
        .into_iter()
        .find(|p| reserved.contains(&p.name.as_ref()))
    {
        return Err(port.name.to_string());
    }
    inner
        .inputs
        .push(Input::<SequenceCommand>::def(cx, COMMANDS_PORT));
    inner
        .outputs
        .push(Output::<SequenceChannelEvent>::def(cx, EVENTS_PORT));
    inner
        .outputs
        .push(Output::<ChannelStatus>::def(cx, STATUS_PORT));
    Ok(inner)
}

/// The ports every channel adds, under the empty context.
pub(crate) fn ports() -> [PortDef; 3] {
    let cx = DefCx::empty();
    [
        Input::<SequenceCommand>::def(&cx, COMMANDS_PORT),
        Output::<SequenceChannelEvent>::def(&cx, EVENTS_PORT),
        Output::<ChannelStatus>::def(&cx, STATUS_PORT),
    ]
}

/// Makes the system `make` builds, wrapped in its channel.
///
/// `cx.def` is the definition [`def`] returned, so the channel's ports are
/// the last input and the last two outputs.
pub(crate) fn make(make: &Arc<MakeFn>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    let Some(Adapter::Channel(channel)) = cx.adapter else {
        return make(cx);
    };
    let outer = Outer::split(cx.inputs, cx.outputs);
    let mut def = cx.def.clone();
    def.inputs.pop();
    def.outputs.truncate(outer.outputs.len());
    let mut inputs: Vec<Vec<RingBuffer>> = outer
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
        outputs: outer.outputs.into_iter().cloned().collect(),
    };
    let mut threads = Threads::new();
    let inner = recipe.make(&mut threads)?;
    Ok(Box::new(Channel {
        commands: command_input(outer.commands),
        core: Core {
            name: channel.name.clone(),
            run: channel.start,
            inner: Some(inner),
            end_reason: None,
            reported_end_reason: None,
            loaded: None,
            recipe,
            threads,
            abort,
            events: writer(outer.events),
            status: writer(outer.status),
        },
    }))
}

/// The outer rings, with the ports `def` appended taken off the end.
struct Outer<'a> {
    inputs: Vec<Vec<&'a RingBuffer>>,
    outputs: Vec<&'a RingBuffer>,
    commands: Vec<&'a RingBuffer>,
    events: &'a RingBuffer,
    status: &'a RingBuffer,
}

impl<'a> Outer<'a> {
    fn split(mut inputs: Vec<Vec<&'a RingBuffer>>, mut outputs: Vec<&'a RingBuffer>) -> Self {
        // PANIC Safety: `def` appended one input and two outputs.
        let commands = inputs.pop().expect("the commands input");
        let status = outputs.pop().expect("the channel_status output");
        let events = outputs.pop().expect("the events output");
        Self {
            inputs,
            outputs,
            commands,
            events,
            status,
        }
    }
}

/// Binds the system's `abort` input, if it has one, to a ring the adapter writes.
fn private_abort(def: &SystemDef, inputs: &mut [Vec<RingBuffer>]) -> Option<Output<AbortRequest>> {
    let at = def.inputs.iter().position(|p| p.name == ABORT_PORT)?;
    let ring = abort_ring();
    inputs[at] = vec![ring.clone()];
    Some(writer::<AbortRequest>(&ring))
}

fn command_input(rings: Vec<&RingBuffer>) -> Input<SequenceCommand> {
    // PANIC Safety: the build pass counts one reader slot per edge, and
    // commands are a message record, which any alignment carries.
    Input::try_new(
        rings
            .into_iter()
            .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
            .collect(),
    )
    .expect("message alignment")
}

/// A ring the adapter alone writes and the system alone reads.
fn abort_ring() -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: a small message record at a small depth fits any host.
        capacity: ring_capacity(AbortRequest::MAX_LEN, ABORT_DEPTH).expect("a small ring"),
        max_readers: 1,
    })
}

fn writer<T: Record>(ring: &RingBuffer) -> Output<T> {
    // PANIC Safety: one ring is allocated per output, and the channel's
    // records need no more than the ring's alignment.
    Output::try_new(ring.writer(NoWake).expect("one writer per output ring"))
        .expect("supported alignment")
}

/// Everything needed to make the system again, over the same rings.
struct SystemRecipe {
    make: Arc<MakeFn>,
    id: String,
    adapter: Adapter,
    def: SystemDef,
    params: serde_json::Value,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
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
        })
    }
}

/// A `Channel` is the step a system with a channel is bound as.
pub struct Channel {
    commands: Input<SequenceCommand>,
    core: Core,
}

struct Core {
    name: String,
    run: RunState,
    inner: Option<Box<dyn Step>>,
    /// An end the adapter set: a failed make, or an unwound step.
    end_reason: Option<EndReason>,
    /// The end last reported on `events`.
    reported_end_reason: Option<EndReason>,
    /// The last name a `Load` accepted, re-applied after a reset.
    loaded: Option<String>,
    recipe: SystemRecipe,
    threads: Threads,
    abort: Option<Output<AbortRequest>>,
    events: Output<SequenceChannelEvent>,
    status: Output<ChannelStatus>,
}

/// The most of a reason an event carries, so every refusal fits its record.
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
            SequenceCommandKind::Load { name } => self.load(name),
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

    fn reset(&mut self) -> SequenceEventKind {
        self.run = RunState::Stopped;
        match self.restart() {
            Ok(()) => SequenceEventKind::Loaded {
                name: self.loaded_name(),
            },
            Err(reason) => self.fail(reason),
        }
    }

    /// Reloads the last load in place, making the system first when it is
    /// gone or loads nothing.
    fn restart(&mut self) -> Result<(), String> {
        if self.inner.is_none() || self.loaded.is_none() {
            self.remake()?;
        }
        match (self.inner.as_mut(), &self.loaded) {
            (Some(inner), Some(name)) => reload(inner, name),
            _ => Ok(()),
        }
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

    /// Drops the system, then makes it again over the same rings.
    fn remake(&mut self) -> Result<(), String> {
        self.inner = None;
        self.end_reason = None;
        let made = crate::panic::catch(|| self.recipe.make(&mut self.threads))
            .ok_or_else(|| "the system's constructor panicked".to_string())?;
        self.inner = Some(made.map_err(|e| e.to_string())?);
        Ok(())
    }

    /// Ends the channel as failed, reporting it now rather than next cycle.
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
        if self.run == RunState::Running
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

    /// Emits the event for an end the first cycle it differs from the last reported.
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
            state: ChannelState::of(self.run, self.ended()).code(),
            loaded: occupant.map_or(u32::MAX, |o| o.index),
            artifact: occupant.map_or(FrameStr::EMPTY, |o| o.artifact),
            hash: occupant.map_or(FrameBytes::ZERO, |o| o.hash),
        };
        let _ = self.status.write(&status);
    }

    fn emit(&mut self, kind: SequenceEventKind) {
        let event = SequenceChannelEvent {
            channel: self.name.clone(),
            kind,
        };
        let _ = self.events.write(&event);
    }
}

/// Loads `name` again, which drops the occupant and creates it afresh.
fn reload(inner: &mut Box<dyn Step>, name: &str) -> Result<(), String> {
    crate::panic::catch(|| inner.load(name))
        .ok_or_else(|| "the load panicked".to_string())?
        .map_err(|e| e.to_string())
}

fn refused(reason: String) -> SequenceEventKind {
    SequenceEventKind::Refused {
        reason: clipped(reason),
    }
}

/// `text` cut to `REASON_CAP` bytes on a char boundary.
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
