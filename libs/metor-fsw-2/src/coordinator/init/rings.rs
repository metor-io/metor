//! Ring sizing, allocation, and private async I/O boundaries.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use metor_fsw_2_core::{
    BoundInput, BoundPort, Delivery, FanIn, LOG_DEPTH, PortConn, PortDesc, PortSchema,
    RegistryEntry, SystemKind, checked_capacity_for,
};
use metor_fsw_ring::{Config, NoWake, Notifier, RingBuffer};
use metor_proto::types::ComponentId;

use super::{ConsEdges, InitGraph, SystemBind};
use crate::coordinator::{AsyncBoundary, BufferRole, RingEntry, RingTable, WireError};
use crate::io_bridge::{IoBridge, RingPump};
use crate::proc::session::SessionDir;

/// The ring-allocation pass product: the canonical owning [`RingTable`], one
/// buffer row per system's outputs, the dedicated host-input rings, and the
/// build-order registry entries (drained by [`super::freeze_registry`]).
pub(crate) struct RingAlloc {
    pub(crate) table: RingTable,
    pub(crate) output_rings: Vec<Vec<RingBuffer>>,
    pub(crate) host_input_rings: HashMap<(usize, usize), RingBuffer>,
    pub(crate) reg_entries: Vec<RegistryEntry>,
    /// Shared-memory session for process systems, owned by the coordinator.
    pub(crate) session: Option<SessionDir>,
    /// The ring file behind each file-backed output buffer, for the worker
    /// manifests (`(system, out_idx)` → path).
    pub(crate) ring_paths: HashMap<(usize, usize), PathBuf>,
    /// The ring file behind each file-backed host-connected input (a process
    /// slot's control ring), for the worker manifests, keyed like
    /// `host_input_rings`.
    pub(crate) host_input_paths: HashMap<(usize, usize), PathBuf>,
}

/// One async system's private typed ports and its graph-visible cycle boundary.
pub(crate) struct AsyncIoPlan {
    pub(crate) inputs: Vec<BoundInput>,
    pub(crate) outputs: Vec<BoundPort>,
    pub(crate) boundary: AsyncBoundary,
}

/// Async I/O plans keyed by the system's registration position.
pub(crate) type AsyncPlumbing = HashMap<usize, AsyncIoPlan>;

impl InitGraph {
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
    /// occupant *prefix* (the occupant's outputs and its Edge inputs'
    /// producers), because the runner tail (the `commands` fan-in, the
    /// self-tap, the status/events outputs) never leaves the coordinator.
    /// Everything else stays heap.
    pub(crate) fn shared_outputs(&self, cons_edges: &ConsEdges) -> HashSet<(usize, usize)> {
        let mut shared = HashSet::new();
        for (sid, sys) in self.systems.iter().enumerate() {
            // How much of the port lists a worker touches: all of a process
            // system's, the occupant prefix of a process slot's.
            let (n_outputs, n_inputs) = match &sys.bind {
                SystemBind::Proc(_) => (
                    crate::coordinator::bind::staged_outputs(&sys.desc),
                    sys.desc.inputs.len(),
                ),
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

    /// Allocate one buffer per output port (status/log included) plus a
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
        // At most one reader per input, receive-all capability, plus slack.
        // Check the sum before per-port arithmetic or any allocation.
        let maximum_readers = self
            .systems
            .iter()
            .try_fold(slack, |count, sys| {
                count
                    .checked_add(sys.desc.inputs.len())?
                    .checked_add(sys.desc.capabilities.len())
            })
            .and_then(|count| count.checked_add(1));
        if maximum_readers.is_none_or(|count| u32::try_from(count).is_err()) {
            return Err(WireError::InvalidRingSize {
                max_size: 0,
                depth,
                max_readers: slack,
            });
        }
        let receive_all_count = self
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

        // One buffer per output port
        for (sid, sys) in self.systems.iter().enumerate() {
            let mut row = Vec::with_capacity(sys.desc.outputs.len());
            for (out_idx, port) in sys.desc.outputs.iter().enumerate() {
                let readers =
                    fan_out.get(&(sid, out_idx)).copied().unwrap_or(0) + receive_all_count + slack;
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
                    alloc_ring(port.delivery, port.max_size, depth, readers)?
                };
                let entry = registry_entry(&instance, port, ring.clone());
                let frame_id = match &port.schema {
                    PortSchema::Table { .. } => port
                        .id()
                        .component()
                        .expect("table port keys on a ComponentId"),
                    PortSchema::Postcard { .. } => entry.key,
                };
                alloc.table.rings.push(RingEntry {
                    ring: ring.clone(),
                    frame_id,
                    role,
                    instance: Some(instance),
                });
                alloc.reg_entries.push(entry);
                row.push(ring);
            }
            alloc.output_rings.push(row);
        }

        // Dedicated rings for host-connected inputs
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
                    alloc_ring(port.delivery, port.max_size, depth, 1 + slack)?
                };
                alloc.table.rings.push(RingEntry {
                    ring: ring.clone(),
                    frame_id: port
                        .id()
                        .component()
                        .expect("v1 host-connected inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        port: in_idx,
                    },
                    instance: Some(instance.clone()),
                });
                alloc.host_input_rings.insert((sid, in_idx), ring);
            }
        }

        Ok(alloc)
    }

    /// Build one isolated, bidirectional ring boundary per async system.
    pub(super) fn plan_async_io(
        &self,
        cons_edges: &ConsEdges,
        alloc: &mut RingAlloc,
    ) -> Result<AsyncPlumbing, WireError> {
        let depth = self.config.default_depth;
        let mut plumbing = HashMap::new();
        for (sid, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Async {
                continue;
            }

            let mut imports = Vec::new();
            let mut bound_inputs = Vec::with_capacity(sys.desc.inputs.len());
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                assert_eq!(
                    port.conn,
                    PortConn::Edge,
                    "async systems expose only edge-connected typed inputs"
                );
                let producers = cons_edges
                    .get(&(sid, in_idx))
                    .map_or(&[][..], Vec::as_slice);
                let mut ports = Vec::with_capacity(producers.len());
                for &(prod_id, out_idx) in producers {
                    let private = alloc_ring(port.delivery, port.max_size, depth, 1)?;
                    let data = Notifier::default();
                    let writer = private
                        .writer(data.clone())
                        .expect("async private input has one boundary writer");
                    let upstream = alloc.output_rings[prod_id][out_idx]
                        .view(NoWake)
                        .expect("producer reader slot reserved at sizing time");
                    imports.push(RingPump::new(upstream, writer, port.delivery));
                    ports.push(BoundPort::matched(private.clone(), Box::new(data.clone())));
                    alloc.table.rings.push(RingEntry {
                        ring: private,
                        frame_id: port
                            .id()
                            .component()
                            .unwrap_or_else(|| ComponentId::new(&port.name)),
                        role: BufferRole::Private {
                            system: sid,
                            port: in_idx,
                        },
                        instance: Some(self.qualify(&sys.name)),
                    });
                }
                bound_inputs.push(match port.fan_in {
                    FanIn::One => BoundInput::One(
                        ports
                            .pop()
                            .expect("edge validation connected this async input"),
                    ),
                    FanIn::Many => BoundInput::Many(ports),
                });
            }

            let mut exports = Vec::with_capacity(sys.desc.outputs.len());
            let mut bound_outputs = Vec::with_capacity(sys.desc.outputs.len());
            for (out_idx, port) in sys.desc.outputs.iter().enumerate() {
                let private = alloc_ring(port.delivery, port.max_size, depth, 1)?;
                let source = private
                    .view(NoWake)
                    .expect("async private output has one boundary reader");
                let public = alloc.output_rings[sid][out_idx]
                    .writer(NoWake)
                    .expect("async boundary owns the public output writer");
                exports.push(RingPump::new(source, public, port.delivery));
                bound_outputs.push(BoundPort::new(private.clone()));
                alloc.table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port
                        .id()
                        .component()
                        .unwrap_or_else(|| ComponentId::new(&port.name)),
                    role: BufferRole::Private {
                        system: sid,
                        port: out_idx,
                    },
                    instance: Some(self.qualify(&sys.name)),
                });
            }

            let boundary = AsyncBoundary::new(
                Arc::<str>::from(sys.name.as_str()),
                IoBridge::new(imports, exports),
            );
            plumbing.insert(
                sid,
                AsyncIoPlan {
                    inputs: bound_inputs,
                    outputs: bound_outputs,
                    boundary,
                },
            );
        }
        Ok(plumbing)
    }
}

/// The `Config` for one ring. A snapshot port is sized at the configured
/// default depth (a latest-wins sample needs little history), a log port at
/// [`LOG_DEPTH`] (an every-record stream must absorb a slow tap).
fn ring_config(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> Result<Config, WireError> {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    let invalid = || WireError::InvalidRingSize {
        max_size,
        depth,
        max_readers,
    };
    let config = Config {
        capacity: checked_capacity_for(max_size, depth).ok_or_else(invalid)?,
        max_readers: max_readers.max(1),
    };
    metor_fsw_ring::checked_region_len(&config).ok_or_else(invalid)?;
    Ok(config)
}

/// Allocate a heap ring.
pub(crate) fn alloc_ring(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> Result<RingBuffer, WireError> {
    Ok(RingBuffer::create_in_memory(ring_config(
        delivery,
        max_size,
        default_depth,
        max_readers,
    )?))
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
        ring_config(delivery, max_size, default_depth, max_readers)?,
    )
    .map_err(|e| WireError::Shm {
        detail: format!("ring `{}`: {e}", path.display()),
    })
}

/// Build a [`RegistryEntry`] for one buffer: the instance-qualified key over a
/// clone of the port's descriptor and a clone of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer) -> RegistryEntry {
    RegistryEntry::new(
        ComponentId::new(&format!("{instance}.{}", port.name)),
        Arc::from(instance),
        port.clone(),
        ring,
    )
}
