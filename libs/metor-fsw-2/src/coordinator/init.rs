//! Collect systems and edges, then build a [`Coordinator`].
//!
//! Systems retain registration order throughout validation, allocation, and
//! binding. Index 0 belongs to the coordinator. Receive-all systems run last.
//! Snapshot edges must point forward unless explicitly delayed; log edges
//! have no same-cycle ordering requirement.
//!
//! Binding follows descriptor port order. Ring reader budgets include edge
//! fan-out, self-taps, receive-all capabilities, and configured spare slots.
//! Async systems exchange data through private rings at their registration
//! position in the cycle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use metor_fsw_2_core::{
    AnySource, BindPorts, Binder, CyclicRunner, CyclicSlot, CyclicSystem, Delivery, LogOutput,
    MAX_MSG_BYTES, MsgOut, PortConn, PortDesc, Registry, RegistryEntry, SlotState, StatusPort,
    SystemDescriptor, SystemKind, host_status_port,
};
use metor_fsw_ring::{NoWake, RingBuffer};
use metor_proto::types::Msg;
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelSpec, SequenceCommand, SequenceRegistry, WiringManifest,
};

use super::bind::{ProcBindCtx, bind_systems};
use super::slot::SlotReg;
use super::{
    AsyncLauncher, AsyncSlot, BoundSystems, CoordChannels, Coordinator, CoordinatorConfig,
    CoordinatorStatus, PortRef, SystemHandle, WireError,
};
use crate::async_system::AsyncSystem;

mod rings;
mod validate;

pub(crate) use rings::{AsyncIoPlan, AsyncPlumbing, RingAlloc};

pub(crate) trait CyclicRegistration {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot>;
}

struct CyclicReg<S> {
    system: S,
}

impl<S> CyclicRegistration for CyclicReg<S>
where
    S: CyclicSystem + 'static,
    S::Output: LogOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <S::Output as BindPorts>::bind(binder);
        Box::new(CyclicRunner::new(self.system, input, output))
    }
}

pub(crate) trait AsyncRegistration {
    /// Bind the bundles over `binder`; `status` is the system's own handle on
    /// the host-appended `system_status` output.
    fn bind(self: Box<Self>, binder: &mut Binder, status: StatusPort) -> Box<dyn AsyncLauncher>;
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
    fn bind(self: Box<Self>, binder: &mut Binder, status: StatusPort) -> Box<dyn AsyncLauncher> {
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <S::Output as BindPorts>::bind(binder);
        Box::new(AsyncSlot {
            system: self.system,
            input,
            output,
            status,
        })
    }
}

/// A pack entry as a cyclic registration: the bind-phase
/// [`Pending`](metor_fsw_2_core::Pending) plus the entry's static display name, so a
/// pack entry rides the ordinary cyclic path with no dedicated `SystemBind`
/// variant.
struct PendingDriver {
    pending: metor_fsw_2_core::Pending,
    entry_name: &'static str,
}

impl CyclicRegistration for PendingDriver {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        let mut src = AnySource::Host(binder);
        let driver = (self.pending)(&mut src, metor_fsw_2_core::Mount::Wired);
        Box::new(metor_fsw_2_core::DriverSlot {
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

/// A registered wired wasm system: the module bytes (shared across every
/// entry the artifact serves), the entry to instantiate, and its postcard
/// params. At [`build`](InitGraph::build) it becomes a
/// [`WasmCyclic`](crate::wasm::WasmCyclic) with its own interpreter instance.
pub(crate) struct WasmReg {
    pub(crate) bytes: Arc<Vec<u8>>,
    /// Manifest position of the entry, resolved by name at resolve time.
    pub(crate) index: u32,
    pub(crate) params: Vec<u8>,
}

pub(crate) enum SystemBind {
    /// The coordinator itself, system #0: a marker registration whose bind arm
    /// wraps the allocated rings into the coordinator's own fields (it is never
    /// pushed into `cyclic`, since the coordinator is the loop).
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
    /// A wired wasm pack entry, bound to a
    /// [`WasmCyclic`](crate::wasm::WasmCyclic) at [`build`](InitGraph::build).
    Wasm(WasmReg),
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
    S::Output: LogOutput + BindPorts + 'static,
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
    entry: &mut metor_fsw_2_core::PackEntry,
    params: metor_fsw_2_core::EntryParams<'_>,
) -> Result<Node, metor_fsw_2_core::MakeError> {
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
    /// The target IR to broadcast as a [`WiringManifest`], set by
    /// [`set_wiring_manifest`](Self::set_wiring_manifest), which also injects the
    /// matching `wiring` Host output onto the coordinator #0 bundle.
    pub(crate) wiring_manifest: Option<WiringManifest>,
    /// The target namespace, prepended to every telemetry instance name at
    /// the registry/announce seam ([`qualify`](Self::qualify)). `None` leaves
    /// names and ids byte-identical to an un-namespaced target. Set by the
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
                PortDesc::of::<crate::SystemStatus>().with_conn(PortConn::Host),
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
    ///
    /// Every node but the coordinator's own gets the host-written
    /// `system_status` output appended here, after everything it declared:
    /// the host publishes that record, so no system declares the port and no
    /// guest is ever staged its ring (see `bind::staged_outputs`).
    pub(crate) fn push_node(&mut self, mut node: Node) -> SystemHandle {
        if !matches!(node.bind, SystemBind::Coordinator) {
            node.desc.outputs.push(host_status_port());
        }
        let id = self.systems.len();
        self.systems.push(node);
        SystemHandle { id }
    }

    /// The handle addressing the coordinator's own system-#0 bundle.
    pub(crate) fn coordinator_handle(&self) -> SystemHandle {
        SystemHandle { id: 0 }
    }

    /// Qualify a telemetry instance name with the target
    /// [`namespace`](Self::namespace): `"sat1.<instance>"` when set, the bare
    /// name otherwise. This is the one seam the prefix rides; registry keys,
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

    /// Register a wired wasm pack entry under an explicit instance name.
    pub(crate) fn add_wasm_cyclic(
        &mut self,
        name: impl Into<String>,
        mut descriptor: SystemDescriptor,
        reg: WasmReg,
    ) -> SystemHandle {
        // Cyclic-only, pinned here like the dl path: the registered kind is
        // never trusted from decoded wire bytes.
        descriptor.kind = SystemKind::Cyclic;
        self.push_system(descriptor, name.into(), SystemBind::Wasm(reg))
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

    /// Validate the graph, size and allocate every ring, bind ports,
    /// auto-provision health/log buffers, and assemble a ready coordinator.
    pub(crate) fn build(self) -> Result<Coordinator, WireError> {
        let init_span = tracing::info_span!("init");
        let _init_span = init_span.enter();
        tracing::debug!(systems = self.systems.len(), "validating graph");
        let cycle_budget = self.cycle_budget()?;
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
        let mut plumbing = self.plan_async_io(&cons_edges, &mut alloc)?;
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
            wasm_fuel_per_poll: config.wasm_fuel_per_poll,
            wasm_memory_limit_bytes: config.wasm_memory_limit_bytes,
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

        let system_count = cyclic.len();
        Ok(Coordinator {
            config,
            cycle_budget,
            cyclic,
            pending_async,
            coord_log: coord.log,
            coord_status: coord.status,
            status_out: coord.status_out,
            stopped: Vec::with_capacity(system_count),
            stopped_scratch: Vec::with_capacity(system_count),
            workers: Vec::with_capacity(system_count),
            workers_scratch: Vec::with_capacity(system_count),
            cycle: 0,
            progress: Arc::new(AtomicU64::new(0)),
            registry,
            channels: CoordChannels {
                control_out: Some(coord.control_out),
                seq_registry_out: coord.seq_registry_out,
                seq_registry,
                wiring_out: coord.wiring_out,
                wiring_manifest,
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
/// fan-out counting, async-boundary planning, and the bind pass.
pub(crate) type ConsEdges = HashMap<(usize, usize), Vec<(usize, usize)>>;

/// Freeze the one registry every consumer's bind pulls. Frames and channels
/// share one keyspace, so a same-instance name collision between a frame and a
/// channel (both `"<instance>.<name>"`) is detectable instead of shadowing.
pub(crate) fn freeze_registry(reg_entries: Vec<RegistryEntry>) -> Result<Arc<Registry>, WireError> {
    let mut seen_keys = HashSet::with_capacity(reg_entries.len());
    for entry in &reg_entries {
        if !seen_keys.insert(entry.key) {
            return Err(WireError::DuplicateRegistryKey {
                key: format!("{}.{}", entry.instance, entry.name()),
            });
        }
    }
    Ok(Arc::new(Registry::new(reg_entries)))
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
/// (`coordinator.system_status`, `coordinator.log`, ...).
const COORDINATOR_INSTANCE: &str = "coordinator";
