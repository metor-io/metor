//! Turns a [`CoordinatorConfig`] into a running [`Coordinator`].
//!
//! Building happens in two passes. The first pass resolves each system's
//! definition and works out which output feeds which input. The second pass
//! allocates one ring per output, with one reader slot for every input that
//! reads from it, and binds each system to its rings.

use core::time::Duration;
use std::collections::HashMap;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use crate::def::{DefCx, Records};
use crate::port::{Output, ring_capacity};
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;

use super::config::{Adapter, BuildOptions, Clock, CoordinatorConfig, PortRef, SystemConfig};
use super::error::BuildError;
use super::params::Params;
use super::run::{Coordinator, Entry};
use super::status::SystemStatus;
use super::table::{MakeCx, SystemTable, TableEntry};

const STATUS_PORT: &str = "status";

const MIN_WALL_RATE: f64 = 0.001;

struct RingSpec {
    system: String,
    port: PortDef,
    readers: usize,
}

type RingIndex = usize;

/// A system whose definition and input edges have been resolved.
struct PlannedSystem<'a> {
    id: &'a str,
    entry: &'a TableEntry,
    def: SystemDef,
    params: Params<'a>,
    /// The adapter this system runs behind, if the config gives one.
    adapter: Option<&'a Adapter>,
    /// The ring of each output port, in definition order.
    outputs: Vec<RingIndex>,
    status: RingIndex,
    inputs: Vec<Vec<RingIndex>>,
}

struct Plan<'a> {
    /// Each system id's position in the config.
    index: HashMap<&'a str, usize>,
    systems: Vec<PlannedSystem<'a>>,
    rings: Vec<RingSpec>,
}

impl PlannedSystem<'_> {
    /// Returns the ring index for a given output port, or `None` if the port is not declared.
    fn output_ring(&self, port: &str) -> Option<RingIndex> {
        if port == STATUS_PORT {
            return Some(self.status);
        }
        let at = self.def.outputs.iter().position(|p| p.name == port)?;
        Some(self.outputs[at])
    }
}

impl CoordinatorConfig {
    /// Builds a [`Coordinator`] from this config, creating systems from `table`.
    pub fn build(self, table: &SystemTable) -> Result<Coordinator, BuildError> {
        self.build_with(table, BuildOptions::default())
    }

    /// Builds a [`Coordinator`] with `options`.
    pub fn build_with(
        self,
        table: &SystemTable,
        options: BuildOptions,
    ) -> Result<Coordinator, BuildError> {
        self.clock.validate()?;
        if options.deterministic {
            self.clock.check_deterministic()?;
        }
        let mut plan = Plan::resolve(&self, table)?;
        check_ids(&plan)?;
        plan.set_reader_counts();
        let rings = plan
            .rings
            .iter()
            .map(|spec| allocate_ring(spec, self.ring_depth))
            .collect::<Result<Vec<_>, _>>()?;
        let mut threads = match options.deterministic {
            true => Threads::deterministic(),
            false => Threads::default(),
        };
        let args = SharedMakeArgs {
            seed: Some(self.seed),
            namespace: self.namespace.as_deref().unwrap_or_default(),
        };
        let entries = make_systems(&plan, &rings, &mut threads, args)?;

        Ok(Coordinator {
            entries,
            clock: self.clock,
            epoch: self.clock.epoch().unwrap_or_else(Timestamp::now),
            cycle: 0,
            rings,
        })
    }
}

impl<'a> Plan<'a> {
    /// Resolves every system's definition, its rings, and its input edges.
    fn resolve(config: &'a CoordinatorConfig, table: &'a SystemTable) -> Result<Self, BuildError> {
        let mut plan = Self {
            index: index_by_id(config)?,
            systems: Vec::with_capacity(config.systems.len()),
            rings: Vec::new(),
        };
        let records = table.records();
        let namespace = config.namespace.as_deref();
        for system in &config.systems {
            let entry = table
                .get(&system.ty)
                .ok_or_else(|| BuildError::UnknownType {
                    id: system.id.clone(),
                    ty: system.ty.clone(),
                })?;
            let def = SystemDef::planned(system, entry, &plan, &records, namespace)?;
            plan.push(system, entry, def, namespace);
        }
        plan.resolve_edges(config)?;
        Ok(plan)
    }

    /// Adds `system`, with one ring per output and one for its status.
    fn push(
        &mut self,
        system: &'a SystemConfig,
        entry: &'a TableEntry,
        def: SystemDef,
        namespace: Option<&str>,
    ) {
        let outputs = def
            .outputs
            .iter()
            .map(|port| self.add_ring(&system.id, port.clone()))
            .collect();
        let status = self.add_ring(&system.id, status_def(&system.id, namespace));
        self.systems.push(PlannedSystem {
            id: &system.id,
            entry,
            inputs: vec![Vec::new(); def.inputs.len()],
            def,
            params: Params(&system.params),
            adapter: system.adapter.as_ref(),
            outputs,
            status,
        });
    }

    fn add_ring(&mut self, system: &str, port: PortDef) -> RingIndex {
        self.rings.push(RingSpec {
            system: system.to_string(),
            port,
            readers: 0,
        });
        self.rings.len() - 1
    }

    /// Returns the ring that input `port` of system `id` reads through `from`.
    ///
    /// Returns `None` if the producing system has not been planned yet.
    fn producer_ring(
        &self,
        id: &str,
        port: &str,
        from: &PortRef,
    ) -> Result<Option<RingIndex>, BuildError> {
        let Some(&at) = self.index.get(from.system.as_str()) else {
            return Err(BuildError::UnknownSystem {
                id: id.to_string(),
                port: port.to_string(),
                from: from.system.clone(),
            });
        };
        let Some(producer) = self.systems.get(at) else {
            return Ok(None);
        };
        let ring = producer.output_ring(&from.port);
        ring.map(Some).ok_or_else(|| BuildError::UnknownOutput {
            system: from.system.clone(),
            port: from.port.clone(),
        })
    }

    /// Records the ring behind every input edge in `config`.
    fn resolve_edges(&mut self, config: &CoordinatorConfig) -> Result<(), BuildError> {
        for (i, system) in config.systems.iter().enumerate() {
            for input in &system.inputs {
                let Some(port) = self.systems[i]
                    .def
                    .inputs
                    .iter()
                    .position(|p| p.name == input.port)
                else {
                    return Err(BuildError::UnknownInput {
                        system: system.id.clone(),
                        port: input.port.clone(),
                    });
                };
                for from in &input.from {
                    if let Some(ring) = self.producer_ring(&system.id, &input.port, from)? {
                        self.systems[i].inputs[port].push(ring);
                    }
                }
            }
        }
        Ok(())
    }

    /// Sets each ring's reader count to the number of edges into it.
    ///
    /// A ring with no edges still gets one slot, because the ring format requires
    /// at least one.
    fn set_reader_counts(&mut self) {
        for edge in self.systems.iter().flat_map(|s| s.inputs.iter().flatten()) {
            self.rings[*edge].readers += 1;
        }
        for ring in &mut self.rings {
            ring.readers = ring.readers.max(1);
        }
    }
}

/// Returns the definition of system `id`'s `status` output.
///
/// The coordinator adds this output to every system. It is namespaced like the
/// system's own ports.
pub(crate) fn status_def(id: &str, namespace: Option<&str>) -> PortDef {
    let cx = DefCx {
        id,
        namespace,
        ..DefCx::empty()
    };
    Output::<SystemStatus>::def(&cx, STATUS_PORT)
}

/// Maps each system id to its position in the config, rejecting duplicate ids.
fn index_by_id(config: &CoordinatorConfig) -> Result<HashMap<&str, usize>, BuildError> {
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

impl SystemDef {
    /// Computes and checks the definition for `system` during the first build
    /// pass.
    ///
    /// Only producers planned earlier in the pass are known.
    fn planned(
        system: &SystemConfig,
        entry: &TableEntry,
        plan: &Plan<'_>,
        records: &Records,
        namespace: Option<&str>,
    ) -> Result<Self, BuildError> {
        let def = Self::instance(system, entry, namespace, records, |port, from| {
            let ring = plan.producer_ring(&system.id, port, from)?;
            Ok(ring.map(|ring| plan.rings[ring].port.clone()))
        })?;
        def.check_port_names(system)?;
        def.check_alignment(&system.id)?;
        def.check_outputs(system)?;
        Ok(def)
    }

    /// Computes the definition that `entry`'s type produces for `system`.
    ///
    /// `producer` looks up the output definition behind each input edge. It
    /// returns `None` when that producer is not resolved yet. That is only
    /// allowed for input ports the type declares statically, because their
    /// definition does not depend on the producer.
    pub(crate) fn instance(
        system: &SystemConfig,
        entry: &TableEntry,
        namespace: Option<&str>,
        records: &Records,
        producer: impl Fn(&str, &PortRef) -> Result<Option<PortDef>, BuildError>,
    ) -> Result<Self, BuildError> {
        let mut edges: Vec<(&str, PortDef)> = Vec::new();
        for input in &system.inputs {
            for from in &input.from {
                match producer(&input.port, from)? {
                    Some(def) => edges.push((input.port.as_str(), def)),
                    None if entry.def.inputs.iter().any(|p| p.name == input.port) => {}
                    None => {
                        return Err(BuildError::DynamicFromLater {
                            system: system.id.clone(),
                            port: input.port.clone(),
                        });
                    }
                }
            }
        }
        let inputs: Vec<(&str, &PortDef)> = edges.iter().map(|(port, def)| (*port, def)).collect();
        let cx = DefCx {
            id: &system.id,
            namespace,
            inputs: &inputs,
            outputs: &system.outputs,
            records,
            params: &system.params,
        };
        (entry.def_fn)(&cx).map_err(|source| BuildError::Def {
            id: system.id.clone(),
            source,
        })
    }

    /// Checks that no two inputs and no two outputs share a name, and that no
    /// output takes the coordinator's `status` name.
    fn check_port_names(&self, system: &SystemConfig) -> Result<(), BuildError> {
        let port = |port: &PortDef| (system.id.clone(), port.name.to_string());
        if let Some((system, port)) = find_duplicate(&self.inputs).map(port) {
            return Err(BuildError::DuplicateInput { system, port });
        }
        if let Some((system, port)) = find_duplicate(&self.outputs).map(port) {
            return Err(BuildError::DuplicateOutput { system, port });
        }
        if self.outputs.iter().any(|p| p.name == STATUS_PORT) {
            return Err(BuildError::ReservedPort {
                ty: system.ty.clone(),
            });
        }
        Ok(())
    }

    /// Checks that no port needs more alignment than ring payloads get.
    fn check_alignment(&self, system: &str) -> Result<(), BuildError> {
        for port in self.inputs.iter().chain(&self.outputs) {
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

    /// Checks that every output listed in the config is an output the type
    /// defines.
    fn check_outputs(&self, system: &SystemConfig) -> Result<(), BuildError> {
        for output in &system.outputs {
            if !self.outputs.iter().any(|port| port.name == output.port) {
                return Err(BuildError::UnknownOutput {
                    system: system.id.clone(),
                    port: output.port.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Returns the first port whose name repeats an earlier port's name.
fn find_duplicate(ports: &[PortDef]) -> Option<&PortDef> {
    ports
        .iter()
        .enumerate()
        .find(|(at, port)| ports[..*at].iter().any(|other| other.name == port.name))
        .map(|(_, port)| port)
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

/// Allocates a ring that holds `spec.port.depth * ring_depth` records.
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
    RingBuffer::create_in_memory(config).map_err(|_| too_large())
}

/// The make arguments every system in the target shares.
#[derive(Clone, Copy)]
struct SharedMakeArgs<'a> {
    seed: Option<u64>,
    namespace: &'a str,
}

/// Makes every planned system, bound to its rings.
fn make_systems(
    plan: &Plan<'_>,
    rings: &[RingBuffer],
    threads: &mut Threads,
    args: SharedMakeArgs<'_>,
) -> Result<Vec<Entry>, BuildError> {
    let mut entries = Vec::with_capacity(plan.systems.len());
    for system in &plan.systems {
        let outputs: Vec<&RingBuffer> = system.outputs.iter().map(|&ring| &rings[ring]).collect();
        let inputs: Vec<Vec<&RingBuffer>> = system
            .inputs
            .iter()
            .map(|edges| edges.iter().map(|&ring| &rings[ring]).collect())
            .collect();
        // PANIC Safety: build allocates one ring per output.
        let status = rings[system.status]
            .writer(NoWake)
            .expect("one writer per output ring");
        let cx = MakeCx {
            id: system.id,
            adapter: system.adapter,
            def: &system.def,
            params: system.params,
            inputs,
            outputs,
            threads,
            seed: args.seed,
            namespace: args.namespace,
        };
        let step = (system.entry.make)(cx).map_err(|source| BuildError::Params {
            id: system.id.to_string(),
            source,
        })?;
        entries.push(Entry {
            name: system.id.to_string(),
            step: Some(step),
            status: Output::new(status),
        });
    }
    Ok(entries)
}

impl Clock {
    pub(crate) fn validate(&self) -> Result<(), BuildError> {
        if let Self::Wall { rate } = self
            && (!rate.is_finite() || *rate < MIN_WALL_RATE)
        {
            return Err(BuildError::InvalidClockRate);
        }
        Ok(())
    }

    /// Checks that cycle times are fixed and strictly increasing: a turned
    /// group keys its turns on them, and its clock counts whole microseconds
    /// from zero.
    fn check_deterministic(&self) -> Result<(), BuildError> {
        match *self {
            Self::Wall { .. } => Err(BuildError::DeterministicWall),
            Self::Simulated { dt, .. } if dt < Duration::from_micros(1) => {
                Err(BuildError::DeterministicZeroDt)
            }
            Self::Simulated { epoch, .. } if epoch.is_none_or(|e| e.0 < 0) => {
                Err(BuildError::DeterministicEpoch)
            }
            Self::Simulated { .. } => Ok(()),
        }
    }

    /// The fixed start of a simulated clock, if it has one.
    pub(crate) fn epoch(&self) -> Option<Timestamp> {
        match self {
            Self::Simulated { epoch, .. } => *epoch,
            Self::Wall { .. } => None,
        }
    }

    /// Returns the wall-clock time budget for one cycle.
    ///
    /// Returns zero for a simulated clock.
    pub(crate) fn cycle_budget(&self) -> Duration {
        match self {
            Clock::Wall { rate } if *rate > 0.0 => {
                Duration::try_from_secs_f64(1.0 / rate).unwrap_or(Duration::MAX)
            }
            _ => Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::coordinator::{InputConfig, OutputConfig, OutputSource, PortRef, SystemConfig};
    use crate::def::DefError;
    use crate::tests::utils::{self, Recorder, pipeline_config, table};
    use crate::{Frame, Record};
    use crate::{port, system};

    /// A second frame named `imu`, so the same record name has two layouts.
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "imu")]
    #[repr(C)]
    struct WideImu {
        #[frame(timestamp)]
        timestamp: Timestamp,
        sample: f64,
        extra: f64,
    }

    #[system]
    fn wide_source(imu: &mut Output<WideImu>) {
        let _ = imu;
    }

    fn tap(inputs: Vec<InputConfig>) -> SystemConfig {
        SystemConfig {
            inputs,
            ..SystemConfig::new("tap", "tap")
        }
    }

    fn emits(record: &str) -> SystemConfig {
        SystemConfig {
            outputs: vec![OutputConfig::with_record("imu", record)],
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
                layout: utils::Imu::schema().layout(),
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
                def.check_alignment("test"),
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
        let port = Output::<utils::Imu>::def(&DefCx::empty(), "sample");
        let instance = SystemConfig::new("instance", "test");
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
            assert_eq!(def.check_port_names(&instance), Err(expected));
        }
    }

    #[test]
    fn test_input_output_shared_name() {
        let port = Output::<utils::Imu>::def(&DefCx::empty(), "sample");
        let instance = SystemConfig::new("instance", "test");
        let def = crate::SystemDef {
            name: "test".into(),
            inputs: vec![port.clone()],
            outputs: vec![port],
        };
        assert_eq!(def.check_port_names(&instance), Ok(()));
        assert_eq!(
            crate::SystemDef::new::<(), (), NoWake>("empty", &crate::DefCx::empty())
                .expect("a static definition")
                .check_port_names(&SystemConfig::new("empty", "empty")),
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
        // imu + nav outputs and logs, plus one status ring each.
        assert_eq!(coordinator.ring_count(), 6);
    }

    #[test]
    fn test_status_ring() {
        let mut config = pipeline_config();
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig::new("status", "nav", "status")],
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
            inputs: vec![InputConfig::new("imu", "imu", "imu")],
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
            InputConfig::new("imu.imu", "imu", "imu"),
            InputConfig::new("nav.nav", "nav", "nav"),
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
                    inputs: vec![InputConfig::new("imu", "emit", "imu")],
                    ..SystemConfig::new("nav", "nav")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("nav", "nav", "nav")],
                    ..SystemConfig::new("control", "control")
                },
            ],
            ..Default::default()
        };
        let mut coordinator = config.build(&table(&recorder)).expect("valid config");
        coordinator.step(Timestamp(1));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 11.0)]);
    }

    /// An output configured with a full definition is used as-is.
    ///
    /// Consumers bind to it even though no registered type declares it.
    #[test]
    fn test_resolve_dynamic_output_def() {
        let recorder = Recorder::default();
        let def = crate::Output::<crate::tests::utils::Imu>::def(
            &DefCx {
                id: "plant",
                namespace: Some("a"),
                ..DefCx::empty()
            },
            "imu",
        );
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig {
                    outputs: vec![OutputConfig {
                        port: "imu".into(),
                        source: OutputSource::Def(def.clone()),
                    }],
                    ..SystemConfig::new("emit", "emit")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("imu", "emit", "imu")],
                    ..SystemConfig::new("nav", "nav")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("nav", "nav", "nav")],
                    ..SystemConfig::new("control", "control")
                },
            ],
            ..Default::default()
        };
        let table = table(&recorder);
        let plan = Plan::resolve(&config, &table).expect("valid config");
        let emitted = &plan.systems[0].def.outputs[0];
        assert_eq!(emitted.name, "imu");
        assert_eq!(emitted.schema, def.schema, "the peer's root is kept");
        assert_eq!(plan.rings[0].port.max_len, def.max_len);
        drop(plan);
        let mut coordinator = config.build(&table).expect("valid config");
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
            table.register("wide", wide_source(), Default::default),
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
            table.register("wide", wide_source(), Default::default),
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
                outputs: vec![OutputConfig::with_record("extra", "imu")],
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

    /// An edge from a later system into an undeclared port fails.
    ///
    /// The producer's definition is not known yet when the port is resolved.
    #[test]
    fn test_reject_dynamic_forward_reference() {
        let mut config = pipeline_config();
        config
            .systems
            .push(tap(vec![InputConfig::new("late.nav", "late", "nav")]));
        config.systems.push(SystemConfig::new("late", "nav"));
        assert_eq!(
            build(config).err(),
            Some(BuildError::DynamicFromLater {
                system: "tap".into(),
                port: "late.nav".into(),
            })
        );
    }

    /// A declared port's producer may appear later in the config.
    ///
    /// The port's definition comes from the type, not the producer.
    #[test]
    fn test_static_forward_reference() {
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig {
                    inputs: vec![InputConfig::new("nav", "nav", "nav")],
                    ..SystemConfig::new("control", "control")
                },
                SystemConfig::new("imu", "imu"),
                SystemConfig {
                    inputs: vec![InputConfig::new("imu", "imu", "imu")],
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

    #[test]
    fn test_zero_cycle_budget() {
        assert_eq!(
            Clock::Simulated {
                dt: Duration::from_millis(5),
                epoch: None,
            }
            .cycle_budget(),
            Duration::ZERO
        );
        let clock = Clock::Wall { rate: f64::MAX };
        assert_eq!(clock.validate(), Ok(()));
        assert_eq!(clock.cycle_budget(), Duration::ZERO);
    }

    #[test]
    fn test_minimum_wall_rate_budget() {
        let clock = Clock::Wall {
            rate: MIN_WALL_RATE,
        };
        assert_eq!(clock.validate(), Ok(()));
        assert_eq!(clock.cycle_budget(), Duration::from_secs(1_000));
    }
}
