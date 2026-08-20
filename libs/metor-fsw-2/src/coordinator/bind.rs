//! The bind pass: over the allocated rings, wrap every registration into its
//! cyclic slot, pending async system, or the coordinator's own ports.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use metor_fsw_ring::{NoWake, RingBuffer};
use metor_proto::types::Msg;
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelEvent, SequenceCommand, SequenceRegistry, WiringManifest,
};

use crate::Frame;
use metor_fsw_2_core::Input;
use metor_fsw_2_core::MsgIn;
use metor_fsw_2_core::Registry;
use metor_fsw_2_core::health::{HealthPort, LogEvent, SystemHealth};
use metor_fsw_2_core::sequence::{SequenceStatus, SlotControlIn};
use metor_fsw_2_core::{Binder, BoundInput, BoundPort};
use metor_fsw_2_core::{FanIn, PortConn, PortId, SystemDescriptor};

use super::init::{
    AsyncPlumbing, ConsEdges, DlReg, Node, ProcReg, RingAlloc, SystemBind, owned_writer,
};
use super::slot::{
    self, AllowedOccupant, OccupantBacking, SlotReg, SlotRunner, SlotStatus, slot_writer,
};
use super::{BoundSystems, CoordinatorPorts, CoordinatorStatus, PendingAsync, WireError};
use metor_fsw_2_core::CyclicSlot;

/// What the proc bind arm needs beyond the shared alloc products: the step
/// deadline and the worker-executable override, both builder-scoped.
pub(super) struct ProcBindCtx {
    pub(super) step_timeout: Duration,
    pub(super) worker_exe: Option<PathBuf>,
    pub(super) max_restarts: u32,
    pub(super) restart_backoff: Duration,
}

/// Build the typed `BoundPort`s a static (host-side) registration binds over:
/// the system's own output buffers, and its inputs in `descriptors()` order. A
/// `One` input views its producer's output; a `Many` input is a multi-view over
/// every wired producer ring. Async registrations use their prebuilt private
/// I/O plan instead of this direct path.
fn bind_static_io(
    id: usize,
    desc: &SystemDescriptor,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
) -> (Vec<BoundPort>, Vec<BoundInput>) {
    let outs: Vec<BoundPort> = (0..desc.outputs.len())
        .map(|out_idx| BoundPort::new(alloc.output_rings[id][out_idx].clone()))
        .collect();
    let ins: Vec<BoundInput> = (0..desc.inputs.len())
        .map(|in_idx| match desc.inputs[in_idx].fan_in {
            FanIn::One => {
                let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                BoundInput::One(BoundPort::new(alloc.output_rings[prod_id][out_idx].clone()))
            }
            FanIn::Many => {
                let ports = cons_edges
                    .get(&(id, in_idx))
                    .map(|producers| {
                        producers
                            .iter()
                            .map(|&(prod_id, out_idx)| {
                                BoundPort::new(alloc.output_rings[prod_id][out_idx].clone())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                BoundInput::Many(ports)
            }
        })
        .collect();
    (outs, ins)
}

/// Bind every system's ports over the allocated rings, consuming the
/// registrations. Each arm mirrors one registration kind; the static
/// (host-side) arms build their typed `BoundPort`s with [`bind_static_io`]
/// and walk them with a [`Binder`]. Only the proc arm can fail (its worker
/// spawn is the one bind-time step that leaves the process).
pub(super) fn bind_systems(
    systems: Vec<Node>,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
    plumbing: &mut AsyncPlumbing,
    registry: &Arc<Registry>,
    proc_ctx: &ProcBindCtx,
) -> Result<BoundSystems, WireError> {
    let mut cyclic: Vec<Box<dyn CyclicSlot>> = Vec::new();
    let mut pending_async: Vec<PendingAsync> = Vec::new();
    // The coordinator's own (#0) ports, wrapped by its bind arm below and
    // unwrapped after the loop (`SystemBind::Coordinator` is always registered first).
    let mut coord: Option<CoordinatorPorts> = None;
    for (id, node) in systems.into_iter().enumerate() {
        let Node { bind, desc, name } = node;
        match bind {
            SystemBind::Coordinator => {
                coord = Some(bind_coordinator(id, &desc, cons_edges, &alloc.output_rings))
            }
            SystemBind::Dl(dl) => cyclic.push(Box::new(bind_dl(
                id,
                dl,
                &desc,
                &name,
                cons_edges,
                &alloc.output_rings,
            ))),
            SystemBind::Proc(proc_reg) => cyclic.push(bind_proc(
                id, proc_reg, &desc, name, cons_edges, alloc, proc_ctx,
            )?),
            SystemBind::Slot(slot_reg) => cyclic.push(Box::new(bind_slot(
                id, slot_reg, &desc, cons_edges, alloc, proc_ctx,
            )?)),
            // The static (host-side) kinds: build typed `BoundPort`s
            // (`bind_static_io`) and walk them with a `Binder`. A pack entry
            // rides this arm too (via `PendingDriver`).
            SystemBind::Cyclic(r) => {
                let (outs, ins) = bind_static_io(id, &desc, cons_edges, alloc);
                let mut binder = Binder::new(&outs, &ins, registry.clone(), &name);
                cyclic.push(r.bind(&mut binder));
            }
            SystemBind::Async(r) => {
                let plan = plumbing
                    .plans
                    .remove(&id)
                    .expect("every async registration has one I/O plan");
                let super::init::AsyncIoPlan {
                    inputs: ins,
                    outputs: outs,
                    boundary,
                } = plan;
                let launcher = {
                    let mut binder = Binder::new(&outs, &ins, registry.clone(), &name);
                    r.bind(&mut binder)
                };
                pending_async.push(PendingAsync { name, launcher });
                cyclic.push(Box::new(boundary));
            }
        }
    }

    Ok(BoundSystems {
        cyclic,
        pending_async,
        // Always registered by InitGraph::new, so the unwrap is structural.
        coord: coord.expect("coordinator #0 bound its ports"),
    })
}

/// The coordinator's own bundle: a marker registration, not a cyclic slot (the
/// coordinator IS the loop). Its declared Host outputs were allocated and
/// registered by the uniform passes; wrap the writers into the coordinator's
/// ports here, single-writer by construction.
fn bind_coordinator(
    id: usize,
    desc: &SystemDescriptor,
    cons_edges: &ConsEdges,
    output_rings: &[Vec<RingBuffer>],
) -> CoordinatorPorts {
    let out_idx = |pid: PortId| {
        desc.outputs
            .iter()
            .position(|p| p.id() == pid)
            .expect("the coordinator #0 bundle declares this output")
    };
    let health_ring = &output_rings[id][out_idx(PortId::Component(SystemHealth::FRAME_ID))];
    let log_ring = &output_rings[id][out_idx(PortId::Packet(LogEvent::ID))];
    let mut health = HealthPort::new(
        slot_writer::<SystemHealth>(health_ring),
        owned_writer::<LogEvent>(log_ring),
    );
    health.set_instance(&desc.name);
    let status_idx = out_idx(PortId::Component(CoordinatorStatus::FRAME_ID));
    let status_out = slot_writer::<CoordinatorStatus>(&output_rings[id][status_idx]);
    let seq_registry_out = owned_writer::<SequenceRegistry>(
        &output_rings[id][out_idx(PortId::Packet(SequenceRegistry::ID))],
    );
    let control_out = owned_writer::<SequenceCommand>(
        &output_rings[id][out_idx(PortId::Packet(SequenceCommand::ID))],
    );
    // The registry-reload fan-in, shaped exactly like a slot's `commands`
    // input: one view per producer explicitly edged into it, zero edges legal.
    let reload_in_idx = desc
        .inputs
        .iter()
        .position(|p| p.conn == PortConn::Edge && p.id() == PortId::Packet(ReloadSequences::ID))
        .expect("the coordinator #0 bundle declares its ReloadSequences input");
    let reload_in = MsgIn::from_views(
        cons_edges
            .get(&(id, reload_in_idx))
            .map(|producers| {
                producers
                    .iter()
                    .map(|&(prod_id, out_idx)| {
                        output_rings[prod_id][out_idx]
                            .view(NoWake)
                            .expect("reload reader slot (edge fan-out sized)")
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    // The `wiring` output exists only when a front-end set a manifest; bind
    // its writer when the #0 bundle declared the port.
    let wiring_out = desc
        .outputs
        .iter()
        .position(|p| p.id() == PortId::Packet(WiringManifest::ID))
        .map(|idx| owned_writer::<WiringManifest>(&output_rings[id][idx]));
    CoordinatorPorts {
        health,
        status_out,
        seq_registry_out,
        control_out,
        reload_in,
        wiring_out,
    }
}

/// A dlopen'd system binds over raw `FswRing` regions, not typed `BoundPort`s:
/// gather the same per-port rings the coordinator allocated as
/// `(base, len, role)` handles in `descriptors()` order and hand them to a
/// `DlSlot`.
fn bind_dl(
    id: usize,
    dl: DlReg,
    desc: &SystemDescriptor,
    name: &str,
    cons_edges: &ConsEdges,
    output_rings: &[Vec<RingBuffer>],
) -> crate::dl::DlSlot {
    use metor_fsw_2_core::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
    let outputs: Vec<FswRing> = (0..desc.outputs.len())
        .map(|out_idx| {
            let (base, len) = output_rings[id][out_idx].region();
            FswRing {
                base,
                len,
                role: ROLE_OUTPUT,
            }
        })
        .collect();
    let inputs: Vec<FswRing> = (0..desc.inputs.len())
        .map(|in_idx| {
            let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
            let (base, len) = output_rings[prod_id][out_idx].region();
            FswRing {
                base,
                len,
                role: ROLE_INPUT,
            }
        })
        .collect();
    // SAFETY: every region named here is a `RingTable`-owned ring that outlives
    // the slot; the coordinator drops `cyclic` (this slot, whose `Drop` calls
    // `fsw_destroy`) before `rings`. The `DlSystem` handle drops right after;
    // the slot keeps its own `Arc<Library>`.
    // Status identity stays type-level (`desc.name`, like a static system's
    // `System::NAME`); the instance name rides separately for log attribution.
    unsafe {
        dl.system.make_slot(
            &dl.params,
            inputs,
            outputs,
            &desc.name,
            name,
            crate::Mount::Wired,
        )
    }
}

/// The proc twin of [`bind_dl`]: gather the same per-port rings, but as
/// session-dir *file paths* for the worker to attach (in the identical
/// positional order, so the worker-side bind contract is untouched), plus
/// host handles of the same rings for death reclamation; then write the
/// manifest, spawn the worker, and wait for it to attach.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bind_proc(
    id: usize,
    proc_reg: ProcReg,
    desc: &SystemDescriptor,
    name: String,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
    ctx: &ProcBindCtx,
) -> Result<Box<dyn CyclicSlot>, WireError> {
    use crate::proc::host::{ProcSlot, SpawnSpec};
    let session = alloc.session.as_ref().expect("proc graphs have a session");
    // Every ring the worker touches, as (host handle, file path): this
    // system's own outputs, then the producer ring behind each input.
    let mut rings: Vec<RingBuffer> = Vec::new();
    let output_paths: Vec<PathBuf> = (0..desc.outputs.len())
        .map(|out_idx| {
            rings.push(alloc.output_rings[id][out_idx].clone());
            alloc.ring_paths[&(id, out_idx)].clone()
        })
        .collect();
    let input_paths: Vec<PathBuf> = (0..desc.inputs.len())
        .map(|in_idx| {
            let (prod, out) = cons_edges[&(id, in_idx)][0];
            rings.push(alloc.output_rings[prod][out].clone());
            alloc.ring_paths[&(prod, out)].clone()
        })
        .collect();
    let spec = SpawnSpec {
        instance: name.clone(),
        system: proc_reg.system,
        artifact: proc_reg.artifact,
        params: proc_reg.params,
        ctl_path: session.path().join(format!("{name}.ctl")),
        manifest_path: session.path().join(format!("{name}.manifest")),
        input_paths,
        output_paths,
        rings,
        worker_exe: ctx.worker_exe.clone(),
        step_timeout: ctx.step_timeout,
        max_restarts: ctx.max_restarts,
        restart_backoff: ctx.restart_backoff,
        name: Arc::from(name.as_str()),
    };
    ProcSlot::spawn(spec)
        .map(|slot| Box::new(slot) as Box<dyn CyclicSlot>)
        .map_err(|detail| WireError::ProcSpawn {
            system: name,
            detail,
        })
}

/// Without a cross-process futex there is no worker protocol; the
/// registration is rejected cleanly at `build()`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn bind_proc(
    _id: usize,
    _proc_reg: ProcReg,
    _desc: &SystemDescriptor,
    name: String,
    _cons_edges: &ConsEdges,
    _alloc: &RingAlloc,
    _ctx: &ProcBindCtx,
) -> Result<Box<dyn CyclicSlot>, WireError> {
    Err(WireError::ProcSpawn {
        system: name,
        detail: "process systems need a cross-process futex (Linux or macOS 14.4+); \
                 unsupported on this target"
            .into(),
    })
}

/// A runtime slot: gather the same per-port regions as the dl arm, but locate
/// the runner's tail ports by their declared shape and hand the runner the
/// control/status writers. No occupant is created here; only `init`/`Load`
/// (runtime) does — for a process slot that also means **no worker is
/// spawned at build**, only the per-occupant manifests are written
/// ([`slot_proc_parts`]).
fn bind_slot(
    id: usize,
    slot_reg: SlotReg,
    desc: &SystemDescriptor,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
    proc_ctx: &ProcBindCtx,
) -> Result<SlotRunner, WireError> {
    use metor_fsw_2_core::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
    let SlotReg {
        allowed,
        initial,
        ports,
        process,
    } = slot_reg;
    let n_occ_inputs = ports.occupant_inputs.len();
    let n_occ_outputs = ports.occupant_outputs.len();
    let proc = if process {
        Some(slot_proc_parts(
            id, desc, &allowed, &ports, cons_edges, alloc, proc_ctx,
        )?)
    } else {
        None
    };
    let output_rings = &alloc.output_rings;
    // The prefix/tail invariant: the occupant's ports are the prefix of each
    // registered list, in the occupant descriptor's own order, so the occupant
    // `FswRing` arrays are a straight prefix map (Edge inputs view their
    // producers; the Host `SlotControlIn` input its dedicated ring) and the
    // occupant-side positional bind contract (the dl ABI) is untouched.
    let inputs: Vec<FswRing> = (0..n_occ_inputs)
        .map(|in_idx| {
            let (base, len) = match desc.inputs[in_idx].conn {
                PortConn::Edge => {
                    let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                    output_rings[prod_id][out_idx].region()
                }
                PortConn::Host => alloc.host_input_rings[&(id, in_idx)].region(),
                PortConn::SelfTap(_) => {
                    unreachable!("the occupant input prefix holds no self-tap")
                }
            };
            FswRing {
                base,
                len,
                role: ROLE_INPUT,
            }
        })
        .collect();
    // Occupant outputs are the prefix of the slot's own buffers (user outputs,
    // SequenceStatus, health, log, in descriptor order).
    let outputs: Vec<FswRing> = (0..n_occ_outputs)
        .map(|out_idx| {
            let (base, len) = output_rings[id][out_idx].region();
            FswRing {
                base,
                len,
                role: ROLE_OUTPUT,
            }
        })
        .collect();

    // --- The runner's tail ports, read straight off the port plan --------------
    // Host cancel writer over the SlotControlIn input's dedicated ring.
    let control_in_idx = ports.control_in_idx();
    debug_assert_eq!(
        desc.inputs[control_in_idx].conn,
        PortConn::Host,
        "the port plan and the registered descriptor agree on the control input"
    );
    let control = slot_writer::<SlotControlIn>(&alloc.host_input_rings[&(id, control_in_idx)]);
    // The slot's command fan-in: one view per producer explicitly edged into
    // the declared `commands` input (zero edges is a legal, command-less slot).
    let cmd_in_idx = ports.commands_in_idx();
    debug_assert_eq!(
        desc.inputs[cmd_in_idx].id(),
        PortId::Packet(SequenceCommand::ID),
        "the port plan and the registered descriptor agree on the commands input"
    );
    let commands = MsgIn::from_views(
        cons_edges
            .get(&(id, cmd_in_idx))
            .map(|producers| {
                producers
                    .iter()
                    .map(|&(prod_id, out_idx)| {
                        output_rings[prod_id][out_idx]
                            .view(NoWake)
                            .expect("command reader slot (edge fan-out sized)")
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    // The declared self-tap over the occupant's own SequenceStatus output (+1
    // fan-out counted at sizing): Progress plus outcome.
    let seq_out_idx = ports.seq_status_out_idx();
    debug_assert_eq!(
        desc.outputs[seq_out_idx].id(),
        PortId::Component(SequenceStatus::FRAME_ID),
        "the port plan and the registered descriptor agree on the self-tap target"
    );
    let seq_status = Input::new(
        output_rings[id][seq_out_idx]
            .view(NoWake)
            .expect("SequenceStatus self-tap reader (fan-out sized)"),
    );
    // Host writers over the runner's output tail: SlotStatus plus the
    // "sequences" events channel (real output indices, no side allocation).
    let status_out = slot_writer::<SlotStatus>(&output_rings[id][ports.status_out_idx()]);
    let events = owned_writer::<SequenceChannelEvent>(&output_rings[id][ports.events_out_idx()]);

    Ok(SlotRunner::new(
        Arc::from(desc.name.as_str()),
        allowed,
        initial,
        inputs,
        outputs,
        control,
        status_out,
        events,
        seq_status,
        commands,
        proc,
    ))
}

/// The proc side of the slot bind, the [`bind_proc`] twin: gather the
/// occupant prefix's rings as session-dir *paths* in the same positional
/// order the `FswRing` arrays use (so the worker-side bind contract is
/// untouched), collect the host handles of the same rings for reclamation
/// after each worker ends, and write one sequence-mode manifest per allowed
/// occupant — the rings are the slot's, so the manifests differ only in
/// artifact and params, and a runtime `Load` just picks one and spawns.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn slot_proc_parts(
    id: usize,
    desc: &SystemDescriptor,
    allowed: &[AllowedOccupant],
    ports: &slot::SlotPorts,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
    ctx: &ProcBindCtx,
) -> Result<slot::ProcParts, WireError> {
    use crate::proc::host::resolve_worker_exe;
    use crate::proc::worker::{RunMode, WorkerManifest};
    let spawn_err = |detail: String| WireError::ProcSpawn {
        system: desc.name.to_string(),
        detail,
    };
    let session = alloc
        .session
        .as_ref()
        .expect("process-slot graphs have a session");
    // Every ring an occupant worker attaches, as (host handle, file path):
    // the occupant prefix's own outputs, then the ring behind each prefix
    // input (an Edge input's producer, the Host control ring's own file).
    let mut rings: Vec<RingBuffer> = Vec::new();
    let output_paths: Vec<PathBuf> = (0..ports.occupant_outputs.len())
        .map(|out_idx| {
            rings.push(alloc.output_rings[id][out_idx].clone());
            alloc.ring_paths[&(id, out_idx)].clone()
        })
        .collect();
    let input_paths: Vec<PathBuf> = (0..ports.occupant_inputs.len())
        .map(|in_idx| match desc.inputs[in_idx].conn {
            PortConn::Edge => {
                let (prod, out) = cons_edges[&(id, in_idx)][0];
                rings.push(alloc.output_rings[prod][out].clone());
                alloc.ring_paths[&(prod, out)].clone()
            }
            PortConn::Host => {
                rings.push(alloc.host_input_rings[&(id, in_idx)].clone());
                alloc.host_input_paths[&(id, in_idx)].clone()
            }
            PortConn::SelfTap(_) => {
                unreachable!("the occupant input prefix holds no self-tap")
            }
        })
        .collect();
    let exe =
        resolve_worker_exe(ctx.worker_exe.as_deref()).map_err(|e| spawn_err(e.to_string()))?;
    let ctl_path = session.path().join(format!("{}.ctl", desc.name));
    let manifests = allowed
        .iter()
        .map(|occ| {
            let OccupantBacking::Artifact(artifact) = &occ.backing else {
                unreachable!("add_slot pins a process slot's occupants to artifact backings");
            };
            let manifest = WorkerManifest::Run {
                abi_version: metor_fsw_2_core::abi::FSW_ABI_VERSION,
                mode: RunMode::Sequence,
                // The worker-side identity is the slot's, whoever occupies it,
                // matching the in-process `make_slot(.., self.name)`.
                instance: desc.name.to_string(),
                system: occ.name.clone(),
                artifact: artifact.clone(),
                params: occ.params.clone(),
                ctl: ctl_path.clone(),
                inputs: input_paths.clone(),
                outputs: output_paths.clone(),
            };
            let path = session
                .path()
                .join(format!("{}.{}.manifest", desc.name, occ.name));
            std::fs::write(
                &path,
                postcard::to_allocvec(&manifest).expect("manifest encodes (postcard)"),
            )
            .map_err(|e| spawn_err(format!("manifest: {e}")))?;
            Ok(path)
        })
        .collect::<Result<Vec<_>, WireError>>()?;
    Ok(slot::ProcParts {
        manifests,
        ctl_path,
        exe,
        rings,
        step_timeout: ctx.step_timeout,
    })
}

/// Without a cross-process futex there is no worker protocol; the process
/// slot is rejected cleanly at `build()`, like a process system.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn slot_proc_parts(
    _id: usize,
    desc: &SystemDescriptor,
    _allowed: &[AllowedOccupant],
    _ports: &slot::SlotPorts,
    _cons_edges: &ConsEdges,
    _alloc: &RingAlloc,
    _ctx: &ProcBindCtx,
) -> Result<slot::ProcParts, WireError> {
    Err(WireError::ProcSpawn {
        system: desc.name.to_string(),
        detail: "process slots need a cross-process futex (Linux or macOS 14.4+); \
                 unsupported on this target"
            .into(),
    })
}
