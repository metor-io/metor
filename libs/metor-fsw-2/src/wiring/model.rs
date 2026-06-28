//! The `Wiring` data model — the single source of truth for a mission, independent
//! of any text format (dl-open.md §6.1).
//!
//! Today's seam: KDL used to *be* the model — `wiring.rs` parsed a document straight
//! into `CoordinatorBuilder` calls. Wave 3a inserts this missing middle layer: a
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

/// The authoritative description of a wired mission (dl-open.md §6.1): the coordinator
/// config, the `cdylib` artifacts it loads, the system instances, the edges, and the
/// optional telemetry downlink. Produced by [`parse`](super::parse) (from KDL) or
/// [`WiringBuilder`](super::WiringBuilder) (from Rust); consumed by
/// [`resolve`](super::resolve).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wiring {
    /// Coordinator-wide config (cycle rate, default ring depth, clock).
    pub coordinator: CoordinatorSpec,
    /// The shared objects this mission loads (one system type per cdylib — §6.1).
    pub artifacts: Vec<Artifact>,
    /// The system instances, static (resolved in the [`Registry`](super::Registry)) or
    /// dl (loaded from an [`Artifact`]).
    pub systems: Vec<SystemSpec>,
    /// The producer → consumer edges (the old `wiring.rs` `Edge`, lifted to data).
    pub edges: Vec<EdgeSpec>,
    /// The telemetry downlink, if any.
    pub telemetry: Option<TelemetrySpec>,
}

/// Coordinator-wide configuration, the serializable mirror of
/// [`CoordinatorConfig`](crate::CoordinatorConfig) (dl-open.md §6.1).
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
/// `Duration` so the model carries no runtime type (dl-open.md §6.1).
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

/// A loadable shared object: "which `.so` + what crate it comes from" (dl-open.md
/// §6.1). One system type per cdylib (the fixed `fsw_*` symbols, §3.0); multiple
/// [`SystemSpec`]s may reference one `Artifact` to instance that type more than once
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

/// One system instance (dl-open.md §6.1). `artifact = None` ⇒ resolve in the static
/// [`Registry`](super::Registry); `Some(id)` ⇒ `dlopen` that [`Artifact`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemSpec {
    /// The instance name (the telemetry prefix, wiring.md §6).
    pub name: String,
    /// The `type=` key (the registry key, or the artifact's `system_type`).
    pub ty: String,
    /// `Some(artifact_id)` ⇒ a dlopen'd system; `None` ⇒ a statically-linked one.
    pub artifact: Option<String>,
    /// Where this system's params come from (dl-open.md §6.3) — see [`ParamSource`].
    pub params: ParamSource,
}

/// Where a [`SystemSpec`]'s params come from — made explicit so the three cases no
/// longer overload one `Vec<u8>` (dl-open.md §6.3, the one-postcard-encoding decision).
///
/// At [`resolve`](super::resolve) each variant becomes the canonical postcard `Params`
/// bytes that cross `fsw_create` (dl) or the KDL node a `FromKdlNode` factory re-parses
/// (static):
///
/// - [`None`](ParamSource::None) — a paramless system (config-less KDL, or a builder
///   system with no `.params(..)`): empty postcard bytes / a minimal synthesized node.
/// - [`Postcard`](ParamSource::Postcard) — the typed Rust [`WiringBuilder::params`](super::SystemSpecBuilder::params)
///   path: a `Params` value already postcard-encoded to its canonical bytes. For a dl
///   system these are exactly the bytes `fsw_create` decodes.
/// - [`Kdl`](ParamSource::Kdl) — the **KDL `system` node's source text**, carried verbatim
///   so [`resolve`](super::resolve) can re-decode it: a **static** system re-parses it via
///   [`FromKdlNode`](super::FromKdlNode) (the host links its `Params`); a **dl** system
///   schema-encodes it against the `.so`'s exported `Params` schema (Wave 3b — the host
///   stays schema-agnostic), producing the **same** bytes the [`Postcard`](ParamSource::Postcard)
///   path produces. Which decoder runs is chosen by [`SystemSpec::artifact`], not by the variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamSource {
    /// No params: empty postcard bytes (dl) / a minimal synthesized node (static).
    None,
    /// Canonical postcard `Params` bytes (the typed Rust builder path).
    Postcard(Vec<u8>),
    /// The KDL `system` node's source text, re-decoded at resolve (static: `FromKdlNode`;
    /// dl: schema-guided postcard encode).
    Kdl(String),
}

impl ParamSource {
    /// `true` for [`None`](ParamSource::None) — a paramless system.
    pub fn is_none(&self) -> bool {
        matches!(self, ParamSource::None)
    }
}

/// One producer → consumer edge, the serializable mirror of the old `wiring.rs` `Edge`
/// (dl-open.md §6.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    /// Producer instance name.
    pub from: String,
    /// Producer output port frame name.
    pub out: String,
    /// Consumer instance name.
    pub to: String,
    /// Consumer input port frame name.
    pub in_: String,
    /// `true` ⇒ a one-cycle-delayed feedback back-edge (`connect_delayed`).
    pub delayed: bool,
}

/// The telemetry downlink, the serializable mirror of
/// [`TelemetryConfig`](crate::TelemetryConfig) (v1: TCP only) plus its
/// [`TelemetryMode`](crate::TelemetryMode) (dl-open.md §6.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySpec {
    /// The TCP downlink address.
    pub addr: SocketAddr,
    /// Which outputs to tap.
    pub mode: TelemetryModeSpec,
}

/// Which outputs the telemetry sink taps — the serializable mirror of
/// [`TelemetryMode`](crate::TelemetryMode).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TelemetryModeSpec {
    /// Tap every registry entry.
    All,
    /// Tap only entries matching one of the instance/frame lists.
    Subset {
        /// Instance names to tap.
        instances: Vec<String>,
        /// Frame names to tap.
        frames: Vec<String>,
    },
}
