//! The [`Wiring`] data model, a plain serializable description of a mission.
//!
//! Both front-ends produce this type. The KDL deserializer in
//! [`parse`](super::parse) and the [`WiringBuilder`](super::WiringBuilder) each
//! build a `Wiring`, and the one shared [`resolve`](super::resolve) consumes
//! it, so anything one front-end can express the other can express too.
//!
//! The specs here deliberately hold no runtime values. A [`ClockSpec`] mirrors
//! [`ClockMode`](crate::ClockMode) with a plain `f64` in place of a `Duration`,
//! a [`CoordinatorSpec`] mirrors [`CoordinatorConfig`](crate::CoordinatorConfig)
//! without a clock value, and so on. Conversion into the runtime types happens
//! in [`resolve`](super::resolve), leaving this module a pure serde data format.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A plain-data description of a complete mission, naming the systems that
/// run, where their code and params come from, and how their ports connect.
///
/// Produced by [`parse`](super::parse) or [`WiringBuilder`](super::WiringBuilder),
/// consumed by [`resolve`](super::resolve). The telemetry downlink and the
/// command uplink appear here as ordinary systems with the built-in registry
/// types [`TCP_DOWNLINK_TYPE`] and [`TCP_UPLINK_TYPE`], not as dedicated fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wiring {
    /// Coordinator-wide config (cycle rate, default ring depth, clock).
    pub coordinator: CoordinatorSpec,
    /// The shared objects this mission loads, one pack per cdylib.
    pub artifacts: Vec<Artifact>,
    /// The system instances, either static (resolved in the
    /// [`Registry`](super::Registry)) or loaded from an [`Artifact`].
    pub systems: Vec<SystemSpec>,
    /// The runtime-loadable slots. Each connects by name like a [`SystemSpec`],
    /// but its occupant is loaded, started, and stopped at runtime from a
    /// pre-opened allowed set.
    pub slots: Vec<SlotSpec>,
    /// The producer-to-consumer edges.
    pub edges: Vec<EdgeSpec>,
}

/// Registry `type=` of the built-in TCP telemetry downlink, a
/// [`TelemetrySystem`](crate::TelemetrySystem) over a
/// [`TcpTransport`](crate::TcpTransport) configured by
/// [`DownlinkParams`](crate::DownlinkParams).
pub const TCP_DOWNLINK_TYPE: &str = "TcpDownlink";

/// Registry `type=` of the built-in TCP command uplink, an
/// [`UplinkSystem`](crate::UplinkSystem) over a
/// [`TcpRecvTransport`](crate::TcpRecvTransport) configured by
/// [`UplinkParams`](crate::UplinkParams).
pub const TCP_UPLINK_TYPE: &str = "TcpUplink";

/// Coordinator-wide configuration, the serializable mirror of
/// [`CoordinatorConfig`](crate::CoordinatorConfig).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorSpec {
    /// The global cycle rate in Hz that the loop holds under a
    /// [`ClockSpec::Wall`] clock.
    pub cycle_rate: f64,
    /// In-flight record depth for a buffer with no rate hint. `None` selects
    /// the framework default, [`DEFAULT_DEPTH`](crate::DEFAULT_DEPTH).
    pub default_depth: Option<usize>,
    /// Which clock drives the per-cycle timestamp.
    pub clock: ClockSpec,
}

/// Which clock drives the run loop, the serializable mirror of
/// [`ClockMode`](crate::ClockMode). Holds plain `f64` seconds rather than a
/// `Duration` so the model carries no runtime type.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClockSpec {
    /// Wall-clock time, paced to `cycle_rate`.
    Wall,
    /// A free-running simulated clock advancing by `dt_secs` each cycle.
    Simulated {
        /// The logical per-cycle step, in seconds.
        dt_secs: f64,
    },
}

/// A loadable pack shared object and the crate it comes from.
///
/// Each cdylib exports one **pack** — any number of system types — through
/// the fixed `fsw_pack_*` symbols; a `system` node's `type=` selects an entry
/// from the opened pack's manifest. Several [`SystemSpec`]s may reference the
/// same artifact (and the same entry) to instance it more than once; the
/// loader opens the object once and runs the create phase per instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// The id that [`SystemSpec::artifact`] references.
    pub id: String,
    /// The cargo package name, used by the build driver as
    /// `cargo build -p <crate_name>`.
    pub crate_name: String,
    /// The produced shared-object file name (`libfoo.so`, `libfoo.dylib`,
    /// `foo.dll`).
    pub cdylib: String,
    /// The resolved artifact location, filled in by
    /// [`build_artifacts`](super::build_artifacts). `None` until built or
    /// located.
    pub path: Option<PathBuf>,
}

/// One system instance. With `artifact = None` the type is resolved in the
/// static [`Registry`](super::Registry); with `Some(id)` it is loaded from
/// that [`Artifact`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemSpec {
    /// The instance name, which is also the telemetry prefix.
    pub name: String,
    /// The `type=` key: a registry key for a static system, or the pack
    /// entry name for a loaded one. Required unless the artifact's pack
    /// exports exactly one entry.
    #[serde(default)]
    pub ty: Option<String>,
    /// `Some(artifact_id)` for a system loaded from a shared object, `None`
    /// for a statically linked one.
    pub artifact: Option<String>,
    /// Where this system's params come from.
    pub params: ParamSource,
    /// `true` runs the artifact in its own worker process (`process=#true`,
    /// `docs/process-systems.md`); requires `artifact`. Default `false`: a
    /// loaded system runs in-process. A document that omits the property
    /// deserializes unchanged.
    #[serde(default)]
    pub process: bool,
}

impl SystemSpec {
    /// A built-in TCP telemetry downlink instance that taps every output, the
    /// spec pushed by [`WiringBuilder::telemetry`](super::WiringBuilder::telemetry)
    /// and the CLI `--telemetry` flag. A subset tap instead declares
    /// `instances`/`frames` children on an ordinary `system` node.
    pub fn tcp_downlink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_DOWNLINK_TYPE, addr)
    }

    /// A built-in TCP command uplink instance, the spec pushed by
    /// [`WiringBuilder::uplink`](super::WiringBuilder::uplink) and the CLI
    /// `--uplink` flag. Its commands are routed by explicit edges
    /// (`connect "<name>" -> … msg="…"`).
    pub fn tcp_uplink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_UPLINK_TYPE, addr)
    }

    /// Both built-ins take a single `addr=` param. It is rendered as KDL node
    /// text because static systems re-decode their params from a node at
    /// resolve; the typed postcard path only serves loaded systems.
    fn tcp_builtin(name: &str, ty: &str, addr: SocketAddr) -> Self {
        Self {
            name: name.to_string(),
            ty: Some(ty.to_string()),
            artifact: None,
            params: ParamSource::Kdl(format!("system \"{name}\" type=\"{ty}\" addr=\"{addr}\"")),
            process: false,
        }
    }
}

/// Where a [`SystemSpec`]'s params come from.
///
/// At [`resolve`](super::resolve) every variant reduces to the same encodings:
/// the canonical postcard `Params` bytes that cross `fsw_create` for a loaded
/// system, or the KDL node the registry factory deserializes for a static one.
/// [`Postcard`](ParamSource::Postcard) carries a `Params` value the Rust
/// builder already encoded, exactly the bytes `fsw_create` decodes.
/// [`Kdl`](ParamSource::Kdl) carries the node's source text verbatim so
/// resolve can re-decode it through the shared KDL params deserializer; a
/// static system deserializes it straight into its typed `Params` (the host
/// links `Params`), while a loaded system schema-encodes it against the
/// object's exported `Params` schema (the host stays schema-agnostic),
/// producing the same bytes the `Postcard` path would. Which decoder runs is
/// decided by [`SystemSpec::artifact`], not by the variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamSource {
    /// No params. Resolves to empty postcard bytes for a loaded system, or a
    /// minimal synthesized node for a static one.
    None,
    /// Canonical postcard `Params` bytes, the typed Rust builder path.
    Postcard(Vec<u8>),
    /// The KDL node's source text, re-decoded at resolve (typed serde
    /// deserialize for static, schema-guided postcard encode for loaded).
    Kdl(String),
}

impl ParamSource {
    /// `true` for a paramless system.
    pub fn is_none(&self) -> bool {
        matches!(self, ParamSource::None)
    }
}

/// A runtime-loadable slot, a fixed position in the cyclic chain whose
/// occupant the host swaps at runtime.
///
/// The `inputs`/`outputs` declare the user-port contract, validated at
/// [`resolve`](super::resolve) against the descriptor every allowed occupant
/// shares. `allow` is the pre-opened candidate set, and an optional `initial`
/// occupant is applied at startup. A slot connects by `name` exactly like a
/// [`SystemSpec`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotSpec {
    /// The slot instance name, also its telemetry prefix and connect/command
    /// address.
    pub name: String,
    /// The declared input user-port frame names.
    pub inputs: Vec<String>,
    /// The declared output user-port frame names.
    pub outputs: Vec<String>,
    /// The allowed occupants, each an [`Artifact`] referenced by id. Must be
    /// non-empty at [`resolve`](super::resolve).
    pub allow: Vec<AllowedOccupantSpec>,
    /// The occupant to apply at startup, if any.
    pub initial: Option<InitialOccupantSpec>,
    /// `true` runs every occupant out of process (`process=#true`,
    /// `docs/process-slots.md`): resolve describes each allowed occupant
    /// through a worker instead of dlopening it, and every `Load` spawns a
    /// worker over the slot's session-dir rings. Per-slot means all-occupants,
    /// so a `Load` can never change the slot's fault domain. Default `false`;
    /// a document that omits the property deserializes unchanged.
    #[serde(default)]
    pub process: bool,
}

/// One allowed occupant of a [`SlotSpec`]: a pack entry named across the
/// slot's artifacts, plus optional default params. The params are a
/// `system`-node-shaped [`ParamSource`], schema-encoded against the
/// occupant's shared object at resolve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AllowedOccupantSpec {
    /// The pack entry name, which is also the name a load command uses.
    pub occupant: String,
    /// The [`Artifact::id`] whose pack exports the entry. Omitted, resolve
    /// searches every artifact for a unique entry of that name; an ambiguous
    /// name is a clean error.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Where this occupant's default params come from.
    pub params: ParamSource,
}

/// The occupant a [`SlotSpec`] applies at startup: which allowed occupant, and
/// what lifecycle state to bring it to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitialOccupantSpec {
    /// The allowed-set occupant id to load at startup.
    pub occupant: String,
    /// The startup lifecycle state to drive it to.
    pub state: SlotInitState,
}

/// The startup lifecycle state of a [`SlotSpec`]'s initial occupant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotInitState {
    /// No initial occupant; the slot starts empty.
    Empty,
    /// Load the occupant at startup, built but not polling.
    Loaded,
    /// Load and start the occupant, running from the first cycle.
    Running,
}

/// Whether an [`EdgeSpec`] wires a component frame or a message channel.
/// `out`/`in_` name a frame for `Frame` and a message type for `Msg`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// A component-frame edge (`connect`/`connect_delayed`), validated by
    /// subset compatibility.
    #[default]
    Frame,
    /// A message edge (`connect_msg`), many-to-many and excluded from cycle
    /// detection.
    Msg,
}

/// One producer-to-consumer edge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    /// Producer instance name.
    pub from: String,
    /// Producer output port name.
    pub out: String,
    /// Consumer instance name.
    pub to: String,
    /// Consumer input port name.
    pub in_: String,
    /// `true` marks a one-cycle-delayed feedback back-edge
    /// (`connect_delayed`). Frame edges only.
    pub delayed: bool,
    /// Frame or message edge. A document that omits the field deserializes as
    /// a frame edge.
    #[serde(default)]
    pub kind: EdgeKind,
}
