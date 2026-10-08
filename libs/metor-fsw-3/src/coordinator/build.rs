//! Turns a [`CoordinatorConfig`] into a running [`Coordinator`].
//!
//! Building happens in two passes. The first pass resolves each system's
//! definition and works out which output feeds which input. The second pass
//! allocates one ring per output, with one reader slot for every input that
//! reads from it, and binds each system to its rings.

use core::time::Duration;
use std::collections::HashMap;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer, checked_region_len};
use metor_proto::types::Timestamp;
use metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP;

use crate::channel::{ABORT_PORT, COMMANDS_PORT};
use crate::def::{DefCx, Records};
use crate::io::{Backing, End, IO_COMPLETE_TYPE, IO_SUBMIT_TYPE, ResourceConfig, SharedIo};
use crate::port::{Output, ring_capacity};
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;

use super::config::{Adapter, Clock, CoordinatorConfig, PortRef, SystemConfig};
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
    /// Builds a [`Coordinator`] from this config, creating systems from `table`.
    pub fn build(self, table: &SystemTable) -> Result<Coordinator, BuildError> {
        self.clock.validate()?;
        check_channels(&self)?;
        check_resources(&self.resources)?;
        let mut plan = resolve(&self, table)?;
        check_ids(&plan)?;
        count_readers(&plan.systems, &mut plan.rings);
        let rings = plan
            .rings
            .iter()
            .map(|spec| allocate_ring(spec, self.ring_depth))
            .collect::<Result<Vec<_>, _>>()?;
        let mut threads = Threads::new();
        let io = SharedIo::default();
        io.borrow_mut().resources = self.resources.clone();
        io.borrow_mut().grants = granted(&self.systems);
        let entries = bind_rings(&plan, &rings, &mut threads, &io)?;

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

        let namespace = config.namespace.as_deref();
        let def = planned_def(system, entry, &plan, &index, &records, namespace)?;
        check_port_names(&system.id, &def)?;
        check_grants(system, &def)?;
        check_interfaces(system, &def, &config.resources)?;
        if def.outputs.iter().any(|p| p.name == STATUS_PORT) {
            return Err(BuildError::ReservedPort {
                ty: system.ty.clone(),
            });
        }
        check_alignment(&system.id, &def)?;
        check_outputs(system, &def)?;
        let base_ring_idx = plan.rings.len();
        let status = status_def(&system.id, namespace);
        plan.rings
            .extend(def.outputs.iter().chain([&status]).map(|port| RingSpec {
                system: system.id.clone(),
                port: port.clone(),
                readers: 0,
            }));
        plan.systems.push(PlannedSystem {
            id: &system.id,
            entry,
            inputs: vec![Vec::new(); def.inputs.len()],
            def,
            params: Params(&system.params),
            adapter: system.adapter.as_ref(),
            base_ring_idx,
        });
    }
    check_io_steps(config, &plan)?;
    resolve_edges(config, &mut plan, &index)?;
    Ok(plan)
}

/// Checks the io steps' place and wiring.
///
/// A config whose systems hold a `Cap` needs one `io_submit`, after every
/// such system, and one `io_complete` after it, each listing exactly those
/// systems in step order. Neither io step takes a channel.
fn check_io_steps(config: &CoordinatorConfig, plan: &Plan<'_>) -> Result<(), BuildError> {
    let io_types = [IO_SUBMIT_TYPE, IO_COMPLETE_TYPE];
    for system in &config.systems {
        if io_types.contains(&system.ty.as_str())
            && matches!(system.adapter, Some(Adapter::Channel(_)))
        {
            let id = system.id.clone();
            return Err(BuildError::IoAdapter { id });
        }
    }
    let granted: Vec<usize> = plan
        .systems
        .iter()
        .enumerate()
        .filter(|(_, s)| s.def.grants.iter().any(|g| g.end == End::Host))
        .map(|(at, _)| at)
        .collect();
    if granted.is_empty() {
        return Ok(());
    }
    let submit = only(config, IO_SUBMIT_TYPE)?;
    let complete = only(config, IO_COMPLETE_TYPE)?;
    let error = |message: String| Err(BuildError::IoSystems { message });
    if complete < submit {
        return error("io_complete comes before io_submit".into());
    }
    if let Some(&late) = granted.iter().find(|&&at| at > submit) {
        let id = &config.systems[late].id;
        return error(format!("`{id}` holds a `Cap` but comes after io_submit"));
    }
    let ids: Vec<&str> = granted
        .iter()
        .map(|&at| config.systems[at].id.as_str())
        .collect();
    let sqs: Vec<&str> = config.systems[submit]
        .inputs
        .iter()
        .map(|i| i.port.as_str())
        .collect();
    let cqs: Vec<&str> = config.systems[complete]
        .outputs
        .iter()
        .map(|o| o.port.as_str())
        .collect();
    if sqs != ids || cqs != ids {
        return error(format!(
            "io_submit and io_complete must list {ids:?}, in that order"
        ));
    }
    Ok(())
}

/// The index of the one system of type `ty`.
fn only(config: &CoordinatorConfig, ty: &str) -> Result<usize, BuildError> {
    let mut found = config
        .systems
        .iter()
        .enumerate()
        .filter(|(_, s)| s.ty == ty);
    match (found.next(), found.next()) {
        (Some((at, _)), None) => Ok(at),
        (None, _) => Err(BuildError::IoSystems {
            message: format!("systems hold a `Cap`, so the config needs one {ty}"),
        }),
        (Some(_), Some(_)) => Err(BuildError::IoSystems {
            message: format!("the config has more than one {ty}"),
        }),
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

/// Checks that channel names are short enough and unique.
///
/// A name must fit the wire format's length cap, and no two systems may use
/// the same one.
fn check_channels(config: &CoordinatorConfig) -> Result<(), BuildError> {
    let names = config
        .systems
        .iter()
        .filter_map(|system| match &system.adapter {
            Some(Adapter::Channel(channel)) => Some(channel.name.as_str()),
            _ => None,
        });
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if name.len() > SEQUENCE_CHANNEL_NAME_CAP {
            return Err(BuildError::ChannelName {
                name: name.to_string(),
                cap: SEQUENCE_CHANNEL_NAME_CAP,
            });
        }
        if !seen.insert(name) {
            return Err(BuildError::DuplicateChannel {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Maps each system id to its position in the config, rejecting duplicate ids.
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

/// Computes the definition for `system` during the first build pass.
///
/// Only producers planned earlier in the pass are known.
fn planned_def(
    system: &SystemConfig,
    entry: &TableEntry,
    plan: &Plan<'_>,
    index: &HashMap<&str, usize>,
    records: &Records,
    namespace: Option<&str>,
) -> Result<SystemDef, BuildError> {
    instance_def(system, entry, namespace, records, |port, from| {
        let ring = producer_ring(plan, index, &system.id, port, from)?;
        Ok(ring.map(|ring| plan.rings[ring].port.clone()))
    })
}

/// Computes the definition that `entry`'s type produces for `system`.
///
/// `producer` looks up the output definition behind each input edge. It returns
/// `None` when that producer is not resolved yet. That is only allowed for input
/// ports the type declares statically, because their definition does not depend
/// on the producer.
pub(crate) fn instance_def(
    system: &SystemConfig,
    entry: &TableEntry,
    namespace: Option<&str>,
    records: &Records,
    producer: impl Fn(&str, &PortRef) -> Result<Option<PortDef>, BuildError>,
) -> Result<SystemDef, BuildError> {
    let channel = matches!(system.adapter, Some(Adapter::Channel(_)));
    let mut edges: Vec<(&str, PortDef)> = Vec::new();
    for input in &system.inputs {
        if channel && input.port == ABORT_PORT {
            return Err(reserved(system, ABORT_PORT));
        }
        // The channel reads its commands; the system never sees them.
        if channel && input.port == COMMANDS_PORT {
            continue;
        }
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
    let def = (entry.def_fn)(&cx).map_err(|source| BuildError::Def {
        id: system.id.clone(),
        source,
    })?;
    if !channel {
        return Ok(def);
    }
    crate::channel::def(def, &cx).map_err(|port| reserved(system, &port))
}

fn reserved(system: &SystemConfig, port: &str) -> BuildError {
    BuildError::ReservedChannelPort {
        id: system.id.clone(),
        port: port.to_string(),
    }
}

/// Returns the ring that input `port` of system `id` reads through `from`.
///
/// Returns `None` if the producing system has not been planned yet.
fn producer_ring(
    plan: &Plan<'_>,
    index: &HashMap<&str, usize>,
    id: &str,
    port: &str,
    from: &PortRef,
) -> Result<Option<RingIndex>, BuildError> {
    let Some(&at) = index.get(from.system.as_str()) else {
        return Err(BuildError::UnknownSystem {
            id: id.to_string(),
            port: port.to_string(),
            from: from.system.clone(),
        });
    };
    let Some(producer) = plan.systems.get(at) else {
        return Ok(None);
    };
    let ring = producer.output_ring(&from.port);
    ring.map(Some).ok_or_else(|| BuildError::UnknownOutput {
        system: from.system.clone(),
        port: from.port.clone(),
    })
}

fn check_port_names(system: &str, def: &crate::SystemDef) -> Result<(), BuildError> {
    let port = |port: &PortDef| (system.to_string(), port.name.to_string());
    if let Some((system, port)) = duplicate(&def.inputs).map(port) {
        return Err(BuildError::DuplicateInput { system, port });
    }
    if let Some((system, port)) = duplicate(&def.outputs).map(port) {
        return Err(BuildError::DuplicateOutput { system, port });
    }
    Ok(())
}

/// Checks that each `Cap` parameter has exactly one grant, and that every
/// grant names one.
fn check_grants(system: &SystemConfig, def: &SystemDef) -> Result<(), BuildError> {
    let error = |param: &str| (system.id.clone(), param.to_string());
    let caps = || def.grants.iter().filter(|g| g.end == End::Host);
    for grant in &system.grants {
        if !caps().any(|cap| cap.param == grant.param) {
            let (system, param) = error(&grant.param);
            return Err(BuildError::UnknownGrant { system, param });
        }
    }
    for cap in caps() {
        match system
            .grants
            .iter()
            .filter(|g| g.param == cap.param)
            .count()
        {
            1 => {}
            0 => {
                let (system, param) = error(&cap.param);
                return Err(BuildError::Ungranted { system, param });
            }
            _ => {
                let (system, param) = error(&cap.param);
                return Err(BuildError::DuplicateGrant { system, param });
            }
        }
    }
    Ok(())
}

/// Each system's grants, for `io_submit`'s make.
fn granted(systems: &[SystemConfig]) -> Vec<(String, Vec<crate::io::Grant>)> {
    let granted = systems.iter().filter(|s| !s.grants.is_empty());
    granted.map(|s| (s.id.clone(), s.grants.clone())).collect()
}

/// Checks that resource ids are unique and that each backing is one this
/// build runs.
fn check_resources(resources: &[ResourceConfig]) -> Result<(), BuildError> {
    for (at, resource) in resources.iter().enumerate() {
        if resources[..at].iter().any(|r| r.id == resource.id) {
            let id = resource.id.clone();
            return Err(BuildError::DuplicateResource { id });
        }
        if !matches!(resource.backing, Backing::Real(_)) {
            return Err(BuildError::Backing {
                resource: resource.id.clone(),
                backing: resource.backing.name().into(),
            });
        }
    }
    Ok(())
}

/// Checks that each grant names a resource that provides its `Cap`'s
/// interface.
fn check_interfaces(
    system: &SystemConfig,
    def: &SystemDef,
    resources: &[ResourceConfig],
) -> Result<(), BuildError> {
    for grant in &system.grants {
        let resource = resources.iter().find(|r| r.id == grant.resource);
        let Some(resource) = resource else {
            return Err(BuildError::UnknownResource {
                system: system.id.clone(),
                param: grant.param.clone(),
                resource: grant.resource.clone(),
            });
        };
        let found = match &resource.backing {
            Backing::Real(kind) => kind.interface(),
            Backing::Sim(_) | Backing::Peer(_) => continue,
        };
        // PANIC Safety: `check_grants` ran first, so every grant names a cap.
        let cap = def
            .grants
            .iter()
            .find(|g| g.param == grant.param)
            .expect("a checked grant");
        if cap.interface != found {
            return Err(BuildError::Interface {
                cap: format!("{}.{}", system.id, grant.param),
                resource: resource.id.clone(),
                expected: cap.interface.to_string(),
                found: found.into(),
            });
        }
    }
    Ok(())
}

/// Returns the first port whose name repeats an earlier port's name.
fn duplicate(ports: &[PortDef]) -> Option<&PortDef> {
    ports
        .iter()
        .enumerate()
        .find(|(at, port)| ports[..*at].iter().any(|other| other.name == port.name))
        .map(|(_, port)| port)
}

/// Checks that every output listed in the config is an output the type defines.
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
                let Some(ring) = producer_ring(plan, index, &system.id, &input.port, from)? else {
                    continue;
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

/// Counts one reader slot for each edge into a ring.
///
/// A ring with no edges still gets one slot, because the ring format requires
/// at least one.
fn count_readers(systems: &[PlannedSystem<'_>], rings: &mut [RingSpec]) {
    for edge in systems.iter().flat_map(|s| s.inputs.iter().flatten()) {
        rings[*edge].readers += 1;
    }
    for ring in rings.iter_mut() {
        ring.readers = ring.readers.max(1);
    }
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
    io: &SharedIo,
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
        let cx = MakeCx {
            id: system.id,
            adapter: system.adapter,
            def: &system.def,
            params: system.params,
            inputs,
            outputs,
            threads,
            io,
        };
        let step = match system.adapter {
            Some(Adapter::Channel(_)) => crate::channel::make(&system.entry.make, cx),
            _ => (system.entry.make)(cx),
        }
        .map_err(|source| BuildError::Params {
            id: system.id.to_string(),
            source,
        })?;
        entries.push(Entry {
            name: system.id.to_string(),
            step: Some(step),
            // PANIC Safety: build allocates one ring per output.
            status: Output::new({
                let ring: &RingBuffer = &rings[system.status_ring()];
                ring.writer(NoWake).expect("one writer per output ring")
            }),
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
            outputs: vec![OutputConfig::record("imu", record)],
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
                grants: Vec::new(),
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
        let port = Output::<utils::Imu>::def(&DefCx::empty(), "sample");
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
                grants: Vec::new(),
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
        let port = Output::<utils::Imu>::def(&DefCx::empty(), "sample");
        let def = crate::SystemDef {
            name: "test".into(),
            inputs: vec![port.clone()],
            outputs: vec![port],
            grants: Vec::new(),
        };
        assert_eq!(check_port_names("instance", &def), Ok(()));
        assert_eq!(
            check_port_names(
                "empty",
                &crate::SystemDef::new::<(), (), NoWake>("empty", &crate::DefCx::empty())
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
        // imu + nav outputs and logs, plus one status ring each.
        assert_eq!(coordinator.rings(), 6);
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
        let plan = resolve(&config, &table).expect("valid config");
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
    fn test_reject_unresolved_peer_output() {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                outputs: vec![OutputConfig {
                    port: "imu".into(),
                    source: OutputSource::Peer(super::super::PeerPort {
                        target: "a".into(),
                        link: "pub".into(),
                        port: "plant.imu".into(),
                    }),
                }],
                ..SystemConfig::new("emit", "emit")
            }],
            ..Default::default()
        };
        assert_eq!(
            build(config).err(),
            Some(BuildError::Def {
                id: "emit".into(),
                source: DefError::Unresolved { port: "imu".into() },
            })
        );
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
                outputs: vec![OutputConfig::record("extra", "imu")],
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
                dt: Duration::from_millis(5)
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
