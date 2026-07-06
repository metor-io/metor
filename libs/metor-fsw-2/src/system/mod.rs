//! The `System` family of traits and the framework wrapper around a system's
//! outputs (system.md §1, §3, §4).
//!
//! A shared [`System`] base carries the common surface (input/output bundle types,
//! name, lifecycle). Two leaf traits a user implements express the one structural
//! difference (system.md §3): [`CyclicSystem`] is coordinator-driven (`execute`
//! once per cycle), [`AsyncSystem`] owns its own `run` loop. The lifecycle is not
//! duplicated.

use core::ops::{Deref, DerefMut};

use metor_fsw_ring::{NoWake, WakeSink, WakeSource};
use metor_proto::types::Timestamp;

use crate::binder::{BindPorts, RingSource};
use crate::coordinator::{CyclicSlot, SlotState};
use crate::descriptor::{PortDecl, PortDesc, SystemDescriptor, SystemKind, split_decls};
use crate::health::{HealthPort, SystemHealth, SystemLog};
use crate::port::Output;

// ---------------------------------------------------------------------------
// Port bundles
// ---------------------------------------------------------------------------

/// A system's input bundle: a struct of [`Input<F>`](crate::Input) ports. Derive
/// with `#[derive(SystemInput)]` to generate `decls` from the port fields.
pub trait SystemInput {
    /// What every field contributes (system.md §5), in field order: the required
    /// producer shape of each wired port, or a bind-time [`Capability`]. Read
    /// before any port exists.
    fn decls() -> Vec<PortDecl>;

    /// The wired-port projection of [`decls`](Self::decls) (capabilities filtered
    /// out) — what edge validation and ring sizing consume.
    fn port_descs() -> Vec<PortDesc> {
        Self::decls().into_iter().filter_map(PortDecl::into_port).collect()
    }
}

/// A system's output bundle: a struct of [`Output<F>`](crate::Output) ports. Derive
/// with `#[derive(SystemOutput)]`. The framework wraps it in [`Out`] to add the
/// implicit health/log ports.
pub trait SystemOutput {
    /// What every field contributes (system.md §5), in field order: the produced
    /// shape of each wired port, or a bind-time [`Capability`] (e.g. the
    /// downlink's [`AllOutputs`](crate::AllOutputs) → `ReceiveAll`).
    fn decls() -> Vec<PortDecl>;

    /// The wired-port projection of [`decls`](Self::decls) (capabilities filtered
    /// out).
    fn port_descs() -> Vec<PortDesc> {
        Self::decls().into_iter().filter_map(PortDecl::into_port).collect()
    }

    /// Sum-and-clear the ports' [`publish`](crate::Output::publish) drop counters
    /// (review E6). Derive-generated; the runner folds a nonzero sum into a
    /// `publish_dropped` health error each cycle. The default keeps hand-written
    /// bundles (which may not track drops) compiling.
    fn take_dropped(&mut self) -> u64 {
        0
    }
}

/// The framework wrapper around a system's user output bundle `O`: it adds the
/// implicit per-system health/log port pair (system.md §4) so `output.health()`
/// is always available, while `Deref`/`DerefMut` expose the user's own ports
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
    /// Bundle a user output struct with its framework-allocated health/log port.
    pub fn new(ports: O, health: HealthPort<WD, WS>) -> Self {
        Self { ports, health }
    }

    /// The system-facing health handle: `output.health().error("kind")` /
    /// `.log(level, msg)` (system.md §4.2). The only error-reporting mechanism.
    pub fn health(&mut self) -> &mut HealthPort<WD, WS> {
        &mut self.health
    }

    /// Disjoint borrows of the user ports and the health handle (review E8a), so
    /// generated delegation (`#[system]`) can lend a `&mut` port *and* the health
    /// handle to the user `execute` at once — `health()` borrows the whole `Out`,
    /// which would conflict with `&mut out.<port>`.
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
        // The user's decls, then the two implicit health/log ports every system gets.
        let mut decls = O::decls();
        decls.push(PortDecl::Port(PortDesc::of::<SystemHealth>()));
        decls.push(PortDecl::Port(PortDesc::of::<SystemLog>()));
        decls
    }

    /// Forward to the user bundle's counters (the framework's own health/log ports
    /// publish through the sizing-aware `write_with`, so they never count drops).
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
    /// Bind the user ports, then the two implicit health/log ports — symmetric to
    /// [`Out::descriptors`] pushing the health/log descriptors after the user
    /// ports (coordinator.md §1.4). Rings are backing-erased, so a dlopen'd
    /// system's `Out<O>` binds over the host's raw regions with this same impl.
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

/// How a concrete system is constructed from its typed params — the
/// **format-independent** half of system construction.
///
/// This trait carries *no* KDL coupling: a `cdylib` exported via
/// [`export_system!`](crate::export_system) only needs `BuildSystem` (its `fsw_create`
/// postcard-decodes `Params` and calls `new`), so the dlopen ABI does not ride the
/// `kdl` feature. The KDL static-registry path (`wiring::Registry::register`) adds
/// only a `Params: serde::de::DeserializeOwned` bound — the same derive the postcard
/// contract already needs — so a statically-linked system registers by impl'ing
/// `BuildSystem` alone.
///
/// `Params` is the value `fsw_create`/the registry factory decodes and hands to
/// [`new`](BuildSystem::new); a paramless system uses `type Params = ()`.
pub trait BuildSystem: Sized {
    /// The params value the system is constructed from (postcard bytes on the dl wire;
    /// serde-deserialized off the KDL node on the static-registry path).
    type Params;
    /// Construct the (pre-init) system from its decoded params.
    fn new(params: Self::Params) -> Self;
}

// ---------------------------------------------------------------------------
// System traits
// ---------------------------------------------------------------------------

/// The shared system surface (system.md §1): the typed input/output bundles, the
/// wiring name, and the once-each lifecycle hooks. A user implements one of
/// [`CyclicSystem`]/[`AsyncSystem`], which both require this.
///
/// Rings are backing-erased, so the one `impl System for Foo` serves both the
/// host (heap rings) and a dlopen'd instance (non-owning views into the host's
/// regions) — no backing generic anywhere.
pub trait System {
    /// The read-only inputs this system consumes.
    type Input: SystemInput + BindPorts;
    /// The owned outputs this system produces (wrapped in [`Out`] for health).
    type Output: SystemOutput + BindPorts;

    /// Wiring name; the prefix the system's health frame hangs off (system.md §4).
    const NAME: &'static str;

    /// Runs once before the first `execute`/`run`. May emit initial frames / health.
    /// Defaults to a no-op, so a system that needs no setup can omit it.
    fn init(&mut self, _output: &mut Self::Output) {}

    /// Runs once at teardown. May flush final frames / health. Defaults to a no-op.
    fn shutdown(&mut self, _output: &mut Self::Output) {}
}

/// A coordinator-driven system: the coordinator calls [`execute`](Self::execute)
/// once per cycle. Inputs are views straight into upstream output buffers; a slow
/// consumer backpressures its producers rather than losing data (system.md §3.1).
pub trait CyclicSystem: System {
    /// One unit of work: read the latest inputs, write outputs. Reports trouble
    /// through `output.health()`, never a return value.
    ///
    /// `now` is the coordinator's per-cycle timestamp (the same value for every
    /// system in one cycle, and the value driving a [`Simulated`](crate::ClockMode)
    /// clock); a system stamps its output frames with it rather than calling
    /// `Timestamp::now()` independently.
    ///
    /// `input` is `&mut` (not `&`, deviating from system.md §1.1): draining a ring
    /// `View` advances its cursor, exactly the `&mut self` reason the doc's own
    /// §2.3 `Input::latest` takes.
    fn execute(&mut self, now: Timestamp, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (system.md §5): the wired ports
    /// per direction, plus the merged capability set of both bundles.
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
}

/// A self-driven system: it owns its own loop, paced by a timer or by awaiting its
/// inputs with the ring `Notifier`. The coordinator spawns [`run`](Self::run)
/// once and does not tick it (system.md §3.2).
#[allow(async_fn_in_trait)]
pub trait AsyncSystem: System {
    /// The system's own loop. Returns when shutting down. Awaits inputs
    /// (`Input::recv`) or sleeps on a timer, calling its work on each wake, and uses
    /// the async output path so a lossless output can suspend for space. `input` is
    /// `&mut` for the same reason as [`CyclicSystem::execute`].
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (system.md §5): the wired ports
    /// per direction, plus the merged capability set of both bundles.
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
}

// ---------------------------------------------------------------------------
// Cyclic driver (the framework wrapper that maintains the standard counters)
// ---------------------------------------------------------------------------

/// Drives a [`CyclicSystem`] and maintains the standard health counters around
/// each `execute` (system.md §4): the execute duration is timed and a health
/// record is published per cycle.
///
/// It owns the bundles between cycles, on behalf of the coordinator that drives it.
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
    /// Assemble a runner from a constructed system and its (hand- or coordinator-
    /// built) port bundles.
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

    /// Run one cycle (coordinator.md §3.3/§3.4). If already stopped, do nothing.
    /// Otherwise time `execute` and publish health. `now` is threaded from the
    /// cycle loop so every system in a cycle shares one timestamp.
    pub fn step(&mut self, now: Timestamp) {
        if self.state.is_stopped() {
            return;
        }
        let start = std::time::Instant::now();
        self.system.execute(now, &mut self.input, &mut self.output);
        let micros = start.elapsed().as_micros() as u64;
        // E6: fold the ports' publish-drop counters into health — a nonzero sum is a
        // ring-sizing bug or a backpressuring reader the system itself could not
        // report (publish is infallible).
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

    /// Borrow the output bundle (e.g. for a test to read a produced port back).
    pub fn output(&mut self) -> &mut Out<O> {
        &mut self.output
    }
}

/// The grown [`CyclicRunner`] erased so the coordinator can hold a heterogeneous
/// `Vec<Box<dyn CyclicSlot>>` (coordinator.md §3.4). Delegates to the inherent
/// methods above; `step` threads the cycle's shared timestamp.
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
