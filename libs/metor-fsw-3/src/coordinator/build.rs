//! Build is responsible for creating new ring buffers for systems and linking them together
//!
//! It takes a pass through the system table to discover all the connections between systems, and
//! then allocated the rings with the appropriate reader capacity.

use std::collections::HashMap;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer, checked_region_len};
use metor_proto::types::Timestamp;

use crate::def::{DefCx, Records};
use crate::port::{Output, ring_capacity};
use crate::system::{PortDef, SystemDef};
use crate::thread::{DEFAULT_THREAD, Threads};

use super::config::{CoordinatorConfig, SystemConfig};
use super::error::BuildError;
use super::params::{ParamError, Params};
use super::status::SystemStatus;
use super::table::{MakeCx, SystemTable, TableEntry};
use super::{Coordinator, Entry};

const STATUS_PORT: &str = "status";

struct RingSpec {
    system: String,
    port: PortDef,
    readers: usize,
}

type RingIndex = usize;

/// One system as build resolved it: the definition its type computed and the
/// rings feeding each of its input ports.
struct PlannedSystem<'a> {
    id: &'a str,
    entry: &'a TableEntry,
    def: SystemDef,
    params: Params<'a>,
    /// The background thread this system is placed on, if it is async.
    thread: &'a str,
    base_ring_idx: RingIndex,
    inputs: Vec<Vec<RingIndex>>,
}

struct Plan<'a> {
    systems: Vec<PlannedSystem<'a>>,
    rings: Vec<RingSpec>,
}

impl PlannedSystem<'_> {
    fn status_ring(&self) -> RingIndex {
        self.base_ring_idx + self.def.outputs.len()
    }

    /// Returns the ring index for a given output port, or `None` if the port is not declared.
    fn output_ring(&self, port: &str) -> Option<RingIndex> {
        if port == STATUS_PORT {
            return Some(self.status_ring());
        }
        let index = self.def.outputs.iter().position(|p| p.name == port)?;
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
        let mut threads = Threads::new();
        let entries = bind_rings(&plan, &rings, &mut threads)?;

        Ok(Coordinator {
            entries,
            threads,
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
    let index = index(config)?;
    let records = table.records();
    let mut plan = Plan {
        systems: Vec::with_capacity(config.systems.len()),
        rings: vec![],
    };

    for system in &config.systems {
        let entry = table
            .get(&system.ty)
            .ok_or_else(|| BuildError::UnknownType {
                id: system.id.clone(),
                ty: system.ty.clone(),
            })?;

        let def = instance_def(system, entry, &plan, &index, &records)?;
        check_port_names(&system.id, &def)?;
        if def.outputs.iter().any(|p| p.name == STATUS_PORT) {
            return Err(BuildError::ReservedPort {
                ty: system.ty.clone(),
            });
        }
        check_alignment(&system.id, &def)?;
        check_outputs(system, &def)?;
        let base_ring_idx = plan.rings.len();
        plan.rings.extend(
            def.outputs
                .iter()
                .map(|port| RingSpec::new(&system.id, port)),
        );
        plan.rings.push(RingSpec::new(
            &system.id,
            &Output::<SystemStatus>::def(STATUS_PORT),
        ));
        plan.systems.push(PlannedSystem {
            id: &system.id,
            entry,
            inputs: vec![Vec::new(); def.inputs.len()],
            def,
            params: Params(&system.params),
            thread: system.thread.as_deref().unwrap_or(DEFAULT_THREAD),
            base_ring_idx,
        });
    }
    resolve_edges(config, &mut plan, &index)?;
    Ok(plan)
}

/// Each system's position in the config, by id.
fn index(config: &CoordinatorConfig) -> Result<HashMap<&str, usize>, BuildError> {
    let mut index = HashMap::with_capacity(config.systems.len());
    for (at, system) in config.systems.iter().enumerate() {
        if index.insert(system.id.as_str(), at).is_some() {
            return Err(BuildError::DuplicateId {
                id: system.id.clone(),
            });
        }
    }
    Ok(index)
}

/// The definition the type computes from this instance's config.
fn instance_def(
    system: &SystemConfig,
    entry: &TableEntry,
    plan: &Plan<'_>,
    index: &HashMap<&str, usize>,
    records: &Records,
) -> Result<SystemDef, BuildError> {
    let producers = producers(system, entry, plan, index)?;
    let inputs: Vec<(&str, &PortDef)> = producers.iter().map(|(port, def)| (*port, def)).collect();
    let cx = DefCx {
        inputs: &inputs,
        outputs: &system.outputs,
        records,
    };
    entry.def_for(&cx).map_err(|source| BuildError::Def {
        id: system.id.clone(),
        source,
    })
}

/// One entry per config edge: the input port it names and the def of the
/// output feeding it. A producer this pass has not resolved yet only feeds a
/// port the type declares, which needs no def of its own.
fn producers<'a>(
    system: &'a SystemConfig,
    entry: &TableEntry,
    plan: &Plan<'_>,
    index: &HashMap<&str, usize>,
) -> Result<Vec<(&'a str, PortDef)>, BuildError> {
    let mut edges = Vec::new();
    for input in &system.inputs {
        for from in &input.from {
            let Some(&at) = index.get(from.system.as_str()) else {
                return Err(BuildError::UnknownSystem {
                    id: system.id.clone(),
                    port: input.port.clone(),
                    from: from.system.clone(),
                });
            };
            let Some(producer) = plan.systems.get(at) else {
                if entry.def.inputs.iter().any(|p| p.name == input.port) {
                    continue;
                }
                return Err(BuildError::DynamicFromLater {
                    system: system.id.clone(),
                    port: input.port.clone(),
                });
            };
            let Some(ring) = producer.output_ring(&from.port) else {
                return Err(BuildError::UnknownOutput {
                    system: from.system.clone(),
                    port: from.port.clone(),
                });
            };
            edges.push((input.port.as_str(), plan.rings[ring].port.clone()));
        }
    }
    Ok(edges)
}

fn check_port_names(system: &str, def: &crate::SystemDef) -> Result<(), BuildError> {
    for (index, port) in def.inputs.iter().enumerate() {
        if def.inputs[..index]
            .iter()
            .any(|other| other.name == port.name)
        {
            return Err(BuildError::DuplicateInput {
                system: system.to_string(),
                port: port.name.to_string(),
            });
        }
    }

    for (index, port) in def.outputs.iter().enumerate() {
        if def.outputs[..index]
            .iter()
            .any(|other| other.name == port.name)
        {
            return Err(BuildError::DuplicateOutput {
                system: system.to_string(),
                port: port.name.to_string(),
            });
        }
    }
    Ok(())
}

/// Every output the config lists is one the type took.
fn check_outputs(system: &SystemConfig, def: &SystemDef) -> Result<(), BuildError> {
    for output in &system.outputs {
        if !def.outputs.iter().any(|port| port.name == output.port) {
            return Err(BuildError::UnknownOutput {
                system: system.id.clone(),
                port: output.port.clone(),
            });
        }
    }
    Ok(())
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
            let port = plan.systems[i]
                .def
                .inputs
                .iter()
                .position(|p| p.name == input.port);
            if input.from.is_empty() && port.is_none() {
                return Err(BuildError::UnknownInput {
                    system: system.id.clone(),
                    port: input.port.clone(),
                });
            }
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
                let Some(port) = port else {
                    return Err(BuildError::UnknownInput {
                        system: system.id.clone(),
                        port: input.port.clone(),
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
            let def = &system.def.inputs[port];
            for &ring in edges {
                let spec = &plan.rings[ring];
                if spec.port.id != def.id {
                    return Err(BuildError::IdMismatch {
                        id: system.id.to_string(),
                        port: def.name.to_string(),
                        from: format!("{}.{}", spec.system, spec.port.name),
                        expected: def.id,
                        found: spec.port.id,
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
        port: spec.port.name.to_string(),
        max_len: spec.port.max_len,
    };
    let depth = spec
        .port
        .depth
        .checked_mul(ring_depth)
        .ok_or_else(too_large)?;
    let capacity = ring_capacity(spec.port.max_len, depth).ok_or_else(too_large)?;
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

fn bind_rings(
    plan: &Plan<'_>,
    rings: &[RingBuffer],
    threads: &mut Threads,
) -> Result<Vec<Entry>, BuildError> {
    let mut entries = Vec::with_capacity(plan.systems.len());
    for system in &plan.systems {
        let outputs: Vec<&RingBuffer> = (0..system.def.outputs.len())
            .map(|i| &rings[system.base_ring_idx + i])
            .collect();
        let inputs: Vec<Vec<&RingBuffer>> = system
            .inputs
            .iter()
            .map(|edges| edges.iter().map(|&ring| &rings[ring]).collect())
            .collect();
        let step = (system.entry.make)(MakeCx {
            id: system.id,
            thread: system.thread,
            def: &system.def,
            params: system.params,
            inputs,
            outputs,
            threads,
        })
        .map_err(|source| build_error(system.id, source))?;
        entries.push(Entry {
            name: system.id.to_string(),
            step: Some(step),
            // PANIC Safety: SystemStatus requires only eight-byte alignment.
            status: Output::try_new({
                let ring: &RingBuffer = &rings[system.status_ring()];
                ring.writer(NoWake).expect("one writer per output ring")
            })
            .expect("supported status alignment"),
        });
    }
    Ok(entries)
}

/// Names the system a `make` failed on, keeping a refused placement its own error.
fn build_error(id: &str, source: ParamError) -> BuildError {
    match source {
        ParamError::Thread { thread } => BuildError::ThreadOnCyclic {
            id: id.to_string(),
            thread,
        },
        source => BuildError::Params {
            id: id.to_string(),
            source,
        },
    }
}

impl RingSpec {
    fn new(system: &str, port: &PortDef) -> Self {
        Self {
            system: system.to_string(),
            port: port.clone(),
            readers: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::coordinator::{InputConfig, OutputConfig, PortRef, SystemConfig};
    use crate::def::DefError;
    use crate::tests::utils::{self, Recorder, pipeline_config, table};
    use crate::{Frame, Record};
    use crate::{port, system};

    /// A second frame named `imu`, so the record name resolves two ways.
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "imu")]
    #[repr(C)]
    struct WideImu {
        #[frame(timestamp)]
        timestamp: Timestamp,
        sample: f64,
        extra: f64,
    }

    struct WideSource;

    #[system]
    impl WideSource {
        fn execute(&mut self, imu: &mut Output<WideImu>) {
            let _ = imu;
        }
    }

    fn tap(inputs: Vec<InputConfig>) -> SystemConfig {
        SystemConfig {
            inputs,
            ..SystemConfig::new("tap", "tap")
        }
    }

    fn emits(record: &str) -> SystemConfig {
        SystemConfig {
            outputs: vec![OutputConfig {
                port: "imu".into(),
                record: record.into(),
            }],
            ..SystemConfig::new("emit", "emit")
        }
    }

    fn source(id: &str) -> SystemConfig {
        SystemConfig::new(id, "imu")
    }

    fn build(config: CoordinatorConfig) -> Result<Coordinator, BuildError> {
        config.build(&table(&Recorder::default()))
    }

    #[test]
    fn test_reject_unsupported_port_alignment() {
        for input in [true, false] {
            let port = crate::PortDef {
                name: "aligned".into(),
                record: utils::Imu::NAME.into(),
                id: utils::Imu::ID,
                max_len: 32,
                alignment: 32,
                depth: 1,
                schema: utils::Imu::schema(),
            };
            let def = crate::SystemDef {
                name: "test".into(),
                inputs: if input {
                    vec![port.clone()]
                } else {
                    Vec::new()
                },
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
    fn test_reject_duplicate_port_names() {
        let port = Output::<utils::Imu>::def("sample");
        for input in [true, false] {
            let def = crate::SystemDef {
                name: "test".into(),
                inputs: if input {
                    vec![port.clone(), port.clone()]
                } else {
                    vec![]
                },
                outputs: if input {
                    vec![]
                } else {
                    vec![port.clone(), port.clone()]
                },
            };
            let expected = if input {
                BuildError::DuplicateInput {
                    system: "instance".into(),
                    port: "sample".into(),
                }
            } else {
                BuildError::DuplicateOutput {
                    system: "instance".into(),
                    port: "sample".into(),
                }
            };
            assert_eq!(check_port_names("instance", &def), Err(expected));
        }
    }

    #[test]
    fn test_input_output_shared_name() {
        let port = Output::<utils::Imu>::def("sample");
        let def = crate::SystemDef {
            name: "test".into(),
            inputs: vec![port.clone()],
            outputs: vec![port],
        };
        assert_eq!(check_port_names("instance", &def), Ok(()));
        assert_eq!(
            check_port_names(
                "empty",
                &crate::SystemDef::new::<(), ()>("empty", &crate::DefCx::empty())
                    .expect("a static definition")
            ),
            Ok(())
        );
    }

    #[test]
    fn test_reject_invalid_wall_rates() {
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
    fn test_build_output_and_status_rings() {
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
    fn test_status_ring() {
        let mut config = pipeline_config();
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig {
                port: "status".into(),
                from: vec![PortRef::new("nav", "status")],
            }],
            ..SystemConfig::new("watch", "status_watch")
        });
        assert!(build(config).is_ok());
    }

    #[test]
    fn test_reject_duplicate_system_id() {
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
    fn test_reject_unregistered_type() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig::new("imu", "gyro")],
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
    fn test_reject_unknown_source_system() {
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
    fn test_reject_unknown_consumer_port() {
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
    fn test_reject_unknown_producer_port() {
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
    fn test_reject_frame_mismatch() {
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
    fn test_reject_ring_address_overflow() {
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
    fn test_reject_region_address_overflow() {
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
    fn test_build_unconnected_output() {
        let config = CoordinatorConfig {
            systems: vec![source("imu")],
            ..Default::default()
        };
        assert!(build(config).is_ok());
    }

    #[test]
    fn test_reject_declared_status_output() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig::new("r", "reserved")],
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
    fn test_param_error_reports_system() {
        let mut config = pipeline_config();
        config.systems[1].params = serde_json::json!({ "gain": 2.0 });
        assert_eq!(
            build(config).err(),
            Some(BuildError::Params {
                id: "nav".into(),
                source: crate::coordinator::ParamError::UnknownKey("gain".into()),
            })
        );
    }

    #[test]
    fn test_reader_slot_per_edge() {
        let mut config = pipeline_config();
        // A second consumer of `imu.imu`, so that ring carries two edges.
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig {
                port: "imu".into(),
                from: vec![PortRef::new("imu", "imu")],
            }],
            ..SystemConfig::new("nav_two", "nav")
        });
        let coordinator = build(config).expect("valid config");
        let imu = &coordinator.rings[0];
        assert_eq!(
            imu.view(NoWake).err(),
            Some(metor_fsw_3_ring::FullReaderTable)
        );
        assert!(imu.writer(NoWake).is_err());
    }

    #[test]
    fn test_dynamic_input_from_edge() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.systems.push(tap(vec![
            InputConfig {
                port: "imu.imu".into(),
                from: vec![PortRef::new("imu", "imu")],
            },
            InputConfig {
                port: "nav.nav".into(),
                from: vec![PortRef::new("nav", "nav")],
            },
        ]));
        let mut coordinator = config.build(&table(&recorder)).expect("valid config");
        coordinator.step(Timestamp(1));
        let seen = recorder.take_taps();
        let ports: Vec<_> = seen.iter().map(|(port, _)| port.as_str()).collect();
        assert_eq!(ports, vec!["imu.imu", "nav.nav"]);
        assert_eq!(utils::Imu::decode(&seen[0].1).expect("decodes").sample, 1.0);
        assert_eq!(
            utils::Nav::decode(&seen[1].1).expect("decodes").estimate,
            2.0
        );
    }

    #[test]
    fn test_reject_multiple_dynamic_producers() {
        let mut config = pipeline_config();
        config
            .systems
            .push(SystemConfig::new("imu_two", "imu_offset"));
        config.systems.push(tap(vec![InputConfig {
            port: "imu.imu".into(),
            from: vec![PortRef::new("imu", "imu"), PortRef::new("imu_two", "imu")],
        }]));
        assert_eq!(
            build(config).err(),
            Some(BuildError::Def {
                id: "tap".into(),
                source: DefError::FanIn {
                    port: "imu.imu".into(),
                },
            })
        );
    }

    #[test]
    fn test_resolve_dynamic_output_record() {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            systems: vec![
                emits("imu"),
                SystemConfig {
                    inputs: vec![InputConfig {
                        port: "imu".into(),
                        from: vec![PortRef::new("emit", "imu")],
                    }],
                    ..SystemConfig::new("nav", "nav")
                },
                SystemConfig {
                    inputs: vec![InputConfig {
                        port: "nav".into(),
                        from: vec![PortRef::new("nav", "nav")],
                    }],
                    ..SystemConfig::new("control", "control")
                },
            ],
            ..Default::default()
        };
        let mut coordinator = config.build(&table(&recorder)).expect("valid config");
        coordinator.step(Timestamp(1));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 11.0)]);
    }

    #[test]
    fn test_reject_unknown_record() {
        let config = CoordinatorConfig {
            systems: vec![emits("gyro")],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::Def {
                id: "emit".into(),
                source: DefError::UnknownRecord {
                    port: "imu".into(),
                    record: "gyro".into(),
                },
            })
        );
    }

    #[test]
    fn test_reject_conflicting_record_definitions() {
        let recorder = Recorder::default();
        let mut table = table(&recorder);
        assert_eq!(
            table.register("wide", || WideSource),
            Err(BuildError::RecordConflict {
                record: "imu".into(),
            })
        );
        let config = CoordinatorConfig {
            systems: vec![emits("imu")],
            ..Default::default()
        };
        assert!(config.build(&table).is_ok());
    }

    #[test]
    fn test_reject_static_record_conflicts() {
        let mut table = table(&Recorder::default());
        assert_eq!(
            table.register("wide", || WideSource),
            Err(BuildError::RecordConflict {
                record: "imu".into(),
            })
        );
        assert!(CoordinatorConfig::default().build(&table).is_ok());
    }

    #[test]
    fn test_reject_undeclared_empty_input() {
        let mut config = pipeline_config();
        config.systems[1].inputs[0].from.clear();
        assert!(build(config.clone()).is_ok());
        config.systems[1].inputs[0].port = "typo".into();
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownInput {
                system: "nav".into(),
                port: "typo".into(),
            })
        );
    }

    #[test]
    fn test_reject_static_output_config() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                outputs: vec![OutputConfig {
                    port: "extra".into(),
                    record: "imu".into(),
                }],
                ..SystemConfig::new("imu", "imu")
            }],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::UnknownOutput {
                system: "imu".into(),
                port: "extra".into(),
            })
        );
    }

    /// A `loop()` edge into an undeclared port has no producer definition yet.
    #[test]
    fn test_reject_dynamic_forward_reference() {
        let mut config = pipeline_config();
        config.systems.push(tap(vec![InputConfig {
            port: "late.nav".into(),
            from: vec![PortRef::new("late", "nav")],
        }]));
        config.systems.push(SystemConfig::new("late", "nav"));
        assert_eq!(
            build(config).err(),
            Some(BuildError::DynamicFromLater {
                system: "tap".into(),
                port: "late.nav".into(),
            })
        );
    }

    /// A declared port takes its definition from the type, so its producer may
    /// come later.
    #[test]
    fn test_static_forward_reference() {
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig {
                    inputs: vec![InputConfig {
                        port: "nav".into(),
                        from: vec![PortRef::new("nav", "nav")],
                    }],
                    ..SystemConfig::new("control", "control")
                },
                SystemConfig::new("imu", "imu"),
                SystemConfig {
                    inputs: vec![InputConfig {
                        port: "imu".into(),
                        from: vec![PortRef::new("imu", "imu")],
                    }],
                    ..SystemConfig::new("nav", "nav")
                },
            ],
            ..Default::default()
        };
        assert!(build(config).is_ok());
    }

    #[test]
    fn test_build_unconnected_input() {
        let mut config = pipeline_config();
        config.systems[1].inputs.clear();
        assert!(build(config).is_ok());
    }
}
