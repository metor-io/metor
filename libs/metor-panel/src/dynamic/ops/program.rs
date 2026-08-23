//! Compiled Python systems, running as streaming nodes.
//!
//! A `@system` is a function over frames, and the panel already has everything
//! a frame needs except the frame: rings that carry `[Timestamp][value]`,
//! content-hashed nodes that dedup, and a worker thread that owns their tasks.
//! So a compiled system becomes one node like any other — what is new is only
//! what happens between reading the inputs and writing the output.
//!
//! ## The run rule, against real rings
//!
//! The design says a system fires when its **driving** input publishes and
//! reads the **latest** of everything else. A disruptor has no `latest()`: it
//! is a fan-out stream, and a reader that is not drained makes its *producer*
//! drop samples. So the latest of a non-driving input is kept where it can be
//! kept cheaply — in the system's own task, refreshed by draining that input's
//! reader with `try_next` on the way past. One task, one reader per port, no
//! shared cells and no second loop. `resample` already reads its inputs this
//! way; this is the same shape with N of them.
//!
//! An input that has never published leaves its cell empty, and the cycle is
//! skipped — the `else return` a Rust system writes, in the one place where
//! the language cannot express it.
//!
//! ## A frame is one sample
//!
//! The system node's ring carries the output frame's *bytes*, not a value: a
//! frame is several fields of several types and no single `ComponentSchema`
//! describes it honestly. [`field`] nodes hang off it, one per output field,
//! each with the schema that field really has. That is also what makes
//! publishing work — a field node is an ordinary value node, so `persist`
//! registers it as `<system>.<field>` with no new machinery.
//!
//! ## Faults park, they do not cascade
//!
//! Every evaluation runs under a fuel grant. A `while True:` burns its grant
//! and a bad index traps; either way the system stops evaluating, records what
//! happened for the pane to show, and keeps draining its inputs so nothing
//! upstream backs up behind it. The module is not torn down and no other
//! system notices. An edit is what clears a fault, because an edit is what can
//! fix one.
//!
//! ## State outlives the instance
//!
//! Editing must not reset the world, so state has to survive a rebuild — but
//! the instance holding it lives inside a spawned task, where a rebuild cannot
//! reach. Rather than ask the task for it, the task publishes it: after every
//! evaluation the state slots are copied into a cell the node owns, which is
//! at most a few words and makes the snapshot always current. A rebuild reads
//! that cell, hands it to the new system, and `metor_expr::state` decides
//! field by field what still matches.

use std::sync::{Arc, Mutex};

use metor_db::ComponentSchema;
use metor_db::disruptor::Disruptor;
use metor_expr::state::Snapshot;
use metor_expr::{Diagnostics, Manifest, Resolver, Ty};
use metor_proto::types::{PrimType, Timestamp};
use wasmi::{Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_value, write_sample,
};
use crate::dynamic::tensor::read_f64_at;

/// Fuel one evaluation may burn. The M4 sweep put a realistic expression in
/// the tens of units and a hundred-term contraction near a thousand, so this
/// is roughly four orders of margin — generous enough that no honest program
/// meets it, tight enough that a runaway loop is a diagnostic within a frame.
pub const DEFAULT_FUEL: u64 = 1_000_000;

/// A program compiled once, ready to instantiate per system.
///
/// Compiling is where the expensive part lives, and it deliberately happens
/// away from the worker thread: building a node blocks the UI until it
/// returns, so the closure that runs there does nothing but instantiate.
pub struct Compiled {
    pub source: Arc<str>,
    pub manifest: Manifest,
    engine: Engine,
    module: Module,
}

impl Compiled {
    pub fn module(source: &str, resolver: &dyn Resolver) -> Result<Self, Diagnostics> {
        Self::from_program(source, metor_expr::compile_module(source, resolver)?)
    }

    /// One bare expression, as the single system a `=` field runs.
    pub fn expression(source: &str, resolver: &dyn Resolver) -> Result<Self, Diagnostics> {
        Self::from_program(source, metor_expr::compile_expr(source, resolver)?)
    }

    fn from_program(source: &str, program: metor_expr::Program) -> Result<Self, Diagnostics> {
        let mut config = wasmi::Config::default();
        config.compilation_mode(wasmi::CompilationMode::Eager);
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &program.wasm[..])
            .expect("a module this crate emitted validates");
        Ok(Compiled {
            source: source.into(),
            manifest: program.manifest,
            engine,
            module,
        })
    }

    /// What identifies one system across a rebuild: the source it was written
    /// in and everything its ports resolved to. An edit elsewhere in the
    /// module leaves this untouched, which is what lets the rest keep running.
    pub fn system_hash(&self, index: usize, ports: &[NodeId]) -> NodeId {
        let system = &self.manifest.systems[index];
        let region = self
            .source
            .get(system.source.start as usize..system.source.end as usize)
            .unwrap_or(&self.source);
        hash_id(op_tag::EXPR_SYSTEM, ports, |h| {
            use std::hash::Hash;
            region.hash(h);
            postcard::to_allocvec(system)
                .expect("a system descriptor encodes")
                .hash(h);
        })
    }
}

/// What a running system is doing, for the pane to render.
///
/// One lock, read once a frame by the UI thread and written only when a
/// system faults — which is to say, almost never.
#[derive(Clone, Default)]
pub struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    /// Why this system stopped evaluating, if it has.
    pub fn fault(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    fn park(&self, why: String) {
        *self.0.lock().unwrap() = Some(why);
    }
}

/// The state slots of a live instance, kept current so a rebuild can read
/// them without reaching into the task that owns them.
#[derive(Clone, Default)]
pub struct StateCell(Arc<Mutex<Snapshot>>);

impl StateCell {
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap().clone()
    }
}

/// One compiled system, running.
pub struct System {
    pub node: Arc<dyn DynamicNode>,
    pub health: Health,
    pub state: StateCell,
}

/// One input port: where its samples come from, and what the host already has
/// for it.
pub struct PortSource {
    pub node: Arc<dyn DynamicNode>,
    /// The most recent sample already committed, if there is one.
    ///
    /// A disruptor reader begins at the write head and never sees what came
    /// before it, so without this a system over a quiet channel waits for a
    /// sample that may not arrive for minutes — and a plot of it looks broken
    /// rather than idle.
    pub seed: Option<(Timestamp, Vec<u8>)>,
}

impl PortSource {
    /// A port with no history behind it.
    pub fn live(node: Arc<dyn DynamicNode>) -> Self {
        PortSource { node, seed: None }
    }
}

/// Spawn the task that drives one system.
///
/// `ports` supplies one source per input port, in the manifest's order; `seed`
/// carries state over from the instance this one replaces.
pub fn system(
    compiled: &Arc<Compiled>,
    index: usize,
    ports: Vec<PortSource>,
    fuel: u64,
    seed: Option<&Snapshot>,
) -> Result<System, BuildError> {
    let desc = compiled
        .manifest
        .systems
        .get(index)
        .ok_or(BuildError::Expr("no such system".into()))?;
    if ports.len() != desc.inputs.len() {
        return Err(BuildError::WrongArity {
            op: "expr.system",
            expected: desc.inputs.len(),
            got: ports.len(),
        });
    }

    let mut instance = Running::new(compiled, index, fuel)?;
    if let Some(seed) = seed {
        instance.seed(seed);
    }
    let state = StateCell::default();
    *state.0.lock().unwrap() = instance.read_state();

    // Every reader subscribes now, not inside the task: a disruptor reader
    // only ever sees what is committed after it is made.
    let driving = desc.driving.unwrap_or(0);
    if ports.is_empty() {
        return Err(BuildError::Expr(
            "a system with no inputs has nothing to fire it".into(),
        ));
    }

    let mut layouts = Vec::with_capacity(ports.len());
    let mut readers = Vec::with_capacity(ports.len());
    let mut latest = Vec::with_capacity(ports.len());
    for (port, source) in desc.inputs.iter().zip(&ports) {
        let schema = require_value(&source.node)?;
        let layout = PortLayout::new(port, &schema)?;
        // Whatever the host already knows about this port becomes its first
        // held value, so a system is not blind to everything published before
        // it existed.
        let mut held = Vec::new();
        if let Some((_, value)) = &source.seed
            && value.len() == layout.sample_bytes
        {
            layout.fill(value, &mut held);
        }
        latest.push(held);
        layouts.push(layout);
        readers.push(Some(source.node.subscribe()));
    }
    // The driving port's own history is what the system fires from once at
    // startup, so a quiet channel still shows its current value.
    let opening = ports[driving].seed.clone().filter(|(_, value)| {
        value.len() == layouts[driving].sample_bytes
    });
    let wired = Ports {
        driving: readers[driving].take().expect("the driving port is wired once"),
        others: readers
            .into_iter()
            .enumerate()
            .filter_map(|(i, reader)| Some((i, reader?)))
            .collect(),
        latest,
        layouts,
    };

    let id = compiled.system_hash(index, &ports.iter().map(|p| p.node.id()).collect::<Vec<_>>());
    let frame_bytes = desc.output.bytes as usize;
    let health = Health::default();
    let node = NodeImpl::spawn(
        id,
        ValueType::Value(ComponentSchema::new(PrimType::U8, &[frame_bytes])),
        ports[driving].node.parent_clock_id(),
        default_ring_bytes(frame_bytes),
        {
            let health = health.clone();
            let state = state.clone();
            move |output| async move {
                let _ports = ports;
                run(instance, wired, driving, opening, output, health, state).await;
            }
        },
    );

    Ok(System {
        node,
        health,
        state,
    })
}

/// The node id [`field`] will give one output field.
///
/// Computable without building anything, so a caller can ask whether an
/// expression is already running before spawning a second copy of it.
pub fn field_id(system: NodeId, field: usize) -> NodeId {
    hash_id(op_tag::EXPR_FIELD, &[system], |h| {
        use std::hash::Hash;
        field.hash(h);
    })
}

/// One field of a system's output frame, as an ordinary value node.
pub fn field(
    compiled: &Arc<Compiled>,
    index: usize,
    field: usize,
    frame: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let desc = &compiled.manifest.systems[index];
    let spec = &desc.output.fields[field];
    let schema = schema_of(&spec.ty);
    let id = field_id(frame.id(), field);
    let mut reader = frame.subscribe();
    let (offset, ty) = (spec.offset as usize, spec.ty.clone());
    let frame_bytes = desc.output.bytes as usize;
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        frame.parent_clock_id(),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _frame = frame;
            let mut scratch = Vec::with_capacity(schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, bytes) in grant.samples() {
                    if bytes.len() != frame_bytes {
                        continue;
                    }
                    read_field(&bytes[offset..], &ty, &mut scratch);
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// The last sample a component already holds, for seeding a port.
///
/// This is the same read `views/binding.rs` does before entering its stream
/// loop, and for the same reason: a fresh reader only sees what is committed
/// from now on, so anything already published is invisible without it.
pub fn latest_sample(db: &metor_db::DB, component: metor_proto::types::ComponentId) -> Option<(Timestamp, Vec<u8>)> {
    db.with_state(|state| {
        let latest = state.get_component(component)?.time_series.latest()?;
        Some((latest.timestamp(), latest.data().to_vec()))
    })
}

/// The component a field of the language publishes as.
pub fn schema_of(ty: &Ty) -> ComponentSchema {
    match ty {
        Ty::F64 => ComponentSchema::new(PrimType::F64, &[]),
        Ty::I64 => ComponentSchema::new(PrimType::I64, &[]),
        Ty::Bool => ComponentSchema::new(PrimType::Bool, &[]),
        Ty::Tensor { shape, .. } => ComponentSchema::new(PrimType::F64, shape),
    }
}

/// Widen a frame slot into the bytes a component carries.
///
/// A frame gives every element eight bytes so nothing straddles; a component
/// gives `bool` one byte, so only that case is more than a copy.
fn read_field(slot: &[u8], ty: &Ty, out: &mut Vec<u8>) {
    out.clear();
    match ty {
        Ty::Bool => out.push(u32::from_le_bytes(slot[..4].try_into().unwrap()) as u8),
        Ty::Tensor { shape, .. } => {
            out.extend_from_slice(&slot[..shape.iter().product::<usize>() * 8])
        }
        _ => out.extend_from_slice(&slot[..8]),
    }
}

/// How a component's bytes become a frame's.
struct PortLayout {
    /// Offset of the port's single field within its frame.
    offset: usize,
    frame_bytes: usize,
    field: Ty,
    source: PrimType,
    elements: usize,
    /// Bytes one sample of the source component occupies.
    sample_bytes: usize,
}

impl PortLayout {
    fn new(port: &metor_expr::Port, schema: &ComponentSchema) -> Result<Self, BuildError> {
        // A port the panel builds is a projection of one component, so it has
        // exactly one field; a multi-field frame is a Phase 3 shape.
        let [field] = port.frame.fields.as_slice() else {
            return Err(BuildError::Expr(format!(
                "`{}` binds a frame of {} fields; the panel binds one component per port",
                port.param,
                port.frame.fields.len()
            )));
        };
        Ok(PortLayout {
            offset: field.offset as usize,
            frame_bytes: port.frame.bytes as usize,
            field: field.ty.clone(),
            source: schema.prim_type,
            elements: schema.dim.iter().product::<usize>().max(1),
            sample_bytes: schema.size(),
        })
    }

    /// Render one component sample as this port's frame bytes.
    fn fill(&self, value: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.resize(self.frame_bytes, 0);
        let slot = &mut out[self.offset..];
        for i in 0..self.elements {
            let v = read_f64_at(value, self.source, i);
            let at = i * 8;
            match self.field {
                Ty::Bool => slot[at..at + 4].copy_from_slice(&((v != 0.0) as u32).to_le_bytes()),
                Ty::I64 => slot[at..at + 8].copy_from_slice(&(v as i64).to_le_bytes()),
                _ => slot[at..at + 8].copy_from_slice(&v.to_le_bytes()),
            }
        }
    }
}

/// The ports of one system.
///
/// The driving reader is a field of its own rather than an entry in the list:
/// it is borrowed across an await while the others are drained, and only
/// disjoint fields can be.
struct Ports {
    driving: NodeReader,
    /// Every other port's reader, with the index of the port it fills.
    others: Vec<(usize, NodeReader)>,
    layouts: Vec<PortLayout>,
    /// The frame bytes each port last contributed. Empty until that port
    /// publishes, which is what makes the run rule's skip decidable.
    latest: Vec<Vec<u8>>,
}

async fn run(
    mut instance: Running,
    mut ports: Ports,
    driving: usize,
    opening: Option<(Timestamp, Vec<u8>)>,
    output: Disruptor,
    health: Health,
    state: StateCell,
) {
    let mut sample = Vec::new();

    // Fire once from what the driving input had already published, so an
    // expression over a channel that is merely quiet shows its value straight
    // away instead of waiting for a sample that may be minutes off.
    if let Some((ts, value)) = &opening {
        ports.layouts[driving].fill(value, &mut sample);
        if evaluate(&mut instance, &ports, driving, &sample, *ts, &output, &state).is_err() {
            return;
        }
    }

    let fault = 'live: loop {
        let Ports {
            driving: reader,
            others,
            layouts,
            latest,
        } = &mut ports;
        let grant = reader.next().await;

        // Refresh the held inputs *after* the driving sample arrives, not
        // before: what an evaluation sees has to be the newest each input has
        // published, not the newest as of the last time this system fired.
        for (i, reader) in others.iter_mut() {
            drain(reader, &layouts[*i], &mut latest[*i]);
        }

        for at in 0..grant.sample_count() {
            let (ts, value) = grant.sample_at(at);
            if value.len() != layouts[driving].sample_bytes {
                continue;
            }
            layouts[driving].fill(value, &mut sample);
            if others.iter().any(|(i, _)| latest[*i].is_empty()) {
                continue;
            }
            for (i, held) in latest.iter().enumerate() {
                let bytes = if i == driving { &sample } else { held };
                if instance.write_port(i, bytes).is_err() {
                    return;
                }
            }
            match instance.eval(ts) {
                Ok(frame) => {
                    write_sample(&output, ts, frame);
                    *state.0.lock().unwrap() = instance.read_state();
                }
                Err(why) => break 'live why,
            }
        }
    };
    health.park(fault);
    parked(ports).await;
}

/// Write every port and evaluate once. `Err` means the instance is unusable
/// and the task should end; a fault during the opening sample is treated the
/// same way the live loop treats one.
fn evaluate(
    instance: &mut Running,
    ports: &Ports,
    driving: usize,
    sample: &[u8],
    ts: Timestamp,
    output: &Disruptor,
    state: &StateCell,
) -> Result<(), ()> {
    if ports
        .others
        .iter()
        .any(|(i, _)| ports.latest[*i].is_empty())
    {
        return Ok(());
    }
    for (i, held) in ports.latest.iter().enumerate() {
        let bytes = if i == driving { sample } else { held.as_slice() };
        instance.write_port(i, bytes)?;
    }
    match instance.eval(ts) {
        Ok(frame) => {
            write_sample(output, ts, frame);
            *state.0.lock().unwrap() = instance.read_state();
            Ok(())
        }
        Err(_) => Ok(()),
    }
}

/// After a fault the system stops computing but keeps reading, so nothing
/// upstream backs up behind a reader that has stopped moving.
async fn parked(mut ports: Ports) -> ! {
    loop {
        while ports.driving.try_next().is_some() {}
        for (i, reader) in ports.others.iter_mut() {
            drain(reader, &ports.layouts[*i], &mut ports.latest[*i]);
        }
        stellarator::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Take everything waiting on a port's reader, keeping the last sample.
fn drain(reader: &mut NodeReader, layout: &PortLayout, seen: &mut Vec<u8>) {
    while let Some(grant) = reader.try_next() {
        let count = grant.sample_count();
        if count == 0 {
            continue;
        }
        let (_, value) = grant.sample_at(count - 1);
        if value.len() == layout.sample_bytes {
            layout.fill(value, seen);
        }
    }
}

/// One instance of the module, driving one system through the region ABI.
struct Running {
    store: Store<()>,
    instance: Instance,
    eval: TypedFunc<i64, i32>,
    args: Vec<u32>,
    ret: u32,
    frame_bytes: usize,
    state: Vec<(metor_expr::state::Slot, u32)>,
    guard: Option<u32>,
    fuel: u64,
    frame: Vec<u8>,
}

impl Running {
    fn new(compiled: &Arc<Compiled>, index: usize, fuel: u64) -> Result<Self, BuildError> {
        let mut store = Store::new(&compiled.engine, ());
        store.set_fuel(fuel).map_err(expr_error)?;
        let instance = Linker::new(&compiled.engine)
            .instantiate_and_start(&mut store, &compiled.module)
            .map_err(expr_error)?;

        let desc = &compiled.manifest.systems[index];
        let accessor = |store: &mut Store<()>, name: &str, arg: i32| -> Result<u32, BuildError> {
            let func: TypedFunc<i32, i32> = instance
                .get_typed_func(&*store, name)
                .map_err(expr_error)?;
            func.call(store, arg).map(|v| v as u32).map_err(expr_error)
        };

        let mut args = Vec::with_capacity(desc.inputs.len());
        for i in 0..desc.inputs.len() {
            args.push(accessor(
                &mut store,
                &format!("{}_arg_ptr", desc.name),
                i as i32,
            )?);
        }
        let ret: TypedFunc<(), i32> = instance
            .get_typed_func(&store, &format!("{}_ret_ptr", desc.name))
            .map_err(expr_error)?;
        let ret = ret.call(&mut store, ()).map_err(expr_error)? as u32;

        let mut state = Vec::new();
        for slot in metor_expr::state::slots(&compiled.manifest) {
            if slot.system != index {
                continue;
            }
            let at = accessor(
                &mut store,
                &format!("{}_state_ptr", desc.name),
                slot.index as i32,
            )?;
            state.push((slot, at));
        }
        let guard = match state.is_empty() {
            true => None,
            false => Some(accessor(
                &mut store,
                &format!("{}_state_ptr", desc.name),
                state.len() as i32,
            )?),
        };

        let eval = instance
            .get_typed_func(&store, &format!("{}_eval", desc.name))
            .map_err(expr_error)?;

        Ok(Running {
            store,
            instance,
            eval,
            args,
            ret,
            frame_bytes: desc.output.bytes as usize,
            state,
            guard,
            fuel,
            frame: vec![0; desc.output.bytes as usize],
        })
    }

    fn memory(&self) -> wasmi::Memory {
        self.instance
            .get_memory(&self.store, "memory")
            .expect("a compiled module exports its memory")
    }

    fn write_port(&mut self, port: usize, bytes: &[u8]) -> Result<(), ()> {
        let at = self.args[port] as usize;
        let memory = self.memory();
        memory.write(&mut self.store, at, bytes).map_err(|_| ())
    }

    /// Seed the state slots from a snapshot and mark the instance seeded, so
    /// the first evaluation does not write its defaults over what arrived.
    fn seed(&mut self, snapshot: &Snapshot) {
        let memory = self.memory();
        for (slot, at) in &self.state {
            let Some((_, bytes)) = snapshot
                .entries
                .iter()
                .find(|(key, bytes)| *key == slot.key && bytes.len() as u32 == slot.bytes)
            else {
                continue;
            };
            let _ = memory.write(&mut self.store, *at as usize, bytes);
        }
        if let Some(guard) = self.guard {
            let _ = memory.write(&mut self.store, guard as usize, &1u32.to_le_bytes());
        }
    }

    fn read_state(&mut self) -> Snapshot {
        let memory = self.memory();
        let mut entries = Vec::with_capacity(self.state.len());
        for (slot, at) in &self.state {
            let mut bytes = vec![0u8; slot.bytes as usize];
            if memory.read(&self.store, *at as usize, &mut bytes).is_ok() {
                entries.push((slot.key.clone(), bytes));
            }
        }
        Snapshot { entries }
    }

    /// One evaluation under a fresh fuel grant, returning the output frame's
    /// bytes or why the system may not run again.
    fn eval(&mut self, ts: Timestamp) -> Result<&[u8], String> {
        self.store
            .set_fuel(self.fuel)
            .map_err(|e| format!("fuel: {e}"))?;
        match self.eval.call(&mut self.store, ts.0) {
            Ok(0) => {}
            Ok(code) => return Err(format!("faulted with code {code}")),
            Err(err) => return Err(trap_message(&err, self.fuel)),
        }
        let memory = self.memory();
        memory
            .read(&self.store, self.ret as usize, &mut self.frame)
            .map_err(|e| format!("output frame: {e}"))?;
        Ok(&self.frame[..self.frame_bytes])
    }
}

/// A trap, said the way the pane should show it.
fn trap_message(err: &wasmi::Error, fuel: u64) -> String {
    let text = err.to_string();
    if text.contains("fuel") || text.contains("Fuel") {
        return format!("burned its {fuel}-unit fuel grant");
    }
    text
}

fn expr_error(err: impl std::fmt::Display) -> BuildError {
    BuildError::Expr(err.to_string())
}
