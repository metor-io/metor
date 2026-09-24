//! The channel adapter: a step that wraps one cyclic system and applies the
//! commands addressed to its channel.

use std::sync::Arc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};

use crate::coordinator::{
    Adapter, End, MakeCx, MakeFn, ParamError, Params, RunState, Step, Stepped, catch_step,
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
    let abort_at = def.inputs.iter().position(|p| p.name == ABORT_PORT);
    let abort = abort_at.map(|at| {
        let ring = abort_ring();
        inputs[at] = vec![ring.clone()];
        writer::<AbortRequest>(&ring)
    });
    let recipe = Recipe {
        make: make.clone(),
        id: cx.id.to_string(),
        adapter: Adapter::Channel(channel.clone()),
        def,
        params: cx.params.0.clone(),
        inputs,
        outputs: outputs.into_iter().cloned().collect(),
    };
    let mut threads = Threads::new();
    let inner = recipe.make(&mut threads)?;
    Ok(Box::new(Channel {
        // PANIC Safety: the build pass counts one reader slot per edge, and
        // commands are a message record, which any alignment carries.
        commands: Input::try_new(
            commands
                .into_iter()
                .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
                .collect(),
        )
        .expect("message alignment"),
        core: Core {
            name: channel.name.clone(),
            run: channel.start,
            inner: Some(inner),
            end: None,
            reported: false,
            loaded: None,
            recipe,
            threads,
            abort,
            events: writer(events),
            status: writer(status),
        },
    }))
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
struct Recipe {
    make: Arc<MakeFn>,
    id: String,
    adapter: Adapter,
    def: SystemDef,
    params: serde_json::Value,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
}

impl Recipe {
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
    end: Option<End>,
    /// Whether the current end has been reported.
    reported: bool,
    /// The last name a `Load` accepted, re-applied after a reset.
    loaded: Option<String>,
    recipe: Recipe,
    /// Empty: a cyclic make places nothing on a thread.
    threads: Threads,
    abort: Option<Output<AbortRequest>>,
    events: Output<SequenceChannelEvent>,
    status: Output<ChannelStatus>,
}

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
            Err(refusal) => SequenceEventKind::Refused {
                reason: refusal.to_string(),
            },
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
            SequenceCommandKind::Abort => {
                if let Some(abort) = self.abort.as_mut() {
                    let _ = abort.write(&AbortRequest {});
                }
                SequenceEventKind::Progress {
                    detail: "abort requested".into(),
                }
            }
            SequenceCommandKind::Reset => self.reset(),
            SequenceCommandKind::Load { name } => self.load(name),
        }
    }

    fn reset(&mut self) -> SequenceEventKind {
        self.run = RunState::Stopped;
        match self.remake() {
            Ok(()) => SequenceEventKind::Loaded {
                name: self.loaded_name(),
            },
            Err(reason) => self.fail(reason),
        }
    }

    fn load(&mut self, name: String) -> SequenceEventKind {
        if self.inner.is_none()
            && let Err(reason) = self.remake()
        {
            return self.fail(reason);
        }
        let Some(inner) = self.inner.as_mut() else {
            return self.fail("no system to load into".into());
        };
        match crate::panic::catch(|| inner.load(&name)) {
            Some(Ok(())) => {
                self.run = RunState::Stopped;
                self.end = None;
                self.reported = false;
                self.loaded = Some(name.clone());
                SequenceEventKind::Loaded { name }
            }
            Some(Err(error)) => SequenceEventKind::Refused {
                reason: error.to_string(),
            },
            None => {
                self.inner = None;
                self.fail("the load panicked".into())
            }
        }
    }

    /// Drops the system, then makes it again and re-applies the last load.
    fn remake(&mut self) -> Result<(), String> {
        self.inner = None;
        self.end = None;
        self.reported = false;
        let made = crate::panic::catch(|| self.recipe.make(&mut self.threads))
            .ok_or_else(|| "the system's constructor panicked".to_string())?;
        let inner = self.inner.insert(made.map_err(|e| e.to_string())?);
        if let Some(name) = &self.loaded {
            inner.load(name).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Ends the channel as failed, reporting it now rather than next cycle.
    fn fail(&mut self, reason: String) -> SequenceEventKind {
        self.end = Some(End::Failed(reason.clone().into()));
        self.reported = true;
        SequenceEventKind::Failed { reason }
    }

    fn loaded_name(&self) -> String {
        self.loaded
            .clone()
            .unwrap_or_else(|| self.recipe.id.clone())
    }

    fn step(&mut self, now: Timestamp) {
        if self.run == RunState::Running
            && self.end.is_none()
            && catch_step(&mut self.inner, now) == Stepped::Unwound
        {
            self.end = Some(End::Panicked);
        }
        self.report_end();
        self.write_status(now);
    }

    fn ended(&self) -> Option<&End> {
        self.end
            .as_ref()
            .or_else(|| self.inner.as_ref().and_then(|inner| inner.ended()))
    }

    /// Emits the event for an end the first cycle it is seen.
    fn report_end(&mut self) {
        let event = match self.ended() {
            None => {
                self.reported = false;
                return;
            }
            Some(_) if self.reported => return,
            Some(end) => end_event(end),
        };
        self.reported = true;
        self.emit(event);
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

fn end_event(end: &End) -> SequenceEventKind {
    match end {
        End::Completed => SequenceEventKind::Completed,
        End::Aborted => SequenceEventKind::Aborted,
        End::Failed(reason) => SequenceEventKind::Failed {
            reason: reason.to_string(),
        },
        End::Panicked => SequenceEventKind::Stopped,
        End::Unloaded => SequenceEventKind::Unloaded,
    }
}

#[cfg(test)]
mod tests;
