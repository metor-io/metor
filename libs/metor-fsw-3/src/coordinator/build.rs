//! Build is responsible for creating new ring buffers for systems and linking them together
//!
//! It takes a pass through the system table to discover all the connections between systems, and
//! then allocated the rings with the appropriate reader capacity.

use std::collections::HashMap;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer, checked_region_len};
use metor_proto::types::{ComponentId, Timestamp};

use crate::port::{Output, ring_capacity};

use super::config::CoordinatorConfig;
use super::error::BuildError;
use super::status::SystemStatus;
use super::table::{SystemTable, TableEntry};
use super::{Coordinator, Entry};

const STATUS_PORT: &str = "status";

struct RingSpec {
    system: String,
    port: String,
    id: ComponentId,
    max_len: usize,
    depth: usize,
    readers: usize,
}

type RingIndex = usize;

struct PlannedSystem<'a> {
    id: &'a str,
    entry: &'a TableEntry,
    base_ring_idx: RingIndex,
    inputs: Vec<Vec<RingIndex>>,
}

struct Plan<'a> {
    systems: Vec<PlannedSystem<'a>>,
    rings: Vec<RingSpec>,
}

impl PlannedSystem<'_> {
    fn status_ring(&self) -> RingIndex {
        self.base_ring_idx + self.entry.def.outputs.len()
    }

    /// Returns the ring index for a given output port, or `None` if the port is not declared.
    fn output_ring(&self, port: &str) -> Option<RingIndex> {
        if port == STATUS_PORT {
            return Some(self.status_ring());
        }
        let index = self.entry.def.outputs.iter().position(|p| p.name == port)?;
        Some(self.base_ring_idx + index)
    }
}

impl CoordinatorConfig {
    /// Builds a [`Coordinator`] from this config and the given system table
    pub fn build(self, table: &SystemTable) -> Result<Coordinator, BuildError> {
        self.clock.validate()?;
        let mut plan = resolve(&self, table)?;
        check_ids(&plan)?;
        count_readers(&plan.systems, &mut plan.rings);
        let rings = allocate_rings(&plan.rings, self.ring_depth)?;
        let entries = bind_rings(&plan, &rings);

        Ok(Coordinator {
            entries,
            clock: self.clock,
            epoch: Timestamp::now(),
            cycle: 0,
            rings,
        })
    }
}

fn resolve<'a>(
    config: &'a CoordinatorConfig,
    table: &'a SystemTable,
) -> Result<Plan<'a>, BuildError> {
    let mut plan = Plan {
        systems: Vec::with_capacity(config.systems.len()),
        rings: vec![],
    };

    let mut index = HashMap::with_capacity(config.systems.len());

    for system in &config.systems {
        if index.contains_key(system.id.as_str()) {
            return Err(BuildError::DuplicateId {
                id: system.id.clone(),
            });
        }

        index.insert(system.id.as_str(), plan.systems.len());

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
        check_alignment(&system.id, &entry.def)?;
        let planned = PlannedSystem {
            id: &system.id,
            entry,
            base_ring_idx: plan.rings.len(),
            inputs: vec![Vec::new(); entry.def.inputs.len()],
        };
        plan.rings.extend(
            entry
                .def
                .outputs
                .iter()
                .map(|port| RingSpec::new(&system.id, port)),
        );
        plan.rings.push(RingSpec::new(
            &system.id,
            &Output::<SystemStatus>::def(STATUS_PORT),
        ));
        plan.systems.push(planned);
    }
    resolve_edges(config, &mut plan, &index)?;
    Ok(plan)
}

fn check_alignment(system: &str, def: &crate::SystemDef) -> Result<(), BuildError> {
    for port in def.inputs.iter().chain(&def.outputs) {
        if port.alignment > metor_fsw_3_ring::PAYLOAD_ALIGNMENT {
            return Err(BuildError::UnsupportedFrameAlignment {
                system: system.to_string(),
                port: port.name.to_string(),
                alignment: port.alignment,
            });
        }
    }
    Ok(())
}

fn resolve_edges(
    config: &CoordinatorConfig,
    plan: &mut Plan<'_>,
    index: &HashMap<&str, usize>,
) -> Result<(), BuildError> {
    for (i, system) in config.systems.iter().enumerate() {
        for input in &system.inputs {
            let def = &plan.systems[i].entry.def;
            let Some(port) = def.inputs.iter().position(|p| p.name == input.port) else {
                return Err(BuildError::UnknownInput {
                    system: system.id.clone(),
                    port: input.port.clone(),
                });
            };
            for from in &input.from {
                let Some(&producer) = index.get(from.system.as_str()) else {
                    return Err(BuildError::UnknownSystem {
                        id: system.id.clone(),
                        port: input.port.clone(),
                        from: from.system.clone(),
                    });
                };
                let Some(ring) = plan.systems[producer].output_ring(&from.port) else {
                    return Err(BuildError::UnknownOutput {
                        system: from.system.clone(),
                        port: from.port.clone(),
                    });
                };
                plan.systems[i].inputs[port].push(ring);
            }
        }
    }
    Ok(())
}

fn check_ids(plan: &Plan<'_>) -> Result<(), BuildError> {
    for system in &plan.systems {
        for (port, edges) in system.inputs.iter().enumerate() {
            let def = system.entry.def.inputs[port];
            for &ring in edges {
                let spec = &plan.rings[ring];
                if spec.id != def.id {
                    return Err(BuildError::IdMismatch {
                        id: system.id.to_string(),
                        port: def.name.to_string(),
                        from: format!("{}.{}", spec.system, spec.port),
                        expected: def.id,
                        found: spec.id,
                    });
                }
            }
        }
    }
    Ok(())
}

/// One reader slot per edge into a ring. A ring with no edges keeps one slot,
/// the fewest the ring format allows.
fn count_readers(systems: &[PlannedSystem<'_>], rings: &mut [RingSpec]) {
    for edge in systems.iter().flat_map(|s| s.inputs.iter().flatten()) {
        rings[*edge].readers += 1;
    }
    for ring in rings.iter_mut() {
        ring.readers = ring.readers.max(1);
    }
}

fn allocate_rings(specs: &[RingSpec], depth: usize) -> Result<Vec<RingBuffer>, BuildError> {
    specs
        .iter()
        .map(|spec| allocate_ring(spec, depth))
        .collect()
}

/// Allocates one ring holding `spec.depth * ring_depth` records.
fn allocate_ring(spec: &RingSpec, ring_depth: usize) -> Result<RingBuffer, BuildError> {
    let too_large = || BuildError::RingTooLarge {
        system: spec.system.clone(),
        port: spec.port.clone(),
        max_len: spec.max_len,
    };
    let depth = spec.depth.checked_mul(ring_depth).ok_or_else(too_large)?;
    let capacity = ring_capacity(spec.max_len, depth).ok_or_else(too_large)?;
    let config = Config {
        capacity,
        max_readers: spec.readers,
    };
    // The capacity fits `usize`, but the region around it may not.
    if checked_region_len(&config).is_none() {
        return Err(too_large());
    }
    Ok(RingBuffer::create_in_memory(config))
}

fn bind_rings(plan: &Plan<'_>, rings: &[RingBuffer]) -> Vec<Entry> {
    plan.systems
        .iter()
        .map(|system| {
            let outputs = system.entry.def.outputs.len();
            let writers = (0..outputs)
                .map(|i| {
                    let ring: &RingBuffer = &rings[system.base_ring_idx + i];
                    ring.writer(NoWake).expect("one writer per output ring")
                })
                .collect();
            let views = system
                .inputs
                .iter()
                .map(|edges| {
                    edges
                        .iter()
                        // PANIC Safety: The ring allocation pass will allocate a ring for every input
                        .map(|&ring| rings[ring].view(NoWake).expect("a counted reader slot"))
                        .collect()
                })
                .collect();
            Entry {
                name: system.id.to_string(),
                step: (system.entry.make)(views, writers),
                // PANIC Safety: SystemStatus requires only eight-byte alignment.
                status: Output::try_new({
                    let ring: &RingBuffer = &rings[system.status_ring()];
                    ring.writer(NoWake).expect("one writer per output ring")
                })
                .expect("supported status alignment"),
            }
        })
        .collect()
}

impl RingSpec {
    fn new(system: &str, port: &crate::PortDef) -> Self {
        Self {
            system: system.to_string(),
            port: port.name.to_string(),
            id: port.id,
            max_len: port.max_len,
            depth: port.depth,
            readers: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use crate::coordinator::{InputConfig, PortRef, SystemConfig};
    use crate::port;
    use crate::tests::utils::{self, Recorder, pipeline_config, table};

    fn source(id: &str) -> SystemConfig {
        SystemConfig {
            id: id.into(),
            ty: "imu".into(),
            inputs: Vec::new(),
        }
    }

    fn build(config: CoordinatorConfig) -> Result<Coordinator, BuildError> {
        config.build(&table(&Recorder::default()))
    }

    #[test]
    fn unsupported_input_and_output_alignment_is_rejected() {
        for input in [true, false] {
            let port = crate::PortDef {
                name: "aligned",
                id: utils::Imu::ID,
                max_len: 32,
                alignment: 32,
                depth: 1,
            };
            let def = crate::SystemDef {
                name: "test",
                inputs: if input { vec![port] } else { Vec::new() },
                outputs: if input { Vec::new() } else { vec![port] },
            };
            assert_eq!(
                check_alignment("test", &def),
                Err(BuildError::UnsupportedFrameAlignment {
                    system: "test".into(),
                    port: "aligned".into(),
                    alignment: 32,
                })
            );
        }
    }

    #[test]
    fn invalid_wall_rates_are_rejected_before_binding() {
        for rate in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
            1e-300,
            0.000_999,
        ] {
            let config = CoordinatorConfig {
                clock: super::super::Clock::Wall { rate },
                ..Default::default()
            };
            assert_eq!(build(config).err(), Some(BuildError::InvalidClockRate));
        }
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
            Some(BuildError::IdMismatch {
                id: "control".into(),
                port: "nav".into(),
                from: "imu.imu".into(),
                expected: utils::Nav::ID,
                found: utils::Imu::ID,
            })
        );
    }

    #[test]
    fn a_ring_larger_than_the_host_can_address_is_rejected() {
        let config = CoordinatorConfig {
            ring_depth: usize::MAX,
            systems: vec![source("imu")],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::RingTooLarge {
                system: "imu".into(),
                port: "imu".into(),
                max_len: utils::Imu::MAX_LEN,
            })
        );
        assert!(port::ring_capacity(utils::Imu::MAX_LEN, usize::MAX).is_none());
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn a_region_past_the_address_space_is_rejected() {
        // A capacity of 2^63 fits `usize`; the region around it does not.
        let config = CoordinatorConfig {
            ring_depth: 1 << 58,
            systems: vec![source("imu")],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::RingTooLarge {
                system: "imu".into(),
                port: "imu".into(),
                max_len: utils::Imu::MAX_LEN,
            })
        );
    }

    #[test]
    fn an_output_with_no_edges_still_builds() {
        let config = CoordinatorConfig {
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
