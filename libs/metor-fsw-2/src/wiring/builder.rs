//! The [`WiringBuilder`] — a fluent Rust front-end onto the [`Wiring`] data model.
//!
//! Everything KDL expresses, Rust expresses, because both produce the same [`Wiring`]:
//!
//! ```no_run
//! # use metor_fsw_2::{WiringBuilder, ClockSpec, TelemetryModeSpec};
//! # #[derive(serde::Serialize)] struct PlantParams { init_angle: f64 }
//! let wiring = WiringBuilder::new()
//!     .coordinator(120.0, ClockSpec::Simulated { dt_secs: 1.0 / 120.0 })
//!     .artifact("plant", "adcs-plant", "adcs_plant", "Plant")
//!     .system("plant").ty("Plant").from_artifact("plant")
//!         .params(PlantParams { init_angle: 0.5 }).end()
//!     .system("nav").ty("Nav").from_static().end()
//!     .connect("plant", "sensors", "nav", "sensors")
//!     .telemetry("127.0.0.1:2240".parse().unwrap(), TelemetryModeSpec::All)
//!     .build();
//! ```
//!
//! [`SystemSpecBuilder::params`] postcard-encodes a typed `Params` into the canonical
//! [`ParamSource::Postcard`] bytes — byte-identical to what the KDL front-end's
//! schema-guided encoder produces for a dl system, and what the `.so`'s `fsw_create`
//! decodes. A paramless system gets [`ParamSource::None`].

use std::net::SocketAddr;

use serde::Serialize;

use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, SystemSpec, TelemetryModeSpec,
    TelemetrySpec, Wiring,
};

/// Fluent constructor for a [`Wiring`]. Start with [`new`](Self::new), set the
/// coordinator, declare artifacts, add systems (via the per-system
/// [`SystemSpecBuilder`]), wire edges, optionally add telemetry, and [`build`](Self::build).
pub struct WiringBuilder {
    coordinator: CoordinatorSpec,
    artifacts: Vec<Artifact>,
    systems: Vec<SystemSpec>,
    slots: Vec<SlotSpec>,
    edges: Vec<EdgeSpec>,
    telemetry: Option<TelemetrySpec>,
    uplink: Option<SocketAddr>,
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
            slots: Vec::new(),
            edges: Vec::new(),
            telemetry: None,
            uplink: None,
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

    /// Declare a loadable [`Artifact`] (one system type per cdylib). `lib_stem` is the
    /// library **stem** (`adcs_plant`); the framework decorates it to the platform's
    /// produced file name (`libadcs_plant.dylib`/`.so` / `adcs_plant.dll`) via
    /// [`cdylib_file_name`](super::cdylib_file_name), matching the KDL `lib=` surface.
    /// `system_type` is the `type=` this `.so` exports. Its `path` is filled by the
    /// build driver ([`build_artifacts`](super::build_artifacts)).
    pub fn artifact(
        mut self,
        id: impl Into<String>,
        crate_name: impl Into<String>,
        lib_stem: impl AsRef<str>,
        system_type: impl Into<String>,
    ) -> Self {
        self.artifacts.push(Artifact {
            id: id.into(),
            crate_name: crate_name.into(),
            cdylib: super::cdylib_file_name(lib_stem.as_ref()),
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
            params: ParamSource::None,
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
            kind: EdgeKind::Frame,
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
            kind: EdgeKind::Frame,
        });
        self
    }

    /// Add a **message** edge (`connect_msg`, `docs/message-wiring.md` §3): route the message
    /// type `msg` from producer `from` to consumer `to`. Both endpoints carry the same message
    /// type (id-equality); message edges are many-to-many and excluded from cycle detection.
    pub fn connect_msg(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        msg: impl Into<String>,
    ) -> Self {
        let msg = msg.into();
        self.edges.push(EdgeSpec {
            from: from.into(),
            out: msg.clone(),
            to: to.into(),
            in_: msg,
            delayed: false,
            kind: EdgeKind::Msg,
        });
        self
    }

    /// Add the telemetry downlink (TCP `addr`, tapping `mode`).
    pub fn telemetry(mut self, addr: SocketAddr, mode: TelemetryModeSpec) -> Self {
        self.telemetry = Some(TelemetrySpec { addr, mode });
        self
    }

    /// Add the command uplink (`docs/messages.md` §4.4): an [`UplinkSystem`](crate::UplinkSystem)
    /// reading panel `SequenceCommand`s off its **own** TCP connection to `addr`, separate from
    /// the telemetry downlink's connection (shared connection is deferred, §4.5).
    pub fn uplink(mut self, addr: SocketAddr) -> Self {
        self.uplink = Some(addr);
        self
    }

    /// Begin a runtime-loadable **slot** named `name`; finish it with
    /// [`SlotSpecBuilder::end`]. Declare its allowed occupants
    /// ([`allow`](SlotSpecBuilder::allow)), its `input`/`output` contract
    /// ([`input`](SlotSpecBuilder::input)/[`output`](SlotSpecBuilder::output)), and an
    /// optional [`initial`](SlotSpecBuilder::initial) occupant before `end` — the Rust
    /// mirror of the KDL `slot` node (sequences-slots.md §5).
    pub fn slot(self, name: impl Into<String>) -> SlotSpecBuilder {
        SlotSpecBuilder {
            parent: self,
            spec: SlotSpec {
                name: name.into(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                allow: Vec::new(),
                initial: None,
            },
        }
    }

    /// Push a fully-built [`SlotSpec`] (the escape hatch for a programmatically-assembled
    /// slot; [`slot`](Self::slot) is the fluent path).
    pub fn add_slot_spec(mut self, spec: SlotSpec) -> Self {
        self.slots.push(spec);
        self
    }

    /// Finish the [`Wiring`].
    pub fn build(self) -> Wiring {
        Wiring {
            coordinator: self.coordinator,
            artifacts: self.artifacts,
            systems: self.systems,
            slots: self.slots,
            edges: self.edges,
            telemetry: self.telemetry,
            uplink: self.uplink,
        }
    }
}

/// The per-slot fluent sub-builder returned by [`WiringBuilder::slot`]; owns the parent
/// builder so the chain flows back through [`end`](Self::end). The Rust mirror of the KDL
/// `slot` node.
pub struct SlotSpecBuilder {
    parent: WiringBuilder,
    spec: SlotSpec,
}

impl SlotSpecBuilder {
    /// Declare an **allowed** occupant by artifact id (the `Load` name, 1:1 for v1), with
    /// no default params ([`ParamSource::None`]). Use
    /// [`allow_with_params`](Self::allow_with_params) for typed default params.
    pub fn allow(mut self, occupant: impl Into<String>) -> Self {
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            params: ParamSource::None,
        });
        self
    }

    /// Declare an allowed occupant carrying typed default params, postcard-encoded into the
    /// canonical [`ParamSource::Postcard`] bytes (byte-identical to the KDL schema-guided
    /// encode — the same equivalence as [`SystemSpecBuilder::params`]).
    pub fn allow_with_params<P: Serialize>(
        mut self,
        occupant: impl Into<String>,
        params: P,
    ) -> Self {
        let bytes = postcard::to_allocvec(&params)
            .expect("params postcard-encode (Serialize is infallible)");
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            params: ParamSource::Postcard(bytes),
        });
        self
    }

    /// Declare an `input` user-port frame name (the validated contract).
    pub fn input(mut self, frame: impl Into<String>) -> Self {
        self.spec.inputs.push(frame.into());
        self
    }

    /// Declare an `output` user-port frame name (the validated contract).
    pub fn output(mut self, frame: impl Into<String>) -> Self {
        self.spec.outputs.push(frame.into());
        self
    }

    /// Set the startup occupant and its [`SlotInitState`].
    pub fn initial(mut self, occupant: impl Into<String>, state: SlotInitState) -> Self {
        self.spec.initial = Some(InitialOccupantSpec {
            occupant: occupant.into(),
            state,
        });
        self
    }

    /// Finish this slot, push it onto the [`Wiring`], and return the parent builder.
    pub fn end(mut self) -> WiringBuilder {
        self.parent.slots.push(self.spec);
        self.parent
    }
}

/// The per-system fluent sub-builder returned by [`WiringBuilder::system`]; owns the
/// parent builder so the chain flows back through [`end`](Self::end).
pub struct SystemSpecBuilder {
    parent: WiringBuilder,
    name: String,
    ty: String,
    artifact: Option<String>,
    params: ParamSource,
}

impl SystemSpecBuilder {
    /// Set the system `type=` key.
    pub fn ty(mut self, ty: impl Into<String>) -> Self {
        self.ty = ty.into();
        self
    }

    /// Resolve this system by `dlopen`'ing the named [`Artifact`].
    pub fn from_artifact(mut self, artifact_id: impl Into<String>) -> Self {
        self.artifact = Some(artifact_id.into());
        self
    }

    /// Resolve this system statically through the [`Registry`](super::Registry).
    /// The default if neither `from_*` is called.
    pub fn from_static(mut self) -> Self {
        self.artifact = None;
        self
    }

    /// Set the typed params, postcard-encoded into the canonical
    /// [`ParamSource::Postcard`] bytes. For a dl system these are exactly the bytes
    /// `fsw_create` decodes — **byte-identical** to what the KDL front-end's
    /// schema-guided encoder produces for the same logical value. A paramless
    /// system can omit this ([`ParamSource::None`]).
    ///
    /// **[`from_artifact`](Self::from_artifact) systems only.** A static system takes
    /// its params through its registered `FromKdlNode` factory — there is no postcard
    /// decode path, so `resolve` rejects the combination with
    /// [`LoadError::StaticPostcardParams`](super::LoadError::StaticPostcardParams)
    /// rather than silently running the system on defaults.
    pub fn params<P: Serialize>(mut self, params: P) -> Self {
        let bytes =
            postcard::to_allocvec(&params).expect("params postcard-encode (Serialize is infallible)");
        self.params = ParamSource::Postcard(bytes);
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
