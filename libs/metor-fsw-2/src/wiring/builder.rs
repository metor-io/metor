//! The [`WiringBuilder`] — a fluent Rust front-end onto the [`Wiring`] data model
//! (dl-open.md §6.2).
//!
//! Everything KDL expresses, Rust expresses, because both produce the same [`Wiring`]:
//!
//! ```no_run
//! # use metor_fsw_2::{WiringBuilder, ClockSpec, TelemetryModeSpec};
//! # #[derive(serde::Serialize)] struct PlantParams { init_angle: f64 }
//! let wiring = WiringBuilder::new()
//!     .coordinator(120.0, ClockSpec::Simulated { dt_secs: 1.0 / 120.0 })
//!     .artifact("plant", "adcs-plant", "libadcs_plant.dylib", "Plant")
//!     .system("plant").ty("Plant").from_artifact("plant")
//!         .params(PlantParams { init_angle: 0.5 }).end()
//!     .system("nav").ty("Nav").from_static().end()
//!     .connect("plant", "sensors", "nav", "sensors")
//!     .telemetry("127.0.0.1:2240".parse().unwrap(), TelemetryModeSpec::All)
//!     .build();
//! ```
//!
//! [`SystemSpecBuilder::params`] postcard-encodes a typed `Params` into the canonical
//! [`SystemSpec::params`] bytes — byte-identical to what the KDL front-end produces for
//! a dl system once Wave 3b lands the schema-guided encoder, and what the `.so`'s
//! `fsw_create` decodes (dl-open.md §6.3). A paramless system gets empty bytes.

use std::net::SocketAddr;

use serde::Serialize;

use super::model::{
    Artifact, ClockSpec, CoordinatorSpec, EdgeSpec, SystemSpec, TelemetryModeSpec, TelemetrySpec,
    Wiring,
};

/// Fluent constructor for a [`Wiring`] (dl-open.md §6.2). Start with [`new`](Self::new),
/// set the coordinator, declare artifacts, add systems (via the per-system
/// [`SystemSpecBuilder`]), wire edges, optionally add telemetry, and [`build`](Self::build).
pub struct WiringBuilder {
    coordinator: CoordinatorSpec,
    artifacts: Vec<Artifact>,
    systems: Vec<SystemSpec>,
    edges: Vec<EdgeSpec>,
    telemetry: Option<TelemetrySpec>,
}

impl Default for WiringBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WiringBuilder {
    /// An empty builder with a default coordinator (100 Hz, framework default depth,
    /// [`Wall`](ClockSpec::Wall) clock) — override with [`coordinator`](Self::coordinator).
    pub fn new() -> Self {
        Self {
            coordinator: CoordinatorSpec {
                cycle_rate: 100.0,
                default_depth: None,
                clock: ClockSpec::Wall,
            },
            artifacts: Vec::new(),
            systems: Vec::new(),
            edges: Vec::new(),
            telemetry: None,
        }
    }

    /// Set the cycle rate and clock (the common case). Use
    /// [`coordinator_spec`](Self::coordinator_spec) to also set `default_depth`.
    pub fn coordinator(mut self, cycle_rate: f64, clock: ClockSpec) -> Self {
        self.coordinator = CoordinatorSpec {
            cycle_rate,
            default_depth: self.coordinator.default_depth,
            clock,
        };
        self
    }

    /// Set the full [`CoordinatorSpec`] (including `default_depth`).
    pub fn coordinator_spec(mut self, spec: CoordinatorSpec) -> Self {
        self.coordinator = spec;
        self
    }

    /// Declare a loadable [`Artifact`] (one system type per cdylib — dl-open.md §6.1).
    /// `cdylib` is the produced file name (`libfoo.dylib`/`libfoo.so`/`foo.dll`);
    /// `system_type` is the `type=` this `.so` exports. Its `path` is filled by the
    /// build driver ([`build_artifacts`](super::build_artifacts)).
    pub fn artifact(
        mut self,
        id: impl Into<String>,
        crate_name: impl Into<String>,
        cdylib: impl Into<String>,
        system_type: impl Into<String>,
    ) -> Self {
        self.artifacts.push(Artifact {
            id: id.into(),
            crate_name: crate_name.into(),
            cdylib: cdylib.into(),
            system_type: system_type.into(),
            path: None,
        });
        self
    }

    /// Begin a system instance named `name`; finish it with
    /// [`SystemSpecBuilder::end`]. Set its type, where it resolves
    /// ([`from_static`](SystemSpecBuilder::from_static) /
    /// [`from_artifact`](SystemSpecBuilder::from_artifact)), and its
    /// [`params`](SystemSpecBuilder::params) before `end`.
    pub fn system(self, name: impl Into<String>) -> SystemSpecBuilder {
        SystemSpecBuilder {
            parent: self,
            name: name.into(),
            ty: String::new(),
            artifact: None,
            params: Vec::new(),
        }
    }

    /// Add a forward (acyclic) edge: producer `from`'s `out` port → consumer `to`'s
    /// `in_` port (by frame name).
    pub fn connect(
        mut self,
        from: impl Into<String>,
        out: impl Into<String>,
        to: impl Into<String>,
        in_: impl Into<String>,
    ) -> Self {
        self.edges.push(EdgeSpec {
            from: from.into(),
            out: out.into(),
            to: to.into(),
            in_: in_.into(),
            delayed: false,
        });
        self
    }

    /// Add a one-cycle-**delayed** feedback back-edge (`connect_delayed`): the back-edge
    /// of a control loop, excluded from cycle detection.
    pub fn connect_delayed(
        mut self,
        from: impl Into<String>,
        out: impl Into<String>,
        to: impl Into<String>,
        in_: impl Into<String>,
    ) -> Self {
        self.edges.push(EdgeSpec {
            from: from.into(),
            out: out.into(),
            to: to.into(),
            in_: in_.into(),
            delayed: true,
        });
        self
    }

    /// Add the telemetry downlink (TCP `addr`, tapping `mode`).
    pub fn telemetry(mut self, addr: SocketAddr, mode: TelemetryModeSpec) -> Self {
        self.telemetry = Some(TelemetrySpec { addr, mode });
        self
    }

    /// Finish the [`Wiring`].
    pub fn build(self) -> Wiring {
        Wiring {
            coordinator: self.coordinator,
            artifacts: self.artifacts,
            systems: self.systems,
            edges: self.edges,
            telemetry: self.telemetry,
        }
    }
}

/// The per-system fluent sub-builder returned by [`WiringBuilder::system`]; owns the
/// parent builder so the chain flows back through [`end`](Self::end).
pub struct SystemSpecBuilder {
    parent: WiringBuilder,
    name: String,
    ty: String,
    artifact: Option<String>,
    params: Vec<u8>,
}

impl SystemSpecBuilder {
    /// Set the system `type=` key.
    pub fn ty(mut self, ty: impl Into<String>) -> Self {
        self.ty = ty.into();
        self
    }

    /// Resolve this system by `dlopen`'ing the named [`Artifact`] (dl-open.md §6.1).
    pub fn from_artifact(mut self, artifact_id: impl Into<String>) -> Self {
        self.artifact = Some(artifact_id.into());
        self
    }

    /// Resolve this system statically through the [`Registry`](super::Registry) (the
    /// WP6 path). The default if neither `from_*` is called.
    pub fn from_static(mut self) -> Self {
        self.artifact = None;
        self
    }

    /// Set the typed params, postcard-encoded into the canonical [`SystemSpec::params`]
    /// bytes (dl-open.md §6.3). For a dl system these are exactly the bytes
    /// `fsw_create` decodes; a paramless system can omit this (empty bytes).
    pub fn params<P: Serialize>(mut self, params: P) -> Self {
        self.params =
            postcard::to_allocvec(&params).expect("params postcard-encode (Serialize is infallible)");
        self
    }

    /// Finish this system, push it onto the [`Wiring`], and return the parent builder.
    pub fn end(mut self) -> WiringBuilder {
        self.parent.systems.push(SystemSpec {
            name: self.name,
            ty: self.ty,
            artifact: self.artifact,
            params: self.params,
        });
        self.parent
    }
}
