//! Async systems on OS threads: the registry of thread groups and the
//! adapter that copies a system's ports across each cycle.

mod group;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use metor_fsw_3_ring::{NoWake, Notifier, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use crate::async_system::{AsyncSystem, Running, Stop};
use crate::coordinator::{Adapter, EndReason, MakeCx, ParamError, Params, Step};
use crate::mirror::{Mirror, Mirrors, mirror_config};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemDef, SystemInputs, SystemOutputs};

use group::{GroupHandle, Member, Panicked};

/// A `Threads` hands out one group per thread name, held only weakly: the
/// systems placed on a group own it, and a name whose group is gone respawns.
#[derive(Default)]
pub struct Threads {
    groups: HashMap<String, Weak<GroupHandle>>,
}

impl Threads {
    pub fn new() -> Self {
        Self::default()
    }

    /// The group named `name`, started if it is not already running.
    fn get_or_spawn(&mut self, name: &str) -> Result<Arc<GroupHandle>, ParamError> {
        if let Some(group) = self.groups.get(name).and_then(Weak::upgrade) {
            return Ok(group);
        }
        let group = group::spawn(name)?;
        self.groups.insert(name.to_string(), Arc::downgrade(&group));
        Ok(group)
    }
}

/// A `Launch` constructs one async system on the thread that will run it.
pub(crate) trait Launch: Send {
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError>;
}

/// One async system's ingredients, owned until its thread constructs it.
struct AsyncLaunch<A: AsyncSystem, M> {
    make: Arc<M>,
    params: serde_json::Value,
    inputs: Vec<InputBinding<Notifier>>,
    outputs: Vec<OutputBinding>,
    _a: PhantomData<fn() -> A>,
}

impl<A, M> Launch for AsyncLaunch<A, M>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError> {
        let (system, mut state) = (self.make)(Params(&self.params))?;
        let mut inputs = A::Inputs::bind(self.inputs);
        let mut outputs = A::Outputs::bind(self.outputs);
        Ok(Box::pin(async move {
            system
                .run(&mut state, &mut inputs, &mut outputs, stop)
                .await;
        }))
    }
}

/// Places one async system on the group `cx` names: mirrors for its ports, the
/// system built and running on that group's thread, and the adapter that
/// copies between them each cycle.
pub(crate) fn place<A, M>(make: Arc<M>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    if let Some(Adapter::Channel(channel)) = cx.adapter {
        return Err(ParamError::Channel {
            channel: channel.name.clone(),
        });
    }
    let thread_name = cx.thread().to_string();
    let (mut thread, bound_in, bound_out) = bind(cx.def, cx.inputs, cx.outputs)?;
    let member = Member {
        launch: Box::new(AsyncLaunch::<A, M> {
            make,
            params: cx.params.0.clone(),
            inputs: bound_in,
            outputs: bound_out,
            _a: PhantomData,
        }),
        panicked: thread.panicked.clone(),
    };
    let group = cx.threads.get_or_spawn(&thread_name)?;
    group.add(member)?;
    thread.group = Some(group);
    Ok(Box::new(thread))
}

/// How much deeper a mirror ring is than the ring it mirrors.
const MIRROR_FACTOR: usize = 4;

/// A `Thread` is the step an async system is bound as.
///
/// Each cycle it drains the real input rings into the system's mirrors and the
/// system's output mirrors into the real rings. Both are byte copies; a mirror
/// with no room drops the record for this system alone.
pub struct Thread {
    /// The real input rings into the mirrors the system reads, whose writers
    /// wake it, and the system's output mirrors into the real rings.
    mirrors: Mirrors<Notifier>,
    panicked: Panicked,
    end: Option<EndReason>,
    group: Option<Arc<GroupHandle>>,
}

impl Step for Thread {
    fn execute(&mut self, now: Timestamp) {
        let _ = self.mirrors.drain_inputs();
        let _ = self.mirrors.drain_outputs();
        self.mirrors.report_drops(now);
        self.check_panic(now);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.mirrors
            .write_line(now, LogLevel::Error, "panic", message);
    }

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}

impl Thread {
    /// Latches the system off once its task has panicked, reporting it once.
    fn check_panic(&mut self, now: Timestamp) {
        if self.end.is_some() {
            return;
        }
        if let Some(message) = self.panicked.take() {
            self.end = Some(EndReason::Panicked);
            self.mirrors
                .write_line(now, LogLevel::Error, "panic", &message);
        }
    }
}

/// The adapter for the cycle thread and the bindings the system's own thread
/// reads and writes.
type Bound = (Thread, Vec<InputBinding<Notifier>>, Vec<OutputBinding>);
type BoundInputs = (Vec<Mirror<Notifier>>, Vec<InputBinding<Notifier>>);
type BoundOutputs = (Vec<Mirror<NoWake>>, Vec<OutputBinding>);

/// Mirrors every port of one async system.
fn bind(
    def: &SystemDef,
    inputs: Vec<Vec<&RingBuffer>>,
    outputs: Vec<&RingBuffer>,
) -> Result<Bound, ParamError> {
    let (edges, bound_in) = bind_inputs(&def.inputs, inputs)?;
    let (copies, bound_out) = bind_outputs(&def.outputs, outputs)?;
    let thread = Thread {
        mirrors: Mirrors::new(def, edges, copies),
        panicked: Panicked::default(),
        end: None,
        group: None,
    };
    Ok((thread, bound_in, bound_out))
}
fn bind_inputs(
    ports: &[PortDef],
    inputs: Vec<Vec<&RingBuffer>>,
) -> Result<BoundInputs, ParamError> {
    let wake = Notifier::default();
    let mut edges = Vec::new();
    let mut bound_in = Vec::with_capacity(ports.len());
    for (port, rings) in ports.iter().zip(inputs) {
        let mut views = Vec::with_capacity(rings.len());
        for ring in rings {
            let mirror = mirror(ring)?;
            // PANIC Safety: a fresh mirror has its writer and one reader slot free.
            edges.push(Mirror::new(
                port.name.clone(),
                ring.view(NoWake).expect("a counted reader slot"),
                mirror.writer(wake.clone()).expect("a fresh mirror"),
            ));
            views.push(mirror.view(wake.clone()).expect("a fresh mirror"));
        }
        bound_in.push(InputBinding {
            def: port.clone(),
            views,
        });
    }
    Ok((edges, bound_in))
}

fn bind_outputs(ports: &[PortDef], outputs: Vec<&RingBuffer>) -> Result<BoundOutputs, ParamError> {
    let mut copies = Vec::with_capacity(ports.len());
    let mut bound_out = Vec::with_capacity(ports.len());
    for (port, ring) in ports.iter().zip(outputs) {
        let mirror = mirror(ring)?;
        // PANIC Safety: the mirror is fresh; build counted one real writer.
        copies.push(Mirror::new(
            port.name.clone(),
            mirror.view(NoWake).expect("a fresh mirror"),
            ring.writer(NoWake).expect("one writer per output ring"),
        ));
        bound_out.push(OutputBinding {
            def: port.clone(),
            writer: mirror.writer(NoWake).expect("a fresh mirror"),
        });
    }

    Ok((copies, bound_out))
}

/// A ring of the same records as `real`, several times as deep.
fn mirror(real: &RingBuffer) -> Result<RingBuffer, ParamError> {
    let (config, _) = mirror_config(real.config().capacity, MIRROR_FACTOR)?;
    Ok(RingBuffer::create_in_memory(config))
}
