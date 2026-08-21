//! Packs: many systems from one crate, one construction point.
//!
//! A [`Pack`] is a list of erased system entries a crate's `pack()` fn builds
//! (see `docs/packs.md`). One pack serves every loading
//! mode: [`Registry::register_pack`](crate::Registry) makes each entry a
//! `type=` in the static registry, and the pack ABI exports the same
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
use crate::descriptor::SystemDescriptor;
use crate::handler::IntoPackEntry;
use crate::sequence::Outcome;
use crate::slot::{CyclicSlot, SlotState};

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

/// The params surface an entry decodes at create, by loading mode. Every
/// surface produces the same value by construction: the value tree encodes
/// against the same schema the postcard bytes decode under.
pub enum EntryParams<'a> {
    /// Canonical postcard bytes (the dl / process-worker path).
    Postcard(&'a [u8]),
    /// A params value tree (the static `ParamSource::Value` path). `src`
    /// is the diagnostic snippet errors anchor on; a value tree carries no
    /// spans of its own.
    Value {
        value: &'a serde_json::Value,
        src: &'a str,
        name: &'a str,
        msgs: &'a crate::message::MsgTable,
        /// The shared state this system attaches to, resolved by name from
        /// the target's `SystemSpec::attach` during the states
        /// pass. `None` for an ordinary system; a shared-state entry's create
        /// requires it.
        attach: Option<&'a AttachTarget>,
    },
}

/// The resolved shared state a system attaches to: the erased `Shared<St>`
/// token a shared entry downcasts at create, plus the state's registry type
/// key for the mismatch diagnostic. Built by the resolver's states pass and
/// threaded through [`EntryParams::Value`]; its fields are crate-internal.
pub struct AttachTarget {
    pub ty: &'static str,
    pub token: std::rc::Rc<dyn core::any::Any>,
}

/// A create-phase failure: bad params, or a moved-in state already taken.
#[derive(Debug, thiserror::Error)]
pub enum MakeError {
    /// The postcard params bytes did not decode as the entry's params type.
    #[error("pack entry params did not decode: {0}")]
    Postcard(#[from] postcard::Error),
    /// The value params surface did not deserialize (spans inside).
    #[error(transparent)]
    Params(Box<crate::params::ParamError>),
    /// A `configure` step failed to resolve a config reference.
    #[error(transparent)]
    Configure(#[from] crate::system::ConfigureError),
    /// The entry was built with [`state`](crate::handler::SystemDef::state)
    /// (a moved-in value) and has already been instantiated once.
    #[error("entry holds moved-in state and was already instantiated (not reloadable)")]
    StateTaken,
    /// A shared-state init fn failed (the state's own construction, e.g. a
    /// listener bind).
    #[error("shared state `{state}` failed to construct: {detail}")]
    StateInit { state: &'static str, detail: String },
    /// An entry attached to a shared state was instantiated before the
    /// state's own wiring declaration constructed it.
    #[error("system attaches to shared state `{state}`, which no wiring declaration constructs")]
    StateNotConstructed { state: &'static str },
    /// An entry attached to a shared state was instantiated a second time.
    #[error("shared-state entry was already instantiated once (not reloadable)")]
    SharedEntryReinstantiated,
    /// A shared-state system's wiring named no state to attach to.
    #[error("system `{system}` attaches to a shared state, but no wiring declaration named one")]
    MissingAttach { system: &'static str },
    /// A shared-state system named a state whose type does not match the
    /// concrete shared type the system attaches to.
    #[error(
        "system `{system}` cannot attach to state `{state}`: its type is not the system's shared-state type"
    )]
    AttachTypeMismatch {
        system: &'static str,
        state: &'static str,
    },
}

/// Decode an entry's typed params from any params surface.
pub fn decode_params<P: DeserializeOwned>(params: EntryParams<'_>) -> Result<P, MakeError> {
    match params {
        EntryParams::Postcard(bytes) => Ok(postcard::from_bytes(bytes)?),
        EntryParams::Value {
            value, src, name, ..
        } => {
            // The empty surface serves unit and all-defaults params alike:
            // `{}` cannot deserialize as `()` through serde_json, so route it
            // through the same dual-shape deserializer the static registry
            // path uses. A required field still falls through to the spanned
            // value decode for its missing-field error.
            if value.as_object().is_some_and(|m| m.is_empty())
                && let Ok(p) = P::deserialize(crate::params::NoParams)
            {
                return Ok(p);
            }
            crate::params::decode_value_params(value, name, src)
                .map_err(|e| MakeError::Params(Box::new(e)))
        }
    }
}

/// Resolve an entry's params surface to complete postcard bytes when the
/// entry declared defaults: an absent config uses the defaults verbatim, a
/// value surface is schema-encoded over the decoded default base (top-level
/// overrides), and explicit postcard bytes pass through complete.
pub fn resolve_defaults(
    params: EntryParams<'_>,
    defaults: &[u8],
    schema: &'static NamedType,
) -> Result<Vec<u8>, MakeError> {
    match params {
        EntryParams::Postcard(bytes) if bytes.is_empty() => Ok(defaults.to_vec()),
        EntryParams::Postcard(bytes) => Ok(bytes.to_vec()),
        EntryParams::Value { value, name, .. } => {
            let owned = postcard_schema::schema::owned::OwnedNamedType::from(schema);
            crate::params::encode_value_params(value, &owned, name, Some(defaults))
                .map_err(|e| MakeError::Params(Box::new(e)))
        }
    }
}

/// The bind-phase half of a created entry: bind ports over the ring source
/// (positionally, in descriptor order) and yield the runnable driver.
pub type Pending =
    Box<dyn for<'a, 'b, 'c> FnOnce(&'a mut AnySource<'b, 'c>, Mount) -> Box<dyn Driver>>;

/// What an entry's create phase yields: the bind-phase half, plus the
/// descriptor this configured instance actually carries when it differs
/// from the entry's static one (configure-minted ports). The host registers
/// the instance descriptor; the positional ABI, which binds against the
/// exported static manifest, rejects the divergence instead.
pub struct Created {
    pub pending: Pending,
    pub instance_desc: Option<SystemDescriptor>,
}

impl From<Pending> for Created {
    fn from(pending: Pending) -> Self {
        Self {
            pending,
            instance_desc: None,
        }
    }
}

pub type CreateFn = Box<dyn for<'p> FnMut(EntryParams<'p>) -> Result<Created, MakeError>>;

/// One erased system entry of a [`Pack`]: its name (the registry `type=` /
/// manifest key), its descriptor (computed from parameter types, no instance
/// needed), its params schema, and the two-phase constructor.
pub struct PackEntry {
    pub name: &'static str,
    pub(crate) descriptor: SystemDescriptor,
    pub(crate) params_schema: &'static NamedType,
    /// Canonical postcard bytes of the entry's declared default params, the
    /// base the params encoder overlays config onto. `None` until declared.
    pub(crate) params_default: Option<Vec<u8>>,
    /// `false` for an entry whose state was moved in with `.state(...)`: it
    /// can be instantiated once, and never as a slot occupant.
    pub(crate) reloadable: bool,
    /// `true` for an entry that attaches to a pack-shared state by name
    /// ([`system_type_shared`](Pack::system_type_shared)): its wiring must
    /// carry an `attach`, and a plain entry must not. Drives the resolver's
    /// attach/shared consistency checks.
    pub shared: bool,
    pub create: CreateFn,
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

    pub fn params_default(&self) -> Option<&[u8]> {
        self.params_default.as_deref()
    }

    pub fn reloadable(&self) -> bool {
        self.reloadable
    }

    /// Run the create phase: decode params, build the user state, and hand
    /// back the bind-phase half with its instance descriptor.
    pub fn create(&mut self, params: EntryParams<'_>) -> Result<Created, MakeError> {
        (self.create)(params)
    }

    /// Route this entry's create through [`resolve_defaults`], so its own
    /// decode honors the declared defaults on every params surface.
    pub fn wrap_create_with_defaults(&mut self) {
        let defaults = self
            .params_default
            .clone()
            .expect("defaults declared before wrapping");
        let schema = self.params_schema;
        let inner = std::mem::replace(
            &mut self.create,
            Box::new(|_| unreachable!("replaced below")),
        );
        let mut inner = inner;
        self.create = Box::new(move |params: EntryParams<'_>| {
            let bytes = resolve_defaults(params, &defaults, schema)?;
            inner(EntryParams::Postcard(&bytes))
        });
    }
}

/// The construction half of one pack-declared shared state: decode the
/// state's own params off its wiring declaration and run the init fn.
pub(crate) type StateCreateFn = Box<dyn for<'p> FnMut(EntryParams<'p>) -> Result<(), MakeError>>;

/// One pack-declared shared state: its name (the wiring `state` type key),
/// its params schema, its construction fn, and the erased cell the resolve
/// passes check construction/attachment against.
pub struct StateEntry {
    pub name: &'static str,
    pub cell: std::rc::Rc<dyn crate::shared::ErasedShared>,
    /// The `Shared<St>` token, erased as `Rc<dyn Any>`, so a system attaching
    /// by name can downcast it back to the concrete `Shared<St>` at create.
    /// Cloned into the resolver's name→[`AttachTarget`] map each states pass.
    pub token: std::rc::Rc<dyn core::any::Any>,
    pub create: StateCreateFn,
}

impl StateEntry {
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Construct the state from its wiring declaration's params.
    pub fn create(&mut self, params: EntryParams<'_>) -> Result<(), MakeError> {
        (self.create)(params)
    }
}

/// A crate's system entries, built by its `pack()` fn.
#[derive(Default)]
pub struct Pack {
    entries: Vec<PackEntry>,
    states: Vec<StateEntry>,
}

impl Pack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare this pack's shared state under `name` and hand back the
    /// [`Shared`](crate::Shared) token attached entries capture — sharing
    /// is scoped to this pack because nothing outside `pack()` can reach
    /// the token. The state is constructed once, from its own wiring
    /// declaration's params; a fallible `init` makes resource acquisition
    /// (a listener bind) a resolve-time error rather than a runtime one.
    pub fn shared_state<S, P, E, F>(&mut self, name: &'static str, mut init: F) -> crate::Shared<S>
    where
        S: crate::SharedLifecycle,
        P: DeserializeOwned + postcard_schema::Schema + 'static,
        E: core::fmt::Display + 'static,
        F: FnMut(P) -> Result<S, E> + 'static,
    {
        let token = crate::Shared::new(name);
        let cell = token.erased();
        // The token erased as `Rc<dyn Any>`, so a system attaching by name can
        // downcast it back to `Shared<S>` at create (`S: SharedLifecycle` is
        // `'static`, so `Shared<S>: 'static` and `Any` holds).
        let token_any: std::rc::Rc<dyn core::any::Any> = std::rc::Rc::new(token.clone());
        let create: StateCreateFn = {
            let token = token.clone();
            Box::new(move |params: EntryParams<'_>| {
                let p: P = decode_params(params)?;
                let state = init(p).map_err(|e| MakeError::StateInit {
                    state: name,
                    detail: e.to_string(),
                })?;
                token.set(state).map_err(|_| MakeError::StateInit {
                    state: name,
                    detail: "already constructed (duplicate declaration)".into(),
                })
            })
        };
        self.states.push(StateEntry {
            name,
            cell,
            token: token_any,
            create,
        });
        token
    }

    /// Register a struct-authored system that attaches to a pack-shared state
    /// *by name*: the target's `SystemSpec::attach`
    /// picks which state instance, and the resolver hands `ctor` the resolved
    /// [`Shared`](crate::Shared) token (the second argument) at create time.
    /// `St` is the concrete shared type the entry binds — a target naming a
    /// state of any other type is an `AttachTypeMismatch`. The driver is
    /// wrapped so the state's [`SharedLifecycle`](crate::SharedLifecycle) hooks
    /// run once across all attached entries. Attached entries are cyclic-only,
    /// instantiable once, and never slot occupants.
    pub fn system_type_shared<T, St>(
        mut self,
        name: &'static str,
        mut ctor: impl FnMut(T::Params, crate::Shared<St>) -> T + 'static,
    ) -> Self
    where
        St: crate::SharedLifecycle,
        T: crate::CyclicSystem + crate::BuildSystem + 'static,
        T::Params: DeserializeOwned + postcard_schema::Schema + 'static,
        T::Input: crate::BindPorts + 'static,
        T::Output: crate::HealthOutput + crate::BindPorts + 'static,
    {
        let mut descriptor = <T as crate::CyclicSystem>::descriptor();
        descriptor.name = name.into();
        let static_desc = descriptor.clone();
        let mut taken = false;
        let create: CreateFn = Box::new(move |params: EntryParams<'_>| {
            if taken {
                return Err(MakeError::SharedEntryReinstantiated);
            }
            // A shared entry resolves only through the static registry's value
            // path; the postcard/dl path never attaches.
            {
                // Copy the resolved attach target + msgs off the value surface
                // before `params` moves into decode (the proven `msgs` pattern).
                let (attach, msgs) = match &params {
                    EntryParams::Value { attach, msgs, .. } => (*attach, Some(*msgs)),
                    EntryParams::Postcard(_) => (None, None),
                };
                let attach = attach.ok_or(MakeError::MissingAttach { system: name })?;
                // Recover the concrete token; a wrong-typed state is a clean
                // mismatch rather than a compile-time impossibility.
                let token: crate::Shared<St> = attach
                    .token
                    .downcast_ref::<crate::Shared<St>>()
                    .ok_or(MakeError::AttachTypeMismatch {
                        system: name,
                        state: attach.ty,
                    })?
                    .clone();
                let cell = token.erased();
                if !cell.is_constructed() {
                    return Err(MakeError::StateNotConstructed { state: cell.name() });
                }
                let p: T::Params = decode_params(params)?;
                let mut system = ctor(p, token);
                if let Some(msgs) = msgs {
                    system.configure(&crate::BuildCtx {
                        msgs,
                        namespace: None,
                    })?;
                }
                taken = true;
                cell.attach();
                let instance_desc = instance_desc_if_minted(&system, &static_desc, name);
                let pending: Pending = Box::new(move |src, mount| {
                    crate::handler::mount_driver(src, mount, move |src| {
                        let input = <T::Input as crate::BindPorts>::bind(src);
                        let output = <T::Output as crate::BindPorts>::bind(src);
                        let inner = RunnerDriver(crate::CyclicRunner::new(system, input, output));
                        Box::new(AttachedDriver::new(Box::new(inner), cell))
                    })
                });
                Ok(Created {
                    pending,
                    instance_desc,
                })
            }
        });
        let mut entry = PackEntry {
            name,
            descriptor,
            params_schema: <T::Params as postcard_schema::Schema>::SCHEMA,
            params_default: None,
            reloadable: false,
            shared: true,
            create,
        };
        if entry.params_default.is_some() {
            entry.wrap_create_with_defaults();
        }
        self.entries.push(entry);
        self
    }

    /// Register a fn-authored system under `name`: the entry
    /// [`system(execute_fn)`](crate::handler::system) built, optionally with
    /// `.init(...)` or `.state(...)`.
    pub fn system(mut self, name: &'static str, def: impl IntoPackEntry) -> Self {
        self.entries.push(def.into_entry(name));
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
        let inputs = std::mem::take(&mut sink.inputs);
        let mut outputs = std::mem::take(&mut sink.outputs);
        outputs.push(crate::PortDesc::of::<crate::SystemHealth>());
        outputs.push(crate::PortDesc::msg_named::<crate::LogEvent>("log"));
        let descriptor = SystemDescriptor {
            name: name.into(),
            kind: crate::SystemKind::Cyclic,
            inputs,
            outputs,
            capabilities: Vec::new(),
        };

        let create: CreateFn = Box::new(move |params: EntryParams<'_>| {
            let params_any = (spec.decode)(params)?;
            let f = f.clone();
            let pending: Pending = Box::new(move |src, mount| {
                let clock = std::rc::Rc::new(crate::sequence::CycleClock::default());
                let drops = std::sync::Arc::new(core::sync::atomic::AtomicU64::new(0));
                let future = {
                    let mut cx = crate::handler::BindCx {
                        src,
                        params: Some(params_any),
                        drops: Some(drops.clone()),
                    };
                    f.build(&mut cx)
                };
                let health = bind_health_tail(src);
                let inner = crate::handler::FutureDriver::new(future, clock, health, drops);
                match mount {
                    Mount::Wired => Box::new(inner) as Box<dyn Driver>,
                    // The occupant tail binds after the entry's own ports:
                    // the cancel input past the user inputs, the status
                    // output past the health/log tail.
                    Mount::SlotOccupant => {
                        let control = crate::Input::bind(src);
                        let status = crate::Output::bind(src);
                        Box::new(crate::handler::OccupantFuture::new(inner, control, status))
                    }
                }
            });
            Ok(pending.into())
        });
        self.entries.push(PackEntry {
            name,
            descriptor,
            params_schema,
            params_default: None,
            reloadable: true,
            shared: false,
            create,
        });
        self
    }

    /// As [`task`](Self::task), with declared default params: a config need
    /// spell only its overrides, on every loading path. `P` must be the
    /// task's `Params<P>` type (checked against the declared schema here).
    pub fn task_with_defaults<M, F, P>(mut self, name: &'static str, f: F, defaults: P) -> Self
    where
        M: 'static,
        F: crate::handler::AsyncSystemFn<M> + Clone,
        P: serde::Serialize + postcard_schema::Schema,
    {
        self = self.task(name, f);
        let entry = self.entries.last_mut().expect("task just pushed");
        assert!(
            core::ptr::eq(entry.params_schema, P::SCHEMA)
                || postcard_schema::schema::owned::OwnedNamedType::from(entry.params_schema)
                    == postcard_schema::schema::owned::OwnedNamedType::from(P::SCHEMA),
            "task `{name}` declares Params<{}> but the defaults are a different type",
            entry.params_schema.name,
        );
        entry.params_default = Some(
            postcard::to_allocvec(&defaults)
                .expect("params postcard-encode (Serialize is infallible)"),
        );
        entry.wrap_create_with_defaults();
        self
    }

    /// The entry named `name`, for pack-entry registration use.
    pub fn entry_mut(&mut self, name: &str) -> Option<&mut PackEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }

    /// The shared-state entry named `name`, for direct construction in
    /// tests and the wiring states pass.
    pub fn state_entry_mut(&mut self, name: &str) -> Option<&mut StateEntry> {
        self.states.iter_mut().find(|s| s.name == name)
    }

    pub fn state_entries(&self) -> impl Iterator<Item = &StateEntry> {
        self.states.iter()
    }

    /// The entry at manifest position `index`, the ABI's addressing.
    pub fn entry_at_mut(&mut self, index: usize) -> Option<&mut PackEntry> {
        self.entries.get_mut(index)
    }

    pub fn entries(&self) -> impl Iterator<Item = &PackEntry> {
        self.entries.iter()
    }

    pub fn into_entries(self) -> Vec<PackEntry> {
        self.entries
    }

    /// Split into system entries and shared-state entries, for hosts that
    /// index both (the static registry).
    pub fn into_parts(self) -> (Vec<PackEntry>, Vec<StateEntry>) {
        (self.entries, self.states)
    }
}

/// The configured instance's descriptor when it differs from the entry's
/// static one (a configure step minted ports), else `None` — the static
/// descriptor stands and hosts skip a clone. Compared by encoding: the
/// descriptor is plain serializable data with no cheaper equality.
fn instance_desc_if_minted<S: crate::CyclicSystem>(
    system: &S,
    static_desc: &SystemDescriptor,
    name: &'static str,
) -> Option<SystemDescriptor> {
    let mut desc = system.instance_descriptor();
    desc.name = name.into();
    let minted = postcard::to_allocvec(&desc).expect("descriptor encodes (postcard)")
        != postcard::to_allocvec(static_desc).expect("descriptor encodes (postcard)");
    minted.then_some(desc)
}

/// A [`CyclicRunner`](crate::CyclicRunner) driven behind the pack seam, for
/// struct-authored entries.
struct RunnerDriver<S>(crate::CyclicRunner<S>)
where
    S: crate::CyclicSystem,
    S::Output: crate::HealthOutput;

impl<S> Driver for RunnerDriver<S>
where
    S: crate::CyclicSystem,
    S::Output: crate::HealthOutput,
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

/// The wrapper around an entry attached to a pack-shared state: fans the
/// state's once-per-instance [`SharedLifecycle`](crate::SharedLifecycle)
/// hooks in around the inner driver's own lifecycle — `start` before the
/// first attached init, `shutdown` after the last attached shutdown.
pub(crate) struct AttachedDriver {
    inner: Box<dyn Driver>,
    cell: std::rc::Rc<dyn crate::shared::ErasedShared>,
}

impl AttachedDriver {
    /// The attach count was taken at entry create (where the wiring's
    /// unused-state check reads it); this pairs the release with it.
    pub fn new(inner: Box<dyn Driver>, cell: std::rc::Rc<dyn crate::shared::ErasedShared>) -> Self {
        Self { inner, cell }
    }
}

impl Driver for AttachedDriver {
    fn init(&mut self) {
        self.cell.ensure_started();
        self.inner.init();
    }

    fn step(&mut self, now: Timestamp) -> StepStatus {
        self.inner.step(now)
    }

    fn shutdown(&mut self) {
        self.inner.shutdown();
        self.cell.release();
    }
}

/// The [`CyclicSlot`] adapter over a bound [`Driver`], the pack twin of
/// [`CyclicRunner`](crate::CyclicRunner)'s slot impl. `Done` latches
/// [`SlotState::Done`] and stops stepping, the same terminal handling a
/// sequence-mode dl slot applies.
pub struct DriverSlot {
    pub driver: Box<dyn Driver>,
    pub name: &'static str,
    pub state: SlotState,
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
