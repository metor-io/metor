//! Run compiled systems as streaming nodes.

use std::sync::{Arc, Mutex};

use metor_db::ComponentSchema;
use metor_db::disruptor::Disruptor;
use metor_expr::state::{RNG_FIELD, Snapshot};
use metor_expr::{Diagnostics, Manifest, Resolver, Ty};
use metor_proto::types::{PrimType, Timestamp};
use wasmi::{Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_value, write_sample,
};
use crate::dynamic::tensor::read_f64_at;

/// Fuel available to one evaluation.
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
        let module =
            Module::new(&engine, &program.wasm[..]).expect("a module this crate emitted validates");
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
        hash_id(op_tag::EXPR_SYSTEM, ports, |h| {
            use std::hash::Hash;
            self.hash_declaration(system.source, system.layout.span, h);
            for &dependency in &system.dependencies {
                let function = &self.manifest.functions[dependency];
                self.hash_declaration(function.source, Default::default(), h);
            }
            let mut descriptor = system.clone();
            descriptor.source = Default::default();
            descriptor.layout = Default::default();
            descriptor.dependencies.clear();
            postcard::to_allocvec(&descriptor)
                .expect("a system descriptor encodes")
                .hash(h);
        })
    }

    fn hash_declaration(
        &self,
        source: metor_expr::Span,
        ignored: metor_expr::Span,
        h: &mut impl std::hash::Hasher,
    ) {
        use std::hash::Hash;
        let start = source.start as usize;
        let end = source.end as usize;
        let ignored_start = ignored.start as usize;
        let ignored_end = ignored.end as usize;
        let Some(region) = self.source.get(start..end) else {
            self.source.hash(h);
            return;
        };
        if start <= ignored_start && ignored_start <= ignored_end && ignored_end <= end {
            self.source[start..ignored_start].hash(h);
            self.source[ignored_end..end].hash(h);
        } else {
            region.hash(h);
        }
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
    // A generator that has never run needs a seed nothing else can supply.
    // Restoring one carries the sequence across an edit; otherwise the clock
    // and the system's identity make a fresh one, so two systems drawing at
    // once do not draw the same numbers.
    let id = compiled.system_hash(
        index,
        &ports.iter().map(|p| p.node.id()).collect::<Vec<_>>(),
    );
    if !seed.is_some_and(|s| s.entries.iter().any(|(key, _)| key.field == RNG_FIELD)) {
        instance.seed_rng(id.0 ^ Timestamp::now().0 as u64);
    }
    let state = StateCell::default();
    *state.0.lock().unwrap() = instance.read_state();

    // A source clocks itself; everything else waits on its driving input.
    // Either way one reader supplies the timestamps the loop turns on.
    let clock = match desc.rate {
        Some(hz) => Some(super::clock::fixed_rate(hz)?),
        None if ports.is_empty() => {
            return Err(BuildError::Expr(
                "a system with no inputs needs @system(rate=) to fire it".into(),
            ));
        }
        None => None,
    };
    let driving = desc.driving.unwrap_or(0);

    let mut schemas = Vec::with_capacity(ports.len());
    for source in &ports {
        schemas.push(require_value(&source.node)?);
    }
    let mut held = Held::new(desc, &schemas, clock.is_none().then_some(driving))?;
    let mut readers = Vec::with_capacity(ports.len());
    for (i, source) in ports.iter().enumerate() {
        // Whatever the host already knows about this port becomes its first
        // held value, so a system is not blind to everything published before
        // it existed.
        if let Some((ts, value)) = &source.seed {
            held.hold(i, *ts, value);
        }
        readers.push(Some(source.node.subscribe()));
    }
    // The driving port's own history is what the system fires from once at
    // startup, so a quiet channel still shows its current value. A source has
    // no such history — its clock ticks immediately.
    let opening = match &clock {
        Some(_) => None,
        None => ports[driving].seed.clone(),
    };
    let wired = Ports {
        driving: match &clock {
            Some(clock) => clock.subscribe(),
            None => readers[driving]
                .take()
                .expect("the driving port is wired once"),
        },
        others: readers
            .into_iter()
            .enumerate()
            .filter_map(|(i, reader)| Some((i, reader?)))
            .collect(),
        held,
    };

    let frame_bytes = desc.output.bytes as usize;
    let health = Health::default();
    let node = NodeImpl::spawn(
        id,
        ValueType::Value(ComponentSchema::new(PrimType::U8, &[frame_bytes][..])),
        match &clock {
            Some(clock) => clock.parent_clock_id(),
            None => ports[driving].node.parent_clock_id(),
        },
        default_ring_bytes(frame_bytes),
        {
            let health = health.clone();
            let state = state.clone();
            move |output| async move {
                let _ports = ports;
                let _clock = clock;
                run(instance, wired, opening, output, health, state).await;
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
pub fn latest_sample(
    db: &metor_db::DB,
    component: metor_proto::types::ComponentId,
) -> Option<(Timestamp, Vec<u8>)> {
    db.with_state(|state| {
        let latest = state.get_component(component)?.time_series.latest()?;
        Some((latest.timestamp(), latest.data().to_vec()))
    })
}

/// The component a field of the language publishes as.
pub fn schema_of(ty: &Ty) -> ComponentSchema {
    match ty {
        Ty::F64 => ComponentSchema::new(PrimType::F64, &[][..]),
        Ty::I64 => ComponentSchema::new(PrimType::I64, &[][..]),
        Ty::Bool => ComponentSchema::new(PrimType::Bool, &[][..]),
        Ty::Tensor { shape, .. } => ComponentSchema::new(PrimType::F64, shape.as_slice()),
    }
}

/// Widen a frame slot into the bytes a component carries.
///
/// A frame gives every element eight bytes so nothing straddles; a component
/// gives `bool` one byte, so only that case is more than a copy.
pub(crate) fn read_field(slot: &[u8], ty: &Ty, out: &mut Vec<u8>) {
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
        // Component ports project exactly one field.
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

/// What every port last contributed, and the rule for firing over it.
///
/// Kept apart from the readers because the rule is the same whether the
/// samples arrive live or are replayed out of the inputs' history: the
/// driving port fires an evaluation, every other port holds its newest
/// value, and nothing fires until each held port has one.
pub(crate) struct Held {
    layouts: Vec<PortLayout>,
    /// The frame bytes each port last contributed. Empty until that port
    /// publishes, which is what makes the skip decidable.
    latest: Vec<Vec<u8>>,
    /// When each held port's newest sample was published, which is what the
    /// guest's `deltat` counts arrivals by.
    stamps: Vec<Timestamp>,
    /// Which port fires the system, or `None` when a clock does.
    driven_port: Option<usize>,
    /// Scratch for the driving port's frame, which is never held.
    sample: Vec<u8>,
}

impl Held {
    pub(crate) fn new(
        desc: &metor_expr::System,
        schemas: &[ComponentSchema],
        driven_port: Option<usize>,
    ) -> Result<Self, BuildError> {
        let mut layouts = Vec::with_capacity(schemas.len());
        for (port, schema) in desc.inputs.iter().zip(schemas) {
            layouts.push(PortLayout::new(port, schema)?);
        }
        Ok(Held {
            latest: vec![Vec::new(); layouts.len()],
            stamps: vec![Timestamp(0); layouts.len()],
            layouts,
            driven_port,
            sample: Vec::new(),
        })
    }

    /// A port published `value`, in its component's bytes. A sample of the
    /// wrong length is not the port's and is ignored.
    pub(crate) fn hold(&mut self, port: usize, ts: Timestamp, value: &[u8]) {
        let layout = &self.layouts[port];
        if value.len() == layout.sample_bytes {
            layout.fill(value, &mut self.latest[port]);
            self.stamps[port] = ts;
        }
    }

    /// Fire once at `ts`, with `driving` as the driving port's sample when an
    /// input fires the system. `Ok(None)` is a skip; `Err` is a fault that
    /// ends the system's evaluating.
    pub(crate) fn fire<'a>(
        &mut self,
        instance: &'a mut Running,
        ts: Timestamp,
        driving: Option<&[u8]>,
    ) -> Result<Option<&'a [u8]>, String> {
        if let (Some(port), Some(value)) = (self.driven_port, driving) {
            let layout = &self.layouts[port];
            if value.len() != layout.sample_bytes {
                return Ok(None);
            }
            layout.fill(value, &mut self.sample);
        }
        if self
            .latest
            .iter()
            .enumerate()
            .any(|(i, held)| Some(i) != self.driven_port && held.is_empty())
        {
            return Ok(None);
        }
        for (i, held) in self.latest.iter().enumerate() {
            let (bytes, stamp) = match self.driven_port == Some(i) {
                true => (&self.sample, ts),
                false => (held, self.stamps[i]),
            };
            instance.write_port(i, bytes, stamp)?;
        }
        instance.eval(ts).map(Some)
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
    held: Held,
}

async fn run(
    mut instance: Running,
    mut ports: Ports,
    opening: Option<(Timestamp, Vec<u8>)>,
    output: Disruptor,
    health: Health,
    state: StateCell,
) {
    // Evaluate the newest saved driving sample before waiting for live input.
    if let Some((ts, value)) = &opening
        && let Err(why) = evaluate(
            &mut instance,
            &mut ports.held,
            *ts,
            Some(value),
            &output,
            &state,
        )
    {
        health.park(why);
        parked(ports).await;
    }

    let fault = 'live: loop {
        let Ports {
            driving: reader,
            others,
            held,
        } = &mut ports;
        let grant = reader.next().await;

        // Refresh the held inputs *after* the driving sample arrives, not
        // before: what an evaluation sees has to be the newest each input has
        // published, not the newest as of the last time this system fired.
        for (i, reader) in others.iter_mut() {
            drain(reader, *i, held);
        }

        for at in 0..grant.sample_count() {
            let (ts, value) = grant.sample_at(at);
            let driving = held.driven_port.is_some().then_some(value);
            if let Err(why) = evaluate(&mut instance, held, ts, driving, &output, &state) {
                break 'live why;
            }
        }
    };
    health.park(fault);
    parked(ports).await;
}

/// Fire once and publish what came out, keeping the state cell current.
fn evaluate(
    instance: &mut Running,
    held: &mut Held,
    ts: Timestamp,
    driving: Option<&[u8]>,
    output: &Disruptor,
    state: &StateCell,
) -> Result<(), String> {
    if let Some(frame) = held.fire(instance, ts, driving)? {
        write_sample(output, ts, frame);
        *state.0.lock().unwrap() = instance.read_state();
    }
    Ok(())
}

/// After a fault the system stops computing but keeps reading, so nothing
/// upstream backs up behind a reader that has stopped moving.
async fn parked(mut ports: Ports) -> ! {
    loop {
        while ports.driving.try_next().is_some() {}
        for (i, reader) in ports.others.iter_mut() {
            drain(reader, *i, &mut ports.held);
        }
        stellarator::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Take everything waiting on a port's reader, keeping the last sample.
fn drain(reader: &mut NodeReader, port: usize, held: &mut Held) {
    while let Some(grant) = reader.try_next() {
        let count = grant.sample_count();
        if count == 0 {
            continue;
        }
        let (ts, value) = grant.sample_at(count - 1);
        held.hold(port, ts, value);
    }
}

/// One instance of the module, driving one system through the region ABI.
pub(crate) struct Running {
    store: Store<()>,
    instance: Instance,
    eval: TypedFunc<i64, i32>,
    args: Vec<u32>,
    /// Where each port's sample stamp goes, relative to its argument block.
    stamps: Vec<u32>,
    ret: u32,
    frame_bytes: usize,
    state: Vec<(metor_expr::state::Slot, u32)>,
    guard: Option<u32>,
    fuel: u64,
    frame: Vec<u8>,
}

impl Running {
    pub(crate) fn new(
        compiled: &Arc<Compiled>,
        index: usize,
        fuel: u64,
    ) -> Result<Self, BuildError> {
        let mut store = Store::new(&compiled.engine, ());
        store.set_fuel(fuel).map_err(expr_error)?;
        let instance = Linker::new(&compiled.engine)
            .instantiate_and_start(&mut store, &compiled.module)
            .map_err(expr_error)?;

        let desc = &compiled.manifest.systems[index];
        let accessor = |store: &mut Store<()>, name: &str, arg: i32| -> Result<u32, BuildError> {
            let func: TypedFunc<i32, i32> =
                instance.get_typed_func(&*store, name).map_err(expr_error)?;
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
            stamps: desc.inputs.iter().map(|p| p.stamp_offset()).collect(),
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

    /// Fill one input block: the frame, then the stamp of the sample it is.
    fn write_port(&mut self, port: usize, bytes: &[u8], ts: Timestamp) -> Result<(), String> {
        let at = self.args[port] as usize;
        let stamp = at + self.stamps[port] as usize;
        let memory = self.memory();
        memory
            .write(&mut self.store, at, bytes)
            .and_then(|()| memory.write(&mut self.store, stamp, &ts.0.to_le_bytes()))
            .map_err(|err| format!("could not write input frame: {err}"))
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

    /// Write the generator's state word, leaving the seed guard alone.
    ///
    /// The guard may be left alone precisely because this field's declared
    /// default is zero, so the guest emits no instruction to seed it and the
    /// first evaluation cannot overwrite what was written here.
    pub(crate) fn seed_rng(&mut self, entropy: u64) {
        let memory = self.memory();
        for (slot, at) in &self.state {
            if slot.key.field == RNG_FIELD {
                let _ = memory.write(&mut self.store, *at as usize, &entropy.to_le_bytes());
            }
        }
    }

    pub(crate) fn read_state(&mut self) -> Snapshot {
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
