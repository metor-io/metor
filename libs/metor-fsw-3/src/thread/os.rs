//! Runs async systems on OS threads.
//!
//! Systems are grouped by thread name, and each group runs one executor on
//! its own thread. The coordinator steps a [`Thread`] adapter for each system,
//! which copies records between the real rings and the system's mirrors.
//!
//! In a deterministic build each group is turned instead: its executor runs
//! on the coordinator thread, once a cycle, at the step of its first member.

mod group;
#[cfg(test)]
mod tests;
mod turned;

use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::{Arc, Weak};

use metor_fsw_3_ring::{NoWake, Notifier, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use crate::async_system::{AsyncSystem, Running, Stop};
use crate::coordinator::{EndReason, MakeCx, ParamError, Params, Step};
use crate::mirror::{Mirror, Mirrors, mirror_config};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemDef, SystemInputs, SystemOutputs};

use group::{GroupHandle, Member, Panicked};
use turned::TurnedGroup;

/// The registry of running thread groups, one per thread name.
///
/// A group lives as long as the systems placed on it. Once they are all
/// dropped, the thread stops, and the next system to name that thread starts
/// a new one.
#[derive(Default)]
pub struct Threads {
    groups: HashMap<String, Weak<GroupHandle>>,
    turned: HashMap<String, RcWeak<TurnedGroup>>,
    /// Whether groups are turned on the coordinator thread.
    deterministic: bool,
}

/// A group, as a `Thread` step holds it.
enum Group {
    Spawned(#[allow(dead_code)] Arc<GroupHandle>),
    Turned(Rc<TurnedGroup>),
}

impl Threads {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry whose groups the coordinator turns, for a deterministic
    /// run.
    pub fn deterministic() -> Self {
        Self {
            deterministic: true,
            ..Self::default()
        }
    }

    /// Adds `member` to the group named `name`, starting the group if it is
    /// not running.
    fn join(&mut self, name: &str, member: Member) -> Result<Group, ParamError> {
        if self.deterministic {
            let group = self.get_or_turn(name)?;
            group.add(member)?;
            return Ok(Group::Turned(group));
        }
        let group = self.get_or_spawn(name)?;
        group.add(member)?;
        Ok(Group::Spawned(group))
    }

    /// Returns the group named `name`, starting it if it is not running.
    fn get_or_spawn(&mut self, name: &str) -> Result<Arc<GroupHandle>, ParamError> {
        if let Some(group) = self.groups.get(name).and_then(Weak::upgrade) {
            return Ok(group);
        }
        let group = group::spawn(name)?;
        self.groups.insert(name.to_string(), Arc::downgrade(&group));
        Ok(group)
    }

    fn get_or_turn(&mut self, name: &str) -> Result<Rc<TurnedGroup>, ParamError> {
        if let Some(group) = self.turned.get(name).and_then(RcWeak::upgrade) {
            return Ok(group);
        }
        let group = TurnedGroup::new(name)?;
        self.turned.insert(name.to_string(), Rc::downgrade(&group));
        Ok(group)
    }
}

/// Builds an async system on the thread that will run it.
pub(crate) trait Launch: Send {
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError>;
}

/// Everything needed to build one async system, sent to its thread.
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

/// Starts an async system on the thread `cx` names.
///
/// The system is built and run on that thread, reading and writing mirror
/// rings. The returned [`Thread`] step copies between the mirrors and the
/// real rings each cycle.
pub(crate) fn place<A, M>(make: Arc<M>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
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
    thread.group = Some(cx.threads.join(&thread_name, member)?);
    Ok(Box::new(thread))
}

/// How many times larger a mirror ring is than the ring it mirrors.
const MIRROR_FACTOR: usize = 4;

/// The coordinator's step for an async system that runs on another thread.
///
/// Each cycle it copies the real input rings into the system's mirrors, and
/// the system's output mirrors into the real rings. When a mirror is full,
/// the record is dropped for this system only; other readers of the real
/// ring still see it.
pub struct Thread {
    /// Copies records in both directions. Writing to an input mirror wakes
    /// the system.
    mirrors: Mirrors<Notifier>,
    panicked: Panicked,
    end: Option<EndReason>,
    group: Option<Group>,
}

impl Step for Thread {
    fn execute(&mut self, now: Timestamp) {
        let _ = self.mirrors.drain_inputs();
        if let Some(Group::Turned(group)) = &self.group {
            group.turn(now);
        }
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
    /// Checks whether the system's task panicked. On the first panic, marks
    /// the system as ended and logs the panic message.
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

/// The adapter step, plus the port bindings the system uses on its own
/// thread.
type Bound = (Thread, Vec<InputBinding<Notifier>>, Vec<OutputBinding>);
type BoundInputs = (Vec<Mirror<Notifier>>, Vec<InputBinding<Notifier>>);
type BoundOutputs = (Vec<Mirror<NoWake>>, Vec<OutputBinding>);

/// Creates a mirror ring for every port of one async system.
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
            // PANIC Safety: a fresh mirror has its writer and its one reader
            // slot free, and the build reserved a reader slot on `ring`.
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
        // PANIC Safety: the mirror is fresh, and the build gives each output
        // ring exactly one writer.
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

/// Creates a mirror ring for `real`, [`MIRROR_FACTOR`] times its capacity.
fn mirror(real: &RingBuffer) -> Result<RingBuffer, ParamError> {
    let (config, _) = mirror_config(real.config().capacity, MIRROR_FACTOR, 1)?;
    Ok(RingBuffer::create_in_memory(config))
}
