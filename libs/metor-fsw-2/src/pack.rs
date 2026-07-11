//! Packs: many systems from one crate, one construction point.
//!
//! A [`Pack`] is a list of erased system entries a crate's `pack()` fn builds
//! (see `docs/design-packs-authoring.md`). One pack serves every loading
//! mode: [`Registry::register_pack`](crate::Registry) makes each entry a
//! `type=` in the static registry, and the pack ABI (v5) exports the same
//! entries from a cdylib. Because `pack()` runs once per load, entries can
//! capture clones of a shared handle built inside it — the construction point
//! two systems sharing an owned resource (a socket, a bus) never had.
//!
//! Construction is two-phase, mirroring the ABI's create/bind split: an
//! entry's `create` decodes params and builds the user state (fail-fast, no
//! rings yet), returning a [`Pending`] that later binds ports over a ring
//! source and yields the runnable [`Driver`]. Descriptors are computed from
//! the parameter *types* at registration, so describing a pack constructs no
//! user state.

use metor_proto::types::Timestamp;
use postcard_schema::schema::NamedType;
use serde::de::DeserializeOwned;

use crate::binder::AnySource;
use crate::coordinator::{CyclicSlot, SlotState};
use crate::handler::IntoPackEntry;
use crate::sequence::Outcome;
use crate::descriptor::SystemDescriptor;

/// One runnable system instance, whatever style authored it. The pack-side
/// convergence of [`CyclicRunner`](crate::CyclicRunner) and the sequence
/// stack: the coordinator (or the ABI shim) inits it once, steps it once per
/// cycle, and shuts it down once.
pub trait Driver {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp) -> StepStatus;
    fn shutdown(&mut self);
}

/// What one step of a [`Driver`] concluded. A cyclic system is always
/// [`Running`](StepStatus::Running); a future-driven occupant whose future
/// returned reports [`Done`](StepStatus::Done) once and is not stepped again.
pub enum StepStatus {
    Running,
    Done(Outcome),
}

/// How an entry is mounted: wired into the graph for the whole run, or
/// loaded into a runtime slot (which appends the framework's occupant
/// control/status tail around the entry's own ports).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mount {
    Wired,
    SlotOccupant,
}

/// The params surface an entry decodes at create, by loading mode. The
/// postcard bytes and the KDL node produce the same value by construction
/// (the KDL front-end encodes against the same schema).
pub enum EntryParams<'a> {
    /// Canonical postcard bytes (the dl / process-worker path).
    Postcard(&'a [u8]),
    /// A `system` node's params surface (the static KDL path). `msgs` is the
    /// registry's message table, for entries whose
    /// [`configure`](crate::BuildSystem::configure) resolves name tokens.
    #[cfg(feature = "kdl")]
    Kdl {
        node: &'a kdl::KdlNode,
        src: &'a str,
        name: &'a str,
        msgs: &'a crate::message::MsgTable,
    },
}

/// A create-phase failure: bad params, or a moved-in state already taken.
#[derive(Debug, thiserror::Error)]
pub enum MakeError {
    /// The postcard params bytes did not decode as the entry's params type.
    #[error("pack entry params did not decode: {0}")]
    Postcard(#[from] postcard::Error),
    /// The KDL params surface did not deserialize (spans inside).
    #[cfg(feature = "kdl")]
    #[error(transparent)]
    Kdl(Box<crate::wiring::LoadError>),
    /// A `configure` step failed to resolve a config reference.
    #[error(transparent)]
    Configure(#[from] crate::system::ConfigureError),
    /// The entry was built with [`state`](crate::handler::SystemDef::state)
    /// (a moved-in value) and has already been instantiated once.
    #[error("entry holds moved-in state and was already instantiated (not reloadable)")]
    StateTaken,
}

/// Decode an entry's typed params from either params surface.
pub(crate) fn decode_params<P: DeserializeOwned>(params: EntryParams<'_>) -> Result<P, MakeError> {
    match params {
        EntryParams::Postcard(bytes) => Ok(postcard::from_bytes(bytes)?),
        #[cfg(feature = "kdl")]
        EntryParams::Kdl {
            node, src, name, ..
        } => crate::wiring::de::from_kdl_node(node, src, name, crate::wiring::SYSTEM_RESERVED, 1)
            .map_err(|e| MakeError::Kdl(Box::new(e))),
    }
}

/// The bind-phase half of a created entry: bind ports over the ring source
/// (positionally, in descriptor order) and yield the runnable driver.
pub type Pending = Box<dyn for<'a, 'b, 'c> FnOnce(&'a mut AnySource<'b, 'c>, Mount) -> Box<dyn Driver>>;

pub(crate) type CreateFn = Box<dyn for<'p> FnMut(EntryParams<'p>) -> Result<Pending, MakeError>>;

/// One erased system entry of a [`Pack`]: its name (the registry `type=` /
/// manifest key), its descriptor (computed from parameter types, no instance
/// needed), its params schema, and the two-phase constructor.
pub struct PackEntry {
    pub(crate) name: &'static str,
    pub(crate) descriptor: SystemDescriptor,
    pub(crate) params_schema: &'static NamedType,
    /// `false` for an entry whose state was moved in with `.state(...)`: it
    /// can be instantiated once, and never as a slot occupant.
    pub(crate) reloadable: bool,
    pub(crate) create: CreateFn,
}

impl PackEntry {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn descriptor(&self) -> &SystemDescriptor {
        &self.descriptor
    }

    pub fn params_schema(&self) -> &'static NamedType {
        self.params_schema
    }

    pub fn reloadable(&self) -> bool {
        self.reloadable
    }

    /// Run the create phase: decode params, build the user state, and hand
    /// back the bind-phase [`Pending`].
    pub fn create(&mut self, params: EntryParams<'_>) -> Result<Pending, MakeError> {
        (self.create)(params)
    }
}

/// A crate's system entries, built by its `pack()` fn.
#[derive(Default)]
pub struct Pack {
    entries: Vec<PackEntry>,
}

impl Pack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fn-authored system under `name`: the entry
    /// [`system(execute_fn)`](crate::handler::system) built, optionally with
    /// `.init(...)` or `.state(...)`.
    pub fn system(mut self, name: &'static str, def: impl IntoPackEntry) -> Self {
        self.entries.push(def.into_entry(name));
        self
    }

    /// Register a struct-authored system type (a [`#[system]`](crate::system)
    /// impl or hand-written trait impls) under `name`. The struct's
    /// `BuildSystem` supplies params and construction; the entry's descriptor
    /// is the type's static one, so config-minted ports and capabilities are
    /// rejected here rather than misbound later.
    pub fn system_type<T, O>(mut self, name: &'static str) -> Self
    where
        T: crate::CyclicSystem<Output = crate::Out<O>> + crate::BuildSystem + 'static,
        T::Params: DeserializeOwned + postcard_schema::Schema + 'static,
        T::Input: crate::BindPorts + 'static,
        O: crate::SystemOutput + crate::BindPorts + 'static,
    {
        let mut descriptor = <T as crate::CyclicSystem>::descriptor();
        assert!(
            descriptor.capabilities.is_empty(),
            "pack entries cannot hold capabilities (`{name}` declares one); \
             capability systems stay on the static registry"
        );
        descriptor.name = name;
        let create: CreateFn = Box::new(move |params: EntryParams<'_>| {
            #[cfg(feature = "kdl")]
            let msgs = match &params {
                EntryParams::Kdl { msgs, .. } => Some(*msgs),
                _ => None,
            };
            let p: T::Params = decode_params(params)?;
            let mut system = T::new(p);
            // The host-context configure phase runs only where a message
            // table exists (the static path); the postcard path is the dl
            // parity path, where configure never runs.
            #[cfg(feature = "kdl")]
            if let Some(msgs) = msgs {
                system.configure(&crate::BuildCtx { msgs })?;
            }
            let pending: Pending = Box::new(move |src, _mount| {
                let input = <T::Input as crate::BindPorts>::bind(src);
                let output = <crate::Out<O> as crate::BindPorts>::bind(src);
                Box::new(RunnerDriver(crate::CyclicRunner::new(system, input, output)))
            });
            Ok(pending)
        });
        self.entries.push(PackEntry {
            name,
            descriptor,
            params_schema: <T::Params as postcard_schema::Schema>::SCHEMA,
            reloadable: true,
            create,
        });
        self
    }

    /// Register an async-fn system under `name`: ports by value, moved into
    /// the future, state in locals — the sequence authoring model as a
    /// general system. The future is polled once per cycle under the
    /// ambient clock, so `wait()`/`now()`/`progress()` work; a future that
    /// returns ends the entry with its [`Outcome`].
    pub fn task<M, F>(mut self, name: &'static str, f: F) -> Self
    where
        M: 'static,
        F: crate::handler::AsyncSystemFn<M> + Clone,
    {
        use crate::handler::{DeclSink, TaskParamsSpec, bind_health_tail};

        let mut sink = DeclSink::default();
        F::decls(&mut sink);
        let spec = sink.task_params.take().unwrap_or_else(TaskParamsSpec::unit);
        let params_schema = spec.schema;
        let (inputs, _) = crate::descriptor::split_decls(std::mem::take(&mut sink.inputs));
        let (mut outputs, _) = crate::descriptor::split_decls(std::mem::take(&mut sink.outputs));
        outputs.push(crate::PortDesc::of::<crate::SystemHealth>());
        outputs.push(crate::PortDesc::of::<crate::SystemLog>());
        let descriptor = SystemDescriptor {
            name,
            kind: crate::SystemKind::Cyclic,
            inputs,
            outputs,
            capabilities: Vec::new(),
        };

        let create: CreateFn = Box::new(move |params: EntryParams<'_>| {
            let params_any = (spec.decode)(params)?;
            let f = f.clone();
            let pending: Pending = Box::new(move |src, _mount| {
                let clock = std::rc::Rc::new(crate::sequence::SeqClock::default());
                let future = {
                    let mut cx = crate::handler::BindCx {
                        src,
                        params: Some(params_any),
                        clock: Some(clock.clone()),
                    };
                    f.build(&mut cx)
                };
                let health = bind_health_tail(src);
                Box::new(crate::handler::FutureDriver::new(future, clock, health))
            });
            Ok(pending)
        });
        self.entries.push(PackEntry {
            name,
            descriptor,
            params_schema,
            reloadable: true,
            create,
        });
        self
    }

    /// The entry named `name`, for direct
    /// [`add_pack_entry`](crate::CoordinatorBuilder::add_pack_entry) use.
    pub fn entry_mut(&mut self, name: &str) -> Option<&mut PackEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }

    pub fn entries(&self) -> impl Iterator<Item = &PackEntry> {
        self.entries.iter()
    }

    pub fn into_entries(self) -> Vec<PackEntry> {
        self.entries
    }
}

/// A [`CyclicRunner`](crate::CyclicRunner) driven behind the pack seam, for
/// struct-authored entries.
struct RunnerDriver<S, O>(crate::CyclicRunner<S, O>)
where
    S: crate::CyclicSystem<Output = crate::Out<O>>,
    O: crate::SystemOutput;

impl<S, O> Driver for RunnerDriver<S, O>
where
    S: crate::CyclicSystem<Output = crate::Out<O>>,
    O: crate::SystemOutput,
{
    fn init(&mut self) {
        self.0.init()
    }
    fn step(&mut self, now: Timestamp) -> StepStatus {
        self.0.step(now);
        StepStatus::Running
    }
    fn shutdown(&mut self) {
        self.0.shutdown()
    }
}

/// The [`CyclicSlot`] adapter over a bound [`Driver`], the pack twin of
/// [`CyclicRunner`](crate::CyclicRunner)'s slot impl. `Done` latches
/// [`SlotState::Done`] and stops stepping, the same terminal handling a
/// sequence-mode dl slot applies.
pub(crate) struct DriverSlot {
    pub(crate) driver: Box<dyn Driver>,
    pub(crate) name: &'static str,
    pub(crate) state: SlotState,
}

impl CyclicSlot for DriverSlot {
    fn init(&mut self) {
        self.driver.init()
    }

    fn step(&mut self, now: Timestamp) {
        if !matches!(self.state, SlotState::Running) {
            return;
        }
        if let StepStatus::Done(outcome) = self.driver.step(now) {
            self.state = SlotState::Done {
                outcome: outcome.run_state(),
            };
        }
    }

    fn shutdown(&mut self) {
        self.driver.shutdown()
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn state(&self) -> &SlotState {
        &self.state
    }
}
