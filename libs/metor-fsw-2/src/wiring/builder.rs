//! Fluent construction of a [`Wiring`] from Rust.
//!
//! [`WiringBuilder`] is the Rust-native counterpart to the Python front-end.
//! Both produce the same [`Wiring`] value, so anything a `mission.py` can
//! declare can be declared here instead.
//!
//! ```no_run
//! # use metor_fsw_2::{WiringBuilder, ClockSpec};
//! # #[derive(serde::Serialize)] struct PlantParams { init_angle: f64 }
//! let wiring = WiringBuilder::new()
//!     .coordinator(120.0, ClockSpec::Simulated { dt_secs: 1.0 / 120.0 })
//!     .artifact("plant", "adcs-plant", "adcs_plant")
//!     .system("plant").ty("Plant").from_artifact("plant")
//!         .params(PlantParams { init_angle: 0.5 }).end()
//!     .system("nav").ty("Nav").from_static().end()
//!     .connect("plant", "sensors", "nav", "sensors")
//!     .telemetry("127.0.0.1:2240".parse().unwrap())
//!     .build();
//! ```
//!
//! Typed params are postcard-encoded into [`ParamSource::Postcard`] bytes.
//! The encoding is byte-identical to what the Python front-end's value tree
//! schema-encodes to for the same logical value, and it is exactly what a
//! loaded library's `fsw_create` decodes. A system without params carries
//! [`ParamSource::None`].

use std::net::SocketAddr;

use serde::Serialize;

use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, IR_VERSION,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, SystemSpec, Wiring,
};

/// Accumulates coordinator settings, artifact declarations, systems, slots,
/// and edges, then assembles them into a [`Wiring`] with
/// [`build`](Self::build). Systems and slots are declared through the
/// [`SystemSpecBuilder`] and [`SlotSpecBuilder`] sub-builders; everything
/// else chains directly on this type.
pub struct WiringBuilder {
    coordinator: CoordinatorSpec,
    artifacts: Vec<Artifact>,
    systems: Vec<SystemSpec>,
    slots: Vec<SlotSpec>,
    edges: Vec<EdgeSpec>,
}

impl Default for WiringBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WiringBuilder {
    /// Creates an empty builder with a 100 Hz, wall-clock coordinator.
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
        }
    }

    /// Sets the cycle rate and clock, leaving `default_depth` untouched. Use
    /// [`coordinator_spec`](Self::coordinator_spec) to set everything at once.
    pub fn coordinator(mut self, cycle_rate: f64, clock: ClockSpec) -> Self {
        self.coordinator = CoordinatorSpec {
            cycle_rate,
            default_depth: self.coordinator.default_depth,
            clock,
        };
        self
    }

    /// Sets the full [`CoordinatorSpec`], including `default_depth`.
    pub fn coordinator_spec(mut self, spec: CoordinatorSpec) -> Self {
        self.coordinator = spec;
        self
    }

    /// Declares a loadable [`Artifact`], one pack (any number of system
    /// types) per cdylib.
    ///
    /// `lib_stem` is the bare library stem (`adcs_plant`), decorated into the
    /// platform's file name (`libadcs_plant.dylib`, `libadcs_plant.so`, or
    /// `adcs_plant.dll`) via [`cdylib_file_name`](super::cdylib_file_name).
    /// A `system` spec's `ty` selects the pack entry. The artifact's `path`
    /// starts out unset; the build driver
    /// ([`build_artifacts`](super::build_artifacts)) fills it in.
    pub fn artifact(
        mut self,
        id: impl Into<String>,
        crate_name: impl Into<String>,
        lib_stem: impl AsRef<str>,
    ) -> Self {
        self.artifacts.push(Artifact {
            id: id.into(),
            crate_name: crate_name.into(),
            cdylib: super::cdylib_file_name(lib_stem.as_ref()),
            path: None,
            manifest_hash: None,
            src: None,
        });
        self
    }

    /// Begins a system instance named `name`, returning a [`SystemSpecBuilder`]
    /// that flows back into this builder through [`SystemSpecBuilder::end`].
    pub fn system(self, name: impl Into<String>) -> SystemSpecBuilder {
        SystemSpecBuilder {
            parent: self,
            name: name.into(),
            ty: None,
            artifact: None,
            params: ParamSource::None,
            process: false,
        }
    }

    /// Adds a forward frame edge from `from`'s `out` port to `to`'s `in_`
    /// port. Forward edges must form an acyclic graph.
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
            src: None,
        });
        self
    }

    /// Adds a frame edge whose value arrives one cycle late. Delayed edges
    /// close feedback loops and are excluded from cycle detection.
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
            src: None,
        });
        self
    }

    /// Routes the message type `msg` from producer `from` to consumer `to`.
    /// Both endpoints carry the same message name, message edges may fan in
    /// and out freely, and they are excluded from cycle detection.
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
            src: None,
        });
        self
    }

    /// Adds the built-in TCP telemetry downlink under the instance name
    /// `"telemetry"`, tapping every output. For a subset tap or a second
    /// downlink, declare an ordinary system with the `TcpDownlink` type
    /// instead. Resolving either requires a registry seeded from
    /// [`Registry::with_builtins`](super::Registry::with_builtins).
    pub fn telemetry(mut self, addr: SocketAddr) -> Self {
        self.systems
            .push(SystemSpec::tcp_downlink("telemetry", addr));
        self
    }

    /// Adds the built-in TCP command uplink under the instance name
    /// `"uplink"`. It reads command messages off its own connection to
    /// `addr`, separate from the downlink's; route its commands onward with
    /// explicit message edges.
    pub fn uplink(mut self, addr: SocketAddr) -> Self {
        self.systems.push(SystemSpec::tcp_uplink("uplink", addr));
        self
    }

    /// Begins a runtime-loadable slot named `name`, returning a
    /// [`SlotSpecBuilder`] that flows back into this builder through
    /// [`SlotSpecBuilder::end`].
    pub fn slot(self, name: impl Into<String>) -> SlotSpecBuilder {
        SlotSpecBuilder {
            parent: self,
            spec: SlotSpec {
                name: name.into(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                allow: Vec::new(),
                initial: None,
                process: false,
                src: None,
                scope: None,
            },
        }
    }

    /// Pushes a fully built [`SlotSpec`], for slots assembled outside the
    /// fluent chain.
    pub fn add_slot_spec(mut self, spec: SlotSpec) -> Self {
        self.slots.push(spec);
        self
    }

    /// Finishes the [`Wiring`].
    pub fn build(self) -> Wiring {
        Wiring {
            ir_version: IR_VERSION,
            coordinator: self.coordinator,
            artifacts: self.artifacts,
            systems: self.systems,
            slots: self.slots,
            edges: self.edges,
            scopes: Vec::new(),
        }
    }
}

/// Declares one runtime-loadable slot as a [`SlotSpec`], covering its port
/// contract, allowed occupants, and startup occupant. Returned by
/// [`WiringBuilder::slot`]; it owns the parent builder, so the chain flows
/// back through [`end`](Self::end).
pub struct SlotSpecBuilder {
    parent: WiringBuilder,
    spec: SlotSpec,
}

impl SlotSpecBuilder {
    /// Allows the named pack entry to load into this slot, with no default
    /// params; resolve searches every artifact for a unique entry of that
    /// name. Use [`allow_from`](Self::allow_from) to name the artifact and
    /// [`allow_with_params`](Self::allow_with_params) to attach typed
    /// defaults.
    pub fn allow(mut self, occupant: impl Into<String>) -> Self {
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            artifact: None,
            params: ParamSource::None,
            src: None,
        });
        self
    }

    /// Allows the named pack entry from a specific artifact.
    pub fn allow_from(
        mut self,
        occupant: impl Into<String>,
        artifact: impl Into<String>,
    ) -> Self {
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            artifact: Some(artifact.into()),
            params: ParamSource::None,
            src: None,
        });
        self
    }

    /// Allows the named occupant with typed default params, postcard-encoded
    /// the same way as [`SystemSpecBuilder::params`].
    pub fn allow_with_params<P: Serialize>(
        mut self,
        occupant: impl Into<String>,
        params: P,
    ) -> Self {
        let bytes = postcard::to_allocvec(&params)
            .expect("params postcard-encode (Serialize is infallible)");
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            artifact: None,
            params: ParamSource::Postcard(bytes),
            src: None,
        });
        self
    }

    /// Allows the named occupant with default params as a value tree, the
    /// [`SystemSpecBuilder::params_value`] twin of
    /// [`allow_with_params`](Self::allow_with_params).
    pub fn allow_with_value(
        mut self,
        occupant: impl Into<String>,
        value: serde_json::Value,
    ) -> Self {
        self.spec.allow.push(AllowedOccupantSpec {
            occupant: occupant.into(),
            artifact: None,
            params: ParamSource::Value(value),
            src: None,
        });
        self
    }

    /// Declares an input frame in the slot's port contract. Every occupant is
    /// validated against the contract when it loads.
    pub fn input(mut self, frame: impl Into<String>) -> Self {
        self.spec.inputs.push(frame.into());
        self
    }

    /// Declares an output frame in the slot's port contract.
    pub fn output(mut self, frame: impl Into<String>) -> Self {
        self.spec.outputs.push(frame.into());
        self
    }

    /// Runs every occupant of this slot in its own worker process, spawned
    /// per `Load`, instead of dlopen'ing occupants into the coordinator — the
    /// builder twin of `process=#true` on a `slot` node
    /// (`docs/process-slots.md`). Per-slot means all-occupants; resolve
    /// describes each allowed occupant through a worker, so the host never
    /// loads their artifacts.
    pub fn process(mut self) -> Self {
        self.spec.process = true;
        self
    }

    /// Sets the occupant loaded at startup and its [`SlotInitState`].
    pub fn initial(mut self, occupant: impl Into<String>, state: SlotInitState) -> Self {
        self.spec.initial = Some(InitialOccupantSpec {
            occupant: occupant.into(),
            state,
        });
        self
    }

    /// Pushes this slot onto the [`Wiring`] and returns the parent builder.
    pub fn end(mut self) -> WiringBuilder {
        self.parent.slots.push(self.spec);
        self.parent
    }
}

/// Declares one system instance as a [`SystemSpec`], covering its type, how
/// it resolves (statically or from a loaded artifact), and its params.
/// Returned by [`WiringBuilder::system`]; it owns the parent builder, so the
/// chain flows back through [`end`](Self::end).
pub struct SystemSpecBuilder {
    parent: WiringBuilder,
    name: String,
    ty: Option<String>,
    artifact: Option<String>,
    params: ParamSource,
    process: bool,
}

impl SystemSpecBuilder {
    /// Sets the system type. A [`from_static`](Self::from_static) system
    /// requires it as the registry key. For a
    /// [`from_artifact`](Self::from_artifact) system it is optional, since
    /// the artifact's `system_type` is authoritative; when given, resolve
    /// checks the two agree.
    pub fn ty(mut self, ty: impl Into<String>) -> Self {
        self.ty = Some(ty.into());
        self
    }

    /// Resolves this system by loading the named [`Artifact`] at run time.
    pub fn from_artifact(mut self, artifact_id: impl Into<String>) -> Self {
        self.artifact = Some(artifact_id.into());
        self
    }

    /// Runs this [`from_artifact`](Self::from_artifact) system in its own
    /// worker process instead of dlopen'ing it in-process — the builder twin
    /// of `process=#true` (`docs/process-systems.md`). Resolve rejects the
    /// toggle on a system without an artifact.
    pub fn process(mut self) -> Self {
        self.process = true;
        self
    }

    /// Resolves this system statically through the
    /// [`Registry`](super::Registry). This is the default when neither
    /// `from_*` is called.
    pub fn from_static(mut self) -> Self {
        self.artifact = None;
        self
    }

    /// Sets the typed params, postcard-encoded into [`ParamSource::Postcard`]
    /// bytes. These are exactly the bytes the loaded library's `fsw_create`
    /// decodes, and match what the Python front-end's value tree encodes to
    /// for the same value.
    ///
    /// Only a [`from_artifact`](Self::from_artifact) system accepts them. A
    /// static system takes its params through its registered
    /// value-tree-deserializing factory and has no postcard decode path, so resolve
    /// rejects the combination with
    /// [`LoadError::StaticPostcardParams`](super::LoadError::StaticPostcardParams)
    /// rather than silently running the system on defaults.
    pub fn params<P: Serialize>(mut self, params: P) -> Self {
        let bytes = postcard::to_allocvec(&params)
            .expect("params postcard-encode (Serialize is infallible)");
        self.params = ParamSource::Postcard(bytes);
        self
    }

    /// Sets the params as a value tree ([`ParamSource::Value`]), which both
    /// system kinds accept: a static system serde-deserializes it (field
    /// defaults honored), a loaded one schema-encodes it to the same postcard
    /// bytes [`params`](Self::params) carries.
    pub fn params_value(mut self, value: serde_json::Value) -> Self {
        self.params = ParamSource::Value(value);
        self
    }

    /// Pushes this system onto the [`Wiring`] and returns the parent builder.
    pub fn end(mut self) -> WiringBuilder {
        self.parent.systems.push(SystemSpec {
            name: self.name,
            ty: self.ty,
            artifact: self.artifact,
            params: self.params,
            process: self.process,
            src: None,
            scope: None,
        });
        self.parent
    }
}
