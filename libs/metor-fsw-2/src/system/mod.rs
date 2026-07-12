//! The traits a user's system implements, and the shims the framework wraps
//! around it.
//!
//! A system is a struct with a typed input bundle and a typed output bundle.
//! [`System`] carries the surface every system shares (the bundle types, the
//! wiring name, and the init/shutdown hooks), and a user implements exactly
//! one of two leaf traits on top of it. A [`CyclicSystem`] is driven by the
//! coordinator, which calls `execute` once per cycle. An [`AsyncSystem`] owns
//! its own `run` loop and paces itself by awaiting its inputs or a timer.
//!
//! Outputs are never handed to a system bare. The framework wraps the user's
//! bundle in [`Out`], which appends an implicit health/log port pair so that
//! `output.health()` is always available. Reporting through that handle is a
//! system's only failure channel; `execute` and `run` return nothing.
//!
//! Construction is split from wiring. [`BuildSystem`] says how a system is
//! made from a decoded params value and knows nothing about config formats,
//! so the same impl serves a statically registered system and one exported
//! from a shared library. Rings are likewise backing-erased: one
//! `impl System` binds over heap rings or over memory regions a host maps in,
//! with no backing generic anywhere.
//!
//! [`CyclicRunner`] is the framework shim between the coordinator and a
//! cyclic system. It owns the bundles between cycles, times each `execute`,
//! and publishes the standard health record per cycle.

use core::ops::{Deref, DerefMut};

use metor_fsw_ring::{NoWake, WakeSink, WakeSource};
use metor_proto::types::Timestamp;

use crate::binder::{BindPorts, RingSource};
use crate::coordinator::{CyclicSlot, SlotState};
use crate::descriptor::{PortDecl, PortDesc, SystemDescriptor, SystemKind, split_decls};
use crate::health::{HealthPort, SystemHealth, SystemLog};
use crate::message::MsgTable;
use crate::port::Output;

/// The read side of a system, a struct whose fields are
/// [`Input<F>`](crate::Input) ports. Derive with `#[derive(SystemInput)]` to
/// generate `decls` from the fields.
pub trait SystemInput {
    /// What every field contributes, in field order: the producer shape a
    /// wired port requires, or a bind-time [`Capability`]. Read before any
    /// port exists.
    fn decls() -> Vec<PortDecl>;

    /// [`decls`](Self::decls) with the capabilities filtered out, leaving the
    /// wired ports that edge validation and ring sizing consume.
    fn port_descs() -> Vec<PortDesc> {
        Self::decls()
            .into_iter()
            .filter_map(PortDecl::into_port)
            .collect()
    }
}

/// A system's output bundle: a struct of [`Output<F>`](crate::Output) ports.
/// Derive with `#[derive(SystemOutput)]`. The framework wraps it in [`Out`]
/// to add the implicit health/log ports.
pub trait SystemOutput {
    /// What every field contributes, in field order: the produced shape of
    /// each wired port, or a bind-time [`Capability`] such as the `ReceiveAll`
    /// an [`AllOutputs`](crate::AllOutputs) field requests.
    fn decls() -> Vec<PortDecl>;

    /// [`decls`](Self::decls) with the capabilities filtered out.
    fn port_descs() -> Vec<PortDesc> {
        Self::decls()
            .into_iter()
            .filter_map(PortDecl::into_port)
            .collect()
    }

    /// Sum and clear the ports' [`publish`](crate::Output::publish) drop
    /// counters. Derive-generated; the runner folds a nonzero sum into a
    /// `publish_dropped` health error each cycle. The default returns zero so
    /// a hand-written bundle that does not track drops still compiles.
    fn take_dropped(&mut self) -> u64 {
        0
    }
}

/// The framework wrapper around a system's output bundle `O`. It carries the
/// implicit per-system health/log port pair so `output.health()` is always
/// available, while `Deref`/`DerefMut` expose the user's own ports
/// (`output.<port>.write(...)`).
pub struct Out<O, WD = NoWake, WS = NoWake>
where
    WD: WakeSource,
    WS: WakeSink,
{
    ports: O,
    health: HealthPort<WD, WS>,
}

impl<O, WD, WS> Out<O, WD, WS>
where
    WD: WakeSource,
    WS: WakeSink,
{
    /// Bundle a user output struct with its framework-allocated health port.
    pub fn new(ports: O, health: HealthPort<WD, WS>) -> Self {
        Self { ports, health }
    }

    /// The handle a system reports through: `output.health().error("kind")`
    /// and `.log(level, msg)`.
    pub fn health(&mut self) -> &mut HealthPort<WD, WS> {
        &mut self.health
    }

    /// Disjoint borrows of the user ports and the health handle. `health()`
    /// borrows the whole `Out` and so conflicts with a live `&mut` to one of
    /// the user ports; generated delegation needs to lend both to the user's
    /// `execute` at once.
    pub fn split(&mut self) -> (&mut O, &mut HealthPort<WD, WS>) {
        (&mut self.ports, &mut self.health)
    }
}

impl<O, WD, WS> Deref for Out<O, WD, WS>
where
    WD: WakeSource,
    WS: WakeSink,
{
    type Target = O;
    fn deref(&self) -> &O {
        &self.ports
    }
}

impl<O, WD, WS> DerefMut for Out<O, WD, WS>
where
    WD: WakeSource,
    WS: WakeSink,
{
    fn deref_mut(&mut self) -> &mut O {
        &mut self.ports
    }
}

impl<O, WD, WS> SystemOutput for Out<O, WD, WS>
where
    O: SystemOutput,
    WD: WakeSource,
    WS: WakeSink,
{
    fn decls() -> Vec<PortDecl> {
        // The user's decls, then the implicit health/log ports every system gets.
        let mut decls = O::decls();
        decls.push(PortDecl::Port(PortDesc::of::<SystemHealth>()));
        decls.push(PortDecl::Port(PortDesc::of::<SystemLog>()));
        decls
    }

    /// Only the user bundle counts drops; the framework's own health/log
    /// ports publish through the sizing-aware path and never drop.
    fn take_dropped(&mut self) -> u64 {
        self.ports.take_dropped()
    }
}

impl<O, WD, WS> BindPorts for Out<O, WD, WS>
where
    O: BindPorts,
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind the user ports, then the two implicit health/log ports, in the
    /// same order [`decls`](SystemOutput::decls) declares them.
    fn bind<S: RingSource>(src: &mut S) -> Self {
        let ports = O::bind(src);
        let health: Output<SystemHealth, WD, WS> = Output::bind(src);
        let log: Output<SystemLog, WD, WS> = Output::bind(src);
        Out::new(ports, HealthPort::new(health, log))
    }
}

// ---------------------------------------------------------------------------
// System construction (kdl-independent)
// ---------------------------------------------------------------------------

/// How a concrete system is constructed from its typed params.
///
/// The trait carries no config-format coupling. A struct system riding a
/// pack ([`Pack::system_type`](crate::Pack::system_type)) needs only
/// `BuildSystem`; the entry's create phase decodes `Params` from postcard
/// bytes and calls [`new`](Self::new). The static registry adds only a
/// `Params: serde::de::DeserializeOwned` bound, satisfied by the same derive
/// the postcard contract already needs, so implementing `BuildSystem` alone
/// is enough to register a system either way.
pub trait BuildSystem: Sized {
    /// The value the system is constructed from. A paramless system uses
    /// `type Params = ()`.
    type Params;
    /// Construct the (pre-init) system from its decoded params.
    fn new(params: Self::Params) -> Self;

    /// Second construction phase, run where host context exists: resolve
    /// config references, for example message name tokens against the
    /// registry's [`MsgTable`]. The registry factory calls it between
    /// [`new`](Self::new) and registration. A system constructed inside a
    /// shared library never sees it, since its params are decoded in
    /// isolation and host tables are out of reach; a system that must load
    /// that way keeps its `Params` self-contained. Defaults to a no-op.
    fn configure(&mut self, _ctx: &BuildCtx) -> Result<(), ConfigureError> {
        Ok(())
    }
}

/// The host state [`BuildSystem::configure`] resolves config references
/// against, holding the tables a `Params` value cannot carry.
pub struct BuildCtx<'a> {
    /// The registered message types, keyed by
    /// [`NamedMsg::NAME`](crate::NamedMsg).
    pub msgs: &'a MsgTable,
}

/// A [`configure`](BuildSystem::configure) failure. It carries no source
/// span; the trait knows nothing about config formats, so the loader that
/// does attaches the span before surfacing the error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigureError {
    /// A configured message name is not in the host's [`MsgTable`].
    #[error("unknown msg `{name}` (registered: {})", available.join(", "))]
    UnknownMsg {
        /// The unresolved name.
        name: String,
        /// Every registered name, for the error listing.
        available: Vec<&'static str>,
    },
}

/// The surface every system shares: the typed input/output bundle types, the
/// wiring name, and the once-each lifecycle hooks. Implement one of
/// [`CyclicSystem`] or [`AsyncSystem`], both of which require this.
pub trait System {
    /// The read-only inputs this system consumes.
    type Input: SystemInput + BindPorts;
    /// The owned outputs this system produces (wrapped in [`Out`] for health).
    type Output: SystemOutput + BindPorts;

    /// The name this system wires under, and the prefix its health frames
    /// hang off.
    const NAME: &'static str;

    /// Runs once before the first `execute`/`run`. May emit initial frames or
    /// health. Defaults to a no-op.
    fn init(&mut self, _output: &mut Self::Output) {}

    /// Runs once at teardown. May flush final frames or health. Defaults to a
    /// no-op.
    fn shutdown(&mut self, _output: &mut Self::Output) {}
}

/// A coordinator-driven system. The coordinator calls
/// [`execute`](Self::execute) once per cycle; inputs are views straight into
/// upstream output buffers, and a slow consumer backpressures its producers
/// rather than losing data.
pub trait CyclicSystem: System {
    /// One unit of work: read the latest inputs, write outputs. Trouble is
    /// reported through `output.health()`, never a return value.
    ///
    /// `now` is the coordinator's timestamp for the cycle. Every system in a
    /// cycle sees the same value, and a [`Simulated`](crate::ClockMode) clock
    /// advances it, so stamp output frames with `now` rather than reading the
    /// wall clock.
    ///
    /// `input` is `&mut` because draining a ring view advances its read
    /// cursor.
    fn execute(&mut self, now: Timestamp, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring: the wired ports per
    /// direction, plus the merged capability set of both bundles.
    fn descriptor() -> SystemDescriptor {
        let (inputs, mut capabilities) = split_decls(<Self::Input as SystemInput>::decls());
        let (outputs, out_caps) = split_decls(<Self::Output as SystemOutput>::decls());
        capabilities.extend(out_caps);
        SystemDescriptor {
            name: Self::NAME,
            kind: SystemKind::Cyclic,
            inputs,
            outputs,
            capabilities,
        }
    }

    /// This instance's descriptor. Override it when the port set depends on
    /// the instance's config, such as minting one output per configured
    /// message. The builder registers this value, so a config-derived port is
    /// sized, wired, and telemetered like any static one.
    fn instance_descriptor(&self) -> SystemDescriptor {
        Self::descriptor()
    }
}

/// A self-driven system. The coordinator spawns [`run`](Self::run) once and
/// never ticks it; the system paces itself with a timer or by awaiting its
/// inputs through the ring `Notifier`.
#[allow(async_fn_in_trait)]
pub trait AsyncSystem: System {
    /// The system's own loop; returns when shutting down. It awaits inputs
    /// (`Input::recv`) or sleeps on a timer, doing its work on each wake, and
    /// uses the async output path so a full output can suspend for space.
    /// `input` is `&mut` for the same reason as [`CyclicSystem::execute`].
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring: the wired ports per
    /// direction, plus the merged capability set of both bundles.
    fn descriptor() -> SystemDescriptor {
        let (inputs, mut capabilities) = split_decls(<Self::Input as SystemInput>::decls());
        let (outputs, out_caps) = split_decls(<Self::Output as SystemOutput>::decls());
        capabilities.extend(out_caps);
        SystemDescriptor {
            name: Self::NAME,
            kind: SystemKind::Async,
            inputs,
            outputs,
            capabilities,
        }
    }

    /// This instance's descriptor. Override it when the port set depends on
    /// the instance's config, such as minting one output per configured
    /// message. The builder registers this value, so a config-derived port is
    /// sized, wired, and telemetered like any static one.
    fn instance_descriptor(&self) -> SystemDescriptor {
        Self::descriptor()
    }
}

/// Drives a [`CyclicSystem`] on behalf of the coordinator. It owns the port
/// bundles between cycles, times each `execute`, and publishes a health
/// record per cycle.
pub struct CyclicRunner<S, O>
where
    S: CyclicSystem<Output = Out<O>>,
    O: SystemOutput,
{
    system: S,
    input: S::Input,
    output: Out<O>,
    state: SlotState,
}

impl<S, O> CyclicRunner<S, O>
where
    S: CyclicSystem<Output = Out<O>>,
    O: SystemOutput,
{
    /// Assemble a runner from a constructed system and its bound port
    /// bundles.
    pub fn new(system: S, input: S::Input, output: Out<O>) -> Self {
        Self {
            system,
            input,
            output,
            state: SlotState::Running,
        }
    }

    /// Run the system's `init` once.
    pub fn init(&mut self) {
        self.system.init(&mut self.output);
    }

    /// Run one cycle: time `execute`, then publish health. Does nothing once
    /// stopped. `now` is threaded from the cycle loop so every system in a
    /// cycle shares one timestamp.
    pub fn step(&mut self, now: Timestamp) {
        if self.state.is_stopped() {
            return;
        }
        let start = std::time::Instant::now();
        self.system.execute(now, &mut self.input, &mut self.output);
        let micros = start.elapsed().as_micros() as u64;
        // Publish is infallible, so a dropped write can only surface here: a
        // nonzero sum means an undersized ring or a backpressuring reader.
        if self.output.take_dropped() > 0 {
            self.output.health().error("publish_dropped");
        }
        self.output.health().end_cycle(now, micros);
    }

    /// This slot's lifecycle state (running vs hard-stopped).
    pub fn state(&self) -> &SlotState {
        &self.state
    }

    /// Run the system's `shutdown` once.
    pub fn shutdown(&mut self) {
        self.system.shutdown(&mut self.output);
    }

    /// Borrow the output bundle, e.g. for a test to read a produced port back.
    pub fn output(&mut self) -> &mut Out<O> {
        &mut self.output
    }
}

/// Type-erased entry points, so the coordinator can hold a heterogeneous
/// `Vec<Box<dyn CyclicSlot>>`. Everything delegates to the inherent methods.
impl<S, O> CyclicSlot for CyclicRunner<S, O>
where
    S: CyclicSystem<Output = Out<O>>,
    O: SystemOutput,
{
    fn init(&mut self) {
        CyclicRunner::init(self)
    }
    fn step(&mut self, now: Timestamp) {
        CyclicRunner::step(self, now)
    }
    fn shutdown(&mut self) {
        CyclicRunner::shutdown(self)
    }
    fn name(&self) -> &'static str {
        S::NAME
    }
    fn state(&self) -> &SlotState {
        CyclicRunner::state(self)
    }
}

#[cfg(test)]
mod tests;
