//! Turns a [`CoordinatorConfig`] into a running [`Coordinator`].
//!
//! Building happens in two passes. The first pass resolves each system's
//! definition and works out which output feeds which input. The second pass
//! allocates one ring per output, with one reader slot for every input that
//! reads from it, and binds each system to its rings.

use core::num::NonZeroUsize;
use core::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use crate::channel::{ABORT_PORT, COMMANDS_PORT};
use crate::def::{DefCx, Records};
use crate::io::{Backing, Cqe, IO_CQ, IO_SQ, IO_TYPE, IoState, ResourceEnd, ResourceIndex};
use crate::port::{Output, ring_capacity};
use crate::record::Record;
use crate::system::{PortDef, SystemDef};
use crate::thread::Threads;

use super::config::{
    Adapter, BuildOptions, Clock, CoordinatorConfig, InputConfig, Location, OutputConfig, PortRef,
    SystemConfig,
};
use super::error::BuildError;
use super::params::MakeError;
use super::params::Params;
use super::run::{Coordinator, Entry};
use super::status::{STATUS_PORT, SystemStatus};
use super::step::{Step, StepState};
use super::table::{MakeCx, MakeFn, SystemTable, TableEntry};

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
    location: Option<&'a Location>,
    /// The ring of each output port, in definition order.
    outputs: Vec<RingIndex>,
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
        let index = self.def.outputs.iter().position(|p| p.name == port)?;
        Some(self.outputs[index])
    }
}

impl CoordinatorConfig {
    /// Builds a [`Coordinator`] from this config, creating systems from `table`.
    pub fn build(self, table: &SystemTable) -> Result<Coordinator, BuildError> {
        self.build_with(table, BuildOptions::default())
    }

    /// Builds a [`Coordinator`] with `options`.
    pub fn build_with(
        mut self,
        table: &SystemTable,
        options: BuildOptions,
    ) -> Result<Coordinator, BuildError> {
        if options.deterministic {
            self.clock.check_deterministic()?;
        }
        check_channels(&self)?;
        let bindings = Bindings::collect(&self)?;
        wire_io(&mut self.systems, &bindings)?;
        let records = Arc::new(table.records());
        let mut plan = Plan::resolve(&self, table, &records, &bindings)?;
        plan.check_ids()?;
        plan.set_reader_counts();
        let rings = plan
            .rings
            .iter()
            .map(|spec| spec.allocate(self.ring_depth))
            .collect::<Result<Vec<_>, _>>()?;
        let mut threads = match options.deterministic {
            true => Threads::deterministic(),
            false => Threads::default(),
        };
        let members = io_members(&plan, &bindings);
        let mut io = IoState::build(&self.resources, members)?;
        let args = SharedMakeArgs {
            seed: self.seed,
            namespace: self.namespace.as_deref(),
            records: &records,
        };
        let mut entries = make_systems(&plan, &rings, &mut threads, &mut io, args)?;
        for group in threads.into_turned() {
            // PANIC Safety: a group starts with the make of an entry.
            let first = entries.iter_mut().find(|e| e.name == group.first());
            first.expect("the group's first member").turned = Some(group);
        }

        Ok(Coordinator {
            entries,
            io,
            clock: self.clock,
            epoch: self.clock.epoch().unwrap_or_else(Timestamp::now),
            cycle: 0,
            rings,
        })
    }
}

impl<'a> Plan<'a> {
    /// Resolves every system's definition, its rings, and its input edges.
    fn resolve(
        config: &'a CoordinatorConfig,
        table: &'a SystemTable,
        records: &Records,
        bindings: &Bindings,
    ) -> Result<Self, BuildError> {
        let mut plan = Self {
            index: index_by_id(config)?,
            systems: Vec::with_capacity(config.systems.len()),
            rings: Vec::new(),
        };
        for system in &config.systems {
            let (entry, def) = plan
                .define(system, table, records, config, bindings)
                .map_err(|e| e.at(system.location.as_ref()))?;
            plan.push(system, entry, def);
        }
        plan.resolve_edges(config)?;
        Ok(plan)
    }

    /// Looks up `system`'s type and computes its definition.
    fn define(
        &self,
        system: &SystemConfig,
        table: &'a SystemTable,
        records: &Records,
        config: &CoordinatorConfig,
        bindings: &Bindings,
    ) -> Result<(&'a TableEntry, SystemDef), BuildError> {
        let entry = table
            .get(&system.ty)
            .ok_or_else(|| BuildError::UnknownType {
                id: system.id.clone(),
                ty: system.ty.clone(),
            })?;
        entry.check_adapter(system)?;
        let namespace = config.namespace.as_deref();
        let def = SystemDef::planned(system, entry, self, records, namespace)?;
        check_bindings(&system.id, &def, bindings.of(&system.id), &config.resources)?;
        Ok((entry, def))
    }

    /// Adds `system`, with one ring per output.
    fn push(&mut self, system: &'a SystemConfig, entry: &'a TableEntry, def: SystemDef) {
        let outputs = def
            .outputs
            .iter()
            .map(|port| self.add_ring(&system.id, port.clone()))
            .collect();
        self.systems.push(PlannedSystem {
            id: &system.id,
            entry,
            inputs: vec![Vec::new(); def.inputs.len()],
            def,
            params: Params(&system.params),
            adapter: system.adapter.as_ref(),
            location: system.location.as_ref(),
            outputs,
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
        let Some(&index) = self.index.get(from.system.as_str()) else {
            return Err(BuildError::UnknownSystem {
                id: id.to_string(),
                port: port.to_string(),
                from: from.system.clone(),
            });
        };
        let Some(producer) = self.systems.get(index) else {
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
        for (index, system) in config.systems.iter().enumerate() {
            self.resolve_system_edges(index, system)
                .map_err(|e| e.at(system.location.as_ref()))?;
        }
        Ok(())
    }

    /// Records the ring behind each input edge of `system`, the system at
    /// `index`.
    fn resolve_system_edges(
        &mut self,
        index: usize,
        system: &SystemConfig,
    ) -> Result<(), BuildError> {
        for input in &system.inputs {
            let Some(port) = self.systems[index]
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
                    self.systems[index].inputs[port].push(ring);
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

    /// Checks that every input edge reads the record id its port expects.
    fn check_ids(&self) -> Result<(), BuildError> {
        for system in &self.systems {
            for (port, edges) in system.inputs.iter().enumerate() {
                let def = &system.def.inputs[port];
                for &ring in edges {
                    let spec = &self.rings[ring];
                    if spec.port.id != def.id {
                        return Err(BuildError::IdMismatch {
                            id: system.id.to_string(),
                            port: def.name.to_string(),
                            from: format!("{}.{}", spec.system, spec.port.name),
                            expected: def.id,
                            found: spec.port.id,
                        }
                        .at(system.location));
                    }
                }
            }
        }
        Ok(())
    }
}

impl SystemConfig {
    /// Returns each input edge the type sees, with the output definition
    /// behind it.
    ///
    /// `producer` looks up that definition. It returns `None` when the producer
    /// is not resolved yet, which leaves the edge out. That is only allowed for
    /// the `declared` inputs, whose definition does not depend on the producer.
    pub(crate) fn edges(
        &self,
        declared: &[PortDef],
        producer: impl Fn(&str, &PortRef) -> Result<Option<PortDef>, BuildError>,
    ) -> Result<Vec<(&str, PortDef)>, BuildError> {
        let adapter = self.adapter.as_ref();
        let mut edges = Vec::new();
        for input in &self.inputs {
            if adapter.is_some_and(|a| a.reserves_input(&input.port)) {
                return Err(BuildError::reserved_channel_port(&self.id, &input.port));
            }
            if adapter.is_some_and(|a| a.reads_input(&input.port)) {
                continue;
            }
            for from in &input.from {
                match producer(&input.port, from)? {
                    Some(def) => edges.push((input.port.as_str(), def)),
                    None if declared.iter().any(|p| p.name == input.port) => {}
                    None => {
                        return Err(BuildError::DynamicFromLater {
                            system: self.id.clone(),
                            port: input.port.clone(),
                        });
                    }
                }
            }
        }
        Ok(edges)
    }
}

impl Adapter {
    /// Whether a system behind the adapter may not take input `port`.
    fn reserves_input(&self, port: &str) -> bool {
        matches!(self, Self::Channel(_)) && port == ABORT_PORT
    }

    /// Whether the adapter reads input `port` itself, so the system never
    /// sees it.
    fn reads_input(&self, port: &str) -> bool {
        matches!(self, Self::Channel(_)) && port == COMMANDS_PORT
    }

    /// Appends the adapter's ports to `def`.
    ///
    /// # Errors
    /// Returns the name of a port of `def` that the adapter's ports take.
    fn add_ports(&self, def: SystemDef, cx: &DefCx<'_>) -> Result<SystemDef, String> {
        match self {
            Self::Thread(_) => Ok(def),
            Self::Channel(_) => crate::channel::def(def, cx),
        }
    }

    /// Makes a system with `make`, behind the adapter.
    ///
    /// `cx.def` is the definition [`Adapter::add_ports`] returned.
    fn make(&self, make: &Arc<MakeFn>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, MakeError> {
        match self {
            Self::Thread(_) => make(cx),
            Self::Channel(channel) => crate::channel::make_channel(make, channel, cx),
        }
    }
}

impl TableEntry {
    /// Checks that the type runs behind the adapter `system` gives it.
    fn check_adapter(&self, system: &SystemConfig) -> Result<(), BuildError> {
        if self.adapters.accepts(system.adapter.as_ref()) {
            return Ok(());
        }
        let adapter = match &system.adapter {
            None => "without an adapter".to_string(),
            Some(Adapter::Thread(thread)) => format!("on thread `{thread}`"),
            Some(Adapter::Channel(channel)) => format!("behind channel `{}`", channel.name),
        };
        Err(BuildError::UnsupportedAdapter {
            id: system.id.clone(),
            ty: system.ty.clone(),
            adapter,
        })
    }
}

/// Wires each system with a `Cap` (a member) to the `fsw.io` instances.
///
/// A member's SQ is read by the first `fsw.io` after it in step order, so one
/// must follow it. Its CQ is written by the last `fsw.io` before it, or by the
/// final one if none comes before. Each `fsw.io` gets its members' ports in
/// step order.
fn wire_io(systems: &mut [SystemConfig], bindings: &Bindings) -> Result<(), BuildError> {
    let members: Vec<usize> = (0..systems.len())
        .filter(|&m| bindings.holds(&systems[m].id))
        .collect();
    let Some(points) = IoPoints::new(systems) else {
        return match members.first() {
            Some(&m) => Err(BuildError::NoIoStep {
                system: systems[m].id.clone(),
            }
            .at(systems[m].location.as_ref())),
            None => Ok(()),
        };
    };
    for m in members {
        let id = systems[m].id.clone();
        let Some(next) = points.next(m) else {
            let error = BuildError::NoIoStepAfter { system: id };
            return Err(error.at(systems[m].location.as_ref()));
        };
        let prev = points.prev(m);
        let prev_id = systems[prev].id.clone();
        systems[next].inputs.push(InputConfig::new(&id, &id, IO_SQ));
        systems[prev]
            .outputs
            .push(OutputConfig::with_record(&id, Cqe::NAME));
        systems[m].inputs.push(InputConfig::new(IO_CQ, prev_id, id));
    }
    Ok(())
}

/// The positions of the `fsw.io` instances in the schedule, never empty.
struct IoPoints {
    positions: Vec<usize>,
}

impl IoPoints {
    /// `None` when the config places no `fsw.io`.
    fn new(systems: &[SystemConfig]) -> Option<Self> {
        let positions: Vec<usize> = (0..systems.len())
            .filter(|&position| systems[position].ty == IO_TYPE)
            .collect();
        (!positions.is_empty()).then_some(Self { positions })
    }

    /// The instance after `position`.
    fn next(&self, position: usize) -> Option<usize> {
        self.positions.iter().copied().find(|&p| p > position)
    }

    /// The instance before `position`, wrapping to the last.
    fn prev(&self, position: usize) -> usize {
        let before = self.positions.iter().rev().copied().find(|&p| p < position);
        // PANIC Safety: `new` makes no `IoPoints` without a position.
        before
            .or(self.positions.last().copied())
            .expect("an fsw.io")
    }
}

/// Checks that no two systems use the same channel name.
fn check_channels(config: &CoordinatorConfig) -> Result<(), BuildError> {
    let mut seen_names = std::collections::HashSet::new();
    for system in &config.systems {
        let Some(Adapter::Channel(channel)) = &system.adapter else {
            continue;
        };
        if !seen_names.insert(channel.name.as_str()) {
            let name = channel.name.to_string();
            return Err(BuildError::DuplicateChannel { name }.at(system.location.as_ref()));
        }
    }
    Ok(())
}

/// Maps each system id to its position in the config, rejecting duplicate ids.
fn index_by_id(config: &CoordinatorConfig) -> Result<HashMap<&str, usize>, BuildError> {
    let mut index = HashMap::with_capacity(config.systems.len());
    for (position, system) in config.systems.iter().enumerate() {
        if index.insert(system.id.as_str(), position).is_some() {
            let id = system.id.clone();
            return Err(BuildError::DuplicateId { id }.at(system.location.as_ref()));
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

    /// Computes the definition that `entry`'s type produces for `system`, with
    /// the adapter's ports and then the coordinator's `status` output last.
    ///
    /// `producer` is as for [`SystemConfig::edges`].
    pub(crate) fn instance(
        system: &SystemConfig,
        entry: &TableEntry,
        namespace: Option<&str>,
        records: &Records,
        producer: impl Fn(&str, &PortRef) -> Result<Option<PortDef>, BuildError>,
    ) -> Result<Self, BuildError> {
        let adapter = system.adapter.as_ref();
        let edges = system.edges(&entry.def.inputs, producer)?;
        let inputs: Vec<(&str, &PortDef)> = edges.iter().map(|(port, def)| (*port, def)).collect();
        let cx = DefCx {
            id: &system.id,
            namespace,
            inputs: &inputs,
            outputs: &system.outputs,
            records,
            params: &system.params,
        };
        let mut def = (entry.def_fn)(&cx).map_err(|source| BuildError::Def {
            id: system.id.clone(),
            source,
        })?;
        if def.outputs.iter().any(|p| p.name == STATUS_PORT) {
            return Err(BuildError::ReservedPort {
                ty: system.ty.clone(),
            });
        }
        if let Some(adapter) = adapter {
            def = adapter
                .add_ports(def, &cx)
                .map_err(|port| BuildError::reserved_channel_port(&system.id, &port))?;
        }
        def.outputs.push(SystemStatus::port_def(&cx));
        Ok(def)
    }

    /// Checks that no two inputs and no two outputs share a name.
    fn check_port_names(&self, system: &SystemConfig) -> Result<(), BuildError> {
        let port = |port: &PortDef| (system.id.clone(), port.name.to_string());
        if let Some((system, port)) = find_duplicate(&self.inputs).map(port) {
            return Err(BuildError::DuplicateInput { system, port });
        }
        if let Some((system, port)) = find_duplicate(&self.outputs).map(port) {
            return Err(BuildError::DuplicateOutput { system, port });
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

/// One `Cap` parameter's resource, and the end it holds.
struct Binding {
    param: String,
    resource: ResourceIndex,
    end: ResourceEnd,
}

/// Each system's `Cap` bindings, by system id.
#[derive(Default)]
struct Bindings(HashMap<String, Vec<Binding>>);

impl Bindings {
    /// Collects each system's bindings: its grants of real resources, and the
    /// holders each sim resource names.
    ///
    /// Rejects a grant of an unknown or a sim resource, a holder the config
    /// does not have, and a parameter bound twice.
    fn collect(config: &CoordinatorConfig) -> Result<Self, BuildError> {
        let mut bound = Self::default();
        for system in &config.systems {
            bound
                .bind_grants(system, config)
                .map_err(|e| e.at(system.location.as_ref()))?;
        }
        for (position, (id, backing)) in config.resources.iter().enumerate() {
            let Backing::Sim(sim) = backing else {
                continue;
            };
            for (holder, end) in [
                (&sim.host, ResourceEnd::Host),
                (&sim.device, ResourceEnd::Device),
            ] {
                if !config.systems.iter().any(|s| s.id == holder.system) {
                    return Err(BuildError::UnknownHolder {
                        resource: id.clone(),
                        system: holder.system.clone(),
                    });
                }
                let resource = resource_index(config, position)?;
                bound.bind(&holder.system, &holder.param, resource, end)?;
            }
        }
        Ok(bound)
    }

    /// Binds each of `system`'s grants to the real resource it names.
    fn bind_grants(
        &mut self,
        system: &SystemConfig,
        config: &CoordinatorConfig,
    ) -> Result<(), BuildError> {
        for (param, resource) in &system.grants {
            let unknown = || BuildError::UnknownResource {
                system: system.id.clone(),
                param: param.clone(),
                resource: resource.clone(),
            };
            let (position, _, backing) = config.resources.get_full(resource).ok_or_else(unknown)?;
            if let Backing::Sim(_) = backing {
                return Err(BuildError::SimGrant {
                    system: system.id.clone(),
                    param: param.clone(),
                    resource: resource.clone(),
                });
            }
            let resource = resource_index(config, position)?;
            self.bind(&system.id, param, resource, ResourceEnd::Host)?;
        }
        Ok(())
    }

    fn bind(
        &mut self,
        system: &str,
        param: &str,
        resource: ResourceIndex,
        end: ResourceEnd,
    ) -> Result<(), BuildError> {
        let caps = self.0.entry(system.to_string()).or_default();
        if caps.iter().any(|b| b.param == param) {
            return Err(BuildError::DuplicateGrant {
                system: system.to_string(),
                param: param.to_string(),
            });
        }
        caps.push(Binding {
            param: param.to_string(),
            resource,
            end,
        });
        Ok(())
    }

    /// `system`'s bindings, in the order they were bound.
    fn of(&self, system: &str) -> &[Binding] {
        self.0.get(system).map_or(&[], Vec::as_slice)
    }

    /// Whether `system` holds a resource.
    fn holds(&self, system: &str) -> bool {
        self.0.contains_key(system)
    }
}

/// The index of the resource at `position` in `config`'s resources.
fn resource_index(
    config: &CoordinatorConfig,
    position: usize,
) -> Result<ResourceIndex, BuildError> {
    let count = config.resources.len();
    ResourceIndex::try_from(position).map_err(|_| BuildError::TooManyResources { count })
}

/// Checks that `system`'s bindings and its `Cap` parameters match one to
/// one, and that each resource provides its parameter's interface.
fn check_bindings(
    system: &str,
    def: &SystemDef,
    bound: &[Binding],
    resources: &IndexMap<String, Backing>,
) -> Result<(), BuildError> {
    let error = |param: &str| (system.to_string(), param.to_string());
    for binding in bound {
        let Some(cap) = def.grants.iter().find(|g| g.param == binding.param) else {
            let (system, param) = error(&binding.param);
            return Err(BuildError::UnknownGrant { system, param });
        };
        let position = usize::from(binding.resource.get());
        // PANIC Safety: `Bindings::collect` took every index from `resources`.
        let (id, backing) = resources.get_index(position).expect("a bound resource");
        let found = backing.interface();
        if cap.interface != found {
            return Err(BuildError::Interface {
                cap: format!("{system}.{}", binding.param),
                resource: id.clone(),
                expected: cap.interface.to_string(),
                found: found.into(),
            });
        }
    }
    if let Some(cap) = def
        .grants
        .iter()
        .find(|g| !bound.iter().any(|b| b.param == g.param))
    {
        let (system, param) = error(&cap.param);
        return Err(BuildError::Ungranted { system, param });
    }
    Ok(())
}

/// Each system with a `Cap`, in config order: its id and the resource index
/// and end of each `Cap` parameter, in definition order.
fn io_members(
    plan: &Plan<'_>,
    bindings: &Bindings,
) -> Vec<(String, Vec<(ResourceIndex, ResourceEnd)>)> {
    let caps = |system: &PlannedSystem<'_>| {
        let bound = bindings.of(system.id);
        let grant = |param: &str| {
            let found = bound.iter().find(|b| b.param == param);
            // PANIC Safety: `check_bindings` bound every `Cap` exactly once.
            let binding = found.expect("a checked binding");
            (binding.resource, binding.end)
        };
        system.def.grants.iter().map(|g| grant(&g.param)).collect()
    };
    plan.systems
        .iter()
        .filter(|system| !system.def.grants.is_empty())
        .map(|system| (system.id.to_string(), caps(system)))
        .collect()
}

/// Returns the first port whose name repeats an earlier port's name.
fn find_duplicate(ports: &[PortDef]) -> Option<&PortDef> {
    ports
        .iter()
        .enumerate()
        .find(|(index, port)| ports[..*index].iter().any(|other| other.name == port.name))
        .map(|(_, port)| port)
}

impl RingSpec {
    /// Allocates a ring that holds `self.port.depth * ring_depth` records.
    fn allocate(&self, ring_depth: NonZeroUsize) -> Result<RingBuffer, BuildError> {
        let too_large = || BuildError::RingTooLarge {
            system: self.system.clone(),
            port: self.port.name.to_string(),
            max_len: self.port.max_len,
        };
        let depth = self
            .port
            .depth
            .checked_mul(ring_depth.get())
            .ok_or_else(too_large)?;
        let capacity = ring_capacity(self.port.max_len, depth).ok_or_else(too_large)?;
        let config = Config {
            capacity,
            max_readers: self.readers,
        };
        RingBuffer::create_in_memory(config).map_err(|_| too_large())
    }
}

/// The make arguments every system in the target shares.
#[derive(Clone, Copy)]
struct SharedMakeArgs<'a> {
    seed: u64,
    namespace: Option<&'a str>,
    records: &'a Arc<Records>,
}

/// Makes every planned system, bound to its rings.
fn make_systems(
    plan: &Plan<'_>,
    rings: &[RingBuffer],
    threads: &mut Threads,
    io: &mut IoState,
    args: SharedMakeArgs<'_>,
) -> Result<Vec<Entry>, BuildError> {
    let mut entries = Vec::with_capacity(plan.systems.len());
    for system in &plan.systems {
        // PANIC Safety: `SystemDef::instance` puts `status` last.
        let (&status, outputs) = system.outputs.split_last().expect("a status output");
        let outputs: Vec<&RingBuffer> = outputs.iter().map(|&ring| &rings[ring]).collect();
        let mut def = system.def.clone();
        def.outputs.pop();
        let inputs: Vec<Vec<&RingBuffer>> = system
            .inputs
            .iter()
            .map(|edges| edges.iter().map(|&ring| &rings[ring]).collect())
            .collect();
        // PANIC Safety: build allocates one ring per output.
        let status = rings[status]
            .writer(NoWake)
            .expect("one writer per output ring");
        let cx = MakeCx {
            id: system.id,
            adapter: system.adapter,
            def: &def,
            params: system.params,
            inputs,
            outputs,
            threads,
            io,
            seed: args.seed,
            namespace: args.namespace,
            records: args.records,
        };
        let step = match system.adapter {
            Some(adapter) => adapter.make(&system.entry.make, cx),
            None => (system.entry.make)(cx),
        }
        .map_err(|source| {
            let id = system.id.to_string();
            BuildError::Make { id, source }.at(system.location)
        })?;
        entries.push(Entry {
            name: system.id.to_string(),
            step: StepState::Live(step),
            status: Output::new(status),
            turned: None,
        });
    }
    Ok(entries)
}

impl Clock {
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

    /// Returns the wall-clock time budget for one cycle, or `None` for a
    /// simulated clock.
    pub(crate) fn cycle_budget(&self) -> Option<Duration> {
        match self {
            Clock::Wall { rate } => {
                Some(Duration::try_from_secs_f64(1.0 / rate.get()).unwrap_or(Duration::MAX))
            }
            Clock::Simulated { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::coordinator::{
        ChannelConfig, InputConfig, OutputConfig, OutputSource, PortRef, SystemConfig, WallRate,
    };
    use crate::def::DefError;
    use crate::tests::utils::{self, Recorder, pipeline_config, table};
    use crate::{Frame, Payload, Record};
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
                layout: utils::Imu::layout(),
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
                log: None,
                grants: Vec::new(),
                io: None,
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
                log: None,
                grants: Vec::new(),
                io: None,
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
            log: None,
            grants: Vec::new(),
            io: None,
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
            assert!(WallRate::try_from(rate).is_err(), "{rate}");
            let json = serde_json::json!({ "Wall": { "rate": rate } });
            assert!(serde_json::from_value::<Clock>(json).is_err(), "{rate}");
        }
        let clock: Clock = serde_json::from_str(r#"{"Wall":{"rate":0.001}}"#).expect("the minimum");
        assert_eq!(
            serde_json::to_string(&clock).expect("encodes"),
            r#"{"Wall":{"rate":0.001}}"#
        );
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
    fn test_instance_puts_status_last() {
        let table = table(&Recorder::default());
        let system = source("imu");
        let entry = table.get(&system.ty).expect("registered");
        let records = table.records();
        let def = SystemDef::instance(&system, entry, Some("sat"), &records, |_, _| Ok(None))
            .expect("a static definition");
        let cx = DefCx {
            id: "imu",
            namespace: Some("sat"),
            ..DefCx::empty()
        };
        assert_eq!(def.outputs.last(), Some(&SystemStatus::port_def(&cx)));
        assert_eq!(def.outputs.len(), entry.def.outputs.len() + 1);
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

    /// An error about a system cites where the target file created it.
    #[test]
    fn test_errors_cite_the_system_location() {
        let at = |line| super::Location {
            file: "target.py".into(),
            line,
        };
        let located = |line, error| BuildError::Located {
            location: at(line),
            source: Box::new(error),
        };
        let mut unknown_type = SystemConfig::new("imu", "gyro");
        unknown_type.location = Some(at(3));
        let config = CoordinatorConfig {
            systems: vec![unknown_type],
            ..Default::default()
        };
        let error = build(config).err().expect("an unknown type");
        assert_eq!(
            error.to_string(),
            "target.py:3: system `imu` has unregistered type `gyro`"
        );

        let mut config = pipeline_config();
        config.systems[1].location = Some(at(7));
        config.systems[1].inputs[0].from = vec![PortRef::new("gyro", "imu")];
        let unknown_system = BuildError::UnknownSystem {
            id: "nav".into(),
            port: "imu".into(),
            from: "gyro".into(),
        };
        assert_eq!(build(config).err(), Some(located(7, unknown_system)));

        let mut twice = source("imu");
        twice.location = Some(at(9));
        let config = CoordinatorConfig {
            systems: vec![source("imu"), twice],
            ..Default::default()
        };
        let duplicate = BuildError::DuplicateId { id: "imu".into() };
        assert_eq!(build(config).err(), Some(located(9, duplicate)));
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
            ring_depth: NonZeroUsize::MAX,
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
            ring_depth: NonZeroUsize::new(1 << 58).expect("nonzero"),
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
            Some(BuildError::Make {
                id: "nav".into(),
                source: crate::coordinator::ParamError::UnknownKey("gain".into()).into(),
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
        let plan = Plan::resolve(&config, &table, &table.records(), &Bindings::default())
            .expect("valid config");
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
    fn test_cycle_budget() {
        let simulated = Clock::Simulated {
            dt: Duration::from_millis(5),
            epoch: None,
        };
        assert_eq!(simulated.cycle_budget(), None);
        let clock = Clock::Wall {
            rate: WallRate::try_from(f64::MAX).expect("finite"),
        };
        assert_eq!(clock.cycle_budget(), Some(Duration::ZERO));
    }

    #[test]
    fn test_minimum_wall_rate_budget() {
        let clock = Clock::Wall {
            rate: WallRate::try_from(WallRate::MIN).expect("the minimum"),
        };
        assert_eq!(clock.cycle_budget(), Some(Duration::from_secs(1_000)));
    }

    #[cfg(unix)]
    mod io {
        use super::*;
        use crate::io::{Cap, Holder, SimKind, SimResource, Uart};

        #[system]
        fn line(uart: &Cap<Uart>) {
            let _ = uart;
        }

        fn table() -> SystemTable {
            let mut table = SystemTable::new();
            crate::builtins::register_builtins(&mut table).unwrap();
            table.register("line", line(), || ()).unwrap();
            table
        }

        fn io(id: &str) -> SystemConfig {
            SystemConfig::new(id, IO_TYPE)
        }

        fn member(id: &str) -> SystemConfig {
            SystemConfig::new(id, "line")
        }

        /// `wire`, a sim uart from `host.uart` to `device.uart`.
        fn config(systems: Vec<SystemConfig>, host: &str, device: &str) -> CoordinatorConfig {
            let sim = SimResource {
                kind: SimKind::Uart,
                host: Holder::new(host, "uart"),
                device: Holder::new(device, "uart"),
            };
            CoordinatorConfig {
                systems,
                resources: [("wire".to_string(), Backing::Sim(sim))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            }
        }

        /// A driver, `io_submit`, a model, then `io_complete`.
        fn nominal() -> Vec<SystemConfig> {
            vec![member("a"), io("io_submit"), member("b"), io("io_complete")]
        }

        fn error(config: CoordinatorConfig) -> BuildError {
            match config.build(&table()) {
                Ok(_) => panic!("the config built"),
                Err(e) => e,
            }
        }

        #[test]
        fn test_io_nominal_placement_builds() {
            config(nominal(), "a", "b").build(&table()).unwrap();
        }

        /// `a`'s CQ wraps around from `io_complete`.
        #[test]
        fn test_io_wiring_follows_step_order() {
            let config = config(nominal(), "a", "b");
            let bindings = Bindings::collect(&config).unwrap();
            let mut systems = config.systems;
            wire_io(&mut systems, &bindings).unwrap();
            let ports = |s: &SystemConfig| {
                let inputs = s.inputs.iter().map(|i| (i.port.clone(), i.from.clone()));
                let outputs = s.outputs.iter().map(|o| o.port.clone());
                (inputs.collect::<Vec<_>>(), outputs.collect::<Vec<_>>())
            };
            let from = |system: &str, port: &str| vec![PortRef::new(system, port)];
            assert_eq!(
                ports(&systems[0]),
                (vec![(IO_CQ.into(), from("io_complete", "a"))], vec![])
            );
            assert_eq!(
                ports(&systems[1]),
                (vec![("a".into(), from("a", IO_SQ))], vec!["b".into()])
            );
            assert_eq!(
                ports(&systems[2]),
                (vec![(IO_CQ.into(), from("io_submit", "b"))], vec![])
            );
            assert_eq!(
                ports(&systems[3]),
                (vec![("b".into(), from("b", IO_SQ))], vec!["a".into()])
            );
        }

        #[test]
        fn test_io_member_after_the_last_instance_is_refused() {
            let mut systems = nominal();
            systems.push(member("c"));
            let mut config = config(systems, "a", "b");
            let second = Backing::Sim(SimResource {
                kind: SimKind::Uart,
                host: Holder::new("c", "uart"),
                device: Holder::new("c2", "uart"),
            });
            config.resources.insert("second".into(), second);
            config.systems.push(member("c2"));
            let e = error(config);
            let after =
                |e: &BuildError| matches!(e, BuildError::NoIoStepAfter { system } if system == "c");
            assert!(after(&e), "{e}");
        }

        #[test]
        fn test_io_member_without_an_instance_is_refused() {
            let e = error(config(vec![member("a"), member("b")], "a", "b"));
            let none =
                |e: &BuildError| matches!(e, BuildError::NoIoStep { system } if system == "a");
            assert!(none(&e), "{e}");
        }

        #[test]
        fn test_io_on_a_thread_is_refused() {
            let mut config = config(nominal(), "a", "b");
            config.systems[3].adapter = Some(Adapter::Thread("io".into()));
            let e = error(config);
            let refused = |e: &BuildError| matches!(e, BuildError::UnsupportedAdapter { id, .. } if id == "io_complete");
            assert!(refused(&e), "{e}");
        }

        #[test]
        fn test_io_on_a_channel_builds() {
            let mut config = config(nominal(), "a", "b");
            config.systems[3].adapter = Some(Adapter::Channel(ChannelConfig {
                name: crate::ChannelName::try_from("io".to_string()).expect("a short name"),
                start: Default::default(),
            }));
            config.build(&table()).unwrap();
        }

        #[test]
        fn test_granting_a_sim_resource_is_refused() {
            let mut config = config(nominal(), "a", "b");
            config.systems[0]
                .grants
                .insert("uart".into(), "wire".into());
            let e = error(config);
            assert!(
                matches!(&e, BuildError::SimGrant { system, .. } if system == "a"),
                "{e}"
            );
        }

        #[test]
        fn test_unknown_holder_is_refused() {
            let e = error(config(nominal(), "a", "ghost"));
            let unknown = |e: &BuildError| matches!(e, BuildError::UnknownHolder { system, .. } if system == "ghost");
            assert!(unknown(&e), "{e}");
        }

        #[test]
        fn test_parameter_bound_twice_is_refused() {
            let e = error(config(nominal(), "a", "a"));
            let twice = |e: &BuildError| matches!(e, BuildError::DuplicateGrant { system, param } if system == "a" && param == "uart");
            assert!(twice(&e), "{e}");
        }

        /// A grant names a real resource. `kind` backs it.
        fn granted(kind: crate::io::RealKind) -> CoordinatorConfig {
            let mut config = config(nominal(), "a", "b");
            config.resources.insert("tty".into(), Backing::Real(kind));
            config
        }

        fn can() -> crate::io::RealKind {
            crate::io::RealKind::Can {
                iface: "can0".parse().expect("a CAN name"),
                filters: Vec::new(),
            }
        }

        #[test]
        fn test_grant_of_another_interface_is_refused() {
            let mut config = granted(can());
            let mut c = member("c");
            c.grants.insert("uart".into(), "tty".into());
            config.systems.insert(1, c);
            let e = error(config);
            let wrong = |e: &BuildError| matches!(e, BuildError::Interface { cap, expected, found, .. } if cap == "c.uart" && expected == "uart" && found == "can");
            assert!(wrong(&e), "{e}");
        }

        #[test]
        fn test_grant_of_a_non_cap_param_is_refused() {
            let mut config = granted(can());
            config.systems[0].grants.insert("gps".into(), "tty".into());
            let e = error(config);
            let unknown = |e: &BuildError| matches!(e, BuildError::UnknownGrant { system, param } if system == "a" && param == "gps");
            assert!(unknown(&e), "{e}");
        }

        #[test]
        fn test_io_errors_cite_the_system_location() {
            let at = |line| {
                Some(crate::coordinator::Location {
                    file: "target.py".into(),
                    line,
                })
            };
            let mut sim_grant = config(nominal(), "a", "b");
            sim_grant.systems[0]
                .grants
                .insert("uart".into(), "wire".into());
            sim_grant.systems[0].location = at(4);
            let e = error(sim_grant);
            assert!(e.to_string().starts_with("target.py:4: "), "{e}");
            let mut no_io = config(vec![member("a"), member("b")], "a", "b");
            no_io.systems[0].location = at(5);
            let e = error(no_io);
            assert!(e.to_string().starts_with("target.py:5: "), "{e}");
        }

        #[test]
        fn test_unbound_cap_is_refused() {
            let mut systems = nominal();
            systems.insert(1, member("c"));
            let e = error(config(systems, "a", "b"));
            let unbound =
                |e: &BuildError| matches!(e, BuildError::Ungranted { system, .. } if system == "c");
            assert!(unbound(&e), "{e}");
        }
    }
}
