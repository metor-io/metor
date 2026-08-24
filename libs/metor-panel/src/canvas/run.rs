//! What a compiled program is, while it is running.
//!
//! The tile owns source text; this owns what that text turned into. The two
//! are kept apart because reconciling has nothing to do with drawing: it takes
//! a fresh [`Compiled`] and the set of declarations already running, keeps
//! every one whose identity survived the edit, builds the rest, and drops what
//! is left — which is what cancels a removed declaration's task and nothing
//! else's.
//!
//! Identity is the whole mechanism. A system is hashed on its own source
//! region plus everything its ports resolved to, so an edit to one body leaves
//! the others — and their state — untouched. A system with the same *name* but
//! a different hash was edited, so its state comes across and
//! `metor_expr::state` decides field by field what of it still means the same
//! thing.

use std::sync::Arc;

use metor_db::DB;
use metor_expr::state::Snapshot;
use metor_expr::{Binding, Decl, Manifest, Span};
use metor_proto::types::ComponentId;

use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL, Health};
use crate::dynamic::ops::{self, db_source, persist};
use crate::dynamic::resolver::DbResolver;
use crate::dynamic::{BuildError, DynamicNode, NodeId, hash_id, op_tag};
use crate::node_editor::worker::WorkerHandle;

/// One declaration, running.
pub struct Running {
    pub name: String,
    /// Every node behind it. Held for their drop: the last `Arc` going away is
    /// what cancels a node's task, so this is the whole of a teardown.
    _nodes: Vec<Arc<dyn DynamicNode>>,
    pub publishes: Vec<String>,
    pub health: Health,
    state: program::StateCell,
    /// What identified this declaration when it was built, so the next compile
    /// can tell whether the edit touched it.
    hash: NodeId,
}

/// Everything the tile's source is currently running.
#[derive(Default)]
pub struct Systems {
    running: Vec<Running>,
}

impl Systems {
    pub fn iter(&self) -> impl Iterator<Item = &Running> {
        self.running.iter()
    }

    /// Reconcile what is running against what the source now says, returning
    /// one diagnostic per declaration that could not be built.
    pub fn reconcile(
        &mut self,
        compiled: &Arc<Compiled>,
        db: &Arc<DB>,
        resolver: &DbResolver,
        worker: &WorkerHandle,
    ) -> Vec<(Span, String)> {
        let mut kept: Vec<Running> = Vec::new();
        let mut failed = Vec::new();
        // Declaration order, because a declaration may read an earlier one and
        // what it reads has to exist by then.
        for decl in compiled.manifest.declarations() {
            let (span, built) = match decl {
                Decl::System(index) => (
                    compiled.manifest.systems[index].source,
                    self.system(compiled, index, db, resolver, worker),
                ),
                Decl::Stage(index) => (
                    compiled.manifest.stages[index].source_span,
                    self.stage(compiled, index, db, resolver, worker),
                ),
            };
            match built {
                Ok(running) => kept.push(running),
                Err(why) => failed.push((span, why)),
            }
        }
        // Dropping what is left cancels the tasks of every declaration the
        // edit removed, and only those.
        self.running = kept;
        failed
    }

    /// Keep a system whose identity survived the edit; build the rest.
    fn system(
        &mut self,
        compiled: &Arc<Compiled>,
        index: usize,
        db: &Arc<DB>,
        resolver: &DbResolver,
        worker: &WorkerHandle,
    ) -> Result<Running, String> {
        let desc = &compiled.manifest.systems[index];

        // Which component each port reads is decided here; the node that reads
        // it is built on the worker thread, because building one spawns a task
        // and tasks belong to that thread. The two separate cleanly because a
        // source node's identity follows from its component id alone.
        let mut sources = Vec::with_capacity(desc.inputs.len());
        for port in &desc.inputs {
            sources.push(component_of(&port.bindings[0], compiled, db, resolver)?);
        }
        let port_ids: Vec<NodeId> = sources.iter().map(|id| db_source::from_db_id(*id)).collect();
        let hash = compiled.system_hash(index, &port_ids);
        if let Some(at) = self.running.iter().position(|r| r.hash == hash) {
            return Ok(self.running.remove(at));
        }

        // Same name, different hash: this system was edited, so its state
        // comes across.
        let seed: Option<Snapshot> = self
            .running
            .iter()
            .find(|r| r.name == desc.name)
            .map(|r| r.state.snapshot());

        let built = {
            let compiled = compiled.clone();
            let db = db.clone();
            let names: Vec<String> = desc.publishes.clone();
            worker.call(move || -> Result<Built, BuildError> {
                let mut ports = Vec::with_capacity(sources.len());
                for id in &sources {
                    ports.push(program::PortSource {
                        node: db_source::from_db(&db, *id)?,
                        seed: program::latest_sample(&db, *id),
                    });
                }
                let system = program::system(&compiled, index, ports, DEFAULT_FUEL, seed.as_ref())?;
                let mut nodes = vec![system.node.clone()];
                for (field, name) in names.iter().enumerate() {
                    let node = program::field(&compiled, index, field, system.node.clone())?;
                    nodes.push(persist::persist(&db, name.clone(), node)?);
                }
                Ok(Built {
                    nodes,
                    health: system.health,
                    state: system.state,
                })
            })
        };
        let built = built
            .ok_or_else(|| "the node worker is gone".to_string())?
            .map_err(|e| e.to_string())?;

        Ok(Running {
            name: desc.name.clone(),
            _nodes: built.nodes,
            publishes: desc.publishes.clone(),
            health: built.health,
            state: built.state,
            hash,
        })
    }

    /// Keep or build one resample stage.
    ///
    /// A stage is not compiled and has no state to carry, so reconciling one
    /// is only the question of whether anything about it changed: what it
    /// reads, how it fills a tick, and how fast it ticks.
    fn stage(
        &mut self,
        compiled: &Arc<Compiled>,
        index: usize,
        db: &Arc<DB>,
        resolver: &DbResolver,
        worker: &WorkerHandle,
    ) -> Result<Running, String> {
        let stage = &compiled.manifest.stages[index];
        let source = component_of(&stage.source, compiled, db, resolver)?;
        let hash = hash_id(op_tag::EXPR_STAGE, &[db_source::from_db_id(source)], |h| {
            use std::hash::Hash;
            stage.name.hash(h);
            (stage.kind == metor_expr::Resample::Linear).hash(h);
            stage.rate.to_bits().hash(h);
        });
        if let Some(at) = self.running.iter().position(|r| r.hash == hash) {
            return Ok(self.running.remove(at));
        }

        let mode = match stage.kind {
            metor_expr::Resample::Zoh => ops::resample::ResampleMode::Zoh,
            metor_expr::Resample::Linear => ops::resample::ResampleMode::Linear,
        };
        let (name, rate) = (stage.name.clone(), stage.rate);
        let nodes = {
            let db = db.clone();
            worker.call(move || -> Result<Vec<Arc<dyn DynamicNode>>, BuildError> {
                let input = db_source::from_db(&db, source)?;
                let clock = ops::clock::fixed_rate(rate)?;
                let resampled = ops::resample::resample(input, clock.clone(), mode)?;
                Ok(vec![clock, persist::persist(&db, name, resampled)?])
            })
        };
        let nodes = nodes
            .ok_or_else(|| "the node worker is gone".to_string())?
            .map_err(|e| e.to_string())?;

        Ok(Running {
            name: stage.name.clone(),
            _nodes: nodes,
            publishes: vec![stage.name.clone()],
            health: Health::default(),
            state: program::StateCell::default(),
            hash,
        })
    }
}

/// One declaration as it comes back from the worker thread.
struct Built {
    nodes: Vec<Arc<dyn DynamicNode>>,
    health: Health,
    state: program::StateCell,
}

/// The component behind one binding, whatever produced it.
///
/// An external component's id is *carried* from the resolver that found it,
/// never re-derived: a producer names its own channels, so hashing the name
/// again agrees with the real id for only about half of all names — and when
/// it disagrees the component looks absent rather than misaddressed. Deriving
/// is right only where `persist` created the component from that same name
/// moments earlier, which is the in-program case below.
fn component_of(
    binding: &Binding,
    compiled: &Arc<Compiled>,
    db: &Arc<DB>,
    resolver: &DbResolver,
) -> Result<ComponentId, String> {
    let produced = match binding {
        Binding::Component(path) => {
            return resolver
                .id_of(path)
                .ok_or_else(|| format!("`{path}` is not a known component"));
        }
        Binding::Produced { system, field } => &compiled.manifest.systems[*system].publishes[*field],
        Binding::Resampled { stage } => &compiled.manifest.stages[*stage].name,
    };
    let id = ComponentId::new(produced);
    match db.with_state(|s| s.get_component(id).is_some()) {
        true => Ok(id),
        false => Err(format!("`{produced}` has not published yet")),
    }
}

/// Whether anything in this manifest is worth showing as running.
pub fn publishes(manifest: &Manifest) -> usize {
    manifest.systems.iter().map(|s| s.publishes.len()).sum::<usize>() + manifest.stages.len()
}
