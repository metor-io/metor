//! The build-time init pipeline: collect systems and edges into an [`InitGraph`],
//! then [`build`](InitGraph::build) it into a ready [`Coordinator`](super::Coordinator).
//!
//! An [`InitGraph`] is the wiring graph as plain data — the registered systems
//! (each a [`Node`]: its type-erased [`SystemBind`], its descriptor, its
//! instance name), the edges between their ports, and the run-scoped overrides.
//! [`build`](InitGraph::build) runs the passes in order — validation, edge resolution, fan-out
//! counting, ring allocation, registry freeze, copy-in planning, bind — each
//! handing its product to the next, and assembles the `Coordinator` literal.
//!
//! # Ordering invariants
//!
//! The whole model rests on one index. A [`Node`]'s position in `systems` is
//! its registration order *and* its cyclic step order, so a front-end pushes
//! systems in the order it wants them to run and nothing reorders them.
//!
//! - **Node #0 is the coordinator.** [`InitGraph::new`] registers the
//!   coordinator's own bundle at index 0 under the reserved name
//!   `"coordinator"`, before any user system.
//! - **Receive-all last.** A cyclic `ReceiveAll` (telemetry) system must be the
//!   last cyclic registration, so its end-of-cycle snapshot observes every
//!   system that stepped before it ([`WireError::ReceiveAllNotLast`]).
//! - **Positional bind.** The bind pass hands each system its rings by walking
//!   its descriptor's port lists in order, so a descriptor's port order is a
//!   fixed contract between sizing and binding.
//!
//! # Execution order
//!
//! Cyclic systems step in registration order, once per cycle. A snapshot edge
//! only observes the current cycle's value when it points forward in that
//! order, so [`build`](InitGraph::build) rejects a backward snapshot edge between cyclic systems
//! ([`WireError::StaleFrameEdge`]) and any feedback loop not broken by an
//! explicit [`connect_delayed`](InitGraph::connect_delayed) edge
//! ([`WireError::FeedbackCycle`]). One-cycle-late sampling is therefore always
//! a declared decision, never an accident of registration order. Log edges
//! carry decoupled event/command streams with no same-cycle dependency and are
//! exempt from both rules, as are edges touching an async endpoint.
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
//! Every ring's `max_readers` is fixed at build time: the counted edge fan-out
//! plus declared self-taps, one slot per receive-all capability, and
//! [`CoordinatorConfig::reader_slack`] spare slots for post-build [`Registry`]
//! taps. Exhausting the budget surfaces as an error at the claim site, not a
//! panic.
//!
//! # The coordinator's own bundle
//!
//! The coordinator registers itself as system #0 under the reserved instance
//! name `"coordinator"`, so its own channels are validated, sized, allocated,
//! and registered by the same passes as every system's; the bind pass wraps
//! the rings into the coordinator's fields instead of a cyclic slot, because
//! the coordinator is the loop rather than a member of it.

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
use crate::system::{AsyncSystem, CyclicRunner, CyclicSystem, HealthOutput};

use super::bind::{ProcBindCtx, bind_systems};
use super::slot::SlotReg;
use super::{
    AsyncLauncher, AsyncSlot, BoundSystems, BufferRole, ClockMode, CoordChannels, Coordinator,
    CoordinatorConfig, CoordinatorStatus, CopyIn, CyclicSlot, NAME_CAP, PortRef, RingEntry,
    RingTable, SlotState, SystemHandle, WireError,
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

impl<S> CyclicRegistration for CyclicReg<S>
where
    S: CyclicSystem + 'static,
    S::Output: HealthOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <S::Output as BindPorts>::bind(binder);
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

/// A pack entry as a cyclic registration: the bind-phase
/// [`Pending`](crate::pack::Pending) plus the entry's static display name, so a
/// pack entry rides the ordinary cyclic path with no dedicated `SystemBind`
/// variant.
struct PendingDriver {
    pending: crate::pack::Pending,
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
/// `Params` blob. At [`build`](InitGraph::build) it becomes a [`DlSlot`](crate::dl) instead of a
/// typed [`CyclicRunner`].
pub(crate) struct DlReg {
    pub(crate) system: crate::dl::DlSystem,
    pub(crate) params: Vec<u8>,
}

/// A registered process system: the artifact path a worker process will dlopen
/// plus its postcard `Params` blob (the host never loads the artifact itself).
/// At [`build`](InitGraph::build) it becomes a [`ProcSlot`](crate::proc).
pub(crate) struct ProcReg {
    pub(crate) artifact: PathBuf,
    pub(crate) params: Vec<u8>,
    /// The pack entry the worker instantiates.
    pub(crate) system: String,
}

pub(crate) enum SystemBind {
    /// The coordinator itself, system #0: a marker registration whose bind arm
    /// wraps the allocated rings into the coordinator's own fields (it is never
    /// pushed into `cyclic` — the coordinator is the loop).
    Coordinator,
    /// A created host system, erased; binds via a [`Binder`]. A pack entry rides
    /// this path too (via [`PendingDriver`]).
    Cyclic(Box<dyn CyclicRegistration>),
    /// An async system; yields a [`PendingAsync`], not a cyclic slot.
    Async(Box<dyn AsyncRegistration>),
    /// A dlopen'd cyclic system, bound to a [`DlSlot`](crate::dl) at [`build`](InitGraph::build).
    Dl(DlReg),
    /// A cross-process cyclic system, spawned as a worker and bound to a
    /// [`ProcSlot`](crate::proc) at [`build`](InitGraph::build).
    Proc(ProcReg),
    /// A runtime-swappable slot, bound to a [`SlotRunner`](super::slot::SlotRunner) at [`build`](InitGraph::build).
    Slot(SlotReg),
}

/// One registered system: its type-erased [`SystemBind`], its registered
/// descriptor, and its instance name.
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) desc: SystemDescriptor,
    pub(crate) bind: SystemBind,
}

/// Build a cyclic system's [`Node`] under `name` from its instance descriptor
/// (what this configured instance actually carries) and an erased [`CyclicReg`]
/// bind.
pub(crate) fn cyclic_node<S>(name: String, system: S) -> Node
where
    S: CyclicSystem + 'static,
    S::Output: HealthOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    let desc = system.instance_descriptor();
    Node {
        name,
        desc,
        bind: SystemBind::Cyclic(Box::new(CyclicReg { system })),
    }
}

/// Build an async system's [`Node`] under `name`, the [`cyclic_node`] twin.
pub(crate) fn async_node<S>(name: String, system: S) -> Node
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    let desc = system.instance_descriptor();
    Node {
        name,
        desc,
        bind: SystemBind::Async(Box::new(AsyncReg { system })),
    }
}

/// Build a pack entry's [`Node`] under `name`, running its create phase now so
/// a bad config fails at registration rather than at [`build`](InitGraph::build). The bind phase
/// runs at [`build`](InitGraph::build) along the ordinary cyclic path (via [`PendingDriver`]).
pub(crate) fn pending_node(
    name: String,
    entry: &mut crate::pack::PackEntry,
    params: crate::pack::EntryParams<'_>,
) -> Result<Node, crate::pack::MakeError> {
    let created = entry.create(params)?;
    let desc = created
        .instance_desc
        .unwrap_or_else(|| entry.descriptor().clone());
    let driver = PendingDriver {
        pending: created.pending,
        entry_name: entry.name(),
    };
    Ok(Node {
        name,
        desc,
        bind: SystemBind::Cyclic(Box::new(driver)),
    })
}

// ---------------------------------------------------------------------------
// The init graph
// ---------------------------------------------------------------------------

/// The wiring graph as plain data: the registered systems in registration
/// order, the edges between their ports, and the run-scoped overrides. Built up
/// by the wiring front-end ([`resolve`](crate::wiring::resolve)), then consumed
/// by [`build`](InitGraph::build).
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
    /// [`set_wiring_manifest`](Self::set_wiring_manifest), which also injects the
    /// matching `wiring` Host output onto the coordinator #0 bundle.
    pub(crate) wiring_manifest: Option<WiringManifest>,
    /// The mission namespace, prepended to every telemetry instance name at
    /// the registry/announce seam ([`qualify`](Self::qualify)). `None` leaves
    /// names and ids byte-identical to an un-namespaced mission. Set by the
    /// front-end from [`CoordinatorSpec::namespace`](crate::ir::CoordinatorSpec);
    /// deliberately not on [`CoordinatorConfig`], which stays `Copy`, and not on
    /// [`Node::name`], which wiring resolves against unprefixed.
    pub(crate) namespace: Option<String>,
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
            namespace: None,
        };
        // Register the coordinator's own channels as system #0, so they flow
        // through the same validate/size/allocate/register passes as every
        // system's (see the module docs). `commands` is untelemetered: inbound
        // control is never echoed on the downlink.
        let desc = SystemDescriptor {
            name: COORDINATOR_INSTANCE.into(),
            kind: SystemKind::Cyclic,
            inputs: vec![PortDesc::msg::<ReloadSequences>()],
            outputs: vec![
                PortDesc::of::<crate::SystemHealth>().with_conn(PortConn::Host),
                PortDesc::msg_named::<crate::LogEvent>("log").with_conn(PortConn::Host),
                PortDesc::of::<CoordinatorStatus>().with_conn(PortConn::Host),
                // Latest-wins boot state, not an event stream: Snapshot
                // delivery is what lets the downlink retain the newest
                // record for late-joining link connections.
                PortDesc::msg_named::<SequenceRegistry>("sequences")
                    .with_delivery(Delivery::Snapshot)
                    .with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceCommand>("commands")
                    .untelemetered()
                    .with_conn(PortConn::Host),
            ],
            capabilities: Vec::new(),
        };
        g.push_system(
            desc,
            COORDINATOR_INSTANCE.to_string(),
            SystemBind::Coordinator,
        );
        g
    }

    /// Record one registration; the returned handle indexes `systems`.
    pub(crate) fn push_system(
        &mut self,
        desc: SystemDescriptor,
        name: String,
        bind: SystemBind,
    ) -> SystemHandle {
        self.push_node(Node { bind, desc, name })
    }

    /// Record one pre-built [`Node`]; the returned handle indexes `systems`.
    /// Registration order is step order, so a front-end pushes in the order it
    /// wants the systems to run.
    pub(crate) fn push_node(&mut self, node: Node) -> SystemHandle {
        let id = self.systems.len();
        self.systems.push(node);
        SystemHandle { id }
    }

    /// The handle addressing the coordinator's own system-#0 bundle.
    pub(crate) fn coordinator_handle(&self) -> SystemHandle {
        SystemHandle { id: 0 }
    }

    /// Qualify a telemetry instance name with the mission
    /// [`namespace`](Self::namespace): `"sat1.<instance>"` when set, the bare
    /// name otherwise. This is the one seam the prefix rides — registry keys,
    /// the announce prefix, and file-backed ring names all pass through it,
    /// while wiring/edge resolution keeps using the unprefixed
    /// [`Node::name`]. The reserved `"coordinator"` bundle is qualified here
    /// too, since it registers as an ordinary node.
    fn qualify(&self, instance: &str) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}.{instance}"),
            None => instance.to_string(),
        }
    }

    /// The registered descriptor of `handle`.
    pub(crate) fn descriptor_of(&self, handle: SystemHandle) -> &SystemDescriptor {
        &self.systems[handle.id].desc
    }

    /// Store `manifest` and inject its `wiring` Host output onto the
    /// coordinator #0 bundle, sized from the concrete payload (a full IR
    /// overruns the default message cap; nothing raises the global cap).
    /// Called again, the latest manifest wins: the single injected port is
    /// replaced in place, never accumulated.
    pub(crate) fn set_wiring_manifest(&mut self, manifest: WiringManifest) {
        // Snapshot: the manifest is latest-wins boot state the downlink
        // retains for late-joining link connections.
        let mut port =
            PortDesc::msg_named::<WiringManifest>("wiring").with_delivery(Delivery::Snapshot);
        port.conn = PortConn::Host;
        port.max_size = wiring_manifest_max_size(&manifest.ir_json);
        let outputs = &mut self.systems[0].desc.outputs;
        match outputs.iter_mut().find(|p| p.name == "wiring") {
            Some(existing) => *existing = port,
            None => outputs.push(port),
        }
        self.wiring_manifest = Some(manifest);
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

    /// Record one edge; the full validation runs in [`solve_edges`] at
    /// [`build`](InitGraph::build). Infallible: resolve builds both [`PortRef`]s from descriptors
    /// it just looked up, so there is nothing to guard here.
    pub(crate) fn connect(&mut self, producer: PortRef, consumer: PortRef) {
        self.edges.push((producer, consumer, false));
    }

    /// Record one intentional one-cycle-delayed feedback edge (the back-edge of
    /// a control loop), excluded from cycle detection. The [`connect`](Self::connect)
    /// twin.
    pub(crate) fn connect_delayed(&mut self, producer: PortRef, consumer: PortRef) {
        self.edges.push((producer, consumer, true));
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

    fn validate_simulated_step(&self) -> Result<(), WireError> {
        if let ClockMode::Simulated { dt } = self.config.clock
            && dt.is_zero()
        {
            return Err(WireError::InvalidSimulatedStep { dt });
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
    /// `(cons_id, in_idx) -> [(prod_id, out_idx)]`. Every rule branches on a
    /// descriptor axis, never on frame-vs-message. Also runs the graph-shape
    /// checks: every feedback loop broken by a `connect_delayed`, registration
    /// order agreeing with the dataflow, every FanIn::One input connected.
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
            let out_idx = prod.outputs.iter().position(|d| d.id() == p.port).ok_or(
                WireError::UnknownPort {
                    system: p.system.id,
                    port: p.port,
                },
            )?;
            let in_idx = cons.inputs.iter().position(|d| d.id() == c.port).ok_or(
                WireError::UnknownPort {
                    system: c.system.id,
                    port: c.port,
                },
            )?;
            if !compatible(&prod.outputs[out_idx], &cons.inputs[in_idx]) {
                return Err(WireError::Incompatible {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
            let in_desc = &cons.inputs[in_idx];
            // A non-Edge input's counterpart is held by its runner, so an edge
            // into it is rejected; Host *outputs* keep accepting consumer edges.
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
        // A backward non-delayed snapshot edge between cyclic systems would
        // read last cycle's value forever: silent staleness that must be
        // declared with `connect_delayed`. Checked after cycle detection so an
        // unbroken loop reports the clearer `FeedbackCycle`. Log edges and
        // async endpoints are exempt (no step-order semantics); self-edges
        // never reach here (rejected above as a one-member cycle).
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

        // --- Input coverage: every FanIn::One Edge input has its one edge ---
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
    /// registry entries. Each receive-all capability counts one extra reader on
    /// *every* buffer's `max_readers`. A buffer in the
    /// [`shared_outputs`](Self::shared_outputs) set is allocated as an mmap
    /// ring file in the run's [`SessionDir`], path recorded for the worker
    /// manifests; a ring is backing-erased, so nothing downstream can tell.
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
                // The one prefixing seam: the registry key, the announce
                // prefix (via `RegistryEntry::instance`), and the ring file
                // name all key off this qualified name; wiring stays on
                // `sys.name`.
                let instance = self.qualify(&sys.name);
                let role = BufferRole::Output {
                    system: sid,
                    port: out_idx,
                };
                // One sizing path (depth by delivery, `alloc_ring`); only the
                // registry-entry shape still splits on the schema.
                let ring = if shared.contains(&(sid, out_idx)) {
                    let session = alloc.session.as_ref().expect("proc graphs have a session");
                    let path = session
                        .path()
                        .join(format!("{instance}.{}.ring", port.name));
                    let ring = alloc_ring_at(&path, port.delivery, port.max_size, depth, readers)?;
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
        // A Host input gets its own ring (its runner holds the writer); one
        // reader slot plus slack covers the occupant reload cycle. No registry
        // entry: inbound control, not an output. SelfTap inputs allocate
        // nothing (already counted in `fan_out`).
        for (sid, sys) in self.systems.iter().enumerate() {
            let instance = self.qualify(&sys.name);
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
                    let path = session
                        .path()
                        .join(format!("{instance}.{}.ring", port.name));
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
                    instance: Some(instance.clone()),
                });
                alloc.host_input_rings.insert((sid, in_idx), ring);
            }
        }

        Ok(alloc)
    }

    /// The boot `SequenceRegistry` payload: one spec per slot, keyed by the
    /// slot's instance name, the channel's wire address.
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

    /// Private copy-in buffers for async snapshot inputs, each with the matched
    /// data `Notifier` the async `recv` parks on (see the module docs); log
    /// inputs read the producers' rings directly, with no copy-in.
    fn plan_copy_ins(&self, cons_edges: &ConsEdges, alloc: &mut RingAlloc) -> AsyncPlumbing {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let mut plumbing = AsyncPlumbing {
            private_inputs: HashMap::new(),
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
                alloc.table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port
                        .id()
                        .component()
                        .expect("copy-in inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        input: in_idx,
                    },
                    instance: Some(self.qualify(&sys.name)),
                });
            }
        }
        plumbing
    }

    /// Validate the graph, size and allocate every ring, bind ports,
    /// auto-provision health/log buffers, and assemble a ready coordinator.
    ///
    /// One orchestrator over named passes, each handing its product to the
    /// next: validation, edge resolution, fan-out counting, ring allocation,
    /// registry freeze, copy-in planning, bind.
    pub(crate) fn build(self) -> Result<Coordinator, WireError> {
        let init_span = tracing::info_span!("init");
        let _init_span = init_span.enter();
        tracing::debug!(systems = self.systems.len(), "validating graph");
        self.validate_cycle_rate()?;
        self.validate_simulated_step()?;
        self.validate_receive_all_last()?;
        self.validate_slot_name_caps()?;
        self.validate_port_axes()?;
        let cons_edges = self.solve_edges()?;
        tracing::debug!(edges = cons_edges.len(), "edges solved");
        let fan_out = self.count_fan_out(&cons_edges);
        let mut alloc = self.alloc_rings(&cons_edges, &fan_out)?;
        tracing::debug!(rings = alloc.reg_entries.len(), "rings allocated");
        let seq_registry = self.seq_registry_payload();
        let registry = freeze_registry(std::mem::take(&mut alloc.reg_entries))?;
        let mut plumbing = self.plan_copy_ins(&cons_edges, &mut alloc);
        let InitGraph {
            config,
            systems,
            worker_exe,
            wiring_manifest,
            ..
        } = self;
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
        tracing::info!(
            cyclic = cyclic.len(),
            r#async = pending_async.len(),
            "graph bound"
        );

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
            channels: CoordChannels {
                control_out: Some(coord.control_out),
                seq_registry_out: coord.seq_registry_out,
                seq_registry,
                seq_registry_emitted: false,
                wiring_out: coord.wiring_out,
                wiring_manifest,
                wiring_emitted: false,
                reload_in: coord.reload_in,
            },
            started: false,
            // Declared last so the canonical ring handles drop after every port.
            rings: alloc.table,
            session: alloc.session,
        })
    }
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
/// matched data notifier and the copy-in jobs.
pub(crate) struct AsyncPlumbing {
    pub(crate) private_inputs: HashMap<(usize, usize), (RingBuffer, Notifier)>,
    pub(crate) copy_ins: Vec<CopyIn>,
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

/// The `Config` for one ring. A snapshot port is sized at the configured
/// default depth (a latest-wins sample needs little history), a log port at
/// [`LOG_DEPTH`] (an every-record stream must absorb a slow tap).
fn ring_config(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> Config {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    Config {
        capacity: capacity_for(max_size, depth),
        max_readers,
    }
}

/// Allocate a heap ring.
pub(crate) fn alloc_ring(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> RingBuffer {
    RingBuffer::create_in_memory(ring_config(delivery, max_size, default_depth, max_readers))
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
    RingBuffer::create_mmap(
        path,
        ring_config(delivery, max_size, default_depth, max_readers),
    )
    .map_err(|e| WireError::Shm {
        detail: format!("ring `{}`: {e}", path.display()),
    })
}

/// Mint the single [`MsgOut`] writer over a coordinator-owned ring, the
/// [`slot_writer`](super::slot::slot_writer) analogue for a message channel. Called
/// exactly once per ring at build; the region's writer claim enforces it.
pub(crate) fn owned_writer<M: Msg>(ring: &RingBuffer) -> MsgOut<M> {
    let writer = ring
        .writer(NoWake)
        .expect("coordinator message ring has exactly one writer");
    MsgOut::new(writer)
}

/// Worst-case record bytes for a [`WiringManifest`] carrying `ir_json`: the
/// 2-byte [`Msg::ID`] plus the postcard body (two ≤5-byte varints and the JSON
/// bytes), rounded up to 1 KiB, with the default message cap as a floor.
fn wiring_manifest_max_size(ir_json: &str) -> usize {
    (ir_json.len() + 12)
        .next_multiple_of(1024)
        .max(MAX_MSG_BYTES)
}

/// The reserved instance name the coordinator's own buffers register under
/// (`coordinator.health`, `coordinator.log`, ...).
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: the instance-qualified key over a
/// clone of the port's descriptor and a clone of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer) -> RegistryEntry {
    RegistryEntry {
        key: ComponentId::new(&format!("{instance}.{}", port.name)),
        instance: Arc::from(instance),
        desc: port.clone(),
        ring,
    }
}
