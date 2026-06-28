//! The `System` family of traits and the framework wrapper around a system's
//! outputs (system.md §1, §3, §4).
//!
//! A shared [`System`] base carries the common surface (input/output bundle types,
//! name, lifecycle). Two leaf traits a user implements express the one structural
//! difference (system.md §3): [`CyclicSystem`] is coordinator-driven (`execute`
//! once per cycle), [`AsyncSystem`] owns its own `run` loop. The lifecycle is not
//! duplicated.

use core::ops::{Deref, DerefMut};

use metor_fsw_ring::{Backing, BoxBacking, NoWake, WakeSink, WakeSource};
use metor_proto::types::Timestamp;

use crate::binder::{BindPorts, RingSource};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::{PortDesc, SystemDescriptor, SystemKind};
use crate::health::{HealthPort, Level, SystemHealth, SystemLog};
use crate::port::Output;

// ---------------------------------------------------------------------------
// Port bundles
// ---------------------------------------------------------------------------

/// A system's input bundle: a struct of [`Input<F>`](crate::Input) ports. Derive
/// with `#[derive(SystemInput)]` to generate `descriptors`/`any_lapped` from the
/// port fields.
pub trait SystemInput {
    /// The required producer shape of every input port (system.md §5), in field
    /// order. Read before any port exists.
    fn descriptors() -> Vec<PortDesc>;

    /// Whether any input port has been lapped (overwrite buffers). The coordinator
    /// checks this on cyclic systems before `execute` (system.md §3.1).
    fn any_lapped(&self) -> bool;
}

/// A system's output bundle: a struct of [`Output<F>`](crate::Output) ports. Derive
/// with `#[derive(SystemOutput)]`. The framework wraps it in [`Out`] to add the
/// implicit health/log ports.
pub trait SystemOutput {
    /// The produced frame of every output port (system.md §5), in field order.
    fn descriptors() -> Vec<PortDesc>;
}

/// The framework wrapper around a system's user output bundle `O`: it adds the
/// implicit per-system health/log port pair (system.md §4) so `output.health()`
/// is always available, while `Deref`/`DerefMut` expose the user's own ports
/// (`output.<port>.write(...)`).
pub struct Out<O, B = BoxBacking, WD = NoWake, WS = NoWake>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    ports: O,
    health: HealthPort<B, WD, WS>,
}

impl<O, B, WD, WS> Out<O, B, WD, WS>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    /// Bundle a user output struct with its framework-allocated health/log port.
    pub fn new(ports: O, health: HealthPort<B, WD, WS>) -> Self {
        Self { ports, health }
    }

    /// The system-facing health handle: `output.health().error("kind")` /
    /// `.log(level, msg)` (system.md §4.2). The only error-reporting mechanism.
    pub fn health(&mut self) -> &mut HealthPort<B, WD, WS> {
        &mut self.health
    }
}

impl<O, B, WD, WS> Deref for Out<O, B, WD, WS>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    type Target = O;
    fn deref(&self) -> &O {
        &self.ports
    }
}

impl<O, B, WD, WS> DerefMut for Out<O, B, WD, WS>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    fn deref_mut(&mut self) -> &mut O {
        &mut self.ports
    }
}

impl<O, B, WD, WS> SystemOutput for Out<O, B, WD, WS>
where
    O: SystemOutput,
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    fn descriptors() -> Vec<PortDesc> {
        // The user's ports, then the two implicit health/log ports every system gets.
        let mut descs = O::descriptors();
        descs.push(PortDesc::of::<SystemHealth>());
        descs.push(PortDesc::of::<SystemLog>());
        descs
    }
}

impl<O, B, WD, WS> BindPorts<B> for Out<O, B, WD, WS>
where
    O: BindPorts<B>,
    B: Backing,
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind the user ports, then the two implicit health/log ports — symmetric to
    /// [`Out::descriptors`] pushing the health/log descriptors after the user
    /// ports (coordinator.md §1.4). Threads the ring source's backing `B` so a
    /// dlopen'd system's `Out<O, RawBacking>` binds over the host's regions (dl-open.md §1.2).
    fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let ports = O::bind(src);
        let health: Output<SystemHealth, B, WD, WS> = Output::bind(src);
        let log: Output<SystemLog, B, WD, WS> = Output::bind(src);
        Out::new(ports, HealthPort::new(health, log))
    }
}

// ---------------------------------------------------------------------------
// System construction (kdl-independent — dl-open.md §3.0, §6.3)
// ---------------------------------------------------------------------------

/// How a concrete system is constructed from its typed params — the
/// **format-independent** half of WP6's old `RegisteredSystem` (dl-open.md §3.0).
///
/// This trait carries *no* KDL coupling: a `cdylib` exported via
/// [`export_system!`](crate::export_system) only needs `BuildSystem` (its `fsw_create`
/// postcard-decodes `Params` and calls `new`), so the dlopen ABI no longer rides the
/// `kdl` feature. The KDL static-registry path layers `RegisteredSystem`
/// (`wiring::RegisteredSystem`) on top, adding the `Params: FromKdlNode` bound — every
/// `BuildSystem` whose `Params` is `FromKdlNode` is automatically a `RegisteredSystem`
/// (a blanket impl), so a statically-linked system is registered exactly as before.
///
/// `Params` is the value `fsw_create`/the registry factory decodes and hands to
/// [`new`](BuildSystem::new); a paramless system uses `type Params = ()`.
pub trait BuildSystem: Sized {
    /// The params value the system is constructed from (postcard bytes on the dl wire;
    /// a `FromKdlNode`-parsed struct on the static-registry path).
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
/// `B` is the ring [`Backing`] the bundles are bound over (dl-open.md §1.2). It
/// defaults to [`BoxBacking`] — the host case — so an ordinary `impl System for Foo`
/// is unchanged; a system whose bundles are `Backing`-generic writes
/// `impl<B: Backing> System<B> for Foo` so a future dlopen'd instance can hold
/// `RawBacking` views into the host's regions.
pub trait System<B: Backing = BoxBacking> {
    /// The read-only inputs this system consumes.
    type Input: SystemInput + BindPorts<B>;
    /// The owned outputs this system produces (wrapped in [`Out`] for health).
    type Output: SystemOutput + BindPorts<B>;

    /// Wiring name; the prefix the system's health frame hangs off (system.md §4).
    const NAME: &'static str;

    /// Runs once before the first `execute`/`run`. May emit initial frames / health.
    /// Defaults to a no-op, so a system that needs no setup can omit it.
    fn init(&mut self, _output: &mut Self::Output) {}

    /// Runs once at teardown. May flush final frames / health. Defaults to a no-op.
    fn shutdown(&mut self, _output: &mut Self::Output) {}
}

/// A coordinator-driven system: the coordinator calls [`execute`](Self::execute)
/// once per cycle. Inputs are views straight into upstream output buffers; a lapped
/// input is a hard error the coordinator acts on (system.md §3.1).
pub trait CyclicSystem<B: Backing = BoxBacking>: System<B> {
    /// One unit of work: read the latest inputs, write outputs. Reports trouble
    /// through `output.health()`, never a return value.
    ///
    /// `now` is the coordinator's per-cycle timestamp (the same value for every
    /// system in one cycle, and the value driving a [`Simulated`](crate::ClockMode)
    /// clock); a system stamps its output frames with it rather than calling
    /// `Timestamp::now()` independently.
    ///
    /// `input` is `&mut` (not `&`, deviating from system.md §1.1): draining a ring
    /// `View` advances its cursor and fills the port's reused scratch, exactly the
    /// `&mut self` reason the doc's own §2.3 `Input::latest` takes.
    fn execute(&mut self, now: Timestamp, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (system.md §5).
    fn descriptor() -> SystemDescriptor {
        SystemDescriptor {
            name: Self::NAME,
            kind: SystemKind::Cyclic,
            inputs: <Self::Input as SystemInput>::descriptors(),
            outputs: <Self::Output as SystemOutput>::descriptors(),
        }
    }
}

/// A self-driven system: it owns its own loop, paced by a timer or by awaiting its
/// inputs with the ring `Notifier`. The coordinator spawns [`run`](Self::run)
/// once and does not tick it (system.md §3.2).
#[allow(async_fn_in_trait)]
pub trait AsyncSystem<B: Backing = BoxBacking>: System<B> {
    /// The system's own loop. Returns when shutting down. Awaits inputs
    /// (`Input::recv`) or sleeps on a timer, calling its work on each wake, and uses
    /// the async output path so a lossless output can suspend for space. `input` is
    /// `&mut` for the same reason as [`CyclicSystem::execute`].
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (system.md §5).
    fn descriptor() -> SystemDescriptor {
        SystemDescriptor {
            name: Self::NAME,
            kind: SystemKind::Async,
            inputs: <Self::Input as SystemInput>::descriptors(),
            outputs: <Self::Output as SystemOutput>::descriptors(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cyclic driver (the framework wrapper that maintains the standard counters)
// ---------------------------------------------------------------------------

/// Drives a [`CyclicSystem`] and maintains the four standard health counters
/// around each `execute` (system.md §4): a lapped input bumps `lapped_inputs`,
/// the execute duration is timed, and a health record is published per cycle.
///
/// This is the WP4 stand-in for the coordinator (WP5): it owns the bundles between
/// cycles, exactly as the coordinator will.
pub struct CyclicRunner<S, O, B = BoxBacking>
where
    B: Backing,
    S: CyclicSystem<B, Output = Out<O, B>>,
    O: SystemOutput,
{
    system: S,
    input: S::Input,
    output: Out<O, B>,
    state: SlotState,
}

impl<S, O, B> CyclicRunner<S, O, B>
where
    B: Backing,
    S: CyclicSystem<B, Output = Out<O, B>>,
    O: SystemOutput,
{
    /// Assemble a runner from a constructed system and its (hand- or coordinator-
    /// built) port bundles.
    pub fn new(system: S, input: S::Input, output: Out<O, B>) -> Self {
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
    /// If an input has lapped, telemeter and permanently stop: charge the lapped
    /// input, log an error, publish a final health record, and flip to `Stopped`.
    /// Otherwise time `execute` and publish health. `now` is threaded from the
    /// cycle loop so every system in a cycle shares one timestamp.
    pub fn step(&mut self, now: Timestamp) {
        if self.state.is_stopped() {
            return;
        }
        if self.input.any_lapped() {
            let h = self.output.health();
            h.record_lapped();
            h.log(Level::Error, "input lapped; system permanently stopped");
            h.end_cycle(now, 0);
            self.state = SlotState::Stopped {
                reason: StopReason::LappedInput,
            };
            return;
        }
        let start = std::time::Instant::now();
        self.system.execute(now, &mut self.input, &mut self.output);
        let micros = start.elapsed().as_micros() as u64;
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
    pub fn output(&mut self) -> &mut Out<O, B> {
        &mut self.output
    }
}

/// The grown [`CyclicRunner`] erased so the coordinator can hold a heterogeneous
/// `Vec<Box<dyn CyclicSlot>>` (coordinator.md §3.4). Delegates to the inherent
/// methods above; `step` threads the cycle's shared timestamp.
impl<S, O, B> CyclicSlot for CyclicRunner<S, O, B>
where
    B: Backing,
    S: CyclicSystem<B, Output = Out<O, B>>,
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
