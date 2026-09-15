//! The build passes: one validation gate, then trust.

use std::collections::HashMap;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer, checked_region_len};
use metor_proto::types::{ComponentId, Timestamp};

use crate::port::{Output, capacity_for};
use crate::{Componentize, Frame};

use super::config::CoordinatorConfig;
use super::error::BuildError;
use super::status::SystemStatus;
use super::table::{SystemTable, TableEntry};
use super::{Coordinator, Entry};

/// The output the coordinator appends to every system.
const STATUS_PORT: &str = "status";

/// One ring to allocate: the output it carries and the readers it must hold.
struct RingSpec {
    system: String,
    port: String,
    frame: ComponentId,
    max_size: usize,
    readers: usize,
}

/// One system with its type resolved, its rings placed, and its input edges
/// named by ring index.
struct Planned<'a> {
    id: &'a str,
    entry: &'a TableEntry,
    /// Index of this system's first output ring; its status ring is the one
    /// past its last output.
    base: usize,
    inputs: Vec<Vec<usize>>,
}

struct Plan<'a> {
    systems: Vec<Planned<'a>>,
    rings: Vec<RingSpec>,
}

impl Planned<'_> {
    fn status_ring(&self) -> usize {
        self.base + self.entry.def.outputs.len()
    }

    /// The ring behind one of this system's output ports, `status` included.
    /// Pass 1 rejects a declared output named `status`, so the two cannot
    /// collide.
    fn output_ring(&self, port: &str) -> Option<usize> {
        if port == STATUS_PORT {
            return Some(self.status_ring());
        }
        let index = self.entry.def.outputs.iter().position(|p| p.name == port)?;
        Some(self.base + index)
    }
}

impl Coordinator {
    /// Validate `config` against `table` and allocate the graph it describes.
    pub fn build(config: CoordinatorConfig, table: &SystemTable) -> Result<Self, BuildError> {
        let mut plan = resolve(&config, table)?;
        check_frames(&plan)?;
        count_readers(&plan.systems, &mut plan.rings, config.reader_slack);
        let rings = allocate(&plan.rings, config.depth)?;
        let entries = bind(&plan, &rings);
        Ok(Coordinator {
            entries,
            clock: config.clock,
            epoch: Timestamp::now(),
            cycle: 0,
            rings,
        })
    }
}

/// Pass 1: ids unique, types registered, every port reference known.
fn resolve<'a>(
    config: &'a CoordinatorConfig,
    table: &'a SystemTable,
) -> Result<Plan<'a>, BuildError> {
    let mut plan = Plan {
        systems: Vec::with_capacity(config.systems.len()),
        rings: Vec::new(),
    };
    let mut index = HashMap::with_capacity(config.systems.len());
    for system in &config.systems {
        if index
            .insert(system.id.as_str(), plan.systems.len())
            .is_some()
        {
            return Err(BuildError::DuplicateId {
                id: system.id.clone(),
            });
        }
        let entry = table
            .get(&system.ty)
            .ok_or_else(|| BuildError::UnknownType {
                id: system.id.clone(),
                ty: system.ty.clone(),
            })?;
        if entry.def.outputs.iter().any(|p| p.name == STATUS_PORT) {
            return Err(BuildError::ReservedPort {
                ty: system.ty.clone(),
            });
        }
        let planned = Planned {
            id: &system.id,
            entry,
            base: plan.rings.len(),
            inputs: vec![Vec::new(); entry.def.inputs.len()],
        };
        plan.rings.extend(
            entry
                .def
                .outputs
                .iter()
                .map(|port| RingSpec::new(&system.id, port.name, port.frame, port.max_size)),
        );
        plan.rings.push(RingSpec::new(
            &system.id,
            STATUS_PORT,
            SystemStatus::ID,
            SystemStatus::MAX_SIZE,
        ));
        plan.systems.push(planned);
    }
    resolve_edges(config, &mut plan, &index)?;
    Ok(plan)
}

/// Pass 1, second half: every edge, resolved once every id is known, so a
/// producer may be listed after its consumer.
fn resolve_edges(
    config: &CoordinatorConfig,
    plan: &mut Plan<'_>,
    index: &HashMap<&str, usize>,
) -> Result<(), BuildError> {
    for (i, system) in config.systems.iter().enumerate() {
        for input in &system.inputs {
            let def = &plan.systems[i].entry.def;
            let port = def
                .inputs
                .iter()
                .position(|p| p.name == input.port)
                .ok_or_else(|| BuildError::UnknownInput {
                    system: system.id.clone(),
                    port: input.port.clone(),
                })?;
            for from in &input.from {
                let producer =
                    *index
                        .get(from.system.as_str())
                        .ok_or_else(|| BuildError::UnknownSystem {
                            id: system.id.clone(),
                            port: input.port.clone(),
                            from: from.system.clone(),
                        })?;
                let ring = plan.systems[producer]
                    .output_ring(&from.port)
                    .ok_or_else(|| BuildError::UnknownOutput {
                        system: from.system.clone(),
                        port: from.port.clone(),
                    })?;
                plan.systems[i].inputs[port].push(ring);
            }
        }
    }
    Ok(())
}

/// Pass 2: the frame on both ends of every edge.
fn check_frames(plan: &Plan<'_>) -> Result<(), BuildError> {
    for system in &plan.systems {
        for (port, edges) in system.inputs.iter().enumerate() {
            let def = system.entry.def.inputs[port];
            for &ring in edges {
                let spec = &plan.rings[ring];
                if spec.frame != def.frame {
                    return Err(BuildError::FrameMismatch {
                        id: system.id.to_string(),
                        port: def.name.to_string(),
                        from: format!("{}.{}", spec.system, spec.port),
                        expected: def.frame,
                        found: spec.frame,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Pass 3: one reader slot per edge into a ring, plus the config's slack. A
/// ring with no edges and no slack still gets one slot. Pass 4 rejects a
/// count the ring format cannot hold.
fn count_readers(systems: &[Planned<'_>], rings: &mut [RingSpec], slack: usize) {
    for ring in rings.iter_mut() {
        ring.readers = slack.max(1);
    }
    for edge in systems.iter().flat_map(|s| s.inputs.iter().flatten()) {
        rings[*edge].readers = rings[*edge].readers.saturating_add(1);
    }
}

/// Pass 4: one ring per output and one per system's status.
fn allocate(specs: &[RingSpec], depth: usize) -> Result<Vec<RingBuffer>, BuildError> {
    specs.iter().map(|spec| ring_for(spec, depth)).collect()
}

fn ring_for(spec: &RingSpec, depth: usize) -> Result<RingBuffer, BuildError> {
    let capacity = capacity_for(spec.max_size, depth).ok_or_else(|| BuildError::RingTooLarge {
        system: spec.system.clone(),
        port: spec.port.clone(),
        max_size: spec.max_size,
    })?;
    let config = Config {
        capacity,
        max_readers: spec.readers,
    };
    if checked_region_len(&config).is_none() {
        return Err(BuildError::TooManyReaders {
            system: spec.system.clone(),
            port: spec.port.clone(),
            readers: spec.readers,
        });
    }
    Ok(RingBuffer::create_in_memory(config))
}

/// Pass 5: one writer per output, one view per edge, then the erased runner.
fn bind(plan: &Plan<'_>, rings: &[RingBuffer]) -> Vec<Entry> {
    plan.systems
        .iter()
        .map(|system| {
            let outputs = system.entry.def.outputs.len();
            let writers = (0..outputs)
                .map(|i| writer(&rings[system.base + i]))
                .collect();
            let views = system
                .inputs
                .iter()
                .map(|edges| {
                    edges
                        .iter()
                        // PANIC Safety: pass 3 sized every reader table by the
                        // edges counted here.
                        .map(|&ring| rings[ring].view(NoWake).expect("a counted reader slot"))
                        .collect()
                })
                .collect();
            Entry {
                name: system.id.to_string(),
                step: (system.entry.make)(views, writers),
                status: Output::new(writer(&rings[system.status_ring()])),
            }
        })
        .collect()
}

/// PANIC Safety: each ring backs exactly one output port, so its single writer
/// is claimed here once.
fn writer(ring: &RingBuffer) -> metor_fsw_3_ring::Writer<NoWake> {
    ring.writer(NoWake).expect("one writer per output ring")
}

impl RingSpec {
    fn new(system: &str, port: &str, frame: ComponentId, max_size: usize) -> Self {
        Self {
            system: system.to_string(),
            port: port.to_string(),
            frame,
            max_size,
            readers: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::fixtures::{self, Recorder, pipeline_config, table};
    use crate::coordinator::{InputConfig, PortRef, SystemConfig};
    use crate::{Frame, port};

    fn source(id: &str) -> SystemConfig {
        SystemConfig {
            id: id.into(),
            ty: "imu".into(),
            inputs: Vec::new(),
        }
    }

    fn build(config: CoordinatorConfig) -> Result<Coordinator, BuildError> {
        Coordinator::build(config, &table(&Recorder::default()))
    }

    #[test]
    fn a_two_system_config_builds_one_ring_per_output_and_status() {
        let mut config = pipeline_config();
        config.systems.truncate(2);
        config.systems[1].inputs[0].from = vec![PortRef::new("imu", "imu")];
        let coordinator = build(config).expect("valid config");
        // imu + nav outputs, plus one status ring each.
        assert_eq!(coordinator.rings(), 4);
        assert_eq!(
            coordinator.entry_names().collect::<Vec<_>>(),
            vec!["imu", "nav"]
        );
    }

    #[test]
    fn a_status_output_is_an_ordinary_ring() {
        let mut config = pipeline_config();
        config.systems.push(SystemConfig {
            id: "watch".into(),
            ty: "status_watch".into(),
            inputs: vec![InputConfig {
                port: "status".into(),
                from: vec![PortRef::new("nav", "status")],
            }],
        });
        assert!(build(config).is_ok());
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        let config = CoordinatorConfig {
            systems: vec![source("imu"), source("imu")],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::DuplicateId { id: "imu".into() })
        );
    }

    #[test]
    fn an_unregistered_type_is_rejected() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                id: "imu".into(),
                ty: "gyro".into(),
                inputs: Vec::new(),
            }],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownType {
                id: "imu".into(),
                ty: "gyro".into(),
            })
        );
    }

    #[test]
    fn an_edge_from_an_unknown_system_is_rejected() {
        let mut config = pipeline_config();
        config.systems[1].inputs[0].from = vec![PortRef::new("gyro", "imu")];
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownSystem {
                id: "nav".into(),
                port: "imu".into(),
                from: "gyro".into(),
            })
        );
    }

    #[test]
    fn an_unknown_consumer_port_is_rejected() {
        let mut config = pipeline_config();
        config.systems[1].inputs[0].port = "gyro".into();
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownInput {
                system: "nav".into(),
                port: "gyro".into(),
            })
        );
    }

    #[test]
    fn an_unknown_producer_port_is_rejected() {
        let mut config = pipeline_config();
        config.systems[1].inputs[0].from = vec![PortRef::new("imu", "gyro")];
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownOutput {
                system: "imu".into(),
                port: "gyro".into(),
            })
        );
    }

    #[test]
    fn an_edge_between_different_frames_is_rejected() {
        let mut config = pipeline_config();
        config.systems[2].inputs[0].from = vec![PortRef::new("imu", "imu")];
        assert_eq!(
            build(config).err(),
            Some(BuildError::FrameMismatch {
                id: "control".into(),
                port: "nav".into(),
                from: "imu.imu".into(),
                expected: fixtures::Nav::ID,
                found: fixtures::Imu::ID,
            })
        );
    }

    #[test]
    fn a_ring_larger_than_the_host_can_address_is_rejected() {
        let config = CoordinatorConfig {
            depth: usize::MAX,
            systems: vec![source("imu")],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::RingTooLarge {
                system: "imu".into(),
                port: "imu".into(),
                max_size: fixtures::Imu::MAX_SIZE,
            })
        );
        assert!(port::capacity_for(fixtures::Imu::MAX_SIZE, usize::MAX).is_none());
    }

    #[test]
    fn a_reader_count_past_the_ring_format_is_rejected() {
        let mut config = pipeline_config();
        config.reader_slack = usize::MAX;
        assert_eq!(
            build(config).err(),
            Some(BuildError::TooManyReaders {
                system: "imu".into(),
                port: "imu".into(),
                readers: usize::MAX,
            })
        );
    }

    #[test]
    fn no_slack_and_no_edges_still_builds() {
        let config = CoordinatorConfig {
            reader_slack: 0,
            systems: vec![source("imu")],
            ..Default::default()
        };
        assert!(build(config).is_ok());
    }

    #[test]
    fn a_declared_status_output_is_rejected() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                id: "r".into(),
                ty: "reserved".into(),
                inputs: Vec::new(),
            }],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::ReservedPort {
                ty: "reserved".into()
            })
        );
    }

    #[test]
    fn an_unconnected_input_is_legal() {
        let mut config = pipeline_config();
        config.systems[1].inputs.clear();
        assert!(build(config).is_ok());
    }
}
