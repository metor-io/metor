//! Runs async systems on OS threads.
//!
//! Systems are grouped by thread name, and each group runs one executor on
//! its own thread. The coordinator steps a [`ThreadAdapter`] for each system,
//! which copies records between the real rings and the system's mirrors.
//!
//! In a deterministic build each group is turned instead: the coordinator
//! owns it and runs its executor on the coordinator thread, once a cycle,
//! just before the step of its first member.

mod group;
#[cfg(test)]
mod tests;
mod turned;

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use metor_fsw_3_ring::{NoWake, Notifier, RingBuffer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use crate::async_system::{AsyncSystem, Running, Stop};
use crate::coordinator::{EndReason, MakeCx, MakeError, Params, Step};
use crate::fn_system::Instance;
use crate::mirror::{Mirror, Mirrors};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemDef};

use super::{LaunchCx, Launched};
use group::{GroupHandle, Member, Panicked};
pub(crate) use turned::TurnedGroup;

/// The registry of running thread groups, one per thread name.
///
/// A group lives as long as the systems placed on it. Once they are all
/// dropped, the thread stops, and the next system to name that thread starts
/// a new one.
pub struct Threads(Groups);

/// The groups of a [`Threads`] registry.
enum Groups {
    /// Each group runs on its own OS thread.
    Spawned(HashMap<String, Weak<GroupHandle>>),
    /// The coordinator turns each group on its own thread.
    Turned(HashMap<String, TurnedGroup>),
}

impl Default for Threads {
    fn default() -> Self {
        Self(Groups::Spawned(HashMap::new()))
    }
}

impl Threads {
    /// A registry whose groups the coordinator turns, for a deterministic
    /// run.
    pub fn deterministic() -> Self {
        Self(Groups::Turned(HashMap::new()))
    }

    /// Hands over the turned groups, for the coordinator to own.
    pub(crate) fn into_turned(self) -> impl Iterator<Item = TurnedGroup> {
        let groups = match self.0 {
            Groups::Spawned(_) => HashMap::new(),
            Groups::Turned(groups) => groups,
        };
        groups.into_values()
    }

    /// Adds the member `id` to the group named `name`, starting the group if
    /// it is not running.
    ///
    /// Returns the spawned group the member keeps alive, if it has one.
    fn join(
        &mut self,
        name: &str,
        id: &str,
        member: Member,
    ) -> Result<Option<Arc<GroupHandle>>, MakeError> {
        match &mut self.0 {
            Groups::Spawned(groups) => {
                let group = match groups.get(name).and_then(Weak::upgrade) {
                    Some(group) => group,
                    None => {
                        let group = GroupHandle::spawn(name)?;
                        groups.insert(name.to_string(), Arc::downgrade(&group));
                        group
                    }
                };
                group.add(member)?;
                Ok(Some(group))
            }
            Groups::Turned(groups) => {
                let group = match groups.entry(name.to_string()) {
                    Entry::Occupied(group) => group.into_mut(),
                    Entry::Vacant(slot) => slot.insert(TurnedGroup::new(name, id)?),
                };
                group.add(member)?;
                Ok(None)
            }
        }
    }
}

/// Builds an async system on the thread that will run it.
pub(crate) trait Launch: Send {
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, MakeError>;
}

/// Everything needed to build one async system, sent to its thread.
struct AsyncLaunch<A: AsyncSystem, M> {
    make: Arc<M>,
    params: serde_json::Value,
    inputs: Vec<InputBinding<Notifier>>,
    outputs: Vec<OutputBinding>,
    instance: Instance,
    _a: PhantomData<fn() -> A>,
}

impl<A, M> Launch for AsyncLaunch<A, M>
where
    A: AsyncSystem + 'static,
    M: Fn(LaunchCx<'_>) -> Result<Launched<A>, MakeError> + Send + Sync + 'static,
{
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, MakeError> {
        let Launched {
            system,
            mut state,
            mut inputs,
            mut outputs,
        } = (self.make)(LaunchCx {
            params: Params(&self.params),
            inputs: self.inputs,
            outputs: self.outputs,
            instance: self.instance,
        })?;
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
/// rings. The returned [`ThreadAdapter`] step copies between the mirrors and
/// the real rings each cycle.
pub(crate) fn place<A, M>(make: Arc<M>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, MakeError>
where
    A: AsyncSystem + 'static,
    M: Fn(LaunchCx<'_>) -> Result<Launched<A>, MakeError> + Send + Sync + 'static,
{
    let instance = Instance::from_make_cx(&cx);
    let thread_name = cx.thread().to_string();
    let bound = Bound::new(cx.def, cx.inputs, cx.outputs)?;
    let panicked = Panicked::default();
    let member = Member {
        launch: Box::new(AsyncLaunch::<A, M> {
            make,
            params: cx.params.0.clone(),
            inputs: bound.inputs,
            outputs: bound.outputs,
            instance,
            _a: PhantomData,
        }),
        panicked: panicked.clone(),
    };
    let group = cx.threads.join(&thread_name, cx.id, member)?;
    Ok(Box::new(ThreadAdapter {
        mirrors: Mirrors::new(cx.def, bound.input_mirrors, bound.output_mirrors),
        panicked,
        end: None,
        _group: group,
    }))
}

/// How many times larger a mirror ring is than the ring it mirrors.
const MIRROR_FACTOR: usize = 4;

/// The coordinator's step for an async system that runs on another thread.
///
/// Each cycle it copies the real input rings into the system's mirrors, and
/// the system's output mirrors into the real rings. When a mirror is full,
/// the record is dropped for this system only; other readers of the real
/// ring still see it.
pub struct ThreadAdapter {
    /// Copies records in both directions. Writing to an input mirror wakes
    /// the system.
    mirrors: Mirrors<Notifier>,
    panicked: Panicked,
    end: Option<EndReason>,
    /// The spawned group, kept running while this system is placed on it.
    _group: Option<Arc<GroupHandle>>,
}

impl Step for ThreadAdapter {
    fn execute(&mut self, now: Timestamp) {
        let fed = self.mirrors.drain_inputs();
        let taken = self.mirrors.drain_outputs();
        if fed.and(taken).is_err() {
            self.end = Some(EndReason::Failed(Cow::Borrowed("corrupt ring")));
            self.mirrors
                .write_line(now, LogLevel::Error, "corrupt", "a mirror ring is corrupt");
        }
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

impl ThreadAdapter {
    /// Checks whether the system's task panicked. On the first panic, marks
    /// the system as ended and logs the panic message.
    fn check_panic(&mut self, now: Timestamp) {
        if self.end.is_some() {
            return;
        }
        if let Some(message) = self.panicked.message() {
            self.end = Some(EndReason::Panicked);
            self.mirrors
                .write_line(now, LogLevel::Error, "panic", message);
        }
    }
}

/// The mirrors of one async system, and the port bindings it uses on its own
/// thread.
struct Bound {
    input_mirrors: Vec<Mirror<Notifier>>,
    output_mirrors: Vec<Mirror<NoWake>>,
    inputs: Vec<InputBinding<Notifier>>,
    outputs: Vec<OutputBinding>,
}

impl Bound {
    /// Creates a mirror ring for every port of `def`.
    fn new(
        def: &SystemDef,
        inputs: Vec<Vec<&RingBuffer>>,
        outputs: Vec<&RingBuffer>,
    ) -> Result<Self, MakeError> {
        let mut bound = Self {
            input_mirrors: Vec::new(),
            output_mirrors: Vec::with_capacity(def.outputs.len()),
            inputs: Vec::with_capacity(def.inputs.len()),
            outputs: Vec::with_capacity(def.outputs.len()),
        };
        bound.bind_inputs(&def.inputs, inputs)?;
        bound.bind_outputs(&def.outputs, outputs)?;
        Ok(bound)
    }

    fn bind_inputs(
        &mut self,
        ports: &[PortDef],
        inputs: Vec<Vec<&RingBuffer>>,
    ) -> Result<(), MakeError> {
        let wake = Notifier::default();
        for (port, rings) in ports.iter().zip(inputs) {
            let mut views = Vec::with_capacity(rings.len());
            for ring in rings {
                let mirror = Mirror::ring_for(ring, MIRROR_FACTOR)?;
                // PANIC Safety: a fresh mirror has its writer and its one reader
                // slot free, and the build reserved a reader slot on `ring`.
                self.input_mirrors.push(Mirror::new(
                    port.name.clone(),
                    ring.view(NoWake).expect("a counted reader slot"),
                    mirror.writer(wake.clone()).expect("a fresh mirror"),
                ));
                views.push(mirror.view(wake.clone()).expect("a fresh mirror"));
            }
            self.inputs.push(InputBinding {
                def: port.clone(),
                views,
            });
        }
        Ok(())
    }

    fn bind_outputs(
        &mut self,
        ports: &[PortDef],
        outputs: Vec<&RingBuffer>,
    ) -> Result<(), MakeError> {
        for (port, ring) in ports.iter().zip(outputs) {
            let mirror = Mirror::ring_for(ring, MIRROR_FACTOR)?;
            // PANIC Safety: the mirror is fresh, and the build gives each output
            // ring exactly one writer.
            self.output_mirrors.push(Mirror::new(
                port.name.clone(),
                mirror.view(NoWake).expect("a fresh mirror"),
                ring.writer(NoWake).expect("one writer per output ring"),
            ));
            self.outputs.push(OutputBinding {
                def: port.clone(),
                writer: mirror.writer(NoWake).expect("a fresh mirror"),
            });
        }
        Ok(())
    }
}
