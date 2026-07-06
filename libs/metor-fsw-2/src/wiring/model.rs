//! The `Wiring` data model — the single source of truth for a mission, independent
//! of any text format.
//!
//! This is the middle layer between a mission's front-ends and the coordinator: a
//! plain, serializable Rust description that **both** front-ends produce (the KDL
//! deserializer in [`parse`](super::parse) and the [`WiringBuilder`](super::WiringBuilder))
//! and that the one shared [`resolve`](super::resolve) consumes. Anything KDL can
//! express, the Rust builder can express, because both target this type.
//!
//! These structs are deliberately **decoupled from the runtime types**: a
//! [`ClockSpec`] mirrors [`ClockMode`](crate::ClockMode) without holding a `Duration`,
//! a [`CoordinatorSpec`] mirrors [`CoordinatorConfig`](crate::CoordinatorConfig)
//! without a clock value, etc. The conversion to the runtime types lives in
//! [`resolve`](super::resolve), so the data model stays a pure, serde-serializable
//! data format (it derives `Serialize`/`Deserialize` to prove it is a real one).

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The authoritative description of a wired mission: the coordinator config, the
/// `cdylib` artifacts it loads, the system instances, the slots, and the edges.
/// Produced by [`parse`](super::parse) (from KDL) or
/// [`WiringBuilder`](super::WiringBuilder) (from Rust); consumed by
/// [`resolve`](super::resolve). The telemetry downlink and the command uplink are
/// ordinary systems here — built-in registry types
/// ([`TCP_DOWNLINK_TYPE`]/[`TCP_UPLINK_TYPE`]), not dedicated fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wiring {
    /// Coordinator-wide config (cycle rate, default ring depth, clock).
    pub coordinator: CoordinatorSpec,
    /// The shared objects this mission loads (one system type per cdylib — §6.1).
    pub artifacts: Vec<Artifact>,
    /// The system instances, static (resolved in the [`Registry`](super::Registry)) or
    /// dl (loaded from an [`Artifact`]).
    pub systems: Vec<SystemSpec>,
    /// The runtime-loadable sequence **slots** (sequences-slots.md §5). Each is an
    /// instance like a [`SystemSpec`] — it `connect`s by name — but its occupant is
    /// `Load`ed/`Start`ed/`Stop`ped at runtime from a pre-opened allowed set.
    pub slots: Vec<SlotSpec>,
    /// The producer → consumer edges.
    pub edges: Vec<EdgeSpec>,
}

/// The registry `type=` of the built-in TCP telemetry downlink
/// ([`TelemetrySystem`](crate::TelemetrySystem) over a
/// [`TcpTransport`](crate::TcpTransport); params: [`DownlinkParams`](crate::DownlinkParams)).
pub const TCP_DOWNLINK_TYPE: &str = "TcpDownlink";

/// The registry `type=` of the built-in TCP command uplink
/// ([`UplinkSystem`](crate::UplinkSystem) over a
/// [`TcpRecvTransport`](crate::TcpRecvTransport); params: [`UplinkParams`](crate::UplinkParams)).
pub const TCP_UPLINK_TYPE: &str = "TcpUplink";

/// Coordinator-wide configuration, the serializable mirror of
/// [`CoordinatorConfig`](crate::CoordinatorConfig).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorSpec {
    /// The global cycle rate (Hz) the loop holds under a [`ClockSpec::Wall`] clock.
    pub cycle_rate: f64,
    /// In-flight record depth for a buffer with no rate hint; `None` ⇒ the framework
    /// default ([`DEFAULT_DEPTH`](crate::DEFAULT_DEPTH)).
    pub default_depth: Option<usize>,
    /// Which clock drives the per-cycle timestamp.
    pub clock: ClockSpec,
}

/// Which clock drives the run loop — the serializable mirror of
/// [`ClockMode`](crate::ClockMode), holding a plain `f64` seconds instead of a
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

/// A loadable shared object: "which `.so` + what crate it comes from". One system
/// type per cdylib (the fixed `fsw_*` symbols); multiple [`SystemSpec`]s may
/// reference one `Artifact` to instance that type more than once
/// (the loader `dlopen`s once and `fsw_create`s per instance).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// The id [`SystemSpec::artifact`] references.
    pub id: String,
    /// The cargo package name, for the build driver (`cargo build -p <crate_name>`).
    pub crate_name: String,
    /// The produced shared-object file name (`libfoo.so` / `libfoo.dylib` / `foo.dll`).
    pub cdylib: String,
    /// The single `type=` this `.so`'s `export_system!` provides.
    pub system_type: String,
    /// The resolved artifact location, set by the build driver
    /// ([`build_artifacts`](super::build_artifacts)); `None` until built/located.
    pub path: Option<PathBuf>,
}

/// One system instance. `artifact = None` ⇒ resolve in the static
/// [`Registry`](super::Registry); `Some(id)` ⇒ `dlopen` that [`Artifact`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemSpec {
    /// The instance name (the telemetry prefix, wiring.md §6).
    pub name: String,
    /// The `type=` key (the registry key, or the artifact's `system_type`).
    /// Required for a static system; optional for a dl system (the artifact's
    /// `system_type` is authoritative, and a given `ty` is validated against it
    /// at [`resolve`](super::resolve)).
    #[serde(default)]
    pub ty: Option<String>,
    /// `Some(artifact_id)` ⇒ a dlopen'd system; `None` ⇒ a statically-linked one.
    pub artifact: Option<String>,
    /// Where this system's params come from — see [`ParamSource`].
    pub params: ParamSource,
}

impl SystemSpec {
    /// A built-in TCP telemetry downlink instance tapping **every** output — the spec
    /// the [`WiringBuilder::telemetry`](super::WiringBuilder::telemetry) sugar and the
    /// CLI `--telemetry` flag push. A subset tap declares the `instances`/`frames`
    /// children on an ordinary `system` node instead.
    pub fn tcp_downlink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_DOWNLINK_TYPE, addr)
    }

    /// A built-in TCP command uplink instance — the spec the
    /// [`WiringBuilder::uplink`](super::WiringBuilder::uplink) sugar and the CLI
    /// `--uplink` flag push. Route its commands with explicit edges
    /// (`connect "<name>" -> … msg="…"`).
    pub fn tcp_uplink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_UPLINK_TYPE, addr)
    }

    /// Both built-ins carry one `addr=` param; render it as the KDL node text the
    /// static resolve path re-decodes (the typed-postcard builder path is dl-only).
    fn tcp_builtin(name: &str, ty: &str, addr: SocketAddr) -> Self {
        Self {
            name: name.to_string(),
            ty: Some(ty.to_string()),
            artifact: None,
            params: ParamSource::Kdl(format!("system \"{name}\" type=\"{ty}\" addr=\"{addr}\"")),
        }
    }
}

/// Where a [`SystemSpec`]'s params come from — an explicit enum so the three cases
/// do not overload one `Vec<u8>` (the one-postcard-encoding decision).
///
/// At [`resolve`](super::resolve) each variant becomes the canonical postcard `Params`
/// bytes that cross `fsw_create` (dl) or the KDL node the registry factory
/// deserializes (static):
///
/// - [`None`](ParamSource::None) — a paramless system (config-less KDL, or a builder
///   system with no `.params(..)`): empty postcard bytes / a minimal synthesized node.
/// - [`Postcard`](ParamSource::Postcard) — the typed Rust [`WiringBuilder::params`](super::SystemSpecBuilder::params)
///   path: a `Params` value already postcard-encoded to its canonical bytes. For a dl
///   system these are exactly the bytes `fsw_create` decodes.
/// - [`Kdl`](ParamSource::Kdl) — the **KDL node's source text**, carried verbatim so
///   [`resolve`](super::resolve) can re-decode it through the shared KDL params
///   deserializer: a **static** system deserializes it into its typed `Params` (the
///   host links `Params`); a **dl** system schema-encodes it against the `.so`'s
///   exported `Params` schema (the host stays schema-agnostic), producing the **same**
///   bytes the [`Postcard`](ParamSource::Postcard) path produces. Which decoder runs is
///   chosen by [`SystemSpec::artifact`], not by the variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamSource {
    /// No params: empty postcard bytes (dl) / a minimal synthesized node (static).
    None,
    /// Canonical postcard `Params` bytes (the typed Rust builder path).
    Postcard(Vec<u8>),
    /// The KDL node's source text, re-decoded at resolve (static: typed serde
    /// deserialize; dl: schema-guided postcard encode).
    Kdl(String),
}

impl ParamSource {
    /// `true` for [`None`](ParamSource::None) — a paramless system.
    pub fn is_none(&self) -> bool {
        matches!(self, ParamSource::None)
    }
}

/// A runtime-loadable **slot** instance (sequences-slots.md §5): a fixed position in the
/// cyclic chain whose occupant the host swaps at runtime. The `inputs`/`outputs` are the
/// declared user-port contract (validated at [`resolve`](super::resolve) against the
/// allowed occupants' shared descriptor); `allow` is the pre-opened candidate set; an
/// optional `initial` occupant is applied at startup. A slot `connect`s by `name` exactly
/// like a [`SystemSpec`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotSpec {
    /// The slot instance name (the telemetry prefix + `connect`/command address).
    pub name: String,
    /// The declared **input** user-port frame names (the validated contract).
    pub inputs: Vec<String>,
    /// The declared **output** user-port frame names (the validated contract).
    pub outputs: Vec<String>,
    /// The allowed occupants, each an [`Artifact`] referenced by id (1:1 with the `Load`
    /// name for v1). Non-empty is required at [`resolve`](super::resolve).
    pub allow: Vec<AllowedOccupantSpec>,
    /// The occupant to apply at startup, if any.
    pub initial: Option<InitialOccupantSpec>,
}

/// One allowed occupant of a [`SlotSpec`]: an [`Artifact`] referenced by id (the `Load`
/// name == the artifact id for v1), plus optional default params (a `system`-node-shaped
/// [`ParamSource`], schema-encoded against the occupant `.so` at resolve).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AllowedOccupantSpec {
    /// The occupant id — the [`Artifact::id`] of the sequence cdylib, and its `Load` name.
    pub occupant: String,
    /// Where this occupant's default params come from — see [`ParamSource`].
    pub params: ParamSource,
}

/// The occupant a [`SlotSpec`] applies at startup (sequences-slots.md §5): which allowed
/// occupant and what lifecycle state to bring it to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitialOccupantSpec {
    /// The allowed-set occupant id to `Load` at startup.
    pub occupant: String,
    /// The startup lifecycle state to drive it to.
    pub state: SlotInitState,
}

/// The startup lifecycle state of a [`SlotSpec`]'s initial occupant: leave the slot empty,
/// `Load` the occupant, or `Load` + `Start` it (running from the first cycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotInitState {
    /// No initial occupant (the slot starts empty).
    Empty,
    /// `Load` the occupant at startup (built but not polling).
    Loaded,
    /// `Load` + `Start` the occupant (running from the first cycle).
    Running,
}

/// Whether an [`EdgeSpec`] wires a component **frame** or a **message** channel
/// (`docs/message-wiring.md` §3). `out`/`in_` name a frame for `Frame` and a message type
/// for `Msg`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// A component-frame edge (`connect`/`connect_delayed`), validated by subset compatibility.
    #[default]
    Frame,
    /// A message edge (`connect_msg`), many-to-many and excluded from cycle detection.
    Msg,
}

/// One producer → consumer edge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    /// Producer instance name.
    pub from: String,
    /// Producer output port name (a frame name for `Frame`, a message type name for `Msg`).
    pub out: String,
    /// Consumer instance name.
    pub to: String,
    /// Consumer input port name (a frame name for `Frame`, a message type name for `Msg`).
    pub in_: String,
    /// `true` ⇒ a one-cycle-delayed feedback back-edge (`connect_delayed`). Frame edges only.
    pub delayed: bool,
    /// Frame vs message edge. Defaults to `Frame` (serde back-compat for existing docs).
    #[serde(default)]
    pub kind: EdgeKind,
}

