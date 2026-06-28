//! Work-Package 6 — the KDL wiring front-end (wiring.md).
//!
//! A pure front-end onto the landed WP5 [`CoordinatorBuilder`]: it parses a KDL
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
use thiserror::Error;

use metor_fsw_ring::BoxBacking;
use metor_proto::types::ComponentId;

use crate::binder::BindPorts;
use crate::coordinator::{
    ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig, PortRef, SystemHandle, WireError,
};
use crate::descriptor::SystemDescriptor;
use crate::dl::{DlError, DlSystem};
use crate::system::{AsyncSystem, BuildSystem, CyclicSystem, Out, SystemOutput};
use crate::telemetry::{TcpTransport, TelemetryConfig, TelemetryMode};

// Wave 3a (dl-open.md §6): the `Wiring` data model, the Rust builder, and the cargo
// build driver. KDL is now *one* deserializer onto `Wiring` (see `parse`/`resolve`
// below); the builder is the other; one shared `resolve` consumes either.
mod build_driver;
mod builder;
mod model;

pub use build_driver::{BuildError, BuildOptions, build_artifacts};
pub use builder::{SystemSpecBuilder, WiringBuilder};
pub use model::{
    Artifact, ClockSpec, CoordinatorSpec, EdgeSpec, SystemSpec, TelemetryModeSpec, TelemetrySpec,
    Wiring,
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

    // --- Wave 3a: dl-open resolution (dl-open.md §6) ---------------------------
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

    /// A dl system (`lib=`) carried KDL params, which require the schema-guided encoder
    /// landing in **Wave 3b**. Until then, give a dl system its params via the Rust
    /// [`WiringBuilder`](crate::WiringBuilder) (dl-open.md §6.3).
    #[error(
        "dl system `{system}` has KDL params, which are not supported yet (Wave 3b): pass params to a \
         dl system via the Rust `WiringBuilder` for now"
    )]
    #[diagnostic(code(fsw_wiring::dl_kdl_params_unsupported))]
    DlKdlParamsUnsupported {
        system: String,
        #[source_code]
        src: String,
        #[label("remove these properties, or build this system via the Rust builder")]
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
/// Wave 3a (dl-open.md §3.0) split construction in two: the format-independent
/// [`BuildSystem`] (`type Params` + `fn new`, in `system.rs`, with **no** kdl coupling
/// — what `export_system!`/the dlopen ABI need) and this thin extension that *only*
/// adds the `Params: FromKdlNode` bound the static factory needs. It is a **blanket
/// marker**: every `BuildSystem` with a `FromKdlNode` `Params` is automatically a
/// `RegisteredSystem`, so a statically-linked system registers exactly as before
/// (it impls [`BuildSystem`], not this) — while a dl-only system needs no `FromKdlNode`
/// impl at all (dl-open.md §6.3).
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
/// `Params` the static path needs (i.e. exactly [`RegisteredSystem`]'s premises —
/// dl-open.md §3.0).
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
}

/// Parse a KDL wiring document, instantiate every system from `registry`, connect
/// the edges, and return a built [`Coordinator`] ready to `run` (wiring.md §5.1).
///
/// Wave 3a (dl-open.md §6.3) re-expresses this as the two-stage
/// `KDL ──[`parse`]──▶ Wiring ──[`resolve`]──▶ Coordinator`, so the data model is the
/// single source of truth and the Rust [`WiringBuilder`] is an equivalent front-end.
/// The public entry point and its behavior are unchanged.
pub fn load(kdl: &str, registry: &Registry) -> Result<Coordinator, LoadError> {
    let wiring = parse(kdl)?;
    resolve(&wiring, registry)
}

/// Deserialize a KDL wiring document into the [`Wiring`] data model (dl-open.md §6.3)
/// — one of the two front-ends onto `Wiring` (the other is [`WiringBuilder`]).
///
/// This is **parse only**: it touches no [`Registry`], `dlopen`s nothing, and does not
/// validate the graph — those are [`resolve`]'s job. It carries the existing
/// `coordinator`/`system`/`connect`/`telemetry` surface and adds the Wave 3a
/// `artifact` node + per-`system` `lib=` reference.
///
/// **Static params** (a `system` with no `lib=`) are carried as the KDL node's source
/// text in [`SystemSpec::params`] when the node has config properties, so the static
/// [`Registry`] factory re-parses them via `FromKdlNode` at [`resolve`] time
/// (behavior-identical to WP6); a config-less static system carries empty params.
///
/// **Dl params in KDL are deferred to Wave 3b**: a `system` with a `lib=` may carry no
/// params (the schema-guided postcard encoder is not landed); one that does is a clear
/// [`LoadError::DlKdlParamsUnsupported`]. Give a dl system params via the Rust
/// [`WiringBuilder`] until then (dl-open.md §6.3).
pub fn parse(kdl: &str) -> Result<Wiring, LoadError> {
    let doc = kdl.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: kdl.to_string(),
        span: (0, kdl.len()).into(),
    })?;

    let coordinator = parse_coordinator(&doc, kdl)?;

    // --- Artifacts pass (dl-open.md §6.3) --------------------------------
    let mut artifacts: Vec<Artifact> = Vec::new();
    for node in doc.nodes() {
        if node.name().value() != "artifact" {
            continue;
        }
        artifacts.push(parse_artifact(node, kdl)?);
    }

    // --- Systems pass (wiring.md §2; dl `lib=` per dl-open.md §6.3) -------
    let mut systems: Vec<SystemSpec> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for node in doc.nodes() {
        if node.name().value() != "system" {
            continue;
        }
        systems.push(parse_system(node, kdl, &mut seen)?);
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
        edges,
        telemetry,
    })
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`] — the **one shared resolver**
/// both front-ends feed (dl-open.md §6.3, §4.3). For each system: a static one
/// (`artifact = None`) is instantiated through the [`Registry`] factory (the WP6 path,
/// unchanged); a dl one is `DlSystem::open`'d from its [`Artifact::path`] and added via
/// [`CoordinatorBuilder::add_dl_cyclic`]. Then the edges are connected, telemetry is
/// added, and the graph is `build()`'d — the validation/sizing/telemetry passes are all
/// reuse, identical for static and dl systems.
///
/// Because a [`Wiring`] is format-independent, resolve-time [`LoadError`]s carry a
/// best-effort source snippet rather than the original document spans (a builder-origin
/// `Wiring` has no text at all); the error *variants* are unchanged, so callers (and the
/// WP6 tests) see the same outcomes.
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

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let src = edge_src(edge);
        let span: SourceSpan = (0, src.len()).into();
        let producer = resolve_endpoint(&instances, &src, &edge.from, &edge.out, Dir::Out, span)?;
        let consumer = resolve_endpoint(&instances, &src, &edge.to, &edge.in_, Dir::In, span)?;
        let result = if edge.delayed {
            builder.connect_delayed(producer, consumer)
        } else {
            builder.connect(producer, consumer)
        };
        result.map_err(|source| LoadError::Wire { source, src, span })?;
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
/// the parse stage stored in [`SystemSpec::params`] (dl-open.md §6.3).
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
    // Reconstruct the node the `FromKdlNode` factory reads its params off.
    let node_src = if spec.params.is_empty() {
        format!("system \"{}\" type=\"{}\"", spec.name, spec.ty)
    } else {
        // Stored by `parse` as the original `system` node's source text (UTF-8).
        String::from_utf8(spec.params.clone()).unwrap_or_else(|_| {
            format!("system \"{}\" type=\"{}\"", spec.name, spec.ty)
        })
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

/// Load a **dl** system: find its [`Artifact`], `DlSystem::open` the resolved `.so`, and
/// register it via [`CoordinatorBuilder::add_dl_cyclic`] with the spec's postcard params
/// (dl-open.md §4.3/§6.3). The reconstructed descriptor is returned for edge validation.
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
    let desc = loaded.descriptor().clone();
    let handle = builder.add_dl_cyclic(&spec.name, loaded, spec.params.clone());
    Ok((handle, desc))
}

/// A best-effort source snippet for a system's resolve-time errors (a [`Wiring`] carries
/// no original document text).
fn system_src(spec: &SystemSpec) -> String {
    format!("system \"{}\" type=\"{}\"", spec.name, spec.ty)
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
/// [`CoordinatorConfig`] (the data-model → runtime boundary, dl-open.md §6.1).
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

/// Parse one `artifact "id" crate="..." lib="libfoo.so" type="Foo"` node into an
/// [`Artifact`] (dl-open.md §6.3). `lib=` is the produced cdylib file name.
fn parse_artifact(node: &KdlNode, src: &str) -> Result<Artifact, LoadError> {
    let missing = |property: &'static str| LoadError::MissingArtifactField {
        property,
        src: src.to_string(),
        span: node.span(),
    };
    let id = first_arg_string(node).ok_or_else(|| missing("id"))?;
    let crate_name = prop_string(node, "crate").ok_or_else(|| missing("crate"))?;
    let cdylib = prop_string(node, "lib").ok_or_else(|| missing("lib"))?;
    let system_type = prop_string(node, "type").ok_or_else(|| missing("type"))?;
    Ok(Artifact {
        id: id.to_string(),
        crate_name: crate_name.to_string(),
        cdylib: cdylib.to_string(),
        system_type: system_type.to_string(),
        path: None,
    })
}

/// Parse one `system` node into a [`SystemSpec`] (dl-open.md §6.3). A `lib=` ⇒ a dl
/// system referencing that [`Artifact`]; otherwise a static system. See [`parse`] for
/// the static-params / dl-params-deferred handling.
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
    let params = if artifact.is_some() {
        // Dl system: KDL params require the Wave 3b schema-guided encoder (dl-open.md §6.3).
        if has_config {
            return Err(LoadError::DlKdlParamsUnsupported {
                system: name.to_string(),
                src: src.to_string(),
                span: node.span(),
            });
        }
        Vec::new()
    } else if has_config {
        // Static system with config: carry the node's source so `resolve_static` can
        // re-parse it through `FromKdlNode` (the host links a static system's `Params`).
        node.to_string().into_bytes()
    } else {
        Vec::new()
    };
    Ok(SystemSpec {
        name: name.to_string(),
        ty: ty.to_string(),
        artifact,
        params,
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
        // Explicit long form.
        let to = prop_string(node, "to").ok_or_else(|| missing("to"))?;
        let out = prop_string(node, "out").ok_or_else(|| missing("out"))?;
        let in_ = prop_string(node, "in").ok_or_else(|| missing("in"))?;
        return Ok(Edge {
            from: from.to_string(),
            to: to.to_string(),
            out: out.to_string(),
            in_: in_.to_string(),
            delayed,
        });
    }

    // Shorthand: the nameless arguments are `"from"`, (optional `->`), `"to"`.
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
    let frame = prop_string(node, "frame").ok_or_else(|| missing("frame"))?;
    Ok(Edge {
        from: from.to_string(),
        to: to.to_string(),
        out: frame.to_string(),
        in_: frame.to_string(),
        delayed,
    })
}

/// Resolve one `(instance, frame)` endpoint to a [`PortRef`], validating the frame
/// name against the instance descriptor's port list — a typo is a load error
/// (wiring.md §4.2).
fn resolve_endpoint(
    instances: &HashMap<String, Instance>,
    src: &str,
    name: &str,
    frame: &str,
    dir: Dir,
    span: SourceSpan,
) -> Result<PortRef, LoadError> {
    let inst = instances.get(name).ok_or_else(|| LoadError::UnknownInstance {
        name: name.to_string(),
        src: src.to_string(),
        span,
    })?;
    let frame_id = ComponentId::new(frame);
    let ports = match dir {
        Dir::Out => &inst.desc.outputs,
        Dir::In => &inst.desc.inputs,
    };
    if !ports.iter().any(|p| p.frame_id == frame_id) {
        return Err(LoadError::UnknownFrame {
            instance: name.to_string(),
            frame: frame.to_string(),
            src: src.to_string(),
            span,
        });
    }
    Ok(PortRef {
        system: inst.handle,
        frame_id,
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
