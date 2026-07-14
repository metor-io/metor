//! Load a coordinator from a [`Wiring`] mission IR.
//!
//! This module is a pure front-end onto the init graph. A [`Wiring`] — a plain
//! data model of systems, slots, edges, and coordinator config — comes either
//! from evaluating a `.py` mission ([`eval_python_mission`]) or from the Rust
//! [`WiringBuilder`]. [`resolve`] walks it, produces a
//! [`Node`](crate::coordinator::init::Node) per system from an app-built
//! [`Registry`], pushes each onto an [`InitGraph`](crate::coordinator::init::InitGraph),
//! records the edges, and hands the graph to
//! [`init::build`](crate::coordinator::init::build). Both front-ends feed the
//! one resolver, so every graph check runs identically for either. No
//! coordinator logic lives here; every error surfaced is a [`LoadError`], a
//! `miette` [`Diagnostic`](miette::Diagnostic) that anchors on the spec's
//! [`SourceRef`] when it has one.
//!
//! ## Params
//!
//! Params reach `resolve` as a [`ParamSource`] — a value tree
//! ([`ParamSource::Value`], the Python front-end's format), the Rust builder's
//! pre-encoded postcard bytes ([`ParamSource::Postcard`]), or nothing
//! ([`ParamSource::None`]). A static system deserializes a typed `S::Params`
//! (serde, field defaults honored). A loaded system's `Params` type is never
//! linked into the host, so a value tree is conformed against the artifact's
//! exported `Params` schema and encoded to postcard bytes
//! ([`encode_value_params`]); pre-encoded bytes pass straight through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use miette::SourceSpan;

use metor_proto::types::ComponentId;

use crate::binder::BindPorts;
use crate::coordinator::init::{self, InitGraph, Node, SystemBind};
use crate::coordinator::slot::{SlotReg, plan_slot};
use crate::coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, InitialOccupant, OccupantBacking,
    PortRef, SlotConfigError, SystemHandle, WireError, validate_slot_spec,
};
use crate::descriptor::{PortId, SystemDescriptor};
use crate::dl::DlSystem;
use crate::message::MsgTable;
use crate::system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, SystemOutput,
};
use crate::telemetry::{TcpRecvTransport, TcpTransport, TelemetrySystem, UplinkSystem};

// The `Wiring` data model, its two front-ends (the Python-eval path and the
// Rust builder), and the cargo build driver for dl artifacts.
mod build_driver;
mod builder;
mod bundle;
mod error;
mod params;
mod py;
mod stubgen;

// The pure-data IR lives at the crate root (feature `wiring-model`) so it is
// available without the front-end; the submodules here reach it as
// `super::model`.
pub(crate) use crate::ir as model;

pub use build_driver::{BuildError, BuildOptions, build_artifacts, build_target};
pub use builder::{SlotSpecBuilder, SystemSpecBuilder, WiringBuilder};
pub use bundle::{
    BundleError, BundleMeta, METOR_EXTENSION, PackageOptions, WIRING_FILE_NAME, load_bundle,
    unpack_metor, write_bundle,
};
pub use error::{Anchor, LoadError, LoadErrorKind};
pub use model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, IR_VERSION,
    InitialOccupantSpec, ParamSource, ScopeSpec, SlotInitState, SlotSpec, SourceRef, SystemSpec,
    TCP_DOWNLINK_TYPE, TCP_UPLINK_TYPE, Wiring,
};
pub use params::encode_value_params;
pub use py::{eval_python_mission, is_python_mission};
pub use stubgen::{StubgenError, StubgenOptions, StubgenReport, stubgen};

/// The shared-object file name for a library `stem` on the host platform
/// (`libfoo.so`, `libfoo.dylib`, `foo.dll`), used by the build driver, the
/// stub generator, and the [`WiringBuilder`] to name an [`Artifact::cdylib`].
pub fn cdylib_file_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

// Re-exported so a system author only needs `metor_fsw_2::wiring`.
pub use crate::register_system;

// ---------------------------------------------------------------------------
// Param deserialization
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Everything a factory decodes to build a [`Node`], erased of the concrete
/// system type.
pub(crate) struct LoadCtx<'a> {
    /// Where the instance's params come from.
    pub params: StaticParams<'a>,
    /// The instance name the system registers under.
    pub name: &'a str,
    /// The registry's message table, which [`BuildSystem::configure`] resolves
    /// config name tokens against.
    pub msgs: &'a MsgTable,
}

/// The params surface a static-path factory decodes, the typed twin of the dl
/// path's [`ParamSource`] reduction.
pub(crate) enum StaticParams<'a> {
    /// A params value tree. `src` is the best-available diagnostic snippet
    /// (a value tree carries no spans of its own), anchored by the spec's
    /// [`SourceRef`] when it has one.
    Value {
        value: &'a serde_json::Value,
        src: &'a str,
    },
    /// No params. Decodes as an all-defaults value, so a required field is
    /// reported as the same missing param an empty params object raises.
    None,
}

impl StaticParams<'_> {
    /// The diagnostic source text, for errors outside the params surface.
    fn diag_src(&self, name: &str) -> String {
        match self {
            StaticParams::Value { src, .. } => (*src).to_string(),
            StaticParams::None => format!("system \"{name}\""),
        }
    }
}

/// A registered factory: decode `ctx.params`, construct the system, run its
/// host-side configure step, and return the finished [`Node`] for the resolver
/// to push. Boxed `Fn`, not a bare `fn`: a pack entry's factory closes over the
/// shared entry it instantiates.
type SystemFactory = Box<dyn Fn(&mut LoadCtx) -> Result<Node, LoadError>>;

/// Marker for a cyclic system's [`IntoNode`] impl.
pub struct CyclicKind;
/// Marker for an async system's [`IntoNode`] impl.
pub struct AsyncKind;

/// Turns a constructed system into an init-graph [`Node`] via the right
/// erasure helper, and reports its static descriptor. The `Kind` parameter
/// keeps the cyclic and async blanket impls from overlapping (a type could in
/// principle implement both system traits), so a single [`Registry::register`]
/// covers either.
///
/// The trait is public (it names a bound on the public
/// [`Registry::register`]), but `into_node` yields the crate-private
/// `Node`: a system author only ever satisfies the trait, never calls it, so
/// the internal return type stays internal.
#[allow(private_interfaces)]
pub trait IntoNode<Kind>: Sized {
    fn into_node(self, name: String) -> Node;
    fn descriptor() -> SystemDescriptor;
}

#[allow(private_interfaces)]
impl<S, O> IntoNode<CyclicKind> for S
where
    S: CyclicSystem<Output = Out<O>> + 'static,
    O: SystemOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn into_node(self, name: String) -> Node {
        init::cyclic_node(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as CyclicSystem>::descriptor()
    }
}

#[allow(private_interfaces)]
impl<S> IntoNode<AsyncKind> for S
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    fn into_node(self, name: String) -> Node {
        init::async_node(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as AsyncSystem>::descriptor()
    }
}

/// The factory [`Registry::register`] stores for one concrete type:
/// deserialize params, `new` the system, run its host-side configure step, and
/// erase it to a [`Node`] behind a plain `fn` pointer. `()` params deserialize
/// from a paramless surface and reject stray properties. The node carries the
/// instance descriptor, not the static one; a system whose configure step
/// minted ports resolves edges against what this instance actually carries.
fn factory<S, K>(ctx: &mut LoadCtx) -> Result<Node, LoadError>
where
    S: BuildSystem + IntoNode<K>,
    S::Params: serde::de::DeserializeOwned,
{
    let params = decode_static_params::<S::Params>(&ctx.params, ctx.name)?;
    let mut system = S::new(params);
    // Resolve config references (message name tokens) against the registry
    // tables the params value cannot carry.
    system
        .configure(&BuildCtx { msgs: ctx.msgs })
        .map_err(|e| match e {
            ConfigureError::UnknownMsg { name, available } => LoadErrorKind::UnknownMsgName {
                system: ctx.name.to_string(),
                msg: name,
                available: available.join(", "),
            }
            .whole(ctx.params.diag_src(ctx.name)),
        })?;
    Ok(system.into_node(ctx.name.to_string()))
}

/// Decode a typed `Params` off a [`StaticParams`] surface: a value tree
/// through serde (unknown keys rejected), no params through the all-defaults
/// [`NoParams`] deserializer.
fn decode_static_params<P: serde::de::DeserializeOwned>(
    params: &StaticParams,
    name: &str,
) -> Result<P, LoadError> {
    match params {
        StaticParams::Value { value, src } => decode_value_params(value, name, src),
        StaticParams::None => P::deserialize(NoParams).map_err(|e| {
            LoadErrorKind::ValueParams {
                system: name.to_string(),
                reason: e.to_string(),
            }
            .at(format!("system \"{name}\""), (0, name.len() + 9).into())
        }),
    }
}

/// A serde deserializer for a params surface that carries no fields: it yields
/// the unit value for `()` and an empty map for a struct (so `#[serde(default)]`
/// fields fill in and a required field is a clean missing-field error). This
/// replaces the paramless synthesized node the old KDL front-end decoded — a
/// `serde_json::Value` alone cannot serve both, since `()` needs `Null` and a
/// defaulted struct needs `{}`.
struct NoParams;

impl<'de> serde::Deserializer<'de> for NoParams {
    type Error = serde::de::value::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        // Default to an empty map, so a struct with defaulted fields decodes.
        v.visit_map(serde::de::value::MapDeserializer::new(
            std::iter::empty::<(&str, &str)>(),
        ))
    }

    fn deserialize_unit<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_none()
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// Deserialize a typed `Params` from a value tree.
///
/// Unlike the dl path's schema conformance, this is plain serde, so
/// `#[serde(default)]` field attributes are honored. `serde_ignored` supplies
/// the typo guard serde_json's deserializer lacks: a key no field consumed is
/// an [`UnknownParam`](LoadErrorKind::UnknownParam), and any other decode failure
/// is a [`ValueParams`](LoadErrorKind::ValueParams) carrying serde's own message.
pub(crate) fn decode_value_params<P: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    system: &str,
    src: &str,
) -> Result<P, LoadError> {
    let mut unknown: Option<String> = None;
    let params = serde_ignored::deserialize(value, |path: serde_ignored::Path<'_>| {
        unknown.get_or_insert_with(|| path.to_string());
    })
    .map_err(|e| {
        LoadErrorKind::ValueParams {
            system: system.to_string(),
            reason: e.to_string(),
        }
        .whole(src)
    })?;
    if let Some(property) = unknown {
        return Err(LoadErrorKind::UnknownParam {
            property,
            system: system.to_string(),
        }
        .whole(src));
    }
    Ok(params)
}

/// One registered type: its factory plus the static descriptor, available
/// without constructing the system so [`resolve`] can order registrations by
/// capability.
struct RegistryEntry {
    factory: SystemFactory,
    descriptor: EntryDescriptor,
}

/// A registered type's descriptor without construction: computed on demand
/// for a type-registered system, carried by value for a pack entry.
enum EntryDescriptor {
    Fn(fn() -> SystemDescriptor),
    Value(Box<SystemDescriptor>),
}

impl EntryDescriptor {
    fn capabilities_contain(&self, cap: crate::Capability) -> bool {
        match self {
            EntryDescriptor::Fn(f) => f().capabilities.contains(&cap),
            EntryDescriptor::Value(d) => d.capabilities.contains(&cap),
        }
    }
}

/// The app-built map from a `type="..."` string to a system factory. It is an
/// explicit table; each system crate can expose a `pub fn register(&mut
/// Registry)` that adds its own systems.
#[derive(Default)]
pub struct Registry {
    factories: HashMap<&'static str, RegistryEntry>,
    /// Message types added via [`register_msg`](Self::register_msg), keyed by
    /// [`NamedMsg::NAME`](crate::NamedMsg). Config name tokens (an uplink's
    /// `msgs` list) resolve against this table in [`BuildSystem::configure`].
    msgs: MsgTable,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with the built-in systems under their `type=`
    /// names: the alarm engine (`"Alarms"`), the TCP telemetry downlink
    /// (`"TcpDownlink"`), and the TCP command uplink (`"TcpUplink"`), plus the
    /// well-known message set in the message table so a mission's `msgs` list
    /// can name any of them out of the box. An app-built registry starts here
    /// and adds its own systems.
    pub fn with_builtins() -> Self {
        use metor_proto_wkt::{
            AlarmAck, AlarmCleared, AlarmDef, AlarmRaised, ReloadSequences, SequenceChannelEvent,
            SequenceCommand, SequenceRegistry,
        };
        let mut r = Self::new();
        r.register::<crate::AlarmSystem, _>("Alarms");
        r.register::<TelemetrySystem<TcpTransport>, _>("TcpDownlink");
        r.register::<UplinkSystem<TcpRecvTransport>, _>("TcpUplink");
        r.register_msg::<SequenceCommand>()
            .register_msg::<SequenceRegistry>()
            .register_msg::<SequenceChannelEvent>()
            .register_msg::<ReloadSequences>()
            .register_msg::<AlarmDef>()
            .register_msg::<AlarmRaised>()
            .register_msg::<AlarmCleared>()
            .register_msg::<AlarmAck>();
        r
    }

    /// Register message type `M` under its stable
    /// [`NamedMsg::NAME`](crate::NamedMsg) token so config lists can name it.
    /// Idempotent.
    pub fn register_msg<M: crate::NamedMsg>(&mut self) -> &mut Self {
        self.msgs.insert::<M>();
        self
    }

    /// Register concrete system `S` under `type_name`.
    ///
    /// `S` supplies its params type and `new` via the format-independent
    /// [`BuildSystem`](crate::BuildSystem); the only params requirement is
    /// `S::Params: DeserializeOwned`, the same derive the postcard contract
    /// already needs, so a statically linked system registers by implementing
    /// `BuildSystem` alone. The cyclic-versus-async branch comes from the
    /// [`IntoNode`] blanket impls, inferred here.
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where
        S: BuildSystem + IntoNode<K> + 'static,
        S::Params: serde::de::DeserializeOwned,
        K: 'static,
    {
        self.factories.insert(
            type_name,
            RegistryEntry {
                factory: Box::new(factory::<S, K>),
                descriptor: EntryDescriptor::Fn(<S as IntoNode<K>>::descriptor),
            },
        );
        self
    }

    /// Register every entry of a pack under its entry name as the `type=`
    /// key, so the same `pack()` a cdylib exports serves a statically-linked
    /// mission. Two instances of one entry construct through the same shared
    /// entry (a non-reloadable `.state(...)` entry rejects the second).
    pub fn register_pack(&mut self, pack: crate::Pack) -> &mut Self {
        use std::cell::RefCell;
        use std::rc::Rc;

        for entry in pack.into_entries() {
            let name = entry.name();
            let descriptor = Box::new(entry.descriptor().clone());
            let entry = Rc::new(RefCell::new(entry));
            let factory = Box::new(move |ctx: &mut LoadCtx| {
                let mut entry = entry.borrow_mut();
                // A paramless surface conforms an empty object against the
                // entry's schema, so its declared defaults fill in — the pack
                // twin of the static `NoParams` decode.
                let empty = serde_json::Value::Object(serde_json::Map::new());
                let synthesized_src;
                let (value, src) = match &ctx.params {
                    StaticParams::Value { value, src } => (*value, *src),
                    StaticParams::None => {
                        synthesized_src = ctx.params.diag_src(ctx.name);
                        (&empty, synthesized_src.as_str())
                    }
                };
                let params = crate::pack::EntryParams::Value {
                    value,
                    src,
                    name: ctx.name,
                    msgs: ctx.msgs,
                };
                // The create phase runs here (a bad config fails at registration);
                // the returned node rides the ordinary cyclic bind path.
                init::pending_node(ctx.name.to_string(), &mut entry, params).map_err(|e| match e {
                    // The params decode already carries its span; unwrap it.
                    crate::pack::MakeError::Params(e) => *e,
                    other => LoadErrorKind::PackCreate {
                        system: ctx.name.to_string(),
                        message: other.to_string(),
                    }
                    .whole(ctx.params.diag_src(ctx.name)),
                })
            });
            self.factories.insert(
                name,
                RegistryEntry {
                    factory,
                    descriptor: EntryDescriptor::Value(descriptor),
                },
            );
        }
        self
    }

    /// Whether `ty` names a registered system whose descriptor carries
    /// [`Capability::ReceiveAll`](crate::Capability). Unknown types answer
    /// `false`; the systems pass reports them as [`LoadErrorKind::UnknownType`] in
    /// document order.
    fn is_receive_all(&self, ty: Option<&str>) -> bool {
        ty.and_then(|ty| self.factories.get(ty))
            .is_some_and(|e| e.descriptor.capabilities_contain(crate::Capability::ReceiveAll))
    }
}

/// Register a concrete system on a [`Registry`] under a `type=` name, keeping
/// the call site terse: `register_system!(registry, ImuDriver => "ImuDriver")`.
#[macro_export]
macro_rules! register_system {
    ($registry:expr, $ty:ty => $name:expr) => {
        $registry.register::<$ty, _>($name)
    };
}

// ---------------------------------------------------------------------------
// The loader
// ---------------------------------------------------------------------------

/// One resolved instance after the systems pass.
struct Instance {
    handle: SystemHandle,
    desc: SystemDescriptor,
}

/// Which port list an endpoint resolves against.
#[derive(Clone, Copy)]
enum Dir {
    Out,
    In,
}

/// Resolve-time overrides that a [`Wiring`] does not itself carry.
///
/// A `Wiring` is a portable mission description, so host-environment policy —
/// where a process worker's executable lives, where the shared-memory session
/// dir is rooted, how a worker's steps are timed and its crashes recovered — is
/// supplied here at [`resolve_with`] time rather than baked into the IR. These
/// are deployment decisions, not mission topology, so they stay off the
/// serialized IR (`wiring.json` is unaffected); each override falls back to the
/// matching [`CoordinatorConfig`] default when `None`. The defaults (re-exec
/// the host binary as the worker, `/dev/shm` or the OS temp dir for sessions,
/// the config's step-timeout and restart policy) are what [`resolve`] uses.
#[derive(Default, Clone, Debug)]
pub struct ResolveOptions {
    /// The process-worker executable, instead of re-executing the host binary.
    /// Tests point this at a fixture worker; a host whose own binary cannot
    /// serve as a worker sets it to a leaner one.
    pub worker_exe: Option<PathBuf>,
    /// The shared-memory session parent dir, instead of the default
    /// (`/dev/shm` when present, else the OS temp dir).
    pub shm_dir: Option<PathBuf>,
    /// Override of [`CoordinatorConfig::proc_step_timeout`]: how long a process
    /// system's step waits for the worker's ack before the cycle moves on.
    pub proc_step_timeout: Option<Duration>,
    /// Override of [`CoordinatorConfig::proc_max_restarts`]: how many times a
    /// dead or panicked worker is respawned over the slot's life (`0` disables
    /// restart).
    pub proc_max_restarts: Option<u32>,
    /// Override of [`CoordinatorConfig::proc_restart_backoff`]: how long a dead
    /// worker's slot waits before respawning.
    pub proc_restart_backoff: Option<Duration>,
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`], the default-option
/// twin of [`resolve_with`].
pub fn resolve(wiring: &Wiring, registry: &Registry) -> Result<Coordinator, LoadError> {
    resolve_with(wiring, registry, ResolveOptions::default())
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`], applying `opts`.
///
/// Both front-ends land here, so static and dl systems go through identical
/// validation, sizing, and telemetry passes. A static system (no `artifact`)
/// is instantiated through the [`Registry`] factory; a dl system is opened
/// from its built [`Artifact::path`] and registered with
/// [`InitGraph::add_dl_cyclic`](crate::coordinator::init::InitGraph::add_dl_cyclic).
///
/// A `Wiring` carries no source text (a builder-origin one never had any), so
/// resolve-time [`LoadError`]s hold a best-effort snippet rather than original
/// document spans. The error variants are the same either way.
pub fn resolve_with(
    wiring: &Wiring,
    registry: &Registry,
    opts: ResolveOptions,
) -> Result<Coordinator, LoadError> {
    if wiring.ir_version != IR_VERSION {
        return Err(LoadErrorKind::IrVersionMismatch {
            found: wiring.ir_version,
            expected: IR_VERSION,
        }
        .bare());
    }
    check_scope_refs(wiring)?;
    check_manifest_hashes(wiring)?;
    let mut config = coordinator_config(&wiring.coordinator);
    // Host-environment policy overrides (see `ResolveOptions`): applied onto the
    // config the IR derived, never onto the IR itself.
    if let Some(timeout) = opts.proc_step_timeout {
        config.proc_step_timeout = timeout;
    }
    if let Some(max) = opts.proc_max_restarts {
        config.proc_max_restarts = max;
    }
    if let Some(backoff) = opts.proc_restart_backoff {
        config.proc_restart_backoff = backoff;
    }
    let mut graph = InitGraph::new(config);
    graph.worker_exe = opts.worker_exe;
    graph.shm_dir = opts.shm_dir;

    // Broadcast the full mission IR as a `WiringManifest` at startup and on
    // reload. Serialized path-stripped so the telemetered topology matches the
    // bundle's `wiring.json` byte-for-byte regardless of the build tree. Both
    // front-ends land here, so the manifest flows whatever the source language.
    let ir_json = serde_json::to_string(&wiring.path_stripped())
        .expect("a resolvable Wiring serializes to JSON");
    graph.set_wiring_manifest(metor_proto_wkt::WiringManifest {
        ir_version: wiring.ir_version,
        ir_json,
    });

    // --- Systems pass: static via the Registry, dl via the loader --------
    let mut instances: HashMap<String, Instance> = HashMap::new();
    // The coordinator's own bundle joins the instance namespace up front.
    // `"coordinator"` is reserved: command edges name it (`connect
    // "coordinator" -> ...`), and a user `system "coordinator"` surfaces as a
    // `DuplicateInstance` error instead of a silent key collision.
    let coord_handle = graph.coordinator_handle();
    instances.insert(
        "coordinator".to_string(),
        Instance {
            handle: coord_handle,
            desc: graph.descriptor_of(coord_handle).clone(),
        },
    );
    // A static `ReceiveAll` system must be the last cyclic registration or
    // `build()` rejects the graph with `ReceiveAllNotLast`, so it is deferred
    // behind every other system and slot. Deferral also gives it the right
    // step position, after every producer and before telemetry. Only the
    // static branch defers; dl systems never carry capabilities.
    let mut deferred: Vec<&SystemSpec> = Vec::new();
    let mut packs = PackCache::default();
    for spec in &wiring.systems {
        let (handle, desc) = match (&spec.artifact, spec.process) {
            (Some(artifact_id), true) => resolve_proc(spec, artifact_id, wiring, &mut graph)?,
            (Some(artifact_id), false) => {
                resolve_dl(spec, artifact_id, wiring, &mut packs, &mut graph)?
            }
            (None, true) => {
                // A process system needs an artifact to spawn a worker; a spec
                // that sets `process` without one lands here.
                return Err(
                    LoadErrorKind::ProcessNeedsArtifact { name: spec.name.clone() }
                        .whole(system_src(spec)),
                );
            }
            (None, false) => {
                if registry.is_receive_all(spec.ty.as_deref()) {
                    deferred.push(spec);
                    continue;
                }
                resolve_static(spec, registry, &mut graph)?
            }
        };
        if instances
            .insert(spec.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(
                LoadErrorKind::DuplicateInstance { name: spec.name.clone() }
                    .whole(system_src(spec)),
            );
        }
    }

    // --- Slots pass: a slot `connect`s by name like a system, so it joins
    //     `instances` before the edges pass.
    for slot in &wiring.slots {
        let (handle, desc) = resolve_slot(slot, wiring, &mut packs, &mut graph)?;
        if instances
            .insert(slot.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(
                LoadErrorKind::DuplicateInstance { name: slot.name.clone() }.whole(slot_src(slot)),
            );
        }
    }

    // --- Deferred receive-all systems: the last cyclic registrations, still
    //     ahead of the edges pass, which only needs the finished instance map.
    for spec in deferred {
        let (handle, desc) = resolve_static(spec, registry, &mut graph)?;
        if instances
            .insert(spec.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(
                LoadErrorKind::DuplicateInstance { name: spec.name.clone() }
                    .whole(system_src(spec)),
            );
        }
    }

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let src = edge_src(edge);
        let span: SourceSpan = (0, src.len()).into();
        let (producer, consumer) = match edge.kind {
            EdgeKind::Frame => (
                resolve_endpoint(
                    &instances,
                    &src,
                    &edge.from,
                    &edge.out,
                    edge.kind,
                    Dir::Out,
                    span,
                )?,
                resolve_endpoint(
                    &instances,
                    &src,
                    &edge.to,
                    &edge.in_,
                    edge.kind,
                    Dir::In,
                    span,
                )?,
            ),
            EdgeKind::Msg => resolve_msg_edge(&instances, &src, edge, span)?,
        };
        // One `connect` entry point for every edge; its behavior is inferred
        // from the connected ports' descriptors, and `EdgeKind` only picked
        // the name-lookup space above. `delayed=#true` into a Log input
        // surfaces `WireError::DelayedLogEdge` at build.
        let result = if edge.delayed {
            graph.connect_delayed(producer, consumer)
        } else {
            graph.connect(producer, consumer)
        };
        result.map_err(|source| LoadErrorKind::Wire { source }.at(src, span))?;
    }

    init::build(graph).map_err(wire_at_build)
}

/// Instantiate a static system through the [`Registry`] factory.
///
/// A value-tree spec passes its params straight through as
/// [`StaticParams::Value`]; a config-less one decodes the entry's defaults via
/// [`StaticParams::None`].
fn resolve_static(
    spec: &SystemSpec,
    registry: &Registry,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    // `type=` is required for a static system; only a dl system can derive it
    // from its artifact. A builder-origin spec could omit it.
    let ty = spec.ty.as_deref().ok_or_else(|| {
        LoadErrorKind::MissingType { name: spec.name.clone() }.whole(system_src(spec))
    })?;
    let factory = registry.factories.get(ty).ok_or_else(|| {
        LoadErrorKind::UnknownType { ty: ty.to_string() }.whole(system_src(spec))
    })?;
    let snippet;
    let params = match &spec.params {
        // Typed builder params ([`ParamSource::Postcard`]) are rejected: the
        // static path deserializes a value tree, not postcard, so the bytes
        // have no decode path and would be silently dropped, leaving the
        // system on defaults.
        ParamSource::Postcard(_) => {
            return Err(LoadErrorKind::StaticPostcardParams {
                system: spec.name.clone(),
                ty: ty.to_string(),
            }
            .whole(system_src(spec)));
        }
        ParamSource::Value(value) => {
            snippet = system_src(spec);
            StaticParams::Value {
                value,
                src: &snippet,
            }
        }
        ParamSource::None => StaticParams::None,
    };
    let node = (factory.factory)(&mut LoadCtx {
        params,
        name: &spec.name,
        msgs: &registry.msgs,
    })?;
    let desc = node.desc.clone();
    let handle = graph.push_node(node);
    Ok((handle, desc))
}

/// The per-resolve cache of opened packs, keyed by artifact id, so an
/// artifact serving several systems or occupants is opened (and its pack
/// constructed) exactly once.
#[derive(Default)]
struct PackCache {
    packs: HashMap<String, crate::dl::DlPack>,
}

impl PackCache {
    /// The opened pack for `artifact_id`, opening it on first use.
    fn open(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        owner: &str,
        src: &str,
        span: SourceSpan,
    ) -> Result<&crate::dl::DlPack, LoadError> {
        if !self.packs.contains_key(artifact_id) {
            let artifact = find_built_artifact(wiring, artifact_id, owner, src, span)?;
            let path = artifact.path.as_ref().expect("checked by find_built_artifact");
            let pack = crate::dl::DlPack::open(path).map_err(|source| {
                LoadErrorKind::DlOpen {
                    system: owner.to_string(),
                    artifact: artifact_id.to_string(),
                    source: Box::new(source),
                }
                .at(src, span)
            })?;
            self.packs.insert(artifact_id.to_string(), pack);
        }
        Ok(&self.packs[artifact_id])
    }
}

/// The two ways an artifact self-describes at resolve: a pack opened in
/// process, or the manifest a describe worker reported for a `process=#true`
/// system or slot (whose artifacts the host never dlopens).
enum EntrySource<'a> {
    Opened {
        pack: &'a crate::dl::DlPack,
        artifact: &'a str,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described {
        entries: &'a [crate::abi::PackEntryDesc],
        artifact: &'a str,
    },
}

impl EntrySource<'_> {
    /// The exported entry names, in manifest order.
    fn entry_names(&self) -> Vec<String> {
        match self {
            EntrySource::Opened { pack, .. } => pack.system_names().map(str::to_string).collect(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => entries
                .iter()
                .map(|e| e.descriptor.name.to_string())
                .collect(),
        }
    }

    /// The sole exported entry name, in which case a spec may omit `type=`.
    fn sole_entry(&self) -> Option<&str> {
        match self {
            EntrySource::Opened { pack, .. } => pack.sole_system(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => match entries {
                [only] => Some(only.descriptor.name.as_str()),
                _ => None,
            },
        }
    }

    /// The artifact id, for diagnostics.
    fn artifact(&self) -> &str {
        match self {
            EntrySource::Opened { artifact, .. } => artifact,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { artifact, .. } => artifact,
        }
    }
}

/// What [`resolve_occupant`] selected for registration: the loaded handle on
/// the dl path (the builder takes it by value), or the descriptor alone on
/// the process path (a worker owns the code; the host keeps the shape).
enum OccupantEntry {
    Opened(DlSystem),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described(SystemDescriptor),
}

impl OccupantEntry {
    /// The loaded handle behind an [`EntrySource::Opened`] resolve.
    fn opened(self) -> DlSystem {
        match self {
            OccupantEntry::Opened(loaded) => loaded,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            OccupantEntry::Described(_) => unreachable!("selected from an opened pack"),
        }
    }

    /// The descriptor behind an [`EntrySource::Described`] resolve.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn described(self) -> SystemDescriptor {
        match self {
            OccupantEntry::Described(desc) => desc,
            OccupantEntry::Opened(_) => unreachable!("selected from a describe worker"),
        }
    }
}

/// Select `entry` from an artifact's self-description (or its sole entry
/// when the spec named none) and encode `params` against the entry's
/// exported `Params` schema — the one open→select→encode path behind
/// [`resolve_dl`], [`resolve_proc`], and both of [`resolve_slot`]'s occupant
/// loops.
///
/// `require_reloadable` is set only for slot occupants, which are
/// instantiated on every load; a wired system is instantiated once, so a
/// non-reloadable `.state(...)` entry is legal there. `owner` names the
/// `system` or `slot` instance in diagnostics.
fn resolve_occupant(
    source: &EntrySource<'_>,
    entry: Option<&str>,
    params: &ParamSource,
    owner: &str,
    require_reloadable: bool,
    src: &str,
    span: SourceSpan,
) -> Result<(OccupantEntry, Vec<u8>), LoadError> {
    let name = match entry {
        Some(name) => name,
        None => source.sole_entry().ok_or_else(|| {
            LoadErrorKind::PackTypeRequired {
                system: owner.to_string(),
                artifact: source.artifact().to_string(),
                available: source.entry_names().join(", "),
            }
            .at(src, span)
        })?,
    };
    let pack_system = |source: crate::dl::DlError| {
        LoadErrorKind::PackSystem {
            system: owner.to_string(),
            source: Box::new(source),
        }
        .at(src, span)
    };
    let not_reloadable = || {
        LoadErrorKind::OccupantNotReloadable {
            slot: owner.to_string(),
            occupant: name.to_string(),
        }
        .at(src, span)
    };
    match source {
        EntrySource::Opened { pack, .. } => {
            let loaded = pack.system(name).map_err(pack_system)?;
            if require_reloadable && !loaded.reloadable() {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                loaded.params_schema(),
                loaded.params_default(),
                owner,
            )?;
            Ok((OccupantEntry::Opened(loaded), params))
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        EntrySource::Described { entries, .. } => {
            let meta = entries
                .iter()
                .find(|e| e.descriptor.name == name)
                .ok_or_else(|| {
                    pack_system(crate::dl::DlError::UnknownPackSystem {
                        name: name.to_string(),
                        available: source.entry_names(),
                    })
                })?;
            if require_reloadable && !meta.reloadable {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                &meta.params_schema,
                meta.params_default.as_deref(),
                owner,
            )?;
            Ok((OccupantEntry::Described(meta.descriptor.clone()), params))
        }
    }
}

/// Resolve a [`ParamSource`] to canonical postcard bytes against an entry's
/// exported `Params` schema and declared defaults. A value tree is
/// schema-encoded (the host never links the type), producing the same bytes
/// a typed builder's `Postcard` source carries; an absent config takes the
/// defaults verbatim.
fn encode_occupant_params(
    params: &ParamSource,
    schema: &postcard_schema::schema::owned::OwnedNamedType,
    defaults: Option<&[u8]>,
    owner: &str,
) -> Result<Vec<u8>, LoadError> {
    Ok(match params {
        ParamSource::None => defaults.unwrap_or_default().to_vec(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Value(value) => encode_value_params(value, schema, owner, defaults)?,
    })
}

/// Load a pack entry and register it via
/// [`InitGraph::add_dl_cyclic`](crate::coordinator::init::InitGraph::add_dl_cyclic). The artifact is opened once per
/// resolve (the cache) and the reconstructed descriptor is returned for edge
/// validation.
fn resolve_dl(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    packs: &mut PackCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let pack = packs.open(wiring, artifact_id, &spec.name, &src, span)?;
    let source = EntrySource::Opened {
        pack,
        artifact: artifact_id,
    };
    let (entry, params) = resolve_occupant(
        &source,
        spec.ty.as_deref(),
        &spec.params,
        &spec.name,
        false,
        &src,
        span,
    )?;
    let loaded = entry.opened();
    let desc = loaded.descriptor().clone();
    let handle = graph.add_dl_cyclic(&spec.name, loaded, params);
    Ok((handle, desc))
}

/// Find an [`Artifact`] by id and require its built `path`, the shared front
/// of the dl and process resolve paths.
fn find_built_artifact<'w>(
    wiring: &'w Wiring,
    artifact_id: &str,
    owner: &str,
    src: &str,
    span: SourceSpan,
) -> Result<&'w Artifact, LoadError> {
    let artifact = wiring
        .artifacts
        .iter()
        .find(|a| a.id == artifact_id)
        .ok_or_else(|| {
            LoadErrorKind::UnknownArtifact {
                system: owner.to_string(),
                artifact: artifact_id.to_string(),
            }
            .at(src, span)
        })?;
    if artifact.path.is_none() {
        return Err(
            LoadErrorKind::ArtifactNotBuilt { artifact: artifact_id.to_string() }.at(src, span),
        );
    }
    Ok(artifact)
}

/// Resolve a `process=#true` system (`docs/process-systems.md`): run a
/// **describe-mode worker** over the built artifact — the host never dlopens
/// it — decode the descriptor and `Params` schema from the worker's bytes,
/// encode the spec's params against that schema, and register through
/// [`InitGraph::add_proc_cyclic`](crate::coordinator::init::InitGraph::add_proc_cyclic). The run worker is spawned later,
/// at `build()`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_proc(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let artifact = find_built_artifact(wiring, artifact_id, &spec.name, &src, span)?;
    let path = artifact.path.as_ref().expect("checked by find_built_artifact");
    let proc_describe = |detail: String| {
        LoadErrorKind::ProcDescribe {
            system: spec.name.clone(),
            artifact: artifact_id.to_string(),
            detail,
        }
        .at(src.clone(), span)
    };
    let bytes = crate::proc::host::describe_via_worker(None, path)
        .map_err(|e| proc_describe(e.to_string()))?;
    let entries: Vec<crate::abi::PackEntryDesc> =
        crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
    let source = EntrySource::Described {
        entries: &entries,
        artifact: artifact_id,
    };
    let (entry, params) = resolve_occupant(
        &source,
        spec.ty.as_deref(),
        &spec.params,
        &spec.name,
        false,
        &src,
        span,
    )?;
    let desc = entry.described();
    let entry_name = desc.name.to_string();
    let handle = graph.add_proc_cyclic(&spec.name, desc.clone(), path.clone(), entry_name, params);
    Ok((handle, desc))
}

/// Without a cross-process futex there is no worker to describe or run.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resolve_proc(
    spec: &SystemSpec,
    _artifact_id: &str,
    _wiring: &Wiring,
    _graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    Err(LoadErrorKind::ProcessUnsupported { name: spec.name.clone() }.whole(system_src(spec)))
}

/// Range-check every scope index in the wiring — the specs' `scope` fields
/// and the table's own `parent` links. The table is front-end metadata, so a
/// bad index is a front-end bug, caught before any system is built.
fn check_scope_refs(wiring: &Wiring) -> Result<(), LoadError> {
    let len = wiring.scopes.len();
    let check = |owner: String, index: Option<usize>| match index {
        Some(index) if index >= len => Err(LoadErrorKind::BadScopeRef { owner, index, len }.bare()),
        _ => Ok(()),
    };
    for scope in &wiring.scopes {
        check(format!("scope `{}`", scope.path), scope.parent)?;
    }
    for spec in &wiring.systems {
        check(format!("system `{}`", spec.name), spec.scope)?;
    }
    for slot in &wiring.slots {
        check(format!("slot `{}`", slot.name), slot.scope)?;
    }
    Ok(())
}

/// Enforce generated-stub freshness: for each artifact whose stub module
/// recorded a `manifest_hash`, compare it against the live pack manifest and
/// fail with [`LoadErrorKind::StaleStubs`] on a mismatch. Artifacts with no
/// recorded hash (builder-authored, hand-written `pack()` handles) or no built
/// path yet are skipped; the dlopen path still opens them.
fn check_manifest_hashes(wiring: &Wiring) -> Result<(), LoadError> {
    for artifact in &wiring.artifacts {
        let Some(recorded) = artifact.manifest_hash.as_deref() else {
            continue;
        };
        let Some(path) = artifact.path.as_deref() else {
            continue;
        };
        // The live manifest is the sidecar the build driver wrote (no dlopen);
        // absent that, describe the built object in process — resolve dlopens
        // it momentarily anyway.
        let bytes = match crate::dl::manifest_sidecar_bytes(path) {
            Some(bytes) => bytes,
            None => match crate::dl::describe_raw(path) {
                Ok(bytes) => bytes,
                // A describe failure is not a staleness verdict; let the
                // dlopen path surface the real load error.
                Err(_) => continue,
            },
        };
        if stubgen::manifest_hash(&bytes) != recorded {
            return Err(LoadErrorKind::StaleStubs { artifact: artifact.id.clone() }.bare());
        }
    }
    Ok(())
}

/// The ` @ file:line:col` suffix locating a spec in its source document, or
/// nothing for a spec with no recorded [`SourceRef`] (the Rust builder).
fn src_anchor(src: Option<&SourceRef>) -> String {
    match src {
        Some(SourceRef {
            file: Some(file),
            line,
            col,
        }) => format!("  @ {file}:{line}:{col}"),
        Some(SourceRef {
            file: None,
            line,
            col,
        }) => format!("  @ {line}:{col}"),
        None => String::new(),
    }
}

/// A best-effort source snippet for a system's resolve-time errors (a
/// [`Wiring`] carries no original document text, only [`SourceRef`] anchors).
fn system_src(spec: &SystemSpec) -> String {
    let anchor = src_anchor(spec.src.as_ref());
    match &spec.ty {
        Some(ty) => format!("system \"{}\" type=\"{}\"{anchor}", spec.name, ty),
        None => format!("system \"{}\"{anchor}", spec.name),
    }
}

/// A best-effort source snippet for a slot's resolve-time errors.
fn slot_src(slot: &SlotSpec) -> String {
    format!("slot \"{}\"{}", slot.name, src_anchor(slot.src.as_ref()))
}

/// The artifact whose pack exports `occ.occupant`: the `artifact=` the allow
/// line named, or (absent one) the unique artifact exporting an entry of
/// that name. No match or more than one is a clean error either way.
fn occupant_artifact(
    wiring: &Wiring,
    packs: &mut PackCache,
    occ: &AllowedOccupantSpec,
    slot: &str,
    src: &str,
    span: SourceSpan,
) -> Result<String, LoadError> {
    if let Some(artifact) = &occ.artifact {
        return Ok(artifact.clone());
    }
    let mut matches: Vec<String> = Vec::new();
    for artifact in &wiring.artifacts {
        let pack = packs.open(wiring, &artifact.id, slot, src, span)?;
        if pack.system_names().any(|n| n == occ.occupant) {
            matches.push(artifact.id.clone());
        }
    }
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(LoadErrorKind::OccupantAmbiguous {
            slot: slot.to_string(),
            occupant: occ.occupant.clone(),
            matches,
        }
        .at(src, span)),
    }
}

/// Map a [`SlotConfigError`] — from the shared pure-spec validation or from
/// [`plan_slot`](crate::coordinator::slot::plan_slot) — onto this front-end's spanned slot
/// diagnostics.
fn slot_config_error(
    err: SlotConfigError,
    slot: &SlotSpec,
    src: &str,
    span: SourceSpan,
) -> LoadError {
    let name = slot.name.clone();
    let src = src.to_string();
    match err {
        SlotConfigError::Empty => LoadErrorKind::EmptySlot { slot: name }.at(src, span),
        SlotConfigError::UnknownInitial { occupant, allowed } => {
            LoadErrorKind::UnknownInitialOccupant {
                slot: name,
                occupant,
                allowed,
            }
            .at(src, span)
        }
        // A mount-reserved port is an occupant-contract defect too: the
        // artifact predates the pack ABI and must be rebuilt.
        SlotConfigError::OccupantMismatch { occupant, .. }
        | SlotConfigError::ReservedPort { occupant, .. } => {
            LoadErrorKind::SlotOccupantMismatch {
                slot: name,
                occupant,
            }
            .at(src, span)
        }
        SlotConfigError::MixedBacking => {
            unreachable!("resolve_slot sources every occupant of a slot from one backing arm")
        }
    }
}

/// Resolve a [`SlotSpec`] into a registered slot.
///
/// Each allowed occupant goes through [`resolve_occupant`] like a dl system —
/// or, for a `process=#true` slot, through [`describe_occupants`], which
/// sources the descriptors from describe workers so the host never dlopens
/// any occupant artifact. Every occupant must share one port contract. The
/// descriptor returned for the edges pass is the one
/// [`plan_slot`](crate::coordinator::slot::plan_slot) derives (the user ports
/// plus the runner tail), not a raw occupant descriptor, and the declared
/// `input`/`output` contract is validated against it.
fn resolve_slot(
    slot: &SlotSpec,
    wiring: &Wiring,
    packs: &mut PackCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = slot_src(slot);
    let span: SourceSpan = (0, src.len()).into();

    // Pure spec validation — a non-empty allow set, and an `initial` name
    // inside it — runs before any artifact is opened, so a typo fails
    // without a dlopen. It checks the `initial` regardless of `state=`: even
    // an `empty` initial names an occupant, and a name that matches nothing
    // is always a mistake.
    let allow_names: Vec<&str> = slot.allow.iter().map(|a| a.occupant.as_str()).collect();
    validate_slot_spec(&allow_names, slot.initial.as_ref().map(|i| i.occupant.as_str()))
        .map_err(|e| slot_config_error(e, slot, &src, span))?;

    // Open and param-encode each allowed occupant. `occupant=` is the allow
    // node's only reserved key and there is no leading name argument, so both
    // line-property and child-node params reach the encoder. A process slot
    // takes the describe-worker path instead; the backing decides the slot's
    // mode in `add_slot`, so the two arms may not mix.
    let allowed: Vec<AllowedOccupant> = if slot.process {
        describe_occupants(slot, wiring, &src, span)?
    } else {
        let mut allowed = Vec::with_capacity(slot.allow.len());
        for occ in &slot.allow {
            let artifact_id =
                occupant_artifact(wiring, packs, occ, &slot.name, &src, span)?;
            let pack = packs.open(wiring, &artifact_id, &slot.name, &src, span)?;
            let source = EntrySource::Opened {
                pack,
                artifact: &artifact_id,
            };
            let (entry, params) = resolve_occupant(
                &source,
                Some(&occ.occupant),
                &occ.params,
                &slot.name,
                true,
                &src,
                span,
            )?;
            allowed.push(AllowedOccupant::dl(occ.occupant.clone(), entry.opened(), params));
        }
        allowed
    };

    let initial = match &slot.initial {
        None => None,
        Some(i) => match i.state {
            SlotInitState::Empty => None,
            SlotInitState::Loaded => Some(InitialOccupant {
                occupant: i.occupant.clone(),
                start: false,
            }),
            SlotInitState::Running => Some(InitialOccupant {
                occupant: i.occupant.clone(),
                start: true,
            }),
        },
    };

    // Derive the registered contract and reject a bad occupant set through the
    // one shared planner. `plan_slot` is the one place the registered contract
    // (user ports plus the slot's own command fan-in) is derived — and the one
    // place the descriptor-level checks (occupant mutual compatibility, the
    // mount-appended ports) run — so this front-end cannot drift from it.
    let (registered, ports, process) = plan_slot(&slot.name, &allowed, initial.as_ref())
        .map_err(|e| slot_config_error(e, slot, &src, span))?;
    let desc = registered.clone();
    let handle = graph.push_node(Node {
        name: slot.name.clone(),
        desc: registered,
        bind: SystemBind::Slot(SlotReg {
            allowed,
            initial,
            ports,
            process,
        }),
    });

    // Every declared `input`/`output` frame must name an edge-connected
    // registered port. The runner-held tail (slot control, slot status, the
    // sequence channels) is not part of the user contract; the edge outputs
    // do include the implicit status and log tail, which a declaration may
    // name but need not.
    use crate::descriptor::PortConn;
    for frame in &slot.inputs {
        if !desc
            .inputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && &p.name == frame)
        {
            return Err(LoadErrorKind::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "input",
                frame: frame.clone(),
            }
            .at(src.clone(), span));
        }
    }
    for frame in &slot.outputs {
        if !desc
            .outputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && &p.name == frame)
        {
            return Err(LoadErrorKind::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "output",
                frame: frame.clone(),
            }
            .at(src.clone(), span));
        }
    }

    Ok((handle, desc))
}

/// Resolve a process slot's allowed set (`docs/process-slots.md` §8): the
/// [`resolve_proc`] recipe once per allowed occupant. Each occupant's built
/// artifact is described by a short-lived **describe worker** — the host
/// never dlopens it, at resolve or at any later `Load` — the descriptor and
/// `Params` schema are decoded from the worker's bytes, and the occupant's
/// params are encoded against that schema exactly as the dl path
/// encodes them. The resulting [`AllowedOccupant`]s carry
/// [`OccupantBacking::Artifact`], which is what makes `add_slot` register
/// the slot process-mode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn describe_occupants(
    slot: &SlotSpec,
    wiring: &Wiring,
    src: &str,
    span: SourceSpan,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    // Manifests by artifact id, so a slot allowing several entries of one
    // pack runs one describe worker for it, not one per occupant.
    let mut manifests: HashMap<String, Vec<crate::abi::PackEntryDesc>> = HashMap::new();
    fn describe(
        manifests: &mut HashMap<String, Vec<crate::abi::PackEntryDesc>>,
        wiring: &Wiring,
        slot: &SlotSpec,
        artifact_id: &str,
        src: &str,
        span: SourceSpan,
    ) -> Result<(), LoadError> {
        if manifests.contains_key(artifact_id) {
            return Ok(());
        }
        let artifact = find_built_artifact(wiring, artifact_id, &slot.name, src, span)?;
        let path = artifact.path.as_ref().expect("checked by find_built_artifact");
        let proc_describe = |detail: String| {
            LoadErrorKind::ProcDescribe {
                system: slot.name.clone(),
                artifact: artifact_id.to_string(),
                detail,
            }
            .at(src, span)
        };
        let bytes = crate::proc::host::describe_via_worker(None, path)
            .map_err(|e| proc_describe(e.to_string()))?;
        let entries =
            crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
        manifests.insert(artifact_id.to_string(), entries);
        Ok(())
    }

    let mut allowed = Vec::with_capacity(slot.allow.len());
    for occ in &slot.allow {
        // The occupant's artifact: named, or the unique artifact whose
        // manifest exports the entry (each described through a worker; the
        // host never dlopens a process slot's artifacts).
        let artifact_id = match &occ.artifact {
            Some(id) => id.clone(),
            None => {
                for artifact in &wiring.artifacts {
                    describe(&mut manifests, wiring, slot, &artifact.id, src, span)?;
                }
                let matches: Vec<String> = manifests
                    .iter()
                    .filter(|(_, entries)| {
                        entries.iter().any(|e| e.descriptor.name == occ.occupant)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                match matches.as_slice() {
                    [only] => only.clone(),
                    _ => {
                        return Err(LoadErrorKind::OccupantAmbiguous {
                            slot: slot.name.clone(),
                            occupant: occ.occupant.clone(),
                            matches,
                        }
                        .at(src, span));
                    }
                }
            }
        };
        describe(&mut manifests, wiring, slot, &artifact_id, src, span)?;
        let source = EntrySource::Described {
            entries: &manifests[&artifact_id],
            artifact: &artifact_id,
        };
        let (entry, params) = resolve_occupant(
            &source,
            Some(&occ.occupant),
            &occ.params,
            &slot.name,
            true,
            src,
            span,
        )?;
        let artifact = find_built_artifact(wiring, &artifact_id, &slot.name, src, span)?;
        let path = artifact.path.as_ref().expect("checked by find_built_artifact");
        allowed.push(AllowedOccupant {
            name: occ.occupant.clone(),
            params,
            descriptor: entry.described(),
            backing: OccupantBacking::Artifact(path.clone()),
        });
    }
    Ok(allowed)
}

/// Without a cross-process futex there is no worker to describe or spawn, so
/// a process slot is rejected like a process system.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn describe_occupants(
    slot: &SlotSpec,
    _wiring: &Wiring,
    src: &str,
    span: SourceSpan,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    Err(LoadErrorKind::ProcessUnsupported { name: slot.name.clone() }.at(src, span))
}

/// A best-effort source snippet for an edge's resolve-time errors.
fn edge_src(edge: &EdgeSpec) -> String {
    let arrow = if edge.delayed { "~>" } else { "->" };
    format!(
        "connect \"{}\" {arrow} \"{}\" out=\"{}\" in=\"{}\"{}",
        edge.from,
        edge.to,
        edge.out,
        edge.in_,
        src_anchor(edge.src.as_ref())
    )
}

/// Convert the serializable [`CoordinatorSpec`] into the runtime
/// [`CoordinatorConfig`].
fn coordinator_config(spec: &CoordinatorSpec) -> CoordinatorConfig {
    let mut config = CoordinatorConfig {
        cycle_rate: spec.cycle_rate,
        ..CoordinatorConfig::default()
    };
    if let Some(depth) = spec.default_depth {
        config.default_depth = depth;
    }
    config.clock = match spec.clock {
        ClockSpec::Wall => ClockMode::Wall,
        ClockSpec::Simulated { dt_secs } => ClockMode::Simulated {
            dt: Duration::from_secs_f64(dt_secs),
        },
    };
    config
}

/// Resolve a `msg=` edge's two endpoints jointly.
///
/// The token names the message type and is matched against each endpoint's
/// packet-port display names. An endpoint whose port carries an overridden
/// display name (a coordinator-minted channel such as `"commands"`) is then
/// matched by the packet id the token resolved to on the other endpoint. Only
/// when neither endpoint matches the token is the edge an
/// [`UnknownMsg`](LoadErrorKind::UnknownMsg).
fn resolve_msg_edge(
    instances: &HashMap<String, Instance>,
    src: &str,
    edge: &EdgeSpec,
    span: SourceSpan,
) -> Result<(PortRef, PortRef), LoadError> {
    let inst = |name: &str| {
        instances
            .get(name)
            .ok_or_else(|| LoadErrorKind::UnknownInstance { name: name.to_string() }.at(src, span))
    };
    let prod = inst(&edge.from)?;
    let cons = inst(&edge.to)?;

    let by_name = |ports: &[crate::descriptor::PortDesc], token: &str| {
        ports
            .iter()
            .find(|p| matches!(p.id(), PortId::Packet(_)) && p.name == token)
            .map(|p| p.id())
    };
    let by_id = |ports: &[crate::descriptor::PortDesc], id: PortId| {
        ports.iter().find(|p| p.id() == id).map(|p| p.id())
    };
    let unknown = |instance: &str, msg: &str| {
        LoadErrorKind::UnknownMsg {
            instance: instance.to_string(),
            msg: msg.to_string(),
        }
        .at(src, span)
    };

    let p_named = by_name(&prod.desc.outputs, &edge.out);
    let c_named = by_name(&cons.desc.inputs, &edge.in_);
    let (p_port, c_port) = match (p_named, c_named) {
        (Some(p), Some(c)) => (p, c),
        (Some(p), None) => (
            p,
            by_id(&cons.desc.inputs, p).ok_or_else(|| unknown(&edge.to, &edge.in_))?,
        ),
        (None, Some(c)) => (
            by_id(&prod.desc.outputs, c).ok_or_else(|| unknown(&edge.from, &edge.out))?,
            c,
        ),
        (None, None) => return Err(unknown(&edge.from, &edge.out)),
    };
    Ok((
        PortRef {
            system: prod.handle,
            port: p_port,
        },
        PortRef {
            system: cons.handle,
            port: c_port,
        },
    ))
}

/// Resolve one `(instance, port)` endpoint to a [`PortRef`], validating the
/// name against the instance descriptor's port list so a typo is a load error.
fn resolve_endpoint(
    instances: &HashMap<String, Instance>,
    src: &str,
    name: &str,
    port_name: &str,
    kind: EdgeKind,
    dir: Dir,
    span: SourceSpan,
) -> Result<PortRef, LoadError> {
    let inst = instances
        .get(name)
        .ok_or_else(|| LoadErrorKind::UnknownInstance { name: name.to_string() }.at(src, span))?;
    let ports = match dir {
        Dir::Out => &inst.desc.outputs,
        Dir::In => &inst.desc.inputs,
    };
    let port = match kind {
        EdgeKind::Frame => {
            let id = PortId::Component(ComponentId::new(port_name));
            if !ports.iter().any(|p| p.id() == id) {
                return Err(LoadErrorKind::UnknownFrame {
                    instance: name.to_string(),
                    frame: port_name.to_string(),
                }
                .at(src, span));
            }
            id
        }
        // A message port is matched by its display name (the message type
        // name); the packet id comes from the matched port, not a name hash,
        // because some message types hand-assign their ids.
        EdgeKind::Msg => {
            let found = ports
                .iter()
                .find(|p| matches!(p.id(), PortId::Packet(_)) && p.name == port_name);
            match found {
                Some(p) => p.id(),
                None => {
                    return Err(LoadErrorKind::UnknownMsg {
                        instance: name.to_string(),
                        msg: port_name.to_string(),
                    }
                    .at(src, span));
                }
            }
        }
    };
    Ok(PortRef {
        system: inst.handle,
        port,
    })
}

/// Wrap a `build()`-time [`WireError`] as a [`LoadErrorKind::Wire`]. A [`Wiring`]
/// is format-independent, so the diagnostic uses the error's own rendered
/// message as its source snippet rather than original document spans; the
/// variant callers and tests match on is unchanged.
fn wire_at_build(err: WireError) -> LoadError {
    let src = err.to_string();
    LoadErrorKind::Wire { source: err }.whole(src)
}

#[cfg(test)]
mod tests;
