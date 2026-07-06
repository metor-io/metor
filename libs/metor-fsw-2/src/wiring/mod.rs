//! The KDL wiring front-end (wiring.md).
//!
//! A pure front-end onto the [`CoordinatorBuilder`]: it parses a KDL
//! document, instantiates each `system` from an app-built [`Registry`], resolves
//! each `connect` edge to a [`PortRef`](crate::PortRef), and `build()`s a
//! [`Coordinator`]. No coordinator logic lives here — every error surfaced is a
//! span-carrying [`LoadError`] (a `miette` [`Diagnostic`](miette::Diagnostic)
//! mirroring `metor-proto-kdl`'s `KdlSchematicError`).
//!
//! ## Schema (the params surface)
//!
//! Scalar params and coordinator config are canonically **properties on the node
//! line**; nested/sequence params are **child nodes** (`docs/design-kdl-serde.md` —
//! both surfaces feed one serde deserializer, `de.rs`). Everything else follows
//! wiring.md §1:
//!
//! ```kdl
//! coordinator cycle_rate=200.0 default_depth=8           // wall clock, paced
//! coordinator cycle_rate=200.0 sim_dt=0.00833            // simulated clock, free-run
//! system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
//! system "nav" type="MekfFilter" gain=0.8 {
//!     pid p=1.0 i=0.5 d=0.1                  // a nested params struct
//!     taps 1 2 3                             // a sequence param
//! }
//! connect "imu" -> "nav" frame="imu"          // shorthand
//! connect from="nav" out="nav" to="log" in="nav"   // explicit long form
//! connect "ctrl" -> "plant" frame="torque_cmd" delayed=#true   // feedback back-edge
//! ```
//!
//! `sim_dt` (seconds, optional) selects a [`ClockMode::Simulated`] clock with that
//! logical per-cycle step (the loop free-runs, no pacing); absent ⇒ a paced `Wall`
//! clock holding `cycle_rate`. `delayed=#true` on a `connect` marks the edge as a
//! one-cycle-delayed feedback back-edge ([`CoordinatorBuilder::connect_delayed`]).

use std::collections::HashMap;
use std::time::Duration;

use kdl::{KdlDocument, KdlNode};
use miette::SourceSpan;

use metor_proto::types::ComponentId;

use crate::binder::BindPorts;
use crate::coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig,
    InitialOccupant, PortRef, SystemHandle, WireError,
};
use crate::descriptor::{PortId, SystemDescriptor, compatible};
use crate::dl::DlSystem;
use crate::message::MsgTable;
use crate::system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, SystemOutput,
};
use crate::telemetry::{TcpRecvTransport, TcpTransport, TelemetrySystem, UplinkSystem};

// The `Wiring` data model, the Rust builder, and the cargo build driver. KDL is *one*
// deserializer onto `Wiring` (see `parse`/`resolve` below); the builder is the other;
// one shared `resolve` consumes either.
mod build_driver;
mod builder;
mod bundle;
mod de;
mod error;
mod kdl_params;
mod model;
mod parse;

pub use build_driver::{BuildError, BuildOptions, build_artifacts};
pub use error::LoadError;
pub use kdl_params::encode_kdl_params;
pub use parse::{cdylib_file_name, parse};
pub use bundle::{BundleError, PackageOptions, load_bundle, write_bundle};
pub use builder::{SlotSpecBuilder, SystemSpecBuilder, WiringBuilder};
pub use model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, SystemSpec, TCP_DOWNLINK_TYPE,
    TCP_UPLINK_TYPE, Wiring,
};

// Re-export the registration macro so a system author only needs
// `metor_fsw_2::wiring`.
pub use crate::register_system;

// ---------------------------------------------------------------------------
// Param deserialization (wiring.md §3)
// ---------------------------------------------------------------------------
//
// One in-house `serde::Deserializer` over a KDL node's params surface (`de.rs`)
// serves BOTH paths: the static registry factory deserializes a typed
// `S::Params: DeserializeOwned`, and the dl path deserializes a
// `serde_json::Value` that the `Params`-schema validation feeds to postcard-dyn.
// The reserved wiring keys (`type=`/`artifact=` on `system` nodes, `occupant=`
// on `allow` nodes) and the leading instance-name argument never reach the
// params struct at all.

/// The reserved line-property keys of a `system` node (its wiring surface).
pub(crate) const SYSTEM_RESERVED: &[&str] = &["type", "artifact"];
/// The reserved line-property keys of a slot `allow` node.
pub(crate) const ALLOW_RESERVED: &[&str] = &["occupant"];

// ---------------------------------------------------------------------------
// The registry (wiring.md §2)
// ---------------------------------------------------------------------------

/// Everything a factory needs and produces, erased of the concrete system type.
pub struct LoadCtx<'a> {
    /// The `system` node (for params + spans).
    pub node: &'a KdlNode,
    /// The full document, for `miette` source-code context.
    pub src: &'a str,
    /// The KDL instance name (the telemetry prefix; passed to `add_*_named`).
    pub name: &'a str,
    /// The builder under construction.
    pub builder: &'a mut CoordinatorBuilder,
    /// The registry's msg table ([`Registry::register_msg`]) — the host context
    /// [`BuildSystem::configure`] resolves config name tokens against.
    pub msgs: &'a MsgTable,
}

/// A registered factory: parse params from `ctx.node`, construct the system, add it
/// to the builder under `ctx.name`, and return its handle + descriptor (for edge
/// validation).
pub type SystemFactory = fn(&mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>;

/// Marker for a cyclic system's [`AddToBuilder`] impl.
pub struct CyclicKind;
/// Marker for an async system's [`AddToBuilder`] impl.
pub struct AsyncKind;

/// Adds a constructed system to the builder with the correct `add_*_named` and
/// reports its descriptor. The `Kind` type parameter keeps the cyclic and async
/// blanket impls from overlapping (a type could in principle implement both system
/// traits), so a single `Registry::register` covers either.
pub trait AddToBuilder<Kind>: Sized {
    fn add_to(self, name: &str, builder: &mut CoordinatorBuilder) -> SystemHandle;
    fn descriptor() -> SystemDescriptor;
}

impl<S, O> AddToBuilder<CyclicKind> for S
where
    S: CyclicSystem<Output = Out<O>> + 'static,
    O: SystemOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn add_to(self, name: &str, builder: &mut CoordinatorBuilder) -> SystemHandle {
        builder.add_cyclic_named(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as CyclicSystem>::descriptor()
    }
}

impl<S> AddToBuilder<AsyncKind> for S
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    fn add_to(self, name: &str, builder: &mut CoordinatorBuilder) -> SystemHandle {
        builder.add_async_named(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as AsyncSystem>::descriptor()
    }
}

/// The factory `Registry::register::<S, _>` stores: the whole "params → `new` →
/// `add_*_named`" dance for one concrete type, erased to a plain `fn` pointer. Bounded
/// on the kdl-independent [`BuildSystem`](crate::BuildSystem) plus the
/// `Params: DeserializeOwned` the static path's KDL deserializer needs. `()` params
/// deserialize from a param-less node (and reject stray properties).
fn factory<S, K>(ctx: &mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>
where
    S: BuildSystem + AddToBuilder<K>,
    S::Params: serde::de::DeserializeOwned,
{
    let params =
        de::from_kdl_node::<S::Params>(ctx.node, ctx.src, ctx.name, SYSTEM_RESERVED, 1)?;
    let mut system = S::new(params);
    // The host-side construction phase: resolve config references (msg name
    // tokens) against the registry tables the params value cannot carry.
    system
        .configure(&BuildCtx { msgs: ctx.msgs })
        .map_err(|e| match e {
            ConfigureError::UnknownMsg { name, available } => LoadError::UnknownMsgName {
                system: ctx.name.to_string(),
                msg: name,
                available: available.join(", "),
                src: ctx.src.to_string(),
                span: (0, ctx.src.len()).into(),
            },
        })?;
    let handle = system.add_to(ctx.name, ctx.builder);
    // The registered (instance) descriptor, not the static one — a config-ported
    // system's edges resolve against what this instance actually carries.
    Ok((handle, ctx.builder.descriptor_of(handle).clone()))
}

/// One registered type: its instantiation factory plus the static descriptor —
/// available without constructing the system, so `resolve` can order registrations
/// by capability (a `ReceiveAll` system defers behind slots, `docs/alarms.md` §7 F1).
struct RegistryEntry {
    factory: SystemFactory,
    descriptor: fn() -> SystemDescriptor,
}

/// The app-built map from a KDL `type="..."` string to a system factory
/// (wiring.md §2.4 — an explicit table, not `inventory`). Each system crate can
/// expose `pub fn register(&mut Registry)` registering its own systems.
#[derive(Default)]
pub struct Registry {
    factories: HashMap<&'static str, RegistryEntry>,
    /// The registered message types ([`register_msg`](Self::register_msg)) — the
    /// [`NamedMsg::NAME`](crate::NamedMsg) → id table config name tokens (the
    /// uplink's `msgs`) resolve against in [`BuildSystem::configure`].
    msgs: MsgTable,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The framework's built-in static systems, registered under their KDL `type=`
    /// names: the alarm engine (`type="Alarms"`, `docs/alarms.md` §2), the TCP
    /// telemetry downlink (`type="TcpDownlink"`, telemetry.md §8), and the TCP
    /// command uplink (`type="TcpUplink"`, `docs/messages.md` §4.4) — plus the wkt
    /// message set in the msg table, so a mission's `msgs` list can name any of
    /// them out of the box.
    /// The CLI runner resolves against this, so every `cli::main()` mission gets the
    /// built-ins with zero Rust; an app-built registry starts here and adds its own.
    pub fn with_builtins() -> Self {
        use metor_proto_wkt::{
            AlarmAck, AlarmCleared, AlarmDef, AlarmRaised, ReloadSequences,
            SequenceChannelEvent, SequenceCommand, SequenceRegistry,
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

    /// Register message type `M` under its stable [`NamedMsg::NAME`](crate::NamedMsg)
    /// token, so config lists (the uplink's `msgs`) can name it. The wkt set comes
    /// pre-seeded by [`with_builtins`](Self::with_builtins); a mission relaying its
    /// own command types registers them here. Idempotent.
    pub fn register_msg<M: crate::NamedMsg>(&mut self) -> &mut Self {
        self.msgs.insert::<M>();
        self
    }

    /// Register concrete system `S` under `type_name`. `S` supplies its params type
    /// and `new` via the format-independent [`BuildSystem`](crate::BuildSystem); the
    /// only KDL-facing requirement is `S::Params: serde::de::DeserializeOwned` (the
    /// same derive the postcard/dl contract already needs), so a statically-linked
    /// system registers by impl'ing `BuildSystem` alone. The `add_*`/descriptor (and
    /// cyclic-vs-async branch) come from the [`AddToBuilder`] blanket impl, inferred
    /// here.
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where
        S: BuildSystem + AddToBuilder<K>,
        S::Params: serde::de::DeserializeOwned,
    {
        self.factories.insert(
            type_name,
            RegistryEntry {
                factory: factory::<S, K>,
                descriptor: <S as AddToBuilder<K>>::descriptor,
            },
        );
        self
    }

    /// Whether a `type=` names a registered system whose descriptor carries
    /// [`Capability::ReceiveAll`](crate::Capability). Unknown types answer `false` —
    /// the systems pass reports them as [`LoadError::UnknownType`] in document order.
    fn is_receive_all(&self, ty: Option<&str>) -> bool {
        ty.and_then(|ty| self.factories.get(ty)).is_some_and(|e| {
            (e.descriptor)()
                .capabilities
                .contains(&crate::Capability::ReceiveAll)
        })
    }
}

/// Register a concrete system on a [`Registry`] under a KDL `type=` name, keeping
/// the call site terse: `register_system!(registry, ImuDriver => "ImuDriver")`.
#[macro_export]
macro_rules! register_system {
    ($registry:expr, $ty:ty => $name:expr) => {
        $registry.register::<$ty, _>($name)
    };
}

// ---------------------------------------------------------------------------
// The loader (wiring.md §5)
// ---------------------------------------------------------------------------

/// One resolved instance after the systems pass (wiring.md §4.1).
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

/// Parse a KDL wiring document, instantiate every system from `registry`, connect
/// the edges, and return a built [`Coordinator`] ready to `run` (wiring.md §5.1).
///
/// This is the two-stage `KDL ──[`parse`]──▶ Wiring ──[`resolve`]──▶ Coordinator`, so
/// the [`Wiring`] data model is the single source of truth and the Rust
/// [`WiringBuilder`] is an equivalent front-end.
pub fn load(kdl: &str, registry: &Registry) -> Result<Coordinator, LoadError> {
    let wiring = parse(kdl)?;
    resolve(&wiring, registry)
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`] — the **one shared resolver**
/// both front-ends feed. For each system: a static one (`artifact = None`) is
/// instantiated through the [`Registry`] factory; a dl one is `DlSystem::open`'d from
/// its [`Artifact::path`] and added via [`CoordinatorBuilder::add_dl_cyclic`]. Then the
/// edges are connected, telemetry is added, and the graph is `build()`'d — the
/// validation/sizing/telemetry passes are shared, identical for static and dl systems.
///
/// Because a [`Wiring`] is format-independent, resolve-time [`LoadError`]s carry a
/// best-effort source snippet rather than the original document spans (a builder-origin
/// `Wiring` has no text at all); the error *variants* are the same either way.
pub fn resolve(wiring: &Wiring, registry: &Registry) -> Result<Coordinator, LoadError> {
    let config = coordinator_config(&wiring.coordinator);
    let mut builder = Coordinator::builder(config);

    // --- Systems pass: static via the Registry, dl via the loader --------
    let mut instances: HashMap<String, Instance> = HashMap::new();
    // The coordinator's own #0 bundle joins the instance namespace up front
    // (`docs/design-command-slots.md` §2.6): `"coordinator"` is reserved — command
    // edges name it (`connect "coordinator" -> … msg="SequenceCommand"`), and a user
    // `system "coordinator"` is a `DuplicateInstance` error instead of a silent
    // registry-key collision.
    let coord_handle = builder.coordinator_handle();
    instances.insert(
        "coordinator".to_string(),
        Instance {
            handle: coord_handle,
            desc: builder.descriptor_of(coord_handle).clone(),
        },
    );
    // A static `ReceiveAll` system (the alarm engine) is DEFERRED behind every other
    // cyclic registration — systems here, slots below — or `build()`'s
    // `ReceiveAllNotLast` rejects the graph (`docs/alarms.md` §7 F1). Deferral also
    // gives it the right step position: after every producer, before telemetry.
    // (dl systems can never carry capabilities — the loader rejects them.)
    let mut deferred: Vec<&SystemSpec> = Vec::new();
    for spec in &wiring.systems {
        let (handle, desc) = match &spec.artifact {
            Some(artifact_id) => resolve_dl(spec, artifact_id, wiring, &mut builder)?,
            None => {
                if registry.is_receive_all(spec.ty.as_deref()) {
                    deferred.push(spec);
                    continue;
                }
                resolve_static(spec, registry, &mut builder)?
            }
        };
        if instances
            .insert(spec.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(LoadError::DuplicateInstance {
                name: spec.name.clone(),
                src: system_src(spec),
                span: (0, system_src(spec).len()).into(),
            });
        }
    }

    // --- Slots pass: a slot resolves to an instance like a system (it `connect`s by
    //     name), so it joins `instances` before the edges pass (sequences-slots.md §5).
    for slot in &wiring.slots {
        let (handle, desc) = resolve_slot(slot, wiring, &mut builder)?;
        if instances
            .insert(slot.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(LoadError::DuplicateInstance {
                name: slot.name.clone(),
                src: slot_src(slot),
                span: (0, slot_src(slot).len()).into(),
            });
        }
    }

    // --- Deferred receive-all systems: the last cyclic registrations, still ahead
    //     of the edges pass (edge resolution only needs the finished instance map).
    for spec in deferred {
        let (handle, desc) = resolve_static(spec, registry, &mut builder)?;
        if instances
            .insert(spec.name.clone(), Instance { handle, desc })
            .is_some()
        {
            return Err(LoadError::DuplicateInstance {
                name: spec.name.clone(),
                src: system_src(spec),
                span: (0, system_src(spec).len()).into(),
            });
        }
    }

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let src = edge_src(edge);
        let span: SourceSpan = (0, src.len()).into();
        // A message edge resolves both endpoints jointly: the `msg=` token names the
        // *message type*, and a port whose display name was overridden (a
        // coordinator-minted channel like `"commands"`) is matched by packet id via
        // the endpoint the token did resolve on.
        let (producer, consumer) = match edge.kind {
            EdgeKind::Frame => (
                resolve_endpoint(&instances, &src, &edge.from, &edge.out, edge.kind, Dir::Out, span)?,
                resolve_endpoint(&instances, &src, &edge.to, &edge.in_, edge.kind, Dir::In, span)?,
            ),
            EdgeKind::Msg => resolve_msg_edge(&instances, &src, edge, span)?,
        };
        // One `connect` entry point for every edge (A7): the edge's behavior is
        // inferred from the connected ports' descriptors. `EdgeKind` only picked the
        // name-lookup space above; `delayed=#true` into a Log input surfaces
        // `WireError::DelayedLogEdge` at build (previously accepted-and-ignored).
        let result = if edge.delayed {
            builder.connect_delayed(producer, consumer)
        } else {
            builder.connect(producer, consumer)
        };
        result.map_err(|source| LoadError::Wire { source, src, span })?;
    }

    builder.build().map_err(wire_at_build)
}

/// Instantiate a **static** system through the [`Registry`] factory. The factory
/// deserializes params off the `system` node, so we reconstruct a [`KdlNode`] from the
/// spec: a config-less system synthesizes a minimal node; a params-bearing one
/// re-parses the KDL source text the parse stage stored in [`SystemSpec::params`].
fn resolve_static(
    spec: &SystemSpec,
    registry: &Registry,
    builder: &mut CoordinatorBuilder,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    // `type=` is required for a static system (only a dl system can derive it from
    // its artifact) — the KDL front-end enforces this at parse; a builder-origin
    // spec could omit it.
    let ty = spec.ty.as_deref().ok_or_else(|| {
        let src = system_src(spec);
        LoadError::MissingType {
            name: spec.name.clone(),
            span: (0, src.len()).into(),
            src,
        }
    })?;
    let factory = registry.factories.get(ty).ok_or_else(|| {
        let src = system_src(spec);
        LoadError::UnknownType {
            ty: ty.to_string(),
            span: (0, src.len()).into(),
            src,
        }
    })?;
    // Reconstruct the node the factory deserializes its params off. A config-less
    // ([`ParamSource::None`]) static system synthesizes a minimal node; a KDL-config
    // static system re-parses its carried node source text. Typed builder params
    // ([`ParamSource::Postcard`]) are **rejected**: the static path deserializes KDL,
    // not postcard, so the bytes have no decode path and would be silently dropped
    // (the system would run on defaults).
    let node_src = match &spec.params {
        ParamSource::None => format!("system \"{}\" type=\"{}\"", spec.name, ty),
        ParamSource::Postcard(_) => {
            let src = system_src(spec);
            return Err(LoadError::StaticPostcardParams {
                system: spec.name.clone(),
                ty: ty.to_string(),
                span: (0, src.len()).into(),
                src,
            });
        }
        ParamSource::Kdl(text) => text.clone(),
    };
    let doc = node_src.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: node_src.clone(),
        span: (0, node_src.len()).into(),
    })?;
    let node = doc.nodes().first().ok_or_else(|| LoadError::MissingInstanceName {
        src: node_src.clone(),
        span: (0, node_src.len()).into(),
    })?;
    (factory.factory)(&mut LoadCtx {
        node,
        src: &node_src,
        name: &spec.name,
        builder,
        msgs: &registry.msgs,
    })
}

/// The ONE artifact-open pipeline (C3b) `resolve_dl` and `resolve_slot` share: find
/// the [`Artifact`] by id, require a built `path`, `DlSystem::open` it, and resolve
/// its [`ParamSource`] to the canonical postcard `Params` bytes (`Kdl` text is
/// schema-encoded against the `.so`'s exported `Params` schema — the same bytes the
/// typed builder produces). `owner` names the `system`/`slot` instance for
/// diagnostics; `reserved`/`skip_args` describe the carried node's wiring surface.
/// Returns the opened handle, the params bytes, and the artifact's exported
/// `system_type` (for `resolve_dl`'s optional `type=` agreement check).
#[allow(clippy::too_many_arguments)]
fn open_occupant<'w>(
    wiring: &'w Wiring,
    artifact_id: &str,
    owner: &str,
    params: &ParamSource,
    reserved: &'static [&'static str],
    skip_args: usize,
    src: &str,
    span: SourceSpan,
) -> Result<(DlSystem, Vec<u8>, &'w Artifact), LoadError> {
    let artifact = wiring
        .artifacts
        .iter()
        .find(|a| a.id == artifact_id)
        .ok_or_else(|| LoadError::UnknownArtifact {
            system: owner.to_string(),
            artifact: artifact_id.to_string(),
            src: src.to_string(),
            span,
        })?;
    let path = artifact.path.as_ref().ok_or_else(|| LoadError::ArtifactNotBuilt {
        artifact: artifact_id.to_string(),
        src: src.to_string(),
        span,
    })?;
    let loaded = DlSystem::open(path).map_err(|source| LoadError::DlOpen {
        system: owner.to_string(),
        artifact: artifact_id.to_string(),
        source: Box::new(source),
        src: src.to_string(),
        span,
    })?;
    // Resolve the params to canonical postcard bytes. `Kdl` is schema-encoded against
    // the `.so`'s exported `Params` schema (the host never links `Params`), producing
    // the SAME bytes the typed `WiringBuilder::params` (`Postcard`) produces.
    let params: Vec<u8> = match params {
        ParamSource::None => Vec::new(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Kdl(node_text) => {
            encode_kdl_params(node_text, loaded.params_schema(), owner, reserved, skip_args)?
        }
    };
    Ok((loaded, params, artifact))
}

/// Load a **dl** system through the shared [`open_occupant`] pipeline and register it
/// via [`CoordinatorBuilder::add_dl_cyclic`]. The reconstructed descriptor is returned
/// for edge validation. The `.so` is opened **once** and reused for both the params
/// encode (its exported `Params` schema) and the bound slot — never opened twice.
fn resolve_dl(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    builder: &mut CoordinatorBuilder,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let (loaded, params, artifact) = open_occupant(
        wiring,
        artifact_id,
        &spec.name,
        &spec.params,
        SYSTEM_RESERVED,
        1,
        &src,
        span,
    )?;
    // `type=` is optional on a dl system (the artifact's `system_type` is
    // authoritative); when given, it must agree.
    if let Some(ty) = &spec.ty
        && ty != &artifact.system_type
    {
        return Err(LoadError::TypeMismatchesArtifact {
            system: spec.name.clone(),
            ty: ty.clone(),
            artifact_type: artifact.system_type.clone(),
            src: src.clone(),
            span,
        });
    }
    let desc = loaded.descriptor().clone();
    let handle = builder.add_dl_cyclic(&spec.name, loaded, params);
    Ok((handle, desc))
}

/// A best-effort source snippet for a system's resolve-time errors (a [`Wiring`] carries
/// no original document text).
fn system_src(spec: &SystemSpec) -> String {
    match &spec.ty {
        Some(ty) => format!("system \"{}\" type=\"{}\"", spec.name, ty),
        None => format!("system \"{}\"", spec.name),
    }
}

/// A best-effort source snippet for a slot's resolve-time errors.
fn slot_src(slot: &SlotSpec) -> String {
    format!("slot \"{}\"", slot.name)
}

/// Resolve a [`SlotSpec`] into a registered slot, mirroring [`resolve_dl`] **per allowed
/// occupant**: find each occupant's [`Artifact`] by id, `DlSystem::open` its built `.so`,
/// resolve its [`ParamSource`] to canonical postcard bytes (the same schema-guided encoder
/// `resolve_dl` uses), and assemble the `(name, DlSystem, params)` allowed set. The slot's
/// **registered descriptor** (the first occupant's descriptor with the trailing
/// [`SlotControlIn`] input removed — what `add_slot`/`build()` register) is returned for
/// the edges pass and validated against the declared `input`/`output` contract.
fn resolve_slot(
    slot: &SlotSpec,
    wiring: &Wiring,
    builder: &mut CoordinatorBuilder,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = slot_src(slot);
    let span: SourceSpan = (0, src.len()).into();

    if slot.allow.is_empty() {
        return Err(LoadError::EmptySlot {
            slot: slot.name.clone(),
            src,
            span,
        });
    }

    // A named `initial` occupant must be in the allowed set — a typo would otherwise
    // boot the slot `Empty` with no diagnostic. Pure spec validation, so it runs before
    // any artifact is opened (and regardless of `state=`: even an `empty` initial names
    // an occupant, and a name that matches nothing is always a mistake).
    if let Some(init) = &slot.initial
        && !slot.allow.iter().any(|a| a.occupant == init.occupant)
    {
        let allowed = slot
            .allow
            .iter()
            .map(|a| a.occupant.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(LoadError::UnknownInitialOccupant {
            slot: slot.name.clone(),
            occupant: init.occupant.clone(),
            allowed,
            src,
            span,
        });
    }

    // Open + param-encode each allowed occupant through the ONE shared pipeline
    // (`open_occupant`, C3b). `occupant=` is the allow node's only reserved key (no
    // leading name arg), so both line-property and child-node params reach the encoder.
    let mut allowed: Vec<AllowedOccupant> = Vec::with_capacity(slot.allow.len());
    for occ in &slot.allow {
        let (loaded, params, _) = open_occupant(
            wiring,
            &occ.occupant,
            &slot.name,
            &occ.params,
            ALLOW_RESERVED,
            0,
            &src,
            span,
        )?;
        allowed.push(AllowedOccupant {
            name: occ.occupant.clone(),
            system: loaded,
            params,
        });
    }

    // Every allowed occupant must share the contract (the slot derives one shape from
    // the allowed set; v1 holds sequence occupants only). A clean error in place of the
    // build-time panic `add_slot` would otherwise raise.
    let base = allowed[0].system.descriptor().clone();
    let ports_match = |a: &[crate::descriptor::PortDesc], b: &[crate::descriptor::PortDesc]| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| compatible(x, y) && compatible(y, x))
    };
    for occ in &allowed[1..] {
        let d = occ.system.descriptor();
        if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
            return Err(LoadError::SlotOccupantMismatch {
                slot: slot.name.clone(),
                occupant: occ.name.clone(),
                src: src.clone(),
                span,
            });
        }
    }

    // Map the startup state to the runtime `InitialOccupant`.
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

    // Register the slot, then read its **registered** descriptor back off the builder
    // — `add_slot` is the ONE place the registered contract (user ports + the slot's
    // own command fan-in) is derived, so the front-end cannot drift from it.
    let handle = builder.add_slot(slot.name.clone(), allowed, initial);
    let registered = builder.descriptor_of(handle).clone();

    // Validate the declared user-port contract: every declared `input`/`output` frame name
    // must name an **edge-connected** registered port (the explicit-contract check,
    // Resolved Q4) — the runner-held tail (the Host `slot_control`/`slot_status`/
    // `sequences`, the `SequenceStatus` self-tap) is not part of the user contract.
    // The Edge outputs include the implicit `SequenceStatus`/health/log tail, which a
    // declaration may name but need not.
    use crate::descriptor::PortConn;
    for frame in &slot.inputs {
        if !registered
            .inputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && p.name == frame)
        {
            return Err(LoadError::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "input",
                frame: frame.clone(),
                src: src.clone(),
                span,
            });
        }
    }
    for frame in &slot.outputs {
        if !registered
            .outputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && p.name == frame)
        {
            return Err(LoadError::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "output",
                frame: frame.clone(),
                src: src.clone(),
                span,
            });
        }
    }

    Ok((handle, registered))
}

/// A best-effort source snippet for an edge's resolve-time errors.
fn edge_src(edge: &EdgeSpec) -> String {
    let arrow = if edge.delayed { "~>" } else { "->" };
    format!(
        "connect \"{}\" {arrow} \"{}\" out=\"{}\" in=\"{}\"",
        edge.from, edge.to, edge.out, edge.in_
    )
}

/// Convert the serializable [`CoordinatorSpec`] into the runtime
/// [`CoordinatorConfig`] (the data-model → runtime boundary).
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

/// Resolve a `msg=` edge's two endpoints **jointly** (`docs/design-command-slots.md`
/// §2.6): the token names the *message type* (`NamedMsg::NAME`), matched against each
/// endpoint's Postcard-port display names; an endpoint whose port carries an
/// overridden display name (a coordinator-minted channel like `"commands"`) is then
/// matched by the **packet id** the token resolved to on the other endpoint. Neither
/// endpoint matching the token is an [`UnknownMsg`](LoadError::UnknownMsg).
fn resolve_msg_edge(
    instances: &HashMap<String, Instance>,
    src: &str,
    edge: &EdgeSpec,
    span: SourceSpan,
) -> Result<(PortRef, PortRef), LoadError> {
    let inst = |name: &str| {
        instances.get(name).ok_or_else(|| LoadError::UnknownInstance {
            name: name.to_string(),
            src: src.to_string(),
            span,
        })
    };
    let prod = inst(&edge.from)?;
    let cons = inst(&edge.to)?;

    let by_name = |ports: &[crate::descriptor::PortDesc], token: &str| {
        ports
            .iter()
            .find(|p| matches!(p.id, PortId::Packet(_)) && p.name == token)
            .map(|p| p.id)
    };
    let by_id = |ports: &[crate::descriptor::PortDesc], id: PortId| {
        ports.iter().find(|p| p.id == id).map(|p| p.id)
    };
    let unknown = |instance: &str, msg: &str| LoadError::UnknownMsg {
        instance: instance.to_string(),
        msg: msg.to_string(),
        src: src.to_string(),
        span,
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

/// Resolve one `(instance, frame)` endpoint to a [`PortRef`], validating the frame
/// name against the instance descriptor's port list — a typo is a load error
/// (wiring.md §4.2).
fn resolve_endpoint(
    instances: &HashMap<String, Instance>,
    src: &str,
    name: &str,
    port_name: &str,
    kind: EdgeKind,
    dir: Dir,
    span: SourceSpan,
) -> Result<PortRef, LoadError> {
    let inst = instances.get(name).ok_or_else(|| LoadError::UnknownInstance {
        name: name.to_string(),
        src: src.to_string(),
        span,
    })?;
    let ports = match dir {
        Dir::Out => &inst.desc.outputs,
        Dir::In => &inst.desc.inputs,
    };
    let port = match kind {
        EdgeKind::Frame => {
            let id = PortId::Component(ComponentId::new(port_name));
            if !ports.iter().any(|p| p.id == id) {
                return Err(LoadError::UnknownFrame {
                    instance: name.to_string(),
                    frame: port_name.to_string(),
                    src: src.to_string(),
                    span,
                });
            }
            id
        }
        // A message port is matched by its display name (the Msg type name); the `PacketId`
        // comes from the matched port, not a name hash — the wkt sequence Msgs hand-assign
        // their ids (`docs/message-wiring.md` §3.4).
        EdgeKind::Msg => {
            let found = ports
                .iter()
                .find(|p| matches!(p.id, PortId::Packet(_)) && p.name == port_name);
            match found {
                Some(p) => p.id,
                None => {
                    return Err(LoadError::UnknownMsg {
                        instance: name.to_string(),
                        msg: port_name.to_string(),
                        src: src.to_string(),
                        span,
                    });
                }
            }
        }
    };
    Ok(PortRef {
        system: inst.handle,
        port,
    })
}

/// Wrap a `build()`-time [`WireError`] as a [`LoadError::Wire`] (wiring.md §5.2 / Q7).
/// A [`Wiring`] is format-independent, so the diagnostic uses the error's own rendered
/// message as its source snippet rather than the original document spans — the variant
/// (what callers/tests match on) is unchanged.
fn wire_at_build(err: WireError) -> LoadError {
    let src = err.to_string();
    let span: SourceSpan = (0, src.len()).into();
    LoadError::Wire {
        source: err,
        src,
        span,
    }
}

#[cfg(test)]
mod tests;
