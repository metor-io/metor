//! The [`Wiring`] data model, a plain serializable description of a mission.
//!
//! This module is the mission IR: pure data plus its serde/serde_json codec,
//! carrying no runtime types, so an evaluated front-end can emit it and the
//! host can re-ingest it with only the `wiring-model` feature. The Python-eval
//! path, the Rust builder, and the shared resolver live under the `wiring`-gated
//! [`wiring`](crate::wiring) module, which re-exports these types.
//!
//! Both front-ends produce this type. The Python `metor_config` recorder emits
//! it as JSON and the [`wiring::WiringBuilder`](crate::wiring::WiringBuilder)
//! builds it directly, and the one shared `wiring::resolve` consumes it, so
//! anything one front-end can express the other can express too.
//!
//! The specs here deliberately hold no runtime values. A [`ClockSpec`] mirrors
//! [`ClockMode`](crate::ClockMode) with a plain `f64` in place of a `Duration`,
//! a [`CoordinatorSpec`] mirrors [`CoordinatorConfig`](crate::CoordinatorConfig)
//! without a clock value, and so on. Conversion into the runtime types happens
//! in `wiring::resolve`, leaving this module a pure serde data format.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The version of the [`Wiring`] data model itself. Both front-ends stamp it
/// and [`resolve`](crate::wiring::resolve) checks it, so a serialized `Wiring` from a
/// different-generation producer fails loudly instead of misresolving.
///
/// v2 dropped the `ParamSource::Kdl` variant with the KDL front-end (phase 4):
/// a wire-shape change, so a v1 bundle fails the version check and must be
/// rebuilt.
///
/// v3 replaced [`Artifact`]'s serialized shared-object file name with the bare
/// library stem ([`Artifact::lib`]) and added the prebuilt-artifact fields
/// (`prebuilt_dir`, `dist`). The file name is derived per target triple at
/// provision time — recording it froze the *recording* host's platform into
/// the IR, which broke cross-target packaging.
pub const IR_VERSION: u32 = 3;

/// A plain-data description of a complete mission, naming the systems that
/// run, where their code and params come from, and how their ports connect.
///
/// Produced by [`parse`](crate::wiring::parse) or [`WiringBuilder`](crate::wiring::WiringBuilder),
/// consumed by [`resolve`](crate::wiring::resolve). The telemetry downlink and the
/// command uplink appear here as ordinary systems with the built-in registry
/// types [`TCP_DOWNLINK_TYPE`] and [`TCP_UPLINK_TYPE`], not as dedicated fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wiring {
    /// The [`IR_VERSION`] this value was produced against. Deliberately not
    /// serde-defaulted: a serialized `Wiring` with no version is an error,
    /// not a guess.
    pub ir_version: u32,
    /// Coordinator-wide config (cycle rate, default ring depth, clock).
    pub coordinator: CoordinatorSpec,
    /// The shared objects this mission loads, one pack per cdylib.
    pub artifacts: Vec<Artifact>,
    /// The system instances, either static (resolved in the
    /// [`Registry`](crate::wiring::Registry)) or loaded from an [`Artifact`].
    pub systems: Vec<SystemSpec>,
    /// The runtime-loadable slots. Each connects by name like a [`SystemSpec`],
    /// but its occupant is loaded, started, and stopped at runtime from a
    /// pre-opened allowed set.
    pub slots: Vec<SlotSpec>,
    /// The producer-to-consumer edges.
    pub edges: Vec<EdgeSpec>,
    /// The scope table that [`SystemSpec::scope`]/[`SlotSpec::scope`] index
    /// into. Consumer metadata only: instance names stay flat and
    /// collision-checked regardless. The Rust builder leaves it empty.
    #[serde(default)]
    pub scopes: Vec<ScopeSpec>,
}

impl Wiring {
    /// A clone with every [`Artifact::path`] and [`Artifact::prebuilt_dir`]
    /// cleared: the relocatable, reproducible form of the IR. Both point into
    /// a build tree or an installed environment, so they are provenance rather
    /// than identity — [`resolve`](crate::wiring::resolve) re-derives them on
    /// load. Stripping them is what lets the bundle's `wiring.json` stay
    /// byte-reproducible and a `WiringManifest` describe the same topology
    /// regardless of where it was built.
    pub fn path_stripped(&self) -> Wiring {
        let mut w = self.clone();
        for artifact in &mut w.artifacts {
            artifact.path = None;
            artifact.prebuilt_dir = None;
        }
        w
    }
}

/// Where a spec came from in the document that declared it: an optional file
/// name and a 1-based line and column, matching miette's rendering. One
/// anchor for now; a stack of caller frames can be added later without
/// breaking this shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The source file, when the front-end knows it (the evaluated
    /// `mission.py`'s path).
    pub file: Option<String>,
    /// 1-based line of the declaring node.
    pub line: u32,
    /// 1-based column of the declaring node.
    pub col: u32,
}

/// One entry in [`Wiring::scopes`]: a named grouping of systems and slots,
/// nested through `parent`. Purely descriptive — consumers reconstruct the
/// block tree from it without parsing instance names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopeSpec {
    /// The scope's full dotted path.
    pub path: String,
    /// Index of the enclosing scope in [`Wiring::scopes`], `None` at the root.
    pub parent: Option<usize>,
    /// Where the scope was opened.
    pub src: Option<SourceRef>,
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
    /// The bare library stem (`foo` for `libfoo.so`/`libfoo.dylib`/`foo.dll`).
    /// The file name is derived per target triple at provision time
    /// ([`cdylib_file_name_for`](crate::wiring::cdylib_file_name_for)); the IR
    /// stays arch-neutral.
    pub lib: String,
    /// The resolved artifact location, filled in by
    /// [`provision_artifacts`](crate::wiring::provision_artifacts). `None` until built or
    /// located.
    pub path: Option<PathBuf>,
    /// Where a prebuilt artifact's per-triple libraries live: a directory
    /// with one `<triple>/` subdirectory per shipped target, each holding the
    /// cdylib and its `.manifest` sidecar — an installed pack wheel's `_libs`
    /// dir, or a local pack's `.metor/libs`. `None` for a crate-built
    /// artifact, which the build driver compiles instead
    /// (`docs/design-packaging.md` §8). Provenance like `path`: stripped by
    /// [`Wiring::path_stripped`].
    #[serde(default)]
    pub prebuilt_dir: Option<PathBuf>,
    /// The published distribution this artifact came from, if any. Pure
    /// provenance, carried into the bundle's `meta.json`.
    #[serde(default)]
    pub dist: Option<DistRef>,
    /// The `sha256:<hex>` hash of the pack manifest the generated stub module
    /// (`metor-fsw stubgen`) was produced against, carried through from the
    /// module's `ARTIFACT` constant. [`resolve`](crate::wiring::resolve)
    /// compares it against the live manifest and refuses a stale stub
    /// ([`StaleStubs`](crate::wiring::LoadErrorKind::StaleStubs)). `None` for a
    /// builder-authored artifact and hand-written `pack()` handles, which skip
    /// the check.
    #[serde(default)]
    pub manifest_hash: Option<String>,
    /// Where this artifact was declared.
    #[serde(default)]
    pub src: Option<SourceRef>,
}

/// The published distribution an [`Artifact`] was installed from, as the
/// generated stub module recorded it (`dist="adcs-pack"`,
/// `dist_version="1.2.0"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DistRef {
    /// The distribution name.
    pub name: String,
    /// The distribution version string.
    pub version: String,
}

/// One system instance. With `artifact = None` the type is resolved in the
/// static [`Registry`](crate::wiring::Registry); with `Some(id)` it is loaded from
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
    /// Where this system was declared.
    #[serde(default)]
    pub src: Option<SourceRef>,
    /// Index of this system's scope in [`Wiring::scopes`], `None` when
    /// unscoped (always, for the Rust builder).
    #[serde(default)]
    pub scope: Option<usize>,
}

impl SystemSpec {
    /// A built-in TCP telemetry downlink instance that taps every output, the
    /// spec pushed by [`WiringBuilder::telemetry`](crate::wiring::WiringBuilder::telemetry)
    /// and the CLI `--telemetry` flag. A subset tap instead declares
    /// `instances`/`frames` children on an ordinary `system` node.
    pub fn tcp_downlink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_DOWNLINK_TYPE, addr)
    }

    /// A built-in TCP command uplink instance, the spec pushed by
    /// [`WiringBuilder::uplink`](crate::wiring::WiringBuilder::uplink) and the CLI
    /// `--uplink` flag. Its commands are routed by explicit edges
    /// (`connect "<name>" -> … msg="…"`).
    pub fn tcp_uplink(name: &str, addr: SocketAddr) -> Self {
        Self::tcp_builtin(name, TCP_UPLINK_TYPE, addr)
    }

    /// Both built-ins take a single `addr=` param, carried as a
    /// [`ParamSource::Value`] tree: the static path deserializes it with
    /// serde, so the `SocketAddr` reads from the JSON string and the params'
    /// `#[serde(default)]` fields are honored.
    fn tcp_builtin(name: &str, ty: &str, addr: SocketAddr) -> Self {
        Self {
            name: name.to_string(),
            ty: Some(ty.to_string()),
            artifact: None,
            params: ParamSource::Value(serde_json::json!({ "addr": addr.to_string() })),
            process: false,
            src: None,
            scope: None,
        }
    }
}

/// Where a [`SystemSpec`]'s params come from.
///
/// At [`resolve`](crate::wiring::resolve) every variant reduces to the same encodings:
/// the canonical postcard `Params` bytes that cross `fsw_create` for a loaded
/// system, or a typed `S::Params` value for a static one. Which decoder runs
/// is decided by [`SystemSpec::artifact`], not by the variant.
/// [`Value`](ParamSource::Value) carries a plain value tree — the format the
/// evaluated Python front-end emits — conformed against the object's exported
/// `Params` schema and postcard-encoded for a loaded system, or
/// serde-deserialized (field defaults honored) for a static one.
/// [`Postcard`](ParamSource::Postcard) carries a `Params` value the Rust
/// builder already encoded, exactly the bytes `fsw_create` decodes; it is
/// dl-only, since a static system has no postcard decode path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamSource {
    /// No params. Resolves to the entry's declared defaults (or empty
    /// postcard bytes) for a loaded system, an all-defaults decode for a
    /// static one.
    None,
    /// Canonical postcard `Params` bytes, the typed Rust builder path.
    /// dl-only; the static path rejects it as
    /// [`StaticPostcardParams`](crate::wiring::LoadErrorKind::StaticPostcardParams).
    Postcard(Vec<u8>),
    /// A params value tree (schema-conform + encode for loaded, serde
    /// deserialize for static).
    Value(serde_json::Value),
}

/// A runtime-loadable slot, a fixed position in the cyclic chain whose
/// occupant the host swaps at runtime.
///
/// The `inputs`/`outputs` declare the user-port contract, validated at
/// [`resolve`](crate::wiring::resolve) against the descriptor every allowed occupant
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
    /// non-empty at [`resolve`](crate::wiring::resolve).
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
    /// Where this slot was declared.
    #[serde(default)]
    pub src: Option<SourceRef>,
    /// Index of this slot's scope in [`Wiring::scopes`], `None` when
    /// unscoped (always, for the Rust builder).
    #[serde(default)]
    pub scope: Option<usize>,
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
    /// Where this `allow` was declared.
    #[serde(default)]
    pub src: Option<SourceRef>,
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
    /// Where this edge was declared.
    #[serde(default)]
    pub src: Option<SourceRef>,
}
