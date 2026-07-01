//! The KDL wiring front-end (wiring.md).
//!
//! A pure front-end onto the [`CoordinatorBuilder`]: it parses a KDL
//! document, instantiates each `system` from an app-built [`Registry`], resolves
//! each `connect` edge to a [`PortRef`](crate::PortRef), and `build()`s a
//! [`Coordinator`]. No coordinator logic lives here — every error surfaced is a
//! span-carrying [`LoadError`] (a `miette` [`Diagnostic`](miette::Diagnostic)
//! mirroring `metor-proto-kdl`'s `KdlSchematicError`).
//!
//! ## Schema (properties on the node line)
//!
//! Params and coordinator config are **properties on the node line**, not a
//! `{ key=value }` children block — the latter is not valid KDL v2 (see the report
//! deviation). Everything else follows wiring.md §1:
//!
//! ```kdl
//! coordinator cycle_rate=200.0 default_depth=8           // wall clock, paced
//! coordinator cycle_rate=200.0 sim_dt=0.00833            // simulated clock, free-run
//! system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
//! system "nav" type="MekfFilter" gain=0.8
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

use kdl::{KdlDocument, KdlNode, KdlValue};
use miette::{Diagnostic, SourceSpan};
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};
use serde_json::{Map, Number, Value};
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
mod model;

pub use build_driver::{BuildError, BuildOptions, build_artifacts};
pub use bundle::{BundleError, PackageOptions, load_bundle, write_bundle};
pub use builder::{SlotSpecBuilder, SystemSpecBuilder, WiringBuilder};
pub use model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, SystemSpec, TelemetryModeSpec,
    TelemetrySpec, Wiring,
};

// Re-export the derive and the registration macro so a system author only needs
// `metor_fsw_2::wiring`.
pub use crate::register_system;
pub use metor_fsw_macros::FromKdlNode;

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

    #[error("missing required property `{property}` for system `{system}`")]
    #[diagnostic(code(fsw_wiring::missing_param))]
    MissingParam {
        property: String,
        system: &'static str,
        #[source_code]
        src: String,
        #[label("this node is missing the property")]
        span: SourceSpan,
    },

    #[error("invalid value for `{property}` on system `{system}`: expected {expected}")]
    #[diagnostic(code(fsw_wiring::invalid_param))]
    InvalidParam {
        property: String,
        system: &'static str,
        expected: String,
        #[source_code]
        src: String,
        #[label("invalid value here")]
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
        #[label("declare an `artifact \"{artifact}\" ...` node, or fix the `lib=` ref")]
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

    /// A dl system's KDL property has no matching field in the `.so`'s exported `Params`
    /// schema — a typo or a stale config (the schema-guided encoder).
    #[error("dl system `{system}` has an unknown param `{property}` (not in its `Params` schema)")]
    #[diagnostic(code(fsw_wiring::dl_unknown_param))]
    DlUnknownParam {
        system: String,
        property: String,
        #[source_code]
        src: String,
        #[label("no `Params` field is named this")]
        span: SourceSpan,
    },

    /// A dl system's `Params` schema field has no corresponding KDL property, and the
    /// field is not an `Option` (so it has no default).
    #[error("dl system `{system}` is missing required param `{property}` (a `Params` schema field)")]
    #[diagnostic(code(fsw_wiring::dl_missing_param))]
    DlMissingParam {
        system: String,
        property: String,
        #[source_code]
        src: String,
        #[label("add this property to the `system` node")]
        span: SourceSpan,
    },

    /// A dl system's KDL property value does not match its `Params` schema field type
    /// (e.g. a string where the schema wants an integer).
    #[error(
        "dl system `{system}` param `{property}` has the wrong type: expected {expected} \
         (per the `Params` schema)"
    )]
    #[diagnostic(code(fsw_wiring::dl_param_type_mismatch))]
    DlParamTypeMismatch {
        system: String,
        property: String,
        expected: String,
        #[source_code]
        src: String,
        #[label("this value does not match the schema field type")]
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

    #[error("`artifact` node is missing required property `{property}`")]
    #[diagnostic(code(fsw_wiring::missing_artifact_field))]
    MissingArtifactField {
        property: &'static str,
        #[source_code]
        src: String,
        #[label("this artifact node is incomplete")]
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

/// Deserialize a system's params from its `system` node's KDL properties. Derive
/// with `#[derive(FromKdlNode)]`; a no-params system uses `type Params = ()`.
pub trait FromKdlNode: Sized {
    fn from_kdl_node(node: &KdlNode, src: &str) -> Result<Self, LoadError>;
}

/// A system with no configurable params.
impl FromKdlNode for () {
    fn from_kdl_node(_node: &KdlNode, _src: &str) -> Result<Self, LoadError> {
        Ok(())
    }
}

/// A scalar/string a KDL property value can decode into — the leaf the
/// [`FromKdlNode`] derive walks for each field.
pub trait FromKdlScalar: Sized {
    /// Decode from a KDL value, or `None` if the value is the wrong shape.
    fn from_value(value: &KdlValue) -> Option<Self>;
    /// A human name of the expected shape, for `LoadError::InvalidParam`.
    const EXPECTED: &'static str;
}

macro_rules! int_scalar {
    ($($t:ty),*) => {$(
        impl FromKdlScalar for $t {
            fn from_value(value: &KdlValue) -> Option<Self> {
                value.as_integer().map(|i| i as $t)
            }
            const EXPECTED: &'static str = "an integer";
        }
    )*};
}
int_scalar!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

macro_rules! float_scalar {
    ($($t:ty),*) => {$(
        impl FromKdlScalar for $t {
            fn from_value(value: &KdlValue) -> Option<Self> {
                // Accept an integer literal where a float is wanted (`rate=200`).
                value
                    .as_float()
                    .or_else(|| value.as_integer().map(|i| i as f64))
                    .map(|f| f as $t)
            }
            const EXPECTED: &'static str = "a number";
        }
    )*};
}
float_scalar!(f32, f64);

impl FromKdlScalar for bool {
    fn from_value(value: &KdlValue) -> Option<Self> {
        value.as_bool()
    }
    const EXPECTED: &'static str = "a boolean (#true/#false)";
}

impl FromKdlScalar for String {
    fn from_value(value: &KdlValue) -> Option<Self> {
        value.as_string().map(|s| s.to_string())
    }
    const EXPECTED: &'static str = "a string";
}

/// Read a **required** property `key` off `node` (used by the derive).
pub fn kdl_required<T: FromKdlScalar>(
    node: &KdlNode,
    key: &str,
    system: &'static str,
    src: &str,
) -> Result<T, LoadError> {
    let value = node.get(key).ok_or_else(|| LoadError::MissingParam {
        property: key.to_string(),
        system,
        src: src.to_string(),
        span: node.span(),
    })?;
    T::from_value(value).ok_or_else(|| LoadError::InvalidParam {
        property: key.to_string(),
        system,
        expected: T::EXPECTED.to_string(),
        src: src.to_string(),
        span: node.span(),
    })
}

/// Read an **optional** property `key` off `node` (used by the derive for
/// `Option<T>` and `#[kdl(default = ..)]` fields). Absent ⇒ `Ok(None)`.
pub fn kdl_optional<T: FromKdlScalar>(
    node: &KdlNode,
    key: &str,
    system: &'static str,
    src: &str,
) -> Result<Option<T>, LoadError> {
    match node.get(key) {
        None => Ok(None),
        Some(value) => T::from_value(value).map(Some).ok_or_else(|| LoadError::InvalidParam {
            property: key.to_string(),
            system,
            expected: T::EXPECTED.to_string(),
            src: src.to_string(),
            span: node.span(),
        }),
    }
}

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

/// The KDL static-registry construction contract (wiring.md §2.3): a
/// [`BuildSystem`](crate::BuildSystem) whose `Params` is [`FromKdlNode`]-parseable.
///
/// Construction is split in two: the format-independent [`BuildSystem`]
/// (`type Params` + `fn new`, in `system.rs`, with **no** kdl coupling — what
/// `export_system!`/the dlopen ABI need) and this thin extension that *only* adds the
/// `Params: FromKdlNode` bound the static factory needs. It is a **blanket marker**:
/// every `BuildSystem` with a `FromKdlNode` `Params` is automatically a
/// `RegisteredSystem`, so a statically-linked system registers by impl'ing
/// [`BuildSystem`] (not this) — while a dl-only system needs no `FromKdlNode` impl
/// at all.
pub trait RegisteredSystem: BuildSystem
where
    <Self as BuildSystem>::Params: FromKdlNode,
{
}

impl<S> RegisteredSystem for S
where
    S: BuildSystem,
    S::Params: FromKdlNode,
{
}

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
/// on the kdl-independent [`BuildSystem`](crate::BuildSystem) plus the `FromKdlNode`
/// `Params` the static path needs (i.e. exactly [`RegisteredSystem`]'s premises).
fn factory<S, K>(ctx: &mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>
where
    S: BuildSystem + AddToBuilder<K>,
    S::Params: FromKdlNode,
{
    let params = <S::Params as FromKdlNode>::from_kdl_node(ctx.node, ctx.src)?;
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
    /// and `new` via [`BuildSystem`](crate::BuildSystem) with a [`FromKdlNode`] `Params`
    /// (i.e. it is a [`RegisteredSystem`]); the `add_*`/descriptor (and cyclic-vs-async
    /// branch) come from the [`AddToBuilder`] blanket impl, inferred here.
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where
        S: BuildSystem + AddToBuilder<K>,
        S::Params: FromKdlNode,
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
/// per-`system` `lib=` reference for dl systems.
///
/// **Params** (for either kind) are carried as the KDL `system` node's source text in
/// [`ParamSource::Kdl`] when the node has config properties; a config-less system carries
/// [`ParamSource::None`]. The decoder is chosen at [`resolve`] time by
/// [`SystemSpec::artifact`]: a **static** system re-parses the text via `FromKdlNode`;
/// a **dl** system schema-encodes it against the `.so`'s exported `Params` schema, so
/// KDL ≡ the Rust builder on the wire.
pub fn parse(kdl: &str) -> Result<Wiring, LoadError> {
    let doc = kdl.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: kdl.to_string(),
        span: (0, kdl.len()).into(),
    })?;

    let coordinator = parse_coordinator(&doc, kdl)?;

    // --- Artifacts pass --------------------------------------------------
    let mut artifacts: Vec<Artifact> = Vec::new();
    for node in doc.nodes() {
        if node.name().value() != "artifact" {
            continue;
        }
        artifacts.push(parse_artifact(node, kdl)?);
    }

    // --- Systems pass (wiring.md §2; dl systems carry a `lib=` ref) ------
    let mut systems: Vec<SystemSpec> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for node in doc.nodes() {
        if node.name().value() != "system" {
            continue;
        }
        systems.push(parse_system(node, kdl, &mut seen)?);
    }

    // --- Slots pass (sequences-slots.md §5; a slot is an instance like a system) ---
    let mut slots: Vec<SlotSpec> = Vec::new();
    for node in doc.nodes() {
        if node.name().value() != "slot" {
            continue;
        }
        let slot = parse_slot(node, kdl, &mut seen)?;
        slots.push(slot);
    }

    // --- Edges pass (wiring.md §4) ---------------------------------------
    let mut edges: Vec<EdgeSpec> = Vec::new();
    for node in doc.nodes() {
        if node.name().value() != "connect" {
            continue;
        }
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

    // --- Telemetry pass (telemetry.md §8) --------------------------------
    let mut telemetry: Option<TelemetrySpec> = None;
    for node in doc.nodes() {
        if node.name().value() != "telemetry" {
            continue;
        }
        let (addr, mode) = parse_telemetry(node, kdl)?;
        telemetry = Some(TelemetrySpec { addr, mode });
    }

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

/// Instantiate a **static** system through the [`Registry`] factory. The factory parses
/// params via `FromKdlNode`, so we reconstruct a [`KdlNode`] from the spec: a config-less
/// system synthesizes a minimal node; a params-bearing one re-parses the KDL source text
/// the parse stage stored in [`SystemSpec::params`].
fn resolve_static(
    spec: &SystemSpec,
    registry: &Registry,
    builder: &mut CoordinatorBuilder,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let factory = registry.factories.get(spec.ty.as_str()).ok_or_else(|| {
        let src = system_src(spec);
        LoadError::UnknownType {
            ty: spec.ty.clone(),
            span: (0, src.len()).into(),
            src,
        }
    })?;
    // Reconstruct the node the `FromKdlNode` factory reads its params off. A config-less
    // ([`ParamSource::None`]) or builder-origin ([`ParamSource::Postcard`]) static system
    // synthesizes a minimal node (the static path is `FromKdlNode`-shaped, not postcard);
    // a KDL-config static system re-parses its carried node source text.
    let minimal = || format!("system \"{}\" type=\"{}\"", spec.name, spec.ty);
    let node_src = match &spec.params {
        ParamSource::None | ParamSource::Postcard(_) => minimal(),
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
            encode_kdl_params(node_text, loaded.params_schema(), &spec.name)?
        }
    };
    let desc = loaded.descriptor().clone();
    let handle = builder.add_dl_cyclic(&spec.name, loaded, params);
    Ok((handle, desc))
}

/// A best-effort source snippet for a system's resolve-time errors (a [`Wiring`] carries
/// no original document text).
fn system_src(spec: &SystemSpec) -> String {
    format!("system \"{}\" type=\"{}\"", spec.name, spec.ty)
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
            ParamSource::Kdl(node_text) => {
                encode_kdl_params(node_text, loaded.params_schema(), &slot.name)?
            }
        };
        allowed.push((occ.occupant.clone(), loaded, params));
    }

    // The registered descriptor = the first occupant's descriptor with the trailing
    // `SlotControlIn` input dropped (what `add_slot`/`build()` register for edge wiring).
    let base = allowed[0].1.descriptor().clone();
    let mut inputs = base.inputs.clone();
    if inputs.last().map(|p| p.id) == Some(PortId::Frame(SlotControlIn::FRAME_ID)) {
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

/// Encode a dl system's KDL `system`-node config into the canonical postcard `Params`
/// bytes, **guided by the `.so`'s exported `Params` schema** (the one-postcard-encoding
/// decision). The host never links the system's `Params` type: it
/// walks the schema's named fields, pulls the matching KDL property for each, coerces it
/// to the field's type, builds a dynamic [`serde_json::Value`], and hands it to
/// [`postcard_dyn::to_stdvec_dyn`] — which produces the **same bytes** the typed Rust
/// builder's [`WiringBuilder::params`](crate::WiringBuilder) postcard-encodes (the byte
/// equality is the headline equivalence gate, asserted in `tests_abi`).
///
/// `node_text` is the `system` node's source ([`ParamSource::Kdl`]); `schema` is
/// [`DlSystem::params_schema`]; `system` names the instance for diagnostics. Errors are
/// span-aware [`LoadError`]s: a property with no schema field
/// ([`DlUnknownParam`](LoadError::DlUnknownParam)), a non-`Option` schema field with no
/// property ([`DlMissingParam`](LoadError::DlMissingParam)), a property whose value type
/// does not match the field ([`DlParamTypeMismatch`](LoadError::DlParamTypeMismatch)), or
/// an un-encodable schema shape ([`DlParamEncode`](LoadError::DlParamEncode)).
pub fn encode_kdl_params(
    node_text: &str,
    schema: &OwnedNamedType,
    system: &str,
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
        .ok_or_else(|| encode_err("the carried params text has no `system` node".into()))?;

    // Only a top-level struct (the usual `#[derive(Schema)] struct Params`) — or a unit
    // (`()`/unit struct, no fields) — maps from flat KDL properties.
    let fields = match &schema.ty {
        OwnedDataModelType::Struct(fields) => fields.as_slice(),
        OwnedDataModelType::Unit | OwnedDataModelType::UnitStruct => &[],
        other => {
            return Err(encode_err(format!(
                "the `Params` schema is `{other:?}`, which KDL properties cannot express \
                 (only a struct of scalar fields, or a unit)"
            )));
        }
    };

    // Each KDL property (everything but the reserved `type=`/`lib=`) must be a schema field.
    for entry in node.entries() {
        if let Some(key) = entry.name().map(|n| n.value())
            && key != "type"
            && key != "lib"
            && !fields.iter().any(|f| f.name == key)
        {
            return Err(LoadError::DlUnknownParam {
                system: system.to_string(),
                property: key.to_string(),
                src: node_text.to_string(),
                span,
            });
        }
    }

    // Walk the schema fields (so JSON object order is canonical, matching the typed
    // builder's struct-field order — the basis of the byte equality).
    let mut obj = Map::new();
    for field in fields {
        match node.get(field.name.as_str()) {
            Some(value) => {
                let json = kdl_value_to_json(&field.ty.ty, value).ok_or_else(|| {
                    LoadError::DlParamTypeMismatch {
                        system: system.to_string(),
                        property: field.name.clone(),
                        expected: leaf_expected(&field.ty.ty),
                        src: node_text.to_string(),
                        span,
                    }
                })?;
                obj.insert(field.name.clone(), json);
            }
            // An `Option` field with no property is `None` (encoded as the null byte); any
            // other absent field is a hard miss (the schema has no defaults).
            None if matches!(field.ty.ty, OwnedDataModelType::Option(_)) => {
                obj.insert(field.name.clone(), Value::Null);
            }
            None => {
                return Err(LoadError::DlMissingParam {
                    system: system.to_string(),
                    property: field.name.clone(),
                    src: node_text.to_string(),
                    span,
                });
            }
        }
    }

    postcard_dyn::to_stdvec_dyn(schema, &Value::Object(obj))
        .map_err(|e| encode_err(format!("dynamic postcard encode failed: {e:?}")))
}

/// Coerce one KDL property value to the [`serde_json::Value`] shape
/// [`postcard_dyn::to_stdvec_dyn`] expects for a schema leaf `ty`, reusing the
/// [`FromKdlScalar`] coercion rules (integers, floats accepting int literals, bools,
/// strings). Returns `None` on a type mismatch (→ [`DlParamTypeMismatch`](LoadError::DlParamTypeMismatch)).
///
/// Only scalar leaves are expressible as flat KDL properties; a non-scalar schema field
/// (a nested struct, seq, map, enum, …) yields `None` here and is surfaced as a mismatch.
fn kdl_value_to_json(ty: &OwnedDataModelType, value: &KdlValue) -> Option<Value> {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => value.as_bool().map(Value::Bool),
        // Signed integers (incl. the `as_i64`-encoded i128 of postcard-dyn).
        T::I8 | T::I16 | T::I32 | T::I64 | T::Isize | T::I128 => value
            .as_integer()
            .and_then(|i| i64::try_from(i).ok())
            .map(|i| Value::Number(Number::from(i))),
        // Unsigned integers (incl. the `as_u64`-encoded u128 of postcard-dyn).
        T::U8 | T::U16 | T::U32 | T::U64 | T::Usize | T::U128 => value
            .as_integer()
            .and_then(|i| u64::try_from(i).ok())
            .map(|u| Value::Number(Number::from(u))),
        // Floats accept an int literal (`rate=200` ⇒ 200.0), matching `FromKdlScalar`.
        T::F32 | T::F64 => value
            .as_float()
            .or_else(|| value.as_integer().map(|i| i as f64))
            .and_then(Number::from_f64)
            .map(Value::Number),
        T::String | T::Char => value.as_string().map(|s| Value::String(s.to_string())),
        // A present `Option<T>` property is the `Some(T)` inner value (a `None` is the
        // *absent* property, handled by the caller).
        T::Option(inner) => kdl_value_to_json(&inner.ty, value),
        _ => None,
    }
}

/// A human name of a schema leaf type, for [`DlParamTypeMismatch`](LoadError::DlParamTypeMismatch).
fn leaf_expected(ty: &OwnedDataModelType) -> String {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => "a boolean (#true/#false)".into(),
        T::I8 | T::I16 | T::I32 | T::I64 | T::Isize | T::I128 => "a signed integer".into(),
        T::U8 | T::U16 | T::U32 | T::U64 | T::Usize | T::U128 => "a non-negative integer".into(),
        T::F32 | T::F64 => "a number".into(),
        T::String | T::Char => "a string".into(),
        T::Option(inner) => format!("{} (or omit the property)", leaf_expected(&inner.ty)),
        other => format!("an unsupported scalar type ({other:?})"),
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

/// Parse one `system` node into a [`SystemSpec`]. A `lib=` ⇒ a dl system referencing
/// that [`Artifact`]; otherwise a static system. See [`parse`] for the static-params /
/// dl-params-deferred handling.
fn parse_system(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SystemSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    let ty = prop_string(node, "type").ok_or_else(|| LoadError::MissingType {
        name: name.to_string(),
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
    let artifact = prop_string(node, "lib").map(str::to_string);
    // Any property other than the reserved `type=`/`lib=` is a config (params) property.
    let has_config = node.entries().iter().any(|e| {
        matches!(e.name().map(|n| n.value()), Some(k) if k != "type" && k != "lib")
    });
    // Both static and dl systems carry the node's source text when configured; the
    // decoder is chosen at `resolve` by `artifact` (static ⇒ `FromKdlNode`, dl ⇒
    // schema-guided postcard encode).
    let params = if has_config {
        ParamSource::Kdl(node.to_string())
    } else {
        ParamSource::None
    };
    Ok(SystemSpec {
        name: name.to_string(),
        ty: ty.to_string(),
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
/// `allow occupant="…" { <optional params> }` is one allowed occupant (its params are
/// carried as [`ParamSource::Kdl`] when the node has child params, else
/// [`ParamSource::None`] — mirroring [`parse_system`]); `initial occupant="…" state="…"`
/// is the startup occupant.
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
                            system: "slot",
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
                            system: "slot",
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
                            system: "slot",
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    // A params child block ⇒ carry the `allow` node's source text for the
                    // resolve-time schema-guided encoder (mirrors `parse_system`'s `Kdl`
                    // decision); no children ⇒ paramless.
                    let params = if child.children().is_some_and(|c| !c.nodes().is_empty()) {
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
                            system: "slot",
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
        system: "telemetry",
        expected: expected.to_string(),
        src: src.to_string(),
        span: node.span(),
    };
    let missing = |property: &str| LoadError::MissingParam {
        property: property.to_string(),
        system: "telemetry",
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

/// Read the single `coordinator` node into a [`CoordinatorSpec`] (wiring.md §1.1).
/// `sim_dt` (seconds) present ⇒ a free-running [`Simulated`](ClockSpec::Simulated)
/// clock; absent ⇒ [`Wall`](ClockSpec::Wall).
fn parse_coordinator(doc: &KdlDocument, src: &str) -> Result<CoordinatorSpec, LoadError> {
    let mut found: Option<&KdlNode> = None;
    for node in doc.nodes() {
        if node.name().value() != "coordinator" {
            continue;
        }
        if found.is_some() {
            return Err(LoadError::MultipleCoordinators {
                src: src.to_string(),
                span: node.span(),
            });
        }
        found = Some(node);
    }
    let node = found.ok_or(LoadError::MissingCoordinator)?;
    let cycle_rate = kdl_required::<f64>(node, "cycle_rate", "coordinator", src)?;
    let default_depth = kdl_optional::<usize>(node, "default_depth", "coordinator", src)?;
    let clock = match kdl_optional::<f64>(node, "sim_dt", "coordinator", src)? {
        Some(dt_secs) => ClockSpec::Simulated { dt_secs },
        None => ClockSpec::Wall,
    };
    Ok(CoordinatorSpec {
        cycle_rate,
        default_depth,
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
            let id = PortId::Frame(ComponentId::new(port_name));
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
                .find(|p| matches!(p.id, PortId::Msg(_)) && p.name == port_name);
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
