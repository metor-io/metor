//! Fluent construction of a [`Wiring`] from Rust.
//!
//! [`WiringBuilder`] is the Rust-native counterpart to the Python front-end.
//! Both produce the same [`Wiring`] value, so anything a `target.py` can
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
//!     .system("nav").ty("Nav").end()
//!     .connect("plant", "sensors", "nav", "sensors")
//!     .serve("127.0.0.1:2240".parse().unwrap())
//!     .build();
//! ```
//!
//! The builder is deliberately a thin recorder. The resolver applies
//! [`validate`](super::validate::validate) to Rust-built and Python-built IR
//! alike, so semantic rules have one implementation and one error surface.
//!
//! [`system`](WiringBuilder::system) builds a [`SystemSpec`] directly. The same
//! builder covers static and artifact-backed systems; the resolver rejects
//! invalid combinations before loading or binding anything.

use std::net::SocketAddr;

use serde::Serialize;

use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, IR_VERSION,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, StateSpec, SystemSpec, Wiring,
};

/// Wraps the [`Wiring`] under construction, exposing a fluent surface over its
/// coordinator, artifacts, systems, slots, and edges. Systems and slots are
/// declared through [`SystemSpecBuilder`] and [`SlotSpecBuilder`] sub-builders;
/// everything else chains directly on this type.
pub struct WiringBuilder {
    wiring: Wiring,
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
            wiring: Wiring {
                ir_version: IR_VERSION,
                coordinator: CoordinatorSpec {
                    cycle_rate: 100.0,
                    default_depth: None,
                    clock: ClockSpec::Wall,
                    namespace: None,
                    wasm_fuel_per_poll: None,
                    wasm_memory_limit_bytes: None,
                },
                artifacts: Vec::new(),
                states: Vec::new(),
                systems: Vec::new(),
                slots: Vec::new(),
                edges: Vec::new(),
                scopes: Vec::new(),
            },
        }
    }

    /// Sets the cycle rate and clock, leaving `default_depth` untouched. Use
    /// [`coordinator_spec`](Self::coordinator_spec) to set everything at once.
    pub fn coordinator(mut self, cycle_rate: f64, clock: ClockSpec) -> Self {
        self.wiring.coordinator.cycle_rate = cycle_rate;
        self.wiring.coordinator.clock = clock;
        self
    }

    /// Sets the full [`CoordinatorSpec`], including `default_depth`.
    pub fn coordinator_spec(mut self, spec: CoordinatorSpec) -> Self {
        self.wiring.coordinator = spec;
        self
    }

    /// Declares a loadable [`Artifact`], one pack (any number of system
    /// types) per cdylib.
    ///
    /// `lib_stem` is the bare library stem (`adcs_plant`), decorated into a
    /// per-triple file name (`libadcs_plant.dylib`, `libadcs_plant.so`, or
    /// `adcs_plant.dll`) when the artifact is provisioned. A `system` spec's
    /// `ty` selects the pack entry. The artifact's `path` starts out unset;
    /// the build driver ([`provision_artifacts`](super::provision_artifacts)) fills it
    /// in.
    ///
    /// Declare a WebAssembly artifact at `path`.
    ///
    /// Unlike [`artifact`](Self::artifact) there is no crate or library stem
    /// to build per triple: a `.wasm` is one arch-neutral file, which is the
    /// property that makes it cheap to uplink.
    pub fn wasm_artifact(
        mut self,
        id: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.wiring.artifacts.push(Artifact {
            id: id.into(),
            kind: crate::ir::ArtifactKind::Wasm,
            crate_name: String::new(),
            lib: String::new(),
            path: Some(path.into()),
            prebuilt_dir: None,
            dist: None,
            manifest_hash: None,
            src: None,
        });
        self
    }

    pub fn artifact(
        mut self,
        id: impl Into<String>,
        crate_name: impl Into<String>,
        lib_stem: impl AsRef<str>,
    ) -> Self {
        self.wiring.artifacts.push(Artifact {
            id: id.into(),
            kind: crate::ir::ArtifactKind::Cdylib,
            crate_name: crate_name.into(),
            lib: lib_stem.as_ref().to_string(),
            path: None,
            prebuilt_dir: None,
            dist: None,
            manifest_hash: None,
            src: None,
        });
        self
    }

    /// Begins a system instance named `name`.
    pub fn system(self, name: impl Into<String>) -> SystemSpecBuilder {
        SystemSpecBuilder {
            parent: self,
            spec: SystemSpec {
                name: name.into(),
                ty: None,
                artifact: None,
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
                attach: None,
            },
        }
    }

    /// Adds a forward frame edge from `from`'s `out` port to `to`'s `in_`
    /// port. Forward edges must form an acyclic graph.
    pub fn connect(
        self,
        from: impl Into<String>,
        out: impl Into<String>,
        to: impl Into<String>,
        in_: impl Into<String>,
    ) -> Self {
        self.push_frame_edge(from, out, to, in_, false)
    }

    /// Adds a frame edge whose value arrives one cycle late. Delayed edges
    /// close feedback loops and are excluded from cycle detection.
    pub fn connect_delayed(
        self,
        from: impl Into<String>,
        out: impl Into<String>,
        to: impl Into<String>,
        in_: impl Into<String>,
    ) -> Self {
        self.push_frame_edge(from, out, to, in_, true)
    }

    fn push_frame_edge(
        mut self,
        from: impl Into<String>,
        out: impl Into<String>,
        to: impl Into<String>,
        in_: impl Into<String>,
        delayed: bool,
    ) -> Self {
        self.wiring.edges.push(EdgeSpec {
            from: from.into(),
            out: out.into(),
            to: to.into(),
            in_: in_.into(),
            delayed,
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
        self.wiring.edges.push(EdgeSpec {
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

    /// Declares a pack-shared state instance: `ty` is the key a registered
    /// pack declared via [`Pack::shared_state`](crate::Pack::shared_state),
    /// and `params` is its construction value tree. Panics on a duplicate
    /// state name or type, the builder's insert-time twin of validate.
    pub fn state(self, name: impl Into<String>, ty: impl Into<String>) -> Self {
        self.state_value(name, ty, serde_json::Value::Object(serde_json::Map::new()))
    }

    /// [`state`](Self::state) with an explicit params value tree.
    pub fn state_value(
        mut self,
        name: impl Into<String>,
        ty: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        let name = name.into();
        let ty = ty.into();
        assert!(
            !self
                .wiring
                .states
                .iter()
                .any(|s| s.name == name || s.ty == ty),
            "state `{name}` (type `{ty}`) is already declared"
        );
        self.wiring.states.push(StateSpec {
            name,
            ty,
            params: ParamSource::Value(params),
            src: None,
        });
        self
    }

    /// Serves the telemetry link: declares the built-in `TcpServer` state
    /// under the name `"link"` listening on `addr`, plus an all-taps
    /// downlink under the instance name `"telemetry"`. For a subset tap,
    /// declare an ordinary system with the `Downlink` type instead.
    /// Resolving requires a registry seeded from
    /// [`Registry::with_builtins`](super::Registry::with_builtins).
    pub fn serve(mut self, addr: SocketAddr) -> Self {
        self.wiring.states.push(StateSpec::tcp_server("link", addr));
        self.push_system(SystemSpec::downlink("telemetry"));
        self
    }

    /// [`serve`](Self::serve) with a human node name advertised over mDNS.
    /// An unnamed link falls back to the OS hostname at advertise time.
    pub fn serve_named(mut self, name: &str, addr: SocketAddr) -> Self {
        self.wiring
            .states
            .push(StateSpec::tcp_server_named("link", addr, Some(name)));
        self.push_system(SystemSpec::downlink("telemetry"));
        self
    }

    /// Adds the built-in command uplink under the instance name `"uplink"`,
    /// relaying the named msgs off the [`serve`](Self::serve) link. Declare
    /// it before its consumers and a command is consumed the same cycle it
    /// is republished; route its commands onward with explicit message
    /// edges.
    pub fn uplink<'a>(mut self, msgs: impl IntoIterator<Item = &'a str>) -> Self {
        let msgs: Vec<&str> = msgs.into_iter().collect();
        self.push_system(SystemSpec::uplink("uplink", &msgs));
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
        self.wiring.slots.push(spec);
        self
    }

    /// Finishes the [`Wiring`].
    pub fn build(self) -> Wiring {
        self.wiring
    }

    /// Push a built-in system spec.
    fn push_system(&mut self, spec: SystemSpec) {
        self.wiring.systems.push(spec);
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
    pub fn allow(self, occupant: impl Into<String>) -> Self {
        self.push_allowed(occupant.into(), None, ParamSource::None)
    }

    /// Allows the named pack entry from a specific artifact. Panics if
    /// `artifact` names no declared artifact.
    pub fn allow_from(self, occupant: impl Into<String>, artifact: impl Into<String>) -> Self {
        self.push_allowed(occupant.into(), Some(artifact.into()), ParamSource::None)
    }

    /// Allows the named occupant with postcard-encoded typed default params.
    pub fn allow_with_params<P: Serialize>(self, occupant: impl Into<String>, params: P) -> Self {
        let bytes = postcard::to_allocvec(&params)
            .expect("params postcard-encode (Serialize is infallible)");
        self.push_allowed(occupant.into(), None, ParamSource::Postcard(bytes))
    }

    /// Allows the named occupant with default params as a value tree.
    pub fn allow_with_value(self, occupant: impl Into<String>, value: serde_json::Value) -> Self {
        self.push_allowed(occupant.into(), None, ParamSource::Value(value))
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
    /// per `Load`, instead of dlopen'ing occupants into the coordinator.
    /// Per-slot means all-occupants; resolve
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
        self.parent.wiring.slots.push(self.spec);
        self.parent
    }

    fn push_allowed(
        mut self,
        occupant: String,
        artifact: Option<String>,
        params: ParamSource,
    ) -> Self {
        self.spec.allow.push(AllowedOccupantSpec {
            occupant,
            artifact,
            params,
            src: None,
        });
        self
    }
}

/// Builds one static or artifact-backed [`SystemSpec`].
pub struct SystemSpecBuilder {
    parent: WiringBuilder,
    spec: SystemSpec,
}

impl SystemSpecBuilder {
    /// Sets the registry type or pack entry name.
    pub fn ty(mut self, ty: impl Into<String>) -> Self {
        self.spec.ty = Some(ty.into());
        self
    }

    /// Sets params as a value tree.
    pub fn params_value(mut self, value: serde_json::Value) -> Self {
        self.spec.params = ParamSource::Value(value);
        self
    }

    /// Loads this system from the named [`Artifact`].
    pub fn from_artifact(mut self, artifact_id: impl Into<String>) -> Self {
        self.spec.artifact = Some(artifact_id.into());
        self
    }

    /// Runs an artifact-backed system in a worker process.
    pub fn process(mut self) -> Self {
        self.spec.process = true;
        self
    }

    /// Sets postcard-encoded typed params for an artifact-backed system.
    pub fn params<P: Serialize>(mut self, params: P) -> Self {
        let bytes = postcard::to_allocvec(&params)
            .expect("params postcard-encode (Serialize is infallible)");
        self.spec.params = ParamSource::Postcard(bytes);
        self
    }

    /// Attaches this system to the pack-shared state declared under `state`
    /// ([`WiringBuilder::state`]). Required for a shared-state system type
    /// (the built-in `Downlink`/`Uplink`), rejected on any other.
    pub fn attach(mut self, state: impl Into<String>) -> Self {
        self.spec.attach = Some(state.into());
        self
    }

    /// Adds the system and returns the parent builder.
    pub fn end(mut self) -> WiringBuilder {
        self.parent.wiring.systems.push(self.spec);
        self.parent
    }
}
