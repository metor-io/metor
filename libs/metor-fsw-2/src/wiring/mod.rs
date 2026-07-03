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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use kdl::{KdlDocument, KdlNode};
use miette::{Diagnostic, SourceSpan};
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};
use serde_json::{Map, Value};
use thiserror::Error;

use metor_fsw_ring::BoxBacking;
use metor_proto::types::ComponentId;

use crate::binder::BindPorts;
use crate::coordinator::{
    ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig, InitialOccupant, PortRef,
    SystemHandle, WireError,
};
use crate::descriptor::{PortId, SystemDescriptor, compatible};
use crate::dl::{DlError, DlSystem};
use crate::frame::Frame;
use crate::sequence::SlotControlIn;
use crate::system::{AsyncSystem, BuildSystem, CyclicSystem, Out, SystemOutput};
use crate::telemetry::{TcpRecvTransport, TcpTransport, TelemetryConfig, TelemetryMode};

// The `Wiring` data model, the Rust builder, and the cargo build driver. KDL is *one*
// deserializer onto `Wiring` (see `parse`/`resolve` below); the builder is the other;
// one shared `resolve` consumes either.
mod build_driver;
mod builder;
mod bundle;
mod de;
mod model;

pub use build_driver::{BuildError, BuildOptions, build_artifacts};
pub use bundle::{BundleError, PackageOptions, load_bundle, write_bundle};
pub use builder::{SlotSpecBuilder, SystemSpecBuilder, WiringBuilder};
pub use model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, SystemSpec, TelemetryModeSpec,
    TelemetrySpec, Wiring,
};

// Re-export the registration macro so a system author only needs
// `metor_fsw_2::wiring`.
pub use crate::register_system;

// ---------------------------------------------------------------------------
// Errors — every failure with source context (wiring.md §5.2)
// ---------------------------------------------------------------------------

/// A wiring-document load failure, each carrying the offending KDL source span.
/// Mirrors `metor-proto-kdl`'s `KdlSchematicError`: same `thiserror`+`miette`
/// derive, same `#[source_code]`/`#[label]` shape.
#[derive(Error, Debug, Diagnostic)]
pub enum LoadError {
    #[error("KDL parse error")]
    #[diagnostic(code(fsw_wiring::parse_error))]
    Parse {
        #[source]
        source: kdl::KdlError,
        #[source_code]
        src: String,
        #[label("here")]
        span: SourceSpan,
    },

    #[error("missing the single `coordinator` node")]
    #[diagnostic(code(fsw_wiring::missing_coordinator))]
    MissingCoordinator,

    #[error("more than one `coordinator` node (exactly one is required)")]
    #[diagnostic(code(fsw_wiring::multiple_coordinators))]
    MultipleCoordinators {
        #[source_code]
        src: String,
        #[label("extra coordinator here")]
        span: SourceSpan,
    },

    #[error("`system` node is missing its instance name (the first argument)")]
    #[diagnostic(code(fsw_wiring::missing_instance_name))]
    MissingInstanceName {
        #[source_code]
        src: String,
        #[label("name this system, e.g. `system \"imu\" ...`")]
        span: SourceSpan,
    },

    #[error("`system \"{name}\"` is missing its `type=` property")]
    #[diagnostic(code(fsw_wiring::missing_type))]
    MissingType {
        name: String,
        #[source_code]
        src: String,
        #[label("add a registered `type=\"...\"`")]
        span: SourceSpan,
    },

    #[error("unknown system type `{ty}` (not in the registry)")]
    #[diagnostic(code(fsw_wiring::unknown_type))]
    UnknownType {
        ty: String,
        #[source_code]
        src: String,
        #[label("register this type before loading")]
        span: SourceSpan,
    },

    #[error("duplicate instance name `{name}`")]
    #[diagnostic(code(fsw_wiring::duplicate_instance))]
    DuplicateInstance {
        name: String,
        #[source_code]
        src: String,
        #[label("instance names must be unique")]
        span: SourceSpan,
    },

    /// A required params field (per the typed `Params` struct or the dl `Params`
    /// schema) has no matching property/child on the node.
    #[error("missing required param `{property}` for system `{system}`")]
    #[diagnostic(code(fsw_wiring::missing_param))]
    MissingParam {
        property: String,
        system: String,
        #[source_code]
        src: String,
        #[label("this node is missing the param")]
        span: SourceSpan,
    },

    /// A params value that does not decode as the field's type (or a params
    /// surface shape error: a stray positional argument, a repeated property, …).
    #[error("invalid value for `{property}` on system `{system}`: expected {expected}")]
    #[diagnostic(code(fsw_wiring::invalid_param))]
    InvalidParam {
        property: String,
        system: String,
        expected: String,
        #[source_code]
        src: String,
        #[label("invalid value here")]
        span: SourceSpan,
    },

    /// A property/child with no matching params field — a typo or a stale config.
    /// Raised uniformly by the static path (the serde deserializer) and the dl
    /// path (the `Params`-schema validation).
    #[error("unknown param `{property}` for system `{system}`")]
    #[diagnostic(code(fsw_wiring::unknown_param))]
    UnknownParam {
        property: String,
        system: String,
        #[source_code]
        src: String,
        #[label("no params field is named this")]
        span: SourceSpan,
    },

    #[error("`connect` is missing required property `{property}`")]
    #[diagnostic(code(fsw_wiring::missing_edge_field))]
    MissingEdgeField {
        property: String,
        #[source_code]
        src: String,
        #[label("this edge is incomplete")]
        span: SourceSpan,
    },

    #[error("unknown instance `{name}` referenced in a `connect`")]
    #[diagnostic(code(fsw_wiring::unknown_instance))]
    UnknownInstance {
        name: String,
        #[source_code]
        src: String,
        #[label("no `system` declares this instance")]
        span: SourceSpan,
    },

    #[error("instance `{instance}` has no port for frame `{frame}`")]
    #[diagnostic(code(fsw_wiring::unknown_frame))]
    UnknownFrame {
        instance: String,
        frame: String,
        #[source_code]
        src: String,
        #[label("misspelled or wrong-direction frame")]
        span: SourceSpan,
    },

    #[error("instance `{instance}` has no message port for `{msg}`")]
    #[diagnostic(code(fsw_wiring::unknown_msg))]
    UnknownMsg {
        instance: String,
        msg: String,
        #[source_code]
        src: String,
        #[label("misspelled or wrong-direction message type")]
        span: SourceSpan,
    },

    #[error("wiring error: {source}")]
    #[diagnostic(code(fsw_wiring::wire))]
    Wire {
        #[source]
        source: WireError,
        #[source_code]
        src: String,
        #[label("introduced here")]
        span: SourceSpan,
    },

    // --- dl-open resolution -------------------------------------------------
    #[error("system `{system}` references unknown artifact `{artifact}`")]
    #[diagnostic(code(fsw_wiring::unknown_artifact))]
    UnknownArtifact {
        system: String,
        artifact: String,
        #[source_code]
        src: String,
        #[label("declare an `artifact \"{artifact}\" ...` node, or fix the `artifact=` ref")]
        span: SourceSpan,
    },

    #[error("artifact `{artifact}` has no resolved path (run the build driver first)")]
    #[diagnostic(code(fsw_wiring::artifact_not_built))]
    ArtifactNotBuilt {
        artifact: String,
        #[source_code]
        src: String,
        #[label("`build_artifacts` must set this artifact's `path` before `resolve`")]
        span: SourceSpan,
    },

    #[error("failed to load the `.so` for system `{system}` (artifact `{artifact}`): {source}")]
    #[diagnostic(code(fsw_wiring::dl_open))]
    DlOpen {
        system: String,
        artifact: String,
        // Boxed: a `DlError` carries a `libloading::Error`, which would otherwise bloat
        // every `LoadError` (the `result_large_err` lint).
        #[source]
        source: Box<DlError>,
        #[source_code]
        src: String,
        #[label("this dl system failed to load")]
        span: SourceSpan,
    },

    /// The dl system's `Params` schema could not be encoded from its KDL config (an
    /// unsupported schema shape, or the dynamic encoder rejected the value).
    #[error("dl system `{system}` params could not be schema-encoded: {reason}")]
    #[diagnostic(code(fsw_wiring::dl_param_encode))]
    DlParamEncode {
        system: String,
        reason: String,
        #[source_code]
        src: String,
        #[label("these params could not be encoded against the `Params` schema")]
        span: SourceSpan,
    },

    /// A **static** (`from_static`) system carries typed builder params
    /// ([`ParamSource::Postcard`]). The static path has no postcard decoder: the
    /// [`Registry`] factory constructs the system by deserializing its params off the
    /// KDL `system` node ([`Registry::register`]), so the postcard bytes would be
    /// silently dropped and the system would run on defaults.
    #[error(
        "static system `{system}` (type `{ty}`) has typed builder params, but a static \
         system takes params through its registered KDL-deserializing factory — express \
         them as KDL config properties (`system \"{system}\" type=\"{ty}\" key=value`), or \
         resolve the system `from_artifact` (only the dl path decodes postcard params)"
    )]
    #[diagnostic(code(fsw_wiring::static_postcard_params))]
    StaticPostcardParams {
        system: String,
        ty: String,
        #[source_code]
        src: String,
        #[label("typed `params(...)` cannot reach a static system")]
        span: SourceSpan,
    },

    #[error("`artifact` node is missing required property `{property}`")]
    #[diagnostic(code(fsw_wiring::missing_artifact_field))]
    MissingArtifactField {
        property: &'static str,
        #[source_code]
        src: String,
        #[label("this artifact node is incomplete")]
        span: SourceSpan,
    },

    /// A top-level node name the wiring schema does not know — a typo would
    /// otherwise be silently skipped.
    #[error("unknown top-level node `{node}`")]
    #[diagnostic(code(fsw_wiring::unknown_top_level_node))]
    UnknownTopLevelNode {
        node: String,
        #[source_code]
        src: String,
        #[label("expected `coordinator`/`artifact`/`system`/`slot`/`connect`/`telemetry`")]
        span: SourceSpan,
    },

    /// `lib=` on a `system` node — renamed to `artifact=` (a guidance error for the
    /// pre-rename spelling; `lib=` on `artifact` nodes still means the library stem).
    #[error(
        "`lib=` on `system` nodes was renamed to `artifact=` (it references an artifact \
         id; `lib=` on `artifact` nodes still means the library stem)"
    )]
    #[diagnostic(code(fsw_wiring::system_lib_renamed))]
    SystemLibRenamed {
        #[source_code]
        src: String,
        #[label("write `artifact=` here")]
        span: SourceSpan,
    },

    /// A dl system's explicit `type=` contradicts the `.so` type its artifact
    /// exports (`type=` is optional on dl systems; when given it must match).
    #[error(
        "system `{system}` declares type `{ty}` but its artifact exports `{artifact_type}`"
    )]
    #[diagnostic(code(fsw_wiring::type_mismatches_artifact))]
    TypeMismatchesArtifact {
        system: String,
        ty: String,
        artifact_type: String,
        #[source_code]
        src: String,
        #[label("drop the `type=` or make it `{artifact_type}`")]
        span: SourceSpan,
    },

    // --- slots (sequences-slots.md §5) --------------------------------------
    #[error("`slot` node has an unknown child `{child}` (expected `input`/`output`/`allow`/`initial`)")]
    #[diagnostic(code(fsw_wiring::unknown_slot_child))]
    UnknownSlotChild {
        child: String,
        #[source_code]
        src: String,
        #[label("remove or rename this child")]
        span: SourceSpan,
    },

    #[error("`slot \"{slot}\"` has no `allow` occupant (a slot needs at least one)")]
    #[diagnostic(code(fsw_wiring::empty_slot))]
    EmptySlot {
        slot: String,
        #[source_code]
        src: String,
        #[label("add an `allow occupant=\"...\"` child")]
        span: SourceSpan,
    },

    #[error("`slot \"{slot}\"` has an invalid initial `state=\"{state}\"` (expected `empty`/`loaded`/`running`)")]
    #[diagnostic(code(fsw_wiring::bad_slot_state))]
    BadSlotState {
        slot: String,
        state: String,
        #[source_code]
        src: String,
        #[label("use `empty`, `loaded`, or `running`")]
        span: SourceSpan,
    },

    /// A `slot`'s declared `input`/`output frame="…"` contract names a frame that the
    /// slot's resolved (registered) descriptor has no matching user port for — a typo or
    /// a stale contract (the explicit-contract validation, Resolved Q4).
    #[error("`slot \"{slot}\"` declares {dir} frame `{frame}` but its occupants have no such user port")]
    #[diagnostic(code(fsw_wiring::slot_contract_mismatch))]
    SlotContractMismatch {
        slot: String,
        dir: &'static str,
        frame: String,
        #[source_code]
        src: String,
        #[label("no occupant {dir} port is named this")]
        span: SourceSpan,
    },

    /// A `slot`'s `initial occupant="…"` names an occupant that is not in the slot's
    /// `allow` set — a typo the slot would otherwise boot `Empty` on with no diagnostic
    /// (the runtime `Load` path validates the same set).
    #[error(
        "`slot \"{slot}\"` initial occupant `{occupant}` is not in the allowed set \
         (allowed: {allowed})"
    )]
    #[diagnostic(code(fsw_wiring::unknown_initial_occupant))]
    UnknownInitialOccupant {
        slot: String,
        occupant: String,
        /// The comma-joined allowed occupant names, for the message/label.
        allowed: String,
        #[source_code]
        src: String,
        #[label("no `allow occupant=` names this (allowed: {allowed})")]
        span: SourceSpan,
    },

    /// Two allowed occupants of one `slot` have incompatible descriptors — the slot derives
    /// its single contract from the allowed set's shared shape (v1 holds sequence occupants
    /// only, so they must agree). A clean error in place of `add_slot`'s build-time panic.
    #[error("`slot \"{slot}\"` occupant `{occupant}` is incompatible with the slot contract")]
    #[diagnostic(code(fsw_wiring::slot_occupant_mismatch))]
    SlotOccupantMismatch {
        slot: String,
        occupant: String,
        #[source_code]
        src: String,
        #[label("this occupant's ports differ from the first allowed occupant's")]
        span: SourceSpan,
    },
}

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
const SYSTEM_RESERVED: &[&str] = &["type", "artifact"];
/// The reserved line-property keys of a slot `allow` node.
const ALLOW_RESERVED: &[&str] = &["occupant"];

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
    O: SystemOutput + BindPorts<BoxBacking> + 'static,
    S::Input: BindPorts<BoxBacking> + 'static,
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
    S::Input: BindPorts<BoxBacking> + 'static,
    S::Output: BindPorts<BoxBacking> + 'static,
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
    let system = S::new(params);
    let handle = system.add_to(ctx.name, ctx.builder);
    Ok((handle, <S as AddToBuilder<K>>::descriptor()))
}

/// The app-built map from a KDL `type="..."` string to a system factory
/// (wiring.md §2.4 — an explicit table, not `inventory`). Each system crate can
/// expose `pub fn register(&mut Registry)` registering its own systems.
#[derive(Default)]
pub struct Registry {
    factories: HashMap<&'static str, SystemFactory>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
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
        self.factories.insert(type_name, factory::<S, K>);
        self
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

/// One parsed `connect` edge (either syntax desugared to the same shape).
struct Edge {
    from: String,
    to: String,
    out: String,
    in_: String,
    /// `delayed=#true` ⇒ a one-cycle-delayed feedback back-edge (`connect_delayed`).
    delayed: bool,
    /// Frame (`frame=`) vs message (`msg=`) edge.
    kind: EdgeKind,
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

/// Deserialize a KDL wiring document into the [`Wiring`] data model — one of the two
/// front-ends onto `Wiring` (the other is [`WiringBuilder`]).
///
/// This is **parse only**: it touches no [`Registry`], `dlopen`s nothing, and does not
/// validate the graph — those are [`resolve`]'s job. It carries the
/// `coordinator`/`system`/`connect`/`telemetry` surface plus the `artifact` node +
/// per-`system` `artifact=` reference for dl systems.
///
/// **Params** (for either kind) are carried as the KDL `system` node's source text in
/// [`ParamSource::Kdl`] when the node has config properties/children; a config-less
/// system carries [`ParamSource::None`]. The decoder is chosen at [`resolve`] time by
/// [`SystemSpec::artifact`]: a **static** system re-deserializes the text into its
/// typed `Params` (`serde`); a **dl** system schema-encodes it against the `.so`'s
/// exported `Params` schema, so KDL ≡ the Rust builder on the wire.
pub fn parse(kdl: &str) -> Result<Wiring, LoadError> {
    let doc = kdl.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: kdl.to_string(),
        span: (0, kdl.len()).into(),
    })?;

    // One pass over the top-level nodes; each list keeps document order, and an
    // unknown node name is a spanned error rather than a silent skip.
    let mut coordinator: Option<CoordinatorSpec> = None;
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut systems: Vec<SystemSpec> = Vec::new();
    let mut slots: Vec<SlotSpec> = Vec::new();
    let mut edges: Vec<EdgeSpec> = Vec::new();
    let mut telemetry: Option<TelemetrySpec> = None;
    let mut seen: HashSet<String> = HashSet::new();

    for node in doc.nodes() {
        match node.name().value() {
            "coordinator" => {
                if coordinator.is_some() {
                    return Err(LoadError::MultipleCoordinators {
                        src: kdl.to_string(),
                        span: node.span(),
                    });
                }
                coordinator = Some(parse_coordinator(node, kdl)?);
            }
            "artifact" => artifacts.push(parse_artifact(node, kdl)?),
            "system" => systems.push(parse_system(node, kdl, &mut seen)?),
            // sequences-slots.md §5: a slot is an instance like a system.
            "slot" => slots.push(parse_slot(node, kdl, &mut seen)?),
            "connect" => {
                let e = parse_edge(node, kdl)?;
                edges.push(EdgeSpec {
                    from: e.from,
                    out: e.out,
                    to: e.to,
                    in_: e.in_,
                    delayed: e.delayed,
                    kind: e.kind,
                });
            }
            "telemetry" => {
                let (addr, mode) = parse_telemetry(node, kdl)?;
                telemetry = Some(TelemetrySpec { addr, mode });
            }
            other => {
                return Err(LoadError::UnknownTopLevelNode {
                    node: other.to_string(),
                    src: kdl.to_string(),
                    span: node.span(),
                });
            }
        }
    }

    let coordinator = coordinator.ok_or(LoadError::MissingCoordinator)?;

    Ok(Wiring {
        coordinator,
        artifacts,
        systems,
        slots,
        edges,
        telemetry,
        // No KDL surface for the uplink yet (`docs/messages.md` §7); set via `--uplink`.
        uplink: None,
    })
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
    for spec in &wiring.systems {
        let (handle, desc) = match &spec.artifact {
            Some(artifact_id) => resolve_dl(spec, artifact_id, wiring, &mut builder)?,
            None => resolve_static(spec, registry, &mut builder)?,
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

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let src = edge_src(edge);
        let span: SourceSpan = (0, src.len()).into();
        let producer =
            resolve_endpoint(&instances, &src, &edge.from, &edge.out, edge.kind, Dir::Out, span)?;
        let consumer =
            resolve_endpoint(&instances, &src, &edge.to, &edge.in_, edge.kind, Dir::In, span)?;
        let result = match edge.kind {
            EdgeKind::Msg => builder.connect_msg(producer, consumer),
            EdgeKind::Frame if edge.delayed => builder.connect_delayed(producer, consumer),
            EdgeKind::Frame => builder.connect(producer, consumer),
        };
        result.map_err(|source| LoadError::Wire { source, src, span })?;
    }

    // --- Uplink: an async system on its own connection (`docs/messages.md` §4.4/§4.5) ---
    if let Some(addr) = wiring.uplink {
        builder.add_uplink(TcpRecvTransport::new(addr));
    }

    // --- Telemetry: registered last (observes every system's fresh output) ---
    if let Some(t) = &wiring.telemetry {
        builder.add_telemetry(TelemetryConfig {
            transport: TcpTransport::new(t.addr),
            mode: mode_from_spec(&t.mode),
        });
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
    factory(&mut LoadCtx {
        node,
        src: &node_src,
        name: &spec.name,
        builder,
    })
}

/// Load a **dl** system: find its [`Artifact`], `DlSystem::open` the resolved `.so`,
/// resolve its [`ParamSource`] into the canonical postcard `Params` bytes, and register it
/// via [`CoordinatorBuilder::add_dl_cyclic`]. The reconstructed descriptor is returned
/// for edge validation.
///
/// The `.so` is opened **once** and reused for both the params encode (its exported
/// `Params` schema, [`DlSystem::params_schema`]) and the bound slot — never opened twice.
fn resolve_dl(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    builder: &mut CoordinatorBuilder,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let artifact = wiring
        .artifacts
        .iter()
        .find(|a| a.id == artifact_id)
        .ok_or_else(|| LoadError::UnknownArtifact {
            system: spec.name.clone(),
            artifact: artifact_id.to_string(),
            src: src.clone(),
            span,
        })?;
    let path = artifact.path.as_ref().ok_or_else(|| LoadError::ArtifactNotBuilt {
        artifact: artifact_id.to_string(),
        src: src.clone(),
        span,
    })?;
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
    let loaded = DlSystem::open(path).map_err(|source| LoadError::DlOpen {
        system: spec.name.clone(),
        artifact: artifact_id.to_string(),
        source: Box::new(source),
        src: src.clone(),
        span,
    })?;
    // Resolve the params to canonical postcard bytes. `Kdl` is schema-encoded against the
    // `.so`'s exported `Params` schema (the host never links `Params`), producing the
    // SAME bytes the typed `WiringBuilder::params` (`Postcard`) produces.
    let params: Vec<u8> = match &spec.params {
        ParamSource::None => Vec::new(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Kdl(node_text) => {
            encode_kdl_params(node_text, loaded.params_schema(), &spec.name, SYSTEM_RESERVED, 1)?
        }
    };
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

    // Open + param-encode each allowed occupant (the per-occupant `resolve_dl` logic).
    let mut allowed: Vec<(String, DlSystem, Vec<u8>)> = Vec::with_capacity(slot.allow.len());
    for occ in &slot.allow {
        let artifact = wiring
            .artifacts
            .iter()
            .find(|a| a.id == occ.occupant)
            .ok_or_else(|| LoadError::UnknownArtifact {
                system: slot.name.clone(),
                artifact: occ.occupant.clone(),
                src: src.clone(),
                span,
            })?;
        let path = artifact.path.as_ref().ok_or_else(|| LoadError::ArtifactNotBuilt {
            artifact: occ.occupant.clone(),
            src: src.clone(),
            span,
        })?;
        let loaded = DlSystem::open(path).map_err(|source| LoadError::DlOpen {
            system: slot.name.clone(),
            artifact: occ.occupant.clone(),
            source: Box::new(source),
            src: src.clone(),
            span,
        })?;
        let params: Vec<u8> = match &occ.params {
            ParamSource::None => Vec::new(),
            ParamSource::Postcard(bytes) => bytes.clone(),
            // `occupant=` is the allow node's only reserved key (no leading name
            // arg), so both line-property and child-node params reach the encoder.
            ParamSource::Kdl(node_text) => {
                encode_kdl_params(node_text, loaded.params_schema(), &slot.name, ALLOW_RESERVED, 0)?
            }
        };
        allowed.push((occ.occupant.clone(), loaded, params));
    }

    // The registered descriptor = the first occupant's descriptor with the trailing
    // `SlotControlIn` input dropped (what `add_slot`/`build()` register for edge wiring).
    let base = allowed[0].1.descriptor().clone();
    let mut inputs = base.inputs.clone();
    if inputs.last().map(|p| p.id) == Some(PortId::Component(SlotControlIn::FRAME_ID)) {
        inputs.pop();
    }
    let registered = SystemDescriptor {
        name: base.name,
        kind: base.kind,
        inputs,
        outputs: base.outputs.clone(),
    };

    // Every other allowed occupant must share the contract (the slot derives one shape from
    // the allowed set; v1 holds sequence occupants only). A clean error in place of the
    // build-time panic `add_slot` would otherwise raise.
    let ports_match = |a: &[crate::descriptor::PortDesc], b: &[crate::descriptor::PortDesc]| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| compatible(x, y) && compatible(y, x))
    };
    for (occ_name, sys, _) in &allowed[1..] {
        let d = sys.descriptor();
        if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
            return Err(LoadError::SlotOccupantMismatch {
                slot: slot.name.clone(),
                occupant: occ_name.clone(),
                src: src.clone(),
                span,
            });
        }
    }

    // Validate the declared user-port contract: every declared `input`/`output` frame name
    // must name a registered user port (the explicit-contract check, Resolved Q4). The
    // registered inputs are user inputs only (the `SlotControlIn` was dropped); the outputs
    // include the implicit `SequenceStatus`/health/log tail, which a declaration may name
    // but need not.
    for frame in &slot.inputs {
        if !registered.inputs.iter().any(|p| p.name == frame) {
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
        if !registered.outputs.iter().any(|p| p.name == frame) {
            return Err(LoadError::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "output",
                frame: frame.clone(),
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

    let handle = builder.add_slot(slot.name.clone(), allowed, initial);
    Ok((handle, registered))
}

/// Encode a dl system's KDL node config into the canonical postcard `Params` bytes,
/// **guided by the `.so`'s exported `Params` schema** (the one-postcard-encoding
/// decision). The host never links the system's `Params` type: the shared KDL
/// deserializer ([`de`]) reads the node's params surface into a dynamic
/// [`serde_json::Value`] (plus a key → span table), the value is conformed to the
/// schema ([`conform_to_schema`]: unknown/missing/type-mismatched fields become
/// named, spanned errors; absent `Option` fields become explicit nulls), and
/// [`postcard_dyn::to_stdvec_dyn`] emits the **same bytes** the typed Rust builder's
/// [`WiringBuilder::params`](crate::WiringBuilder) postcard-encodes (the byte
/// equality is the headline equivalence gate, asserted in `tests_abi`).
///
/// `node_text` is the carried node source ([`ParamSource::Kdl`]); `schema` is
/// [`DlSystem::params_schema`]; `system` names the instance for diagnostics;
/// `reserved`/`skip_args` name the node's wiring surface (`type=`/`artifact=` + the
/// instance name on `system` nodes, `occupant=` on `allow` nodes). Errors are
/// span-aware [`LoadError`]s: [`UnknownParam`](LoadError::UnknownParam),
/// [`MissingParam`](LoadError::MissingParam), [`InvalidParam`](LoadError::InvalidParam),
/// or [`DlParamEncode`](LoadError::DlParamEncode) for an un-encodable schema shape.
pub fn encode_kdl_params(
    node_text: &str,
    schema: &OwnedNamedType,
    system: &str,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<Vec<u8>, LoadError> {
    let span: SourceSpan = (0, node_text.len()).into();
    let encode_err = |reason: String| LoadError::DlParamEncode {
        system: system.to_string(),
        reason,
        src: node_text.to_string(),
        span,
    };

    let doc = node_text.parse::<KdlDocument>().map_err(|e| encode_err(e.to_string()))?;
    let node = doc
        .nodes()
        .first()
        .ok_or_else(|| encode_err("the carried params text has no node".into()))?;

    let (value, spans) = de::params_value(node, node_text, system, reserved, skip_args)?;
    let value = conform_to_schema(&schema.ty, value, &spans, system, node_text, node.span())
        .map_err(|e| match e {
            Conform::Load(err) => *err,
            Conform::Shape(reason) => encode_err(reason),
        })?;

    postcard_dyn::to_stdvec_dyn(schema, &value)
        .map_err(|e| encode_err(format!("dynamic postcard encode failed: {e:?}")))
}

/// A [`conform_to_schema`] failure: a named, spanned params diagnostic, or a schema
/// shape the walk cannot model (surfaced as [`DlParamEncode`](LoadError::DlParamEncode)).
enum Conform {
    Load(Box<LoadError>),
    Shape(String),
}

impl From<LoadError> for Conform {
    fn from(e: LoadError) -> Self {
        Conform::Load(Box::new(e))
    }
}

/// Conform a deserialized params [`Value`] to a `Params` schema **struct** (or unit):
/// every JSON key must be a schema field ([`UnknownParam`](LoadError::UnknownParam)),
/// every non-`Option` schema field must be present ([`MissingParam`](LoadError::MissingParam),
/// absent `Option`s get an explicit `Null` — postcard-dyn requires it), and each field
/// value is checked/recursed by [`conform_value`]. The output object holds schema
/// field order (order is irrelevant to postcard-dyn, which iterates the schema).
fn conform_to_schema(
    ty: &OwnedDataModelType,
    value: Value,
    spans: &HashMap<String, SourceSpan>,
    system: &str,
    src: &str,
    node_span: SourceSpan,
) -> Result<Value, Conform> {
    let fields = match ty {
        OwnedDataModelType::Struct(fields) => fields.as_slice(),
        OwnedDataModelType::Unit | OwnedDataModelType::UnitStruct => &[],
        other => {
            return Err(Conform::Shape(format!(
                "the `Params` schema is `{other:?}`, which a KDL params surface cannot \
                 express (only a struct, or a unit)"
            )));
        }
    };
    let mut obj = match value {
        Value::Object(map) => map,
        other => {
            return Err(Conform::Shape(format!(
                "expected a params object for a struct schema, got `{other}`"
            )));
        }
    };

    // Every key must name a schema field (the typo guard, with the entry's span).
    for key in obj.keys() {
        if !fields.iter().any(|f| f.name == *key) {
            return Err(LoadError::UnknownParam {
                property: key.clone(),
                system: system.to_string(),
                src: src.to_string(),
                span: spans.get(key).copied().unwrap_or(node_span),
            }
            .into());
        }
    }

    let mut out = Map::new();
    for field in fields {
        let span = spans.get(&field.name).copied().unwrap_or(node_span);
        match obj.remove(&field.name) {
            Some(v) => {
                let v = conform_value(&field.ty.ty, v, &field.name, span, system, src)?;
                out.insert(field.name.clone(), v);
            }
            // An `Option` field with no property is `None` (postcard-dyn needs the
            // explicit null); any other absent field is a hard miss (no defaults).
            None if matches!(field.ty.ty, OwnedDataModelType::Option(_)) => {
                out.insert(field.name.clone(), Value::Null);
            }
            None => {
                return Err(LoadError::MissingParam {
                    property: field.name.clone(),
                    system: system.to_string(),
                    src: src.to_string(),
                    span: node_span,
                }
                .into());
            }
        }
    }
    Ok(Value::Object(out))
}

/// Check/recurse one field value against its schema type. Leaves are type- and
/// range-checked with [`leaf_expected`] wording; nested structs/sequences/options
/// recurse (nested errors carry the **top-level** property's span — the side table
/// is top-level only, v1). Any schema shape the walk does not model passes through
/// to postcard-dyn, whose failure surfaces as
/// [`DlParamEncode`](LoadError::DlParamEncode) — never silently wrong bytes.
fn conform_value(
    ty: &OwnedDataModelType,
    value: Value,
    property: &str,
    span: SourceSpan,
    system: &str,
    src: &str,
) -> Result<Value, Conform> {
    use OwnedDataModelType as T;
    let mismatch = || -> Conform {
        LoadError::InvalidParam {
            property: property.to_string(),
            system: system.to_string(),
            expected: leaf_expected(ty),
            src: src.to_string(),
            span,
        }
        .into()
    };
    // The signed/unsigned width an integer leaf must fit.
    let int_ok = |min: i128, max: i128| -> bool {
        match &value {
            Value::Number(n) => n
                .as_i64()
                .map(|i| (i as i128) >= min && (i as i128) <= max)
                .or_else(|| n.as_u64().map(|u| (u as i128) <= max))
                .unwrap_or(false),
            _ => false,
        }
    };
    match ty {
        T::Bool => value.is_boolean().then_some(value).ok_or_else(mismatch),
        T::I8 => int_ok(i8::MIN as i128, i8::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::I16 => int_ok(i16::MIN as i128, i16::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::I32 => int_ok(i32::MIN as i128, i32::MAX as i128).then_some(value).ok_or_else(mismatch),
        // postcard-dyn reads i64/isize/i128 via `as_i64` — i64 range on the wire.
        T::I64 | T::Isize | T::I128 => {
            int_ok(i64::MIN as i128, i64::MAX as i128).then_some(value).ok_or_else(mismatch)
        }
        T::U8 => int_ok(0, u8::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::U16 => int_ok(0, u16::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::U32 => int_ok(0, u32::MAX as i128).then_some(value).ok_or_else(mismatch),
        // postcard-dyn reads u64/usize/u128 via `as_u64` — u64 range on the wire.
        T::U64 | T::Usize | T::U128 => {
            int_ok(0, u64::MAX as i128).then_some(value).ok_or_else(mismatch)
        }
        // postcard-dyn's `as_f64` accepts integer literals (`rate=200` ⇒ 200.0).
        T::F32 | T::F64 => value.is_number().then_some(value).ok_or_else(mismatch),
        T::String => value.is_string().then_some(value).ok_or_else(mismatch),
        T::Char => match &value {
            Value::String(s) if s.chars().count() == 1 => Ok(value),
            _ => Err(mismatch()),
        },
        // A present `Option<T>` is the `Some(T)` inner value; `#null` is `None`.
        T::Option(inner) => match value {
            Value::Null => Ok(Value::Null),
            v => conform_value(&inner.ty, v, property, span, system, src),
        },
        // Nested structs recurse with the same rules; the top-level property's span
        // is the fallback for everything inside.
        T::Struct(_) | T::Unit | T::UnitStruct => {
            conform_to_schema(ty, value, &HashMap::new(), system, src, span)
        }
        T::Seq(inner) => match value {
            Value::Array(items) => {
                let items = items
                    .into_iter()
                    .map(|v| conform_value(&inner.ty, v, property, span, system, src))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Array(items))
            }
            _ => Err(mismatch()),
        },
        T::NewtypeStruct(inner) => conform_value(&inner.ty, value, property, span, system, src),
        // Tuples/maps/enums/etc.: pass through — postcard-dyn models them, and its
        // failure surfaces as `DlParamEncode` (never silently wrong bytes).
        _ => Ok(value),
    }
}

/// A human name of a schema leaf type, for [`InvalidParam`](LoadError::InvalidParam)
/// on the dl (schema-guided) path.
fn leaf_expected(ty: &OwnedDataModelType) -> String {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => "a boolean (#true/#false)".into(),
        T::I8 | T::I16 | T::I32 | T::I64 | T::Isize | T::I128 => "a signed integer".into(),
        T::U8 | T::U16 | T::U32 | T::U64 | T::Usize | T::U128 => "a non-negative integer".into(),
        T::F32 | T::F64 => "a number".into(),
        T::String | T::Char => "a string".into(),
        T::Option(inner) => format!("{} (or omit the property)", leaf_expected(&inner.ty)),
        T::Seq(inner) => format!("a sequence of {}", leaf_expected(&inner.ty)),
        T::Struct(_) => "a nested node of params".into(),
        other => format!("an unsupported type ({other:?})"),
    }
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

/// Convert the serializable [`TelemetryModeSpec`] into the runtime [`TelemetryMode`].
fn mode_from_spec(mode: &TelemetryModeSpec) -> TelemetryMode {
    match mode {
        TelemetryModeSpec::All => TelemetryMode::All,
        TelemetryModeSpec::Subset { instances, frames } => TelemetryMode::Subset {
            instances: instances.clone(),
            frames: frames.clone(),
        },
    }
}

/// Decorate a library **stem** into the platform's produced `cdylib` file name:
/// `lib{stem}.dylib` (macOS), `lib{stem}.so` (Linux/other), `{stem}.dll` (Windows).
///
/// The wiring's `artifact` `lib=` and the Rust builder both carry the bare stem
/// (`adcs_plant`), so a single mission document is portable across a dev mac and a
/// Linux flight target; the framework computes the concrete file name here at parse
/// time. [`Artifact::cdylib`](crate::Artifact) then holds that produced name — the
/// build driver, the bundle, and `resolve` all consume it unchanged.
pub fn cdylib_file_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Parse one `artifact "id" crate="..." lib="foo" type="Foo"` node into an
/// [`Artifact`]. `lib=` is the library **stem** (`foo`); the produced file name is
/// computed per-platform via [`cdylib_file_name`] so a static mission document stays
/// portable across host OSes.
fn parse_artifact(node: &KdlNode, src: &str) -> Result<Artifact, LoadError> {
    let missing = |property: &'static str| LoadError::MissingArtifactField {
        property,
        src: src.to_string(),
        span: node.span(),
    };
    let id = first_arg_string(node).ok_or_else(|| missing("id"))?;
    let crate_name = prop_string(node, "crate").ok_or_else(|| missing("crate"))?;
    let stem = prop_string(node, "lib").ok_or_else(|| missing("lib"))?;
    let system_type = prop_string(node, "type").ok_or_else(|| missing("type"))?;
    Ok(Artifact {
        id: id.to_string(),
        crate_name: crate_name.to_string(),
        cdylib: cdylib_file_name(stem),
        system_type: system_type.to_string(),
        path: None,
    })
}

/// Parse one `system` node into a [`SystemSpec`]. An `artifact=` ⇒ a dl system
/// referencing that [`Artifact`] (its `type=` is then optional — the artifact's
/// `system_type` is authoritative, and a given `type=` is validated at resolve);
/// otherwise a static system, whose `type=` is required. See [`parse`] for the
/// static-params / dl-params-deferred handling.
fn parse_system(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SystemSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    // The pre-rename `lib=` spelling gets a guidance error pointing at the entry.
    if let Some(entry) = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some("lib"))
    {
        return Err(LoadError::SystemLibRenamed {
            src: src.to_string(),
            span: entry.span(),
        });
    }
    let artifact = prop_string(node, "artifact").map(str::to_string);
    let ty = prop_string(node, "type").map(str::to_string);
    if ty.is_none() && artifact.is_none() {
        return Err(LoadError::MissingType {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }
    if !seen.insert(name.to_string()) {
        return Err(LoadError::DuplicateInstance {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }
    // Any property other than the reserved `type=`/`artifact=`, or any child node,
    // is params config.
    let has_config = node
        .entries()
        .iter()
        .any(|e| matches!(e.name().map(|n| n.value()), Some(k) if !SYSTEM_RESERVED.contains(&k)))
        || node.children().is_some_and(|c| !c.nodes().is_empty());
    // Both static and dl systems carry the node's source text when configured; the
    // decoder is chosen at `resolve` by `artifact` (static ⇒ the typed serde
    // deserialize, dl ⇒ schema-guided postcard encode).
    let params = if has_config {
        ParamSource::Kdl(node.to_string())
    } else {
        ParamSource::None
    };
    Ok(SystemSpec {
        name: name.to_string(),
        ty,
        artifact,
        params,
    })
}

/// Parse one `slot "name" { input/output/allow/initial }` node into a [`SlotSpec`]
/// (sequences-slots.md §5). The slot name is the node's first string arg (like a
/// `system`/`artifact` name) and shares the instance namespace (`seen`), so a name
/// colliding with a system is a [`DuplicateInstance`](LoadError::DuplicateInstance).
///
/// Children: `input frame="…"`/`output frame="…"` declare the user-port contract;
/// `allow occupant="…" …` is one allowed occupant — everything on it other than the
/// reserved `occupant=` is its default params, as line properties
/// (`allow occupant="x" gain=0.8`) and/or children (`allow occupant="x" { gain 0.8 }`),
/// carried as [`ParamSource::Kdl`] (else [`ParamSource::None`] — mirroring
/// [`parse_system`]); `initial occupant="…" state="…"` is the startup occupant.
fn parse_slot(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SlotSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    if !seen.insert(name.to_string()) {
        return Err(LoadError::DuplicateInstance {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }

    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();
    let mut allow: Vec<AllowedOccupantSpec> = Vec::new();
    let mut initial: Option<InitialOccupantSpec> = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "input" => {
                    let frame = prop_string(child, "frame").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    inputs.push(frame.to_string());
                }
                "output" => {
                    let frame = prop_string(child, "frame").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    outputs.push(frame.to_string());
                }
                "allow" => {
                    let occupant = prop_string(child, "occupant").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    // Any non-reserved line property OR any child ⇒ params: carry the
                    // `allow` node's source text for the resolve-time schema-guided
                    // encoder (mirrors `parse_system`'s `Kdl` decision); else paramless.
                    let has_config = child.entries().iter().any(|e| {
                        matches!(e.name().map(|n| n.value()), Some(k) if !ALLOW_RESERVED.contains(&k))
                    }) || child.children().is_some_and(|c| !c.nodes().is_empty());
                    let params = if has_config {
                        ParamSource::Kdl(child.to_string())
                    } else {
                        ParamSource::None
                    };
                    allow.push(AllowedOccupantSpec {
                        occupant: occupant.to_string(),
                        params,
                    });
                }
                "initial" => {
                    let occupant = prop_string(child, "occupant").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    // `state` is optional; an `initial` with no `state` defaults to
                    // `loaded` (built but not auto-started — the conservative default).
                    let state = match prop_string(child, "state") {
                        None | Some("loaded") => SlotInitState::Loaded,
                        Some("running") => SlotInitState::Running,
                        Some("empty") => SlotInitState::Empty,
                        Some(other) => {
                            return Err(LoadError::BadSlotState {
                                slot: name.to_string(),
                                state: other.to_string(),
                                src: src.to_string(),
                                span: child.span(),
                            });
                        }
                    };
                    initial = Some(InitialOccupantSpec {
                        occupant: occupant.to_string(),
                        state,
                    });
                }
                other => {
                    return Err(LoadError::UnknownSlotChild {
                        child: other.to_string(),
                        src: src.to_string(),
                        span: child.span(),
                    });
                }
            }
        }
    }

    Ok(SlotSpec {
        name: name.to_string(),
        inputs,
        outputs,
        allow,
        initial,
    })
}

/// Parse a `telemetry` node into a `(addr, mode)` pair (telemetry.md §8):
///
/// ```kdl
/// telemetry {
///     transport "tcp" addr="127.0.0.1:2240"
///     mode "all"                       // or: mode "subset"
///     // subset only:
///     // tap instance="imu_left"
///     // tap frame="control"
/// }
/// ```
fn parse_telemetry(
    node: &KdlNode,
    src: &str,
) -> Result<(std::net::SocketAddr, TelemetryModeSpec), LoadError> {
    let invalid = |property: &str, expected: &str| LoadError::InvalidParam {
        property: property.to_string(),
        system: "telemetry".to_string(),
        expected: expected.to_string(),
        src: src.to_string(),
        span: node.span(),
    };
    let missing = |property: &str| LoadError::MissingParam {
        property: property.to_string(),
        system: "telemetry".to_string(),
        src: src.to_string(),
        span: node.span(),
    };

    let children = node.children().ok_or_else(|| missing("transport"))?;

    // transport "tcp" addr="..."  — v1 supports only "tcp".
    let transport_node = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "transport")
        .ok_or_else(|| missing("transport"))?;
    match first_arg_string(transport_node) {
        Some("tcp") => {}
        _ => return Err(invalid("transport", "\"tcp\" (the only v1 transport)")),
    }
    let addr_str = prop_string(transport_node, "addr").ok_or_else(|| missing("addr"))?;
    let addr = addr_str
        .parse::<std::net::SocketAddr>()
        .map_err(|_| invalid("addr", "a socket address like 127.0.0.1:2240"))?;

    // mode "all" | "subset"  — subset taps the `tap instance=.. / frame=..` children.
    let mode_str = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "mode")
        .and_then(first_arg_string)
        .unwrap_or("all");
    let mode = match mode_str {
        "all" => TelemetryModeSpec::All,
        "subset" => {
            let mut instances = Vec::new();
            let mut frames = Vec::new();
            for tap in children.nodes().iter().filter(|n| n.name().value() == "tap") {
                if let Some(i) = prop_string(tap, "instance") {
                    instances.push(i.to_string());
                }
                if let Some(f) = prop_string(tap, "frame") {
                    frames.push(f.to_string());
                }
            }
            TelemetryModeSpec::Subset { instances, frames }
        }
        _ => return Err(invalid("mode", "\"all\" or \"subset\"")),
    };
    Ok((addr, mode))
}

/// Read one `coordinator` node into a [`CoordinatorSpec`] (wiring.md §1.1).
/// `sim_dt` (seconds) present ⇒ a free-running [`Simulated`](ClockSpec::Simulated)
/// clock; absent ⇒ [`Wall`](ClockSpec::Wall). Deserialized through the shared KDL
/// params deserializer, so an unknown/misspelled coordinator property is a spanned
/// [`UnknownParam`](LoadError::UnknownParam) and a bad value is entry-precise.
fn parse_coordinator(node: &KdlNode, src: &str) -> Result<CoordinatorSpec, LoadError> {
    #[derive(serde::Deserialize)]
    struct CoordinatorProps {
        cycle_rate: f64,
        default_depth: Option<usize>,
        sim_dt: Option<f64>,
    }
    let props: CoordinatorProps = de::from_kdl_node(node, src, "coordinator", &[], 0)?;
    let clock = match props.sim_dt {
        Some(dt_secs) => ClockSpec::Simulated { dt_secs },
        None => ClockSpec::Wall,
    };
    Ok(CoordinatorSpec {
        cycle_rate: props.cycle_rate,
        default_depth: props.default_depth,
        clock,
    })
}

/// Parse a `connect` edge in either the shorthand (`"a" -> "b" frame="f"`) or the
/// explicit (`from=.. out=.. to=.. in=..`) form (wiring.md §1.3).
fn parse_edge(node: &KdlNode, src: &str) -> Result<Edge, LoadError> {
    let span = node.span();
    let missing = |property: &str| LoadError::MissingEdgeField {
        property: property.to_string(),
        src: src.to_string(),
        span,
    };
    // `delayed=#true` marks a one-cycle-delayed feedback back-edge (default false).
    let delayed = node.get("delayed").and_then(|v| v.as_bool()).unwrap_or(false);

    if let Some(from) = prop_string(node, "from") {
        // Explicit long form — frame edges only (a message edge uses the `msg=` shorthand).
        let to = prop_string(node, "to").ok_or_else(|| missing("to"))?;
        let out = prop_string(node, "out").ok_or_else(|| missing("out"))?;
        let in_ = prop_string(node, "in").ok_or_else(|| missing("in"))?;
        return Ok(Edge {
            from: from.to_string(),
            to: to.to_string(),
            out: out.to_string(),
            in_: in_.to_string(),
            delayed,
            kind: EdgeKind::Frame,
        });
    }

    // Shorthand: the nameless arguments are `"from"`, (optional `->`), `"to"`; the port is
    // named by exactly one of `frame=` (a component frame) or `msg=` (a message type).
    let args: Vec<&str> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .filter_map(|e| e.value().as_string())
        .collect();
    let (from, to) = match args.as_slice() {
        [from, "->", to] => (*from, *to),
        [from, to] => (*from, *to),
        _ => return Err(missing("from/to")),
    };
    let (port_name, kind) = match (prop_string(node, "frame"), prop_string(node, "msg")) {
        (Some(frame), None) => (frame, EdgeKind::Frame),
        (None, Some(msg)) => (msg, EdgeKind::Msg),
        (Some(_), Some(_)) => return Err(missing("frame/msg (name exactly one)")),
        (None, None) => return Err(missing("frame")),
    };
    Ok(Edge {
        from: from.to_string(),
        to: to.to_string(),
        out: port_name.to_string(),
        in_: port_name.to_string(),
        delayed,
        kind,
    })
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

/// The first nameless string argument of a node (e.g. a `system` instance name).
fn first_arg_string(node: &KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
}

/// A node's string-valued property `key`.
fn prop_string<'a>(node: &'a KdlNode, key: &str) -> Option<&'a str> {
    node.get(key).and_then(|v| v.as_string())
}

#[cfg(test)]
mod tests;
