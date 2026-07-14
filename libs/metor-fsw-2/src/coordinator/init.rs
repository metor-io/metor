//! The build-time init pipeline: collect systems and edges into an [`InitGraph`],
//! then [`build`] it into a ready [`Coordinator`](super::Coordinator).
//!
//! An [`InitGraph`] is the wiring graph as plain data — the registered systems
//! (each a [`Node`]: its type-erased [`SystemBind`], its descriptor, its
//! instance name), the edges between their ports, and the run-scoped overrides.
//! [`build`] runs the passes in order — validation, edge resolution, fan-out
//! counting, ring allocation, registry freeze, copy-in planning, bind — each
//! handing its product to the next, and assembles the `Coordinator` literal.
//!
//! # Execution order
//!
//! Cyclic systems step in registration order, once per cycle. A snapshot edge
//! only observes the current cycle's value when it points forward in that
//! order, so [`build`] rejects a backward snapshot edge between cyclic systems
//! ([`WireError::StaleFrameEdge`]) and any feedback loop not broken by an
//! explicit [`connect_delayed`](super::CoordinatorBuilder::connect_delayed) edge
//! ([`WireError::FeedbackCycle`]). One-cycle-late sampling is therefore always
//! a declared decision, never an accident of registration order. Log edges
//! carry decoupled event/command streams with no same-cycle dependency and are
//! exempt from both rules, as are edges touching an async endpoint.
//!
//! # Port connections
//!
//! An input's [`PortConn`] says who feeds it. An `Edge` input is wired by
//! [`connect`](super::CoordinatorBuilder::connect). A `Host` input's counterpart
//! is held by the system's runner over a dedicated ring (a slot's cancel frame,
//! for example). A `SelfTap` input is a read view over one of the system's own
//! outputs. Edges into `Host` or `SelfTap` inputs are rejected; `Host`
//! *outputs* still accept consumer edges (the coordinator's own command
//! channel is one).
//!
//! # Async systems and copy-ins
//!
//! An async system runs on its own task, off the cycle clock, so it cannot be
//! step-gated. Each of its edge-connected snapshot inputs is decoupled through
//! a private ring: after the cyclic step loop, the coordinator mirrors the
//! newest upstream record into the private ring, whose data notifier wakes the
//! task's parked `recv`. Log inputs need no copy-in; they read the producers'
//! rings directly and are poll-drained.
//!
//! # Reader budgets
//!
//! Every ring's `max_readers` is fixed at build time. The budget is the
//! counted edge fan-out plus declared self-taps, one slot per receive-all
//! capability in the graph, and [`CoordinatorConfig::reader_slack`] spare
//! slots for taps claimed through the [`Registry`] after build. Exhausting the
//! budget surfaces as an error at the claim site, not a panic.
//!
//! # The coordinator's own bundle
//!
//! The coordinator registers itself as system #0 under the reserved instance
//! name `"coordinator"`, declaring its own channels (health, log, status, the
//! boot sequence registry, the operator command channel) as an ordinary
//! descriptor. They are validated, sized, allocated, and registered by the
//! same passes as every other system's; the bind pass wraps the allocated
//! rings into the coordinator's fields instead of a cyclic slot, because the
//! coordinator is the loop rather than a member of it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use metor_fsw_ring::{Config, NoWake, Notifier, RingBuffer};
use metor_proto::types::{ComponentId, Msg};
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelSpec, SequenceCommand, SequenceRegistry, WiringManifest,
};

use crate::binder::{AnySource, BindPorts, Binder};
use crate::descriptor::{
    Delivery, FanIn, PortConn, PortDesc, PortSchema, SystemDescriptor, SystemKind, compatible,
};
use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgOut};
use crate::port::capacity_for;
use crate::proc::session::SessionDir;
use crate::registry::{Registry, RegistryEntry};
use crate::system::{AsyncSystem, CyclicRunner, CyclicSystem, Out, System, SystemOutput};

use super::bind::{ProcBindCtx, bind_systems};
use super::slot::{
    self, AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError, SlotReg,
    validate_slot_spec,
};
use super::{
    AsyncLauncher, AsyncSlot, BoundSystems, BufferRole, Coordinator, CoordinatorConfig,
    CoordinatorStatus, CopyIn, CyclicSlot, NAME_CAP, PortRef, RingEntry, RingTable, SlotState,
    SystemHandle, WireError,
};

// ---------------------------------------------------------------------------
// Registration (type erasure of the boxed systems)
// ---------------------------------------------------------------------------

pub(crate) trait CyclicRegistration {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot>;
}

struct CyclicReg<S> {
    system: S,
}

impl<S, O> CyclicRegistration for CyclicReg<S>
where
    S: CyclicSystem<Output = Out<O>> + 'static,
    O: SystemOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        // The host binds over its own pre-allocated heap rings via the `Binder`
        // ring source; a dlopen'd system runs the identical (backing-erased)
        // bind on its own side of the ABI over non-owning attaches.
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <Out<O> as BindPorts>::bind(binder);
        Box::new(CyclicRunner::new(self.system, input, output))
    }
}

pub(crate) trait AsyncRegistration {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn AsyncLauncher>;
}

struct AsyncReg<S> {
    system: S,
}

impl<S> AsyncRegistration for AsyncReg<S>
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn AsyncLauncher> {
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <S::Output as BindPorts>::bind(binder);
        Box::new(AsyncSlot {
            system: self.system,
            input,
            output,
        })
    }
}

/// A pack entry as a cyclic registration: the bind-phase [`Pending`](crate::pack::Pending)
/// plus the entry's static display name. Its `bind` binds the ports over an
/// [`AnySource::Host`], calls the pending closure with
/// [`Mount::Wired`](crate::pack::Mount), and wraps the boxed
/// [`Driver`](crate::pack::Driver) in a running
/// [`DriverSlot`](crate::pack::DriverSlot), so a pack entry rides the ordinary
/// cyclic path with no dedicated `SystemBind` variant.
struct PendingDriver {
    pending: crate::pack::Pending,
    /// The entry's own name, the slot's static display name (the instance name
    /// lives on the enclosing [`Node`], as for every kind).
    entry_name: &'static str,
}

impl CyclicRegistration for PendingDriver {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        let mut src = AnySource::Host(binder);
        let driver = (self.pending)(&mut src, crate::pack::Mount::Wired);
        Box::new(crate::pack::DriverSlot {
            driver,
            name: self.entry_name,
            state: SlotState::Running,
        })
    }
}

/// A registered dlopen'd cyclic system: the loaded handle plus its postcard
/// `Params` blob. At [`build`] it becomes a [`DlSlot`](crate::dl) instead of a
/// typed [`CyclicRunner`]; everything before that (descriptor push, edge
/// validation, ring sizing/allocation, registry entry) is the same as the
/// static-system path.
pub(crate) struct DlReg {
    pub(crate) system: crate::dl::DlSystem,
    pub(crate) params: Vec<u8>,
}

/// A registered process system: the artifact path a worker process will
/// dlopen plus its postcard `Params` blob. The descriptor (on the enclosing
/// [`Node`]) arrived as decoded describe-worker bytes — the host holds no
/// `DlSystem` and never loads the artifact itself. At [`build`] it becomes a
/// [`ProcSlot`](crate::proc); everything before that is the same uniform pass
/// as every other kind.
pub(crate) struct ProcReg {
    pub(crate) artifact: PathBuf,
    pub(crate) params: Vec<u8>,
    /// The pack entry the worker instantiates.
    pub(crate) system: String,
}

pub(crate) enum SystemBind {
    /// The coordinator itself, registered as system #0 under the reserved
    /// instance name `"coordinator"`. A marker registration: its declared
    /// outputs are allocated and registered by the uniform passes like any
    /// system's, but it is never pushed into `cyclic` (the coordinator is the
    /// loop); the bind arm wraps the allocated rings into the coordinator's own
    /// fields instead.
    Coordinator,
    /// A created host system, erased; binds via a [`Binder`]. A pack entry rides
    /// this path too (via [`PendingDriver`]).
    Cyclic(Box<dyn CyclicRegistration>),
    /// An async system; yields a [`PendingAsync`], not a cyclic slot.
    Async(Box<dyn AsyncRegistration>),
    /// A dlopen'd cyclic system, bound to a [`DlSlot`](crate::dl) at [`build`].
    Dl(DlReg),
    /// A cross-process cyclic system, spawned as a worker and bound to a
    /// [`ProcSlot`](crate::proc) at [`build`].
    Proc(ProcReg),
    /// A runtime-swappable slot, bound to a [`SlotRunner`](slot::SlotRunner) at [`build`].
    Slot(SlotReg),
}

/// One registered system: its type-erased [`SystemBind`], its registered
/// descriptor (what [`build`] validates, sizes, and wires), and its instance
/// name (defaults to `System::NAME`; a wiring file supplies a distinct name
/// per instance).
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) desc: SystemDescriptor,
    pub(crate) bind: SystemBind,
}

// ---------------------------------------------------------------------------
// The init graph
// ---------------------------------------------------------------------------

/// The wiring graph as plain data: the registered systems in registration
/// order, the edges between their ports, and the run-scoped overrides. Built up
/// through the [`CoordinatorBuilder`](super::CoordinatorBuilder) shim, then
/// consumed by [`build`].
pub(crate) struct InitGraph {
    pub(crate) config: CoordinatorConfig,
    /// The registered systems; index == registration order == step order.
    pub(crate) systems: Vec<Node>,
    /// Each registered edge `(producer, consumer, delayed)`. A `delayed` edge
    /// is an intentional one-cycle-delayed feedback edge, excluded from cycle
    /// detection.
    pub(crate) edges: Vec<(PortRef, PortRef, bool)>,
    /// Override of the worker executable process systems spawn; `None`
    /// re-executes the host binary.
    pub(crate) worker_exe: Option<PathBuf>,
    /// Override of the shared-memory session root; `None` picks `/dev/shm`
    /// when present, else the OS temp dir.
    pub(crate) shm_dir: Option<PathBuf>,
    /// The mission IR to broadcast as a [`WiringManifest`], set by
    /// [`set_wiring_manifest`](Self::set_wiring_manifest). `Some` means the
    /// coordinator #0 bundle already carries a `wiring` Host output (injected at
    /// set time), sized from the concrete payload; `None` (a graph used without
    /// a front-end) leaves the coordinator with no wiring channel.
    pub(crate) wiring_manifest: Option<WiringManifest>,
}

impl InitGraph {
    pub(crate) fn new(config: CoordinatorConfig) -> Self {
        let mut g = Self {
            config,
            systems: Vec::new(),
            edges: Vec::new(),
            worker_exe: None,
            shm_dir: None,
            wiring_manifest: None,
        };
        // The coordinator registers itself as system #0 under the reserved
        // instance name `"coordinator"`: an ordinary declared bundle, so its
        // channels are wired, sized, and registered by the same passes as
        // every system's. Every output is host-connected (the coordinator
        // itself holds the writers; a Host OUTPUT still accepts consumer
        // edges); the registry keys are `coordinator.health` / `.log` /
        // `.coordinator_status` / `.sequences` / `.commands`.
        //
        // - `commands` is the operator channel behind the take-once
        //   [`Coordinator::control_handle`]; commands reach a slot only over an
        //   explicit `"coordinator" -> <slot>` edge. Untelemetered, since
        //   inbound control is never echoed on the downlink.
        // - `sequences` carries the boot `SequenceRegistry`, telemetered so
        //   downstream consumers can list the channels; the `ReloadSequences`
        //   fan-in is its request channel (an ordinary edge input, zero edges
        //   legal), drained each cycle to re-emit the registry on demand for
        //   consumers that missed the boot message.
        let desc = SystemDescriptor {
            name: COORDINATOR_INSTANCE.into(),
            kind: SystemKind::Cyclic,
            inputs: vec![PortDesc::msg::<ReloadSequences>()],
            outputs: vec![
                PortDesc::of::<crate::SystemHealth>().with_conn(PortConn::Host),
                PortDesc::of::<crate::SystemLog>().with_conn(PortConn::Host),
                PortDesc::of::<CoordinatorStatus>().with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceRegistry>("sequences").with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceCommand>("commands")
                    .untelemetered()
                    .with_conn(PortConn::Host),
            ],
            capabilities: Vec::new(),
        };
        g.push_system(desc, COORDINATOR_INSTANCE.to_string(), SystemBind::Coordinator);
        g
    }

    /// Record one registration; the returned handle indexes `systems`.
    pub(crate) fn push_system(
        &mut self,
        desc: SystemDescriptor,
        name: String,
        bind: SystemBind,
    ) -> SystemHandle {
        let id = self.systems.len();
        self.systems.push(Node { bind, desc, name });
        SystemHandle { id }
    }

    /// The handle addressing the coordinator's own system-#0 bundle.
    pub(crate) fn coordinator_handle(&self) -> SystemHandle {
        SystemHandle { id: 0 }
    }

    /// The registered descriptor of `handle`.
    pub(crate) fn descriptor_of(&self, handle: SystemHandle) -> &SystemDescriptor {
        &self.systems[handle.id].desc
    }

    /// Store `manifest` and inject its `wiring` Host output onto the
    /// coordinator #0 bundle immediately, so the port is sized, allocated,
    /// registered, and bound by the ordinary passes like every other output.
    /// Its ring is sized from the concrete payload — a full IR overruns the
    /// default message cap — via an overridden `max_size`; nothing raises the
    /// global cap. Called again, the latest manifest wins: the single injected
    /// port is replaced in place, never accumulated.
    pub(crate) fn set_wiring_manifest(&mut self, manifest: WiringManifest) {
        let mut port = PortDesc::msg_named::<WiringManifest>("wiring");
        port.conn = PortConn::Host;
        port.max_size = wiring_manifest_max_size(&manifest.ir_json);
        let outputs = &mut self.systems[0].desc.outputs;
        match outputs.iter_mut().find(|p| p.name == "wiring") {
            Some(existing) => *existing = port,
            None => outputs.push(port),
        }
        self.wiring_manifest = Some(manifest);
    }

    /// Register a cyclic system under its type's `System::NAME` instance name.
    pub(crate) fn add_cyclic<S, O>(&mut self, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        self.add_cyclic_named(<S as System>::NAME, system)
    }

    /// Register a cyclic system under an explicit instance name.
    pub(crate) fn add_cyclic_named<S, O>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        // The instance descriptor, not the static one: a system whose port set
        // depends on its config registers what this instance actually carries.
        let desc = system.instance_descriptor();
        self.push_system(desc, name.into(), SystemBind::Cyclic(Box::new(CyclicReg { system })))
    }

    /// Register an async system under its type's `System::NAME` instance name.
    pub(crate) fn add_async<S>(&mut self, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        self.add_async_named(<S as System>::NAME, system)
    }

    /// Register an async system under an explicit instance name.
    pub(crate) fn add_async_named<S>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        // The instance descriptor, not the static one (see `add_cyclic_named`).
        let desc = system.instance_descriptor();
        self.push_system(desc, name.into(), SystemBind::Async(Box::new(AsyncReg { system })))
    }

    /// Register a pack entry under an explicit instance name, running its
    /// create phase (params decode + state construction) now so a bad config
    /// fails at registration, not at [`build`]. The bind phase runs at
    /// [`build`] over the entry's descriptor like any static system's, along
    /// the ordinary cyclic path (via [`PendingDriver`]).
    pub(crate) fn add_pack_entry(
        &mut self,
        name: impl Into<String>,
        entry: &mut crate::pack::PackEntry,
        params: crate::pack::EntryParams<'_>,
    ) -> Result<SystemHandle, crate::pack::MakeError> {
        let pending = entry.create(params)?;
        let driver = PendingDriver {
            pending,
            entry_name: entry.name(),
        };
        Ok(self.push_system(
            entry.descriptor().clone(),
            name.into(),
            SystemBind::Cyclic(Box::new(driver)),
        ))
    }

    /// Register a dlopen'd cyclic system under an explicit instance name.
    pub(crate) fn add_dl_cyclic(
        &mut self,
        name: impl Into<String>,
        loaded: crate::dl::DlSystem,
        params: Vec<u8>,
    ) -> SystemHandle {
        let mut desc = loaded.descriptor().clone();
        // Dl systems are cyclic-only: the registered kind is pinned here,
        // never trusted from the decoded manifest.
        desc.kind = SystemKind::Cyclic;
        self.push_system(
            desc,
            name.into(),
            SystemBind::Dl(DlReg {
                system: loaded,
                params,
            }),
        )
    }

    /// Register a cross-process cyclic system under an explicit instance name.
    pub(crate) fn add_proc_cyclic(
        &mut self,
        name: impl Into<String>,
        mut descriptor: SystemDescriptor,
        artifact: PathBuf,
        system: impl Into<String>,
        params: Vec<u8>,
    ) -> SystemHandle {
        // Cyclic-only, pinned here like the dl path: the registered kind is
        // never trusted from decoded wire bytes.
        descriptor.kind = SystemKind::Cyclic;
        self.push_system(
            descriptor,
            name.into(),
            SystemBind::Proc(ProcReg {
                artifact,
                params,
                system: system.into(),
            }),
        )
    }

    /// Register a runtime-swappable slot. See
    /// [`CoordinatorBuilder::add_slot`](super::CoordinatorBuilder::add_slot) for
    /// the full contract.
    pub(crate) fn add_slot(
        &mut self,
        name: impl Into<String>,
        allowed: Vec<AllowedOccupant>,
        initial: Option<InitialOccupant>,
    ) -> Result<SystemHandle, SlotConfigError> {
        let names: Vec<&str> = allowed.iter().map(|a| a.name.as_str()).collect();
        validate_slot_spec(&names, initial.as_ref().map(|i| i.occupant.as_str()))?;
        // Per-slot means all-occupants: the isolation boundary is the slot's
        // position in the cycle, and a mixed allow set would make `Load`
        // silently change the fault domain.
        let n_proc = allowed
            .iter()
            .filter(|a| matches!(a.backing, OccupantBacking::Artifact(_)))
            .count();
        if n_proc != 0 && n_proc != allowed.len() {
            return Err(SlotConfigError::MixedBacking);
        }
        let process = n_proc == allowed.len();
        // Every allowed occupant must share the contract; the slot sizes and
        // validates to the first occupant's descriptor (mutual subset).
        let base = &allowed[0].descriptor;
        for occ in &allowed[1..] {
            let d = &occ.descriptor;
            let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(x, y)| compatible(x, y) && compatible(y, x))
            };
            if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
                return Err(SlotConfigError::OccupantMismatch {
                    occupant: occ.name.clone(),
                    base: allowed[0].name.clone(),
                });
            }
        }
        let ports = slot::SlotPorts::for_occupant(base, &allowed[0].name)?;

        let name: String = name.into();
        // The registered descriptor name is the slot's instance name (a leaked
        // `&'static str` for the descriptor field and the `SlotRunner` identity).
        let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
        let registered = ports.registered(leaked);

        Ok(self.push_system(
            registered,
            name,
            SystemBind::Slot(SlotReg {
                allowed,
                initial,
                ports,
                process,
            }),
        ))
    }

    /// Record one edge; the cheap connect-time guards run in [`check_edge`],
    /// the full validation in [`solve_edges`].
    pub(crate) fn push_edge(&mut self, producer: PortRef, consumer: PortRef, delayed: bool) {
        self.edges.push((producer, consumer, delayed));
    }

    // -----------------------------------------------------------------------
    // build() passes, in order.
    // -----------------------------------------------------------------------

    /// A Wall clock turns `cycle_rate` into the per-cycle pacing budget in
    /// `run_for`; reject an unusable rate here so the failure is a build-time
    /// `WireError`, not a `Duration::from_secs_f64` panic mid-run. A
    /// `Simulated` clock ignores the rate, so it is deliberately not validated
    /// there.
    fn validate_cycle_rate(&self) -> Result<(), WireError> {
        if matches!(self.config.clock, super::ClockMode::Wall)
            && !(self.config.cycle_rate.is_finite() && self.config.cycle_rate > 0.0)
        {
            return Err(WireError::InvalidCycleRate {
                rate: self.config.cycle_rate,
            });
        }
        Ok(())
    }

    /// Receive-all (telemetry) systems must register last. The downlink's
    /// end-of-cycle snapshot only observes systems stepping before it, so a
    /// cyclic system registered after it would telemeter one cycle stale.
    /// Enforced, not silently reordered: reordering registrations would change
    /// the step order the stale-edge diagnostics validate. Async systems are
    /// exempt (they run off their own task, not the registration-ordered loop).
    fn validate_receive_all_last(&self) -> Result<(), WireError> {
        let mut first_receive_all: Option<usize> = None;
        for (s, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Cyclic {
                continue;
            }
            let has_receive_all = sys
                .desc
                .capabilities
                .contains(&crate::Capability::ReceiveAll);
            if has_receive_all {
                first_receive_all.get_or_insert(s);
            } else if let Some(t) = first_receive_all {
                return Err(WireError::ReceiveAllNotLast {
                    system: sys.name.clone(),
                    receive_all: self.systems[t].name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Slot instance names are wire addresses: enforce the [`NAME_CAP`]. A
    /// `SequenceCommand` addresses a slot by its instance name, and the same
    /// name packs into fixed-size status frames; a longer name would telemeter
    /// truncated while addressing untruncated, so it is a build error, never a
    /// truncation.
    fn validate_slot_name_caps(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            if matches!(sys.bind, SystemBind::Slot(_)) && sys.desc.name.len() > NAME_CAP {
                return Err(WireError::SlotNameTooLong {
                    name: sys.desc.name.to_string(),
                    len: sys.desc.name.len(),
                });
            }
        }
        Ok(())
    }

    /// Per-descriptor axis validation, needing no edges: FanIn::Many with
    /// Delivery::Snapshot is rejected (latest-wins across producers is
    /// ill-defined).
    fn validate_port_axes(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            for port in &sys.desc.inputs {
                if port.fan_in == FanIn::Many && port.delivery == Delivery::Snapshot {
                    return Err(WireError::SnapshotFanIn {
                        system: sys.desc.name.clone(),
                        port: port.id(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate every edge and build the one connection map,
    /// `(cons_id, in_idx) -> [(prod_id, out_idx)]`, covering every input: a
    /// FanIn::One input holds exactly one entry (enforced here), a FanIn::Many
    /// input zero or more. Every rule branches on a descriptor axis, never on
    /// frame-vs-message. Also runs the graph-shape checks over the map: every
    /// feedback loop must be broken by a `connect_delayed`, registration order
    /// must agree with the dataflow, and every FanIn::One input must be
    /// connected.
    pub(crate) fn solve_edges(&self) -> Result<ConsEdges, WireError> {
        let n = self.systems.len();
        let mut cons_edges: ConsEdges = HashMap::new();
        // System-level adjacency over the non-delayed SNAPSHOT edges only, for
        // cycle detection: a remaining cycle is an unbroken feedback loop. Log
        // edges are excluded; a log is a decoupled event/command stream, not a
        // same-cycle dependency.
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let out_idx =
                prod.outputs
                    .iter()
                    .position(|d| d.id() == p.port)
                    .ok_or(WireError::UnknownPort {
                        system: p.system.id,
                        port: p.port,
                    })?;
            let in_idx =
                cons.inputs
                    .iter()
                    .position(|d| d.id() == c.port)
                    .ok_or(WireError::UnknownPort {
                        system: c.system.id,
                        port: c.port,
                    })?;
            if !compatible(&prod.outputs[out_idx], &cons.inputs[in_idx]) {
                return Err(WireError::Incompatible {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
            let in_desc = &cons.inputs[in_idx];
            // A host-connected input's counterpart is held by the system's
            // runner (the slot's cancel writer, a self-tap over its own
            // output), so an edge into it is rejected. Host *outputs* keep
            // accepting consumer edges: the coordinator's `commands` channel is
            // exactly a Host output slots read over explicit edges.
            if in_desc.conn != PortConn::Edge {
                return Err(WireError::HostPort {
                    system: cons.name.clone(),
                    port: c.port,
                });
            }
            // `delayed` marks a one-cycle-late snapshot sample; on a log input
            // it is meaningless and rejected instead of silently ignored.
            if *delayed && in_desc.delivery == Delivery::Log {
                return Err(WireError::DelayedLogEdge {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
            let producers = cons_edges.entry((c.system.id, in_idx)).or_default();
            match in_desc.fan_in {
                // Exactly one edge per input.
                FanIn::One => {
                    if !producers.is_empty() {
                        return Err(WireError::DoubleConnect {
                            system: cons.name.clone(),
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
                // Fan-in (append). Distinct producers may fan in freely, but an
                // exact duplicate of one edge would deliver every record twice.
                FanIn::Many => {
                    if producers.contains(&(p.system.id, out_idx)) {
                        return Err(WireError::DuplicateEdge {
                            producer: prod.name.clone(),
                            consumer: cons.name.clone(),
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
            }
            // Self-edges included: a system plainly connected to itself is the
            // tightest feedback loop (it can only ever read its own previous
            // cycle's value), so it must be declared with `connect_delayed` like
            // any other loop; the DFS reports it as a one-member `FeedbackCycle`.
            if in_desc.delivery == Delivery::Snapshot && !delayed {
                forward_adj[p.system.id].push(c.system.id);
            }
        }

        // --- Every feedback loop must be broken by a `connect_delayed` --------
        if let Some(cycle) = find_cycle(&forward_adj) {
            return Err(WireError::FeedbackCycle {
                systems: cycle
                    .into_iter()
                    .map(|id| self.systems[id].desc.name.clone())
                    .collect(),
            });
        }

        // --- Registration order must agree with the dataflow ------------------
        // The cyclic step loop runs in registration order, so a non-delayed
        // snapshot edge between two cyclic systems whose consumer registered
        // before its producer would read last cycle's value forever: silent
        // staleness that must instead be declared with `connect_delayed`.
        // Checked after cycle detection so a genuine unbroken loop (which
        // always contains a backward edge) reports the clearer `FeedbackCycle`.
        // Log edges are exempt (a decoupled stream); so are edges with an async
        // endpoint (async systems run off the post-step copy-in or their own
        // task, so their registration index carries no ordering semantics).
        // Self-edges never reach here (rejected above as a one-member cycle).
        for (p, c, delayed) in &self.edges {
            if *delayed {
                continue;
            }
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let in_delivery = cons
                .inputs
                .iter()
                .find(|d| d.id() == c.port)
                .map(|d| d.delivery);
            if in_delivery != Some(Delivery::Snapshot) {
                continue;
            }
            let both_cyclic = prod.kind == SystemKind::Cyclic && cons.kind == SystemKind::Cyclic;
            if both_cyclic && c.system.id < p.system.id {
                return Err(WireError::StaleFrameEdge {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
        }

        // --- Input coverage: a FanIn::One input must be connected exactly once ---
        // Exactly-once is the edge pass above; existence is here. A FanIn::Many
        // input may have zero producers; a non-Edge input is fed by its runner,
        // never an edge.
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn == PortConn::Edge
                    && port.fan_in == FanIn::One
                    && !cons_edges.contains_key(&(sid, in_idx))
                {
                    return Err(WireError::UnconnectedInput {
                        system: sys.desc.name.clone(),
                        port: port.id(),
                    });
                }
            }
        }

        Ok(cons_edges)
    }

    /// Fan-out per output port: one uniform count over the one connection map.
    /// A declared self-tap is one more reader on the system's *own* output,
    /// counted here so the budget is explicit rather than slack-covered.
    pub(crate) fn count_fan_out(&self, cons_edges: &ConsEdges) -> HashMap<(usize, usize), usize> {
        let mut fan_out: HashMap<(usize, usize), usize> = HashMap::new();
        for producers in cons_edges.values() {
            for &(prod_id, out_idx) in producers {
                *fan_out.entry((prod_id, out_idx)).or_insert(0) += 1;
            }
        }
        for (sid, sys) in self.systems.iter().enumerate() {
            for port in &sys.desc.inputs {
                let PortConn::SelfTap(pid) = port.conn else {
                    continue;
                };
                let out_idx = sys
                    .desc
                    .outputs
                    .iter()
                    .position(|o| o.id() == pid)
                    .expect("a SelfTap names one of the system's own outputs");
                *fan_out.entry((sid, out_idx)).or_insert(0) += 1;
            }
        }
        fan_out
    }

    /// Which output buffers cross a process boundary and must therefore be
    /// file-backed: every output of a process system, plus every output some
    /// process system consumes over an edge. A process slot crosses only its
    /// occupant *prefix* — the occupant's outputs and its Edge inputs'
    /// producers — because the runner tail (the `commands` fan-in, the
    /// self-tap, the status/events outputs) never leaves the coordinator.
    /// Everything else stays heap.
    pub(crate) fn shared_outputs(&self, cons_edges: &ConsEdges) -> HashSet<(usize, usize)> {
        let mut shared = HashSet::new();
        for (sid, sys) in self.systems.iter().enumerate() {
            // How much of the port lists a worker touches: all of a process
            // system's, the occupant prefix of a process slot's.
            let (n_outputs, n_inputs) = match &sys.bind {
                SystemBind::Proc(_) => (sys.desc.outputs.len(), sys.desc.inputs.len()),
                SystemBind::Slot(slot_reg) if slot_reg.process => (
                    slot_reg.ports.occupant_outputs.len(),
                    slot_reg.ports.occupant_inputs.len(),
                ),
                _ => continue,
            };
            for out_idx in 0..n_outputs {
                shared.insert((sid, out_idx));
            }
            for in_idx in 0..n_inputs {
                if let Some(producers) = cons_edges.get(&(sid, in_idx)) {
                    shared.extend(producers.iter().copied());
                }
            }
        }
        shared
    }

    /// Allocate one buffer per output port (health/log included) plus a
    /// dedicated ring per host-connected input, collecting the build-order
    /// registry entries: one list over every registered buffer, frames and
    /// message channels alike. Each receive-all capability in the graph is an
    /// extra fan-out reader on *every* buffer, so every ring's `max_readers`
    /// includes it, derived from the declared `ReceiveAll` capabilities with
    /// no per-consumer bookkeeping.
    ///
    /// A buffer in the [`shared_outputs`](Self::shared_outputs) set is
    /// allocated as an mmap ring file in the run's [`SessionDir`] (created
    /// lazily on the first one) and its path recorded for the worker
    /// manifests; the in-process handle over the same mapping is used
    /// everywhere the heap ring would have been — a ring is backing-erased,
    /// so nothing downstream can tell.
    pub(crate) fn alloc_rings(
        &self,
        cons_edges: &ConsEdges,
        fan_out: &HashMap<(usize, usize), usize>,
    ) -> Result<RingAlloc, WireError> {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let n_reg = self
            .systems
            .iter()
            .flat_map(|sys| sys.desc.capabilities.iter())
            .filter(|c| **c == crate::Capability::ReceiveAll)
            .count();
        let shared = self.shared_outputs(cons_edges);

        // A graph with any process system or process slot gets its session
        // directory up front: even a (pathological) portless worker still
        // needs somewhere for its control block and manifest.
        let needs_session = |bind: &SystemBind| {
            matches!(bind, SystemBind::Proc(_))
                || matches!(bind, SystemBind::Slot(slot_reg) if slot_reg.process)
        };
        let session = if self.systems.iter().any(|s| needs_session(&s.bind)) {
            Some(
                SessionDir::create(self.shm_dir.as_deref()).map_err(|e| WireError::Shm {
                    detail: e.to_string(),
                })?,
            )
        } else {
            None
        };
        let mut alloc = RingAlloc {
            table: RingTable { rings: Vec::new() },
            output_rings: Vec::with_capacity(self.systems.len()),
            host_input_rings: HashMap::new(),
            reg_entries: Vec::new(),
            session,
            ring_paths: HashMap::new(),
            host_input_paths: HashMap::new(),
        };

        // --- One buffer per output port -----------------------------------
        for (sid, sys) in self.systems.iter().enumerate() {
            let mut row = Vec::with_capacity(sys.desc.outputs.len());
            for (out_idx, port) in sys.desc.outputs.iter().enumerate() {
                let readers = fan_out.get(&(sid, out_idx)).copied().unwrap_or(0) + n_reg + slack;
                let instance = sys.name.clone();
                let role = BufferRole::Output {
                    system: sid,
                    port: out_idx,
                };
                // One sizing path (depth by delivery, `alloc_ring`); only the
                // registry-entry shape still splits on the schema. Command
                // channels are ordinary outputs here: a slot reads a producer
                // only over an explicit edge, so the edge fan-out counts its
                // readers exactly.
                let ring = if shared.contains(&(sid, out_idx)) {
                    let session = alloc.session.as_ref().expect("proc graphs have a session");
                    let path = session.path().join(format!("{instance}.{}.ring", port.name));
                    let ring =
                        alloc_ring_at(&path, port.delivery, port.max_size, depth, readers)?;
                    alloc.ring_paths.insert((sid, out_idx), path);
                    ring
                } else {
                    alloc_ring(port.delivery, port.max_size, depth, readers)
                };
                match &port.schema {
                    PortSchema::Table { .. } => {
                        alloc
                            .reg_entries
                            .push(registry_entry(&instance, port, ring.clone()));
                        alloc.table.rings.push(RingEntry {
                            ring: ring.clone(),
                            frame_id: port
                                .id()
                                .component()
                                .expect("table port keys on a ComponentId"),
                            role,
                            instance: Some(instance),
                        });
                    }
                    PortSchema::Postcard { .. } => {
                        // Registered like any buffer; the downlink taps it
                        // unless the port opted out via `telemetered = false`
                        // (a command channel, for example).
                        let entry = registry_entry(&instance, port, ring.clone());
                        alloc.table.rings.push(RingEntry {
                            ring: ring.clone(),
                            frame_id: entry.key,
                            role,
                            instance: Some(instance),
                        });
                        alloc.reg_entries.push(entry);
                    }
                }
                row.push(ring);
            }
            alloc.output_rings.push(row);
        }

        // --- Dedicated rings for host-connected inputs ---------------------
        // A Host input's counterpart is its runner's writer (the slot's cancel
        // frame), so it gets its own ring instead of a producer edge. The
        // occupant attaches one read `View` per Load (released on each
        // Stop/Reset/Unload), so 1 reader slot plus slack covers the reload
        // cycle. No registry entry: it is inbound control, not an output.
        // SelfTap inputs allocate nothing (they view the system's own output,
        // already counted in `fan_out`).
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn != PortConn::Host {
                    continue;
                }
                // A process slot's control ring is the one Host-connected
                // input that crosses outward: the writer stays host-side (the
                // runner's cancel `Output`) while the occupant's read `View`
                // attaches in the worker, so it is file-backed like a crossing
                // output, path recorded for the worker manifests.
                let ring = if matches!(&sys.bind, SystemBind::Slot(slot_reg) if slot_reg.process) {
                    let session = alloc.session.as_ref().expect("proc graphs have a session");
                    let path = session.path().join(format!("{}.{}.ring", sys.name, port.name));
                    let ring =
                        alloc_ring_at(&path, port.delivery, port.max_size, depth, 1 + slack)?;
                    alloc.host_input_paths.insert((sid, in_idx), path);
                    ring
                } else {
                    alloc_ring(port.delivery, port.max_size, depth, 1 + slack)
                };
                alloc.table.rings.push(RingEntry {
                    ring: ring.clone(),
                    frame_id: port
                        .id()
                        .component()
                        .expect("v1 host-connected inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        input: in_idx,
                    },
                    instance: Some(sys.name.clone()),
                });
                alloc.host_input_rings.insert((sid, in_idx), ring);
            }
        }

        Ok(alloc)
    }

    /// The boot `SequenceRegistry` payload: one spec per slot, keyed by the
    /// slot's instance name, the channel's wire address. There is no
    /// build-order channel id.
    fn seq_registry_payload(&self) -> SequenceRegistry {
        let channels = self
            .systems
            .iter()
            .filter_map(|sys| match &sys.bind {
                SystemBind::Slot(slot_reg) => Some(SequenceChannelSpec {
                    name: sys.desc.name.to_string(),
                    available: slot_reg.allowed.iter().map(|a| a.name.clone()).collect(),
                }),
                _ => None,
            })
            .collect();
        SequenceRegistry { channels }
    }

    /// Private copy-in buffers for async inputs, keyed on the delivery axis.
    /// An async system cannot be step-gated, so an async snapshot input is
    /// decoupled through a private latest-wins copy-in ring, which also
    /// supplies the matched data `Notifier` the async `recv` parks on. Log
    /// inputs use a direct fan-in multi-view, an every-record log the consumer
    /// poll-drains, with no copy-in.
    fn plan_copy_ins(&self, cons_edges: &ConsEdges, alloc: &mut RingAlloc) -> AsyncPlumbing {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let mut plumbing = AsyncPlumbing {
            private_inputs: HashMap::new(),
            async_wakes: vec![Vec::new(); self.systems.len()],
            copy_ins: Vec::new(),
        };
        for (sid, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Async {
                continue;
            }
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                // Only edge-connected snapshot inputs are copy-in decoupled; a
                // Host/SelfTap input is fed by its runner, not a producer edge.
                if port.delivery == Delivery::Log || port.conn != PortConn::Edge {
                    continue;
                }
                let (prod_id, out_idx) = cons_edges[&(sid, in_idx)][0];
                let private = alloc_ring(port.delivery, port.max_size, depth, 1 + slack);
                let data = Notifier::default();
                // The matched DATA notifier wakes the parked async `recv`; the
                // copy-in uses `try_write` and skips a full private ring. Each
                // private copy-in ring is created here and gets this one
                // writer, so the claim is always free.
                let writer = private
                    .writer(data.clone())
                    .expect("private copy-in ring has exactly one writer");
                let upstream = alloc.output_rings[prod_id][out_idx]
                    .view(NoWake)
                    .expect("producer reader slot reserved at sizing time");
                plumbing.copy_ins.push(CopyIn {
                    upstream,
                    writer,
                    last_committed: u64::MAX,
                });
                plumbing
                    .private_inputs
                    .insert((sid, in_idx), (private.clone(), data.clone()));
                plumbing.async_wakes[sid].push(data);
                alloc.table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port.id().component().expect("copy-in inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        input: in_idx,
                    },
                    instance: Some(sys.name.clone()),
                });
            }
        }
        plumbing
    }
}

// ---------------------------------------------------------------------------
// build() and its products
// ---------------------------------------------------------------------------

/// Validate the graph, size and allocate every ring, bind ports,
/// auto-provision health/log buffers, and assemble a ready coordinator.
///
/// One orchestrator over named passes, each handing its product to the
/// next: validation, edge resolution, fan-out counting, ring allocation,
/// registry freeze, copy-in planning, bind.
pub(crate) fn build(graph: InitGraph) -> Result<Coordinator, WireError> {
    graph.validate_cycle_rate()?;
    graph.validate_receive_all_last()?;
    graph.validate_slot_name_caps()?;
    graph.validate_port_axes()?;
    let cons_edges = graph.solve_edges()?;
    let fan_out = graph.count_fan_out(&cons_edges);
    let mut alloc = graph.alloc_rings(&cons_edges, &fan_out)?;
    let seq_registry = graph.seq_registry_payload();
    let registry = freeze_registry(std::mem::take(&mut alloc.reg_entries))?;
    let mut plumbing = graph.plan_copy_ins(&cons_edges, &mut alloc);
    let InitGraph {
        config,
        systems,
        worker_exe,
        wiring_manifest,
        ..
    } = graph;
    let proc_ctx = ProcBindCtx {
        step_timeout: config.proc_step_timeout,
        worker_exe,
        max_restarts: config.proc_max_restarts,
        restart_backoff: config.proc_restart_backoff,
    };
    let BoundSystems {
        cyclic,
        pending_async,
        coord,
    } = bind_systems(
        systems,
        &cons_edges,
        &alloc,
        &mut plumbing,
        &registry,
        &proc_ctx,
    )?;

    Ok(Coordinator {
        config,
        cyclic,
        pending_async,
        copy_ins: plumbing.copy_ins,
        coord_health: coord.health,
        status_out: coord.status_out,
        stopped: Vec::new(),
        stopped_scratch: Vec::new(),
        workers: Vec::new(),
        workers_scratch: Vec::new(),
        cycle: 0,
        progress: Arc::new(AtomicU64::new(0)),
        registry,
        control_out: Some(coord.control_out),
        seq_registry_out: coord.seq_registry_out,
        seq_registry,
        seq_registry_emitted: false,
        wiring_out: coord.wiring_out,
        wiring_manifest,
        wiring_emitted: false,
        reload_in: coord.reload_in,
        started: false,
        // Declared last so the canonical ring handles drop after every port.
        rings: alloc.table,
        session: alloc.session,
    })
}

/// The one connection map, `(consumer, input-index)` to the producer endpoints
/// explicitly wired into it. Product of [`InitGraph::solve_edges`]; consumed by
/// fan-out counting, copy-in planning, and the bind pass.
pub(crate) type ConsEdges = HashMap<(usize, usize), Vec<(usize, usize)>>;

/// The ring-allocation pass product: the canonical owning [`RingTable`], one
/// buffer row per system's outputs, the dedicated host-input rings, and the
/// build-order registry entries (drained by [`freeze_registry`]).
pub(crate) struct RingAlloc {
    pub(crate) table: RingTable,
    pub(crate) output_rings: Vec<Vec<RingBuffer>>,
    pub(crate) host_input_rings: HashMap<(usize, usize), RingBuffer>,
    pub(crate) reg_entries: Vec<RegistryEntry>,
    /// The run's shared-memory session, created lazily by the first
    /// file-backed ring; `None` for a graph with no process systems. Moves
    /// into the [`Coordinator`], which owns the directory's lifetime.
    pub(crate) session: Option<SessionDir>,
    /// The ring file behind each file-backed output buffer, for the worker
    /// manifests (`(system, out_idx)` → path).
    pub(crate) ring_paths: HashMap<(usize, usize), PathBuf>,
    /// The ring file behind each file-backed host-connected input (a process
    /// slot's control ring), for the worker manifests, keyed like
    /// `host_input_rings`.
    pub(crate) host_input_paths: HashMap<(usize, usize), PathBuf>,
}

/// The copy-in planning product: each async snapshot input's private ring plus
/// matched data notifier, the per-system wake lists (for teardown), and the
/// copy-in jobs.
pub(crate) struct AsyncPlumbing {
    pub(crate) private_inputs: HashMap<(usize, usize), (RingBuffer, Notifier)>,
    pub(crate) async_wakes: Vec<Vec<Notifier>>,
    pub(crate) copy_ins: Vec<CopyIn>,
}

/// The cheap connect-time guards: the endpoints name real systems and both
/// halves address the same port id. The full compatibility and structural
/// validation runs in [`InitGraph::solve_edges`]; this only catches the cheap
/// mistakes early, so the shim's fallible `connect` can surface them at the
/// call site while [`InitGraph::push_edge`] stays infallible.
pub(crate) fn check_edge(
    systems: &[Node],
    producer: PortRef,
    consumer: PortRef,
) -> Result<(), WireError> {
    if producer.system.id >= systems.len() {
        return Err(WireError::UnknownSystem {
            id: producer.system.id,
        });
    }
    if consumer.system.id >= systems.len() {
        return Err(WireError::UnknownSystem {
            id: consumer.system.id,
        });
    }
    if producer.port != consumer.port {
        return Err(WireError::PortIdMismatch {
            producer: producer.port,
            consumer: consumer.port,
        });
    }
    Ok(())
}

/// Freeze the one registry every consumer's bind pulls. Frames and channels
/// share one keyspace, so a same-instance name collision between a frame and a
/// channel (both `"<instance>.<name>"`) is detectable instead of shadowing.
pub(crate) fn freeze_registry(reg_entries: Vec<RegistryEntry>) -> Result<Arc<Registry>, WireError> {
    let mut seen_keys: HashMap<ComponentId, usize> = HashMap::new();
    for (i, e) in reg_entries.iter().enumerate() {
        if seen_keys.insert(e.key, i).is_some() {
            return Err(WireError::DuplicateRegistryKey {
                key: format!("{}.{}", e.instance, e.name()),
            });
        }
    }
    Ok(Arc::new(Registry::new(reg_entries)))
}

/// Find any directed cycle in the system graph (over the non-delayed edges),
/// returning its members in loop order, or `None` if the graph is acyclic. A
/// plain depth-first search colouring nodes white/grey/black; a back-edge to a
/// grey (on-stack) node closes a cycle, reconstructed from the DFS stack.
fn find_cycle(adj: &[Vec<usize>]) -> Option<Vec<usize>> {
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;
    let n = adj.len();
    let mut color = vec![WHITE; n];
    let mut stack: Vec<usize> = Vec::new();

    fn dfs(
        u: usize,
        adj: &[Vec<usize>],
        color: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        color[u] = GREY;
        stack.push(u);
        for &v in &adj[u] {
            match color[v] {
                GREY => {
                    // Back-edge: the cycle is the stack tail from `v` onward.
                    let start = stack.iter().position(|&x| x == v).unwrap_or(0);
                    return Some(stack[start..].to_vec());
                }
                WHITE => {
                    if let Some(c) = dfs(v, adj, color, stack) {
                        return Some(c);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        color[u] = BLACK;
        None
    }

    for s in 0..n {
        if color[s] == WHITE
            && let Some(c) = dfs(s, adj, &mut color, &mut stack)
        {
            return Some(c);
        }
    }
    None
}

/// The one ring-sizing helper. A snapshot port is sized at the configured
/// default depth (a latest-wins sample needs little history), a log port at
/// [`LOG_DEPTH`] (an every-record stream must absorb a slow tap).
pub(crate) fn alloc_ring(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> RingBuffer {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    RingBuffer::create_in_memory(Config {
        capacity: capacity_for(max_size, depth),
        max_readers,
    })
}

/// The mmap sibling of [`alloc_ring`]: identical sizing, but the region is a
/// file in the run's session directory, attachable by a worker process. An
/// I/O failure is a build-time [`WireError::Shm`].
fn alloc_ring_at(
    path: &std::path::Path,
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> Result<RingBuffer, WireError> {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    RingBuffer::create_mmap(
        path,
        Config {
            capacity: capacity_for(max_size, depth),
            max_readers,
        },
    )
    .map_err(|e| WireError::Shm {
        detail: format!("ring `{}`: {e}", path.display()),
    })
}

/// Mint the single [`MsgOut`] writer over a coordinator-owned ring, the
/// [`slot_writer`](slot::slot_writer) analogue for a message channel. Called
/// exactly once per ring at build; the region's writer claim enforces it.
pub(crate) fn owned_writer<M: Msg>(ring: &RingBuffer) -> MsgOut<M> {
    // Each coordinator-owned message ring gets its single writer minted
    // exactly once at build, so the claim is always free here.
    let writer = ring
        .writer(NoWake)
        .expect("coordinator message ring has exactly one writer");
    MsgOut::new(writer)
}

/// Worst-case record bytes for a [`WiringManifest`] carrying `ir_json`, the
/// `max_size` the coordinator sizes the `wiring` ring from. A record is the
/// 2-byte [`Msg::ID`] plus the postcard body: the `u32` `ir_version` (≤5-byte
/// varint), the JSON's length prefix (≤5-byte varint), and the JSON bytes.
/// Rounded up to a 1 KiB boundary for headroom, with the default message cap
/// as a floor so a small mission's ring is no smaller than an ordinary one.
fn wiring_manifest_max_size(ir_json: &str) -> usize {
    (ir_json.len() + 12)
        .next_multiple_of(1024)
        .max(MAX_MSG_BYTES)
}

/// The synthetic instance prefix coordinator-owned buffers register under:
/// they have no system instance, so their qualified key is
/// `coordinator.health` / `coordinator.log` / `coordinator.coordinator_status`.
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: the instance-qualified key over
/// a clone of the port's descriptor, capturing a clone of the ring as the
/// read source. The announce form is derived from the descriptor on demand.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer) -> RegistryEntry {
    RegistryEntry {
        key: ComponentId::new(&format!("{instance}.{}", port.name)),
        instance: Arc::from(instance),
        desc: port.clone(),
        ring,
    }
}
