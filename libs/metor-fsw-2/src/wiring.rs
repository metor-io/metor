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

use std::collections::HashMap;
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
use crate::system::{AsyncSystem, CyclicSystem, Out, SystemOutput};
use crate::telemetry::{TcpTransport, TelemetryConfig, TelemetryMode};

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

/// A concrete system opts in by declaring its params type and how to build itself
/// (wiring.md §2.3). The `add`/`descriptor` halves come for free from the
/// [`AddToBuilder`] blanket impls — keyed on the system *kind*, resolved at compile
/// time, so KDL never declares cyclic vs async.
pub trait RegisteredSystem: Sized {
    /// The params struct deserialized from the KDL config (wiring.md §3).
    type Params: FromKdlNode;
    /// Construct the (pre-init) system from its params.
    fn new(params: Self::Params) -> Self;
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
/// `add_*_named`" dance for one concrete type, erased to a plain `fn` pointer.
fn factory<S, K>(ctx: &mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>
where
    S: RegisteredSystem + AddToBuilder<K>,
{
    let params = S::Params::from_kdl_node(ctx.node, ctx.src)?;
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
    /// and `new` via [`RegisteredSystem`]; the `add_*`/descriptor (and cyclic-vs-
    /// async branch) come from the [`AddToBuilder`] blanket impl, inferred here.
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where
        S: RegisteredSystem + AddToBuilder<K>,
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
    span: SourceSpan,
}

/// Parse a KDL wiring document, instantiate every system from `registry`, connect
/// the edges, and return a built [`Coordinator`] ready to `run` (wiring.md §5.1).
pub fn load(kdl: &str, registry: &Registry) -> Result<Coordinator, LoadError> {
    let doc = kdl.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: kdl.to_string(),
        span: (0, kdl.len()).into(),
    })?;

    let config = parse_coordinator(&doc, kdl)?;
    let mut builder = Coordinator::builder(config);

    // --- Systems pass (wiring.md §2) -------------------------------------
    let mut instances: HashMap<String, Instance> = HashMap::new();
    for node in doc.nodes() {
        if node.name().value() != "system" {
            continue;
        }
        let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
            src: kdl.to_string(),
            span: node.span(),
        })?;
        let ty = prop_string(node, "type").ok_or_else(|| LoadError::MissingType {
            name: name.to_string(),
            src: kdl.to_string(),
            span: node.span(),
        })?;
        let factory = registry
            .factories
            .get(ty)
            .ok_or_else(|| LoadError::UnknownType {
                ty: ty.to_string(),
                src: kdl.to_string(),
                span: node.span(),
            })?;
        let (handle, desc) = factory(&mut LoadCtx {
            node,
            src: kdl,
            name,
            builder: &mut builder,
        })?;
        if instances
            .insert(name.to_string(), Instance { handle, desc })
            .is_some()
        {
            return Err(LoadError::DuplicateInstance {
                name: name.to_string(),
                src: kdl.to_string(),
                span: node.span(),
            });
        }
    }

    // --- Edges pass (wiring.md §4) ---------------------------------------
    let mut edge_spans: Vec<(ComponentId, SourceSpan)> = Vec::new();
    for node in doc.nodes() {
        if node.name().value() != "connect" {
            continue;
        }
        let edge = parse_edge(node, kdl)?;
        let producer = resolve(&instances, kdl, &edge.from, &edge.out, Dir::Out, edge.span)?;
        let consumer = resolve(&instances, kdl, &edge.to, &edge.in_, Dir::In, edge.span)?;
        edge_spans.push((producer.frame_id, edge.span));
        let result = if edge.delayed {
            builder.connect_delayed(producer, consumer)
        } else {
            builder.connect(producer, consumer)
        };
        result.map_err(|source| LoadError::Wire {
            source,
            src: kdl.to_string(),
            span: edge.span,
        })?;
    }

    // --- Telemetry pass (telemetry.md §8) --------------------------------
    // Added after every `system`, so the downlink is registered last (its end-of-cycle
    // snapshot observes every system's fresh output).
    for node in doc.nodes() {
        if node.name().value() != "telemetry" {
            continue;
        }
        let (addr, mode) = parse_telemetry(node, kdl)?;
        builder.add_telemetry(TelemetryConfig {
            transport: TcpTransport::new(addr),
            mode,
        });
    }

    // --- Build (wiring.md §3 pass) ---------------------------------------
    builder.build().map_err(|e| wire_at_build(e, kdl, &edge_spans))
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
fn parse_telemetry(node: &KdlNode, src: &str) -> Result<(std::net::SocketAddr, TelemetryMode), LoadError> {
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
        "all" => TelemetryMode::All,
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
            TelemetryMode::Subset { instances, frames }
        }
        _ => return Err(invalid("mode", "\"all\" or \"subset\"")),
    };
    Ok((addr, mode))
}

/// Read the single `coordinator` node into a [`CoordinatorConfig`] (wiring.md §1.1).
fn parse_coordinator(doc: &KdlDocument, src: &str) -> Result<CoordinatorConfig, LoadError> {
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
    let mut config = CoordinatorConfig::default();
    config.cycle_rate = kdl_required::<f64>(node, "cycle_rate", "coordinator", src)?;
    if let Some(depth) = kdl_optional::<usize>(node, "default_depth", "coordinator", src)? {
        config.default_depth = depth;
    }
    // `sim_dt` (seconds) present ⇒ a free-running simulated clock; absent ⇒ `Wall`.
    if let Some(sim_dt) = kdl_optional::<f64>(node, "sim_dt", "coordinator", src)? {
        config.clock = ClockMode::Simulated {
            dt: Duration::from_secs_f64(sim_dt),
        };
    }
    Ok(config)
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
            span,
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
        span,
    })
}

/// Resolve one `(instance, frame)` endpoint to a [`PortRef`], validating the frame
/// name against the instance descriptor's port list — a typo is a load error
/// (wiring.md §4.2).
fn resolve(
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

/// Best-effort map of a `build()`-time [`WireError`] back to a source span: errors
/// naming a `frame_id` point at the `connect` that introduced that frame; the rest
/// point at the whole document (wiring.md §5.2 / Q7).
fn wire_at_build(err: WireError, src: &str, edges: &[(ComponentId, SourceSpan)]) -> LoadError {
    let doc_span: SourceSpan = (0, src.len()).into();
    let span = match &err {
        WireError::Incompatible { frame_id, .. }
        | WireError::UnconnectedInput { frame_id, .. }
        | WireError::DoubleConnect { frame_id, .. }
        | WireError::UnknownPort { frame_id, .. } => edges
            .iter()
            .find(|(f, _)| f == frame_id)
            .map(|(_, s)| *s)
            .unwrap_or(doc_span),
        WireError::UnknownSystem { .. }
        | WireError::FrameIdMismatch { .. }
        | WireError::FeedbackCycle { .. } => doc_span,
    };
    LoadError::Wire {
        source: err,
        src: src.to_string(),
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
