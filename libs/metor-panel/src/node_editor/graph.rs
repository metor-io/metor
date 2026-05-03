//! `NodeGraph`: the editor's data model. Holds the spec'd nodes, the edges
//! between them, and the live `BuildState` for each node. Mutations are
//! plain method calls; the owning `NodeEditor` view debounces and triggers
//! `rebuild` after each mutation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::SharedString;
use metor_db::DB;

use crate::dynamic::{BuildError, DynamicNode, DynamicRegistry, NodeId};
use crate::node_editor::coordinator::OwnerId;
use crate::node_editor::registry::{Arity, descriptor_for};
use crate::node_editor::spec::{NodeSpec, build as build_spec, compute_node_id};
use crate::node_editor::worker::{DynamicWorker, WorkerHandle};

/// Editor-stable handle for a node. Survives spec edits (unlike `NodeId`,
/// which is content-addressed and changes on any arg mutation).
pub type FlowId = SharedString;

#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

pub enum BuildState {
    Pending,
    Built(Arc<dyn DynamicNode>),
    Error(BuildError),
}

impl BuildState {
    pub fn as_built(&self) -> Option<&Arc<dyn DynamicNode>> {
        if let BuildState::Built(node) = self {
            Some(node)
        } else {
            None
        }
    }

    pub fn id(&self) -> Option<NodeId> {
        self.as_built().map(|n| n.id())
    }

    pub fn error(&self) -> Option<&BuildError> {
        if let BuildState::Error(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

pub struct NodeEntry {
    pub spec: NodeSpec,
    pub position: Position,
    pub computed_id: Option<NodeId>,
    pub build: BuildState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdgeEntry {
    pub source: FlowId,
    pub target: FlowId,
    /// Slot on the target node. For variadic ops (`Mean`) this is ignored
    /// during build — sources are sorted by `(y, x)` of their position.
    pub target_socket: usize,
}

pub struct NodeGraph {
    pub nodes: HashMap<FlowId, NodeEntry>,
    pub edges: Vec<EdgeEntry>,
    pub owner_id: OwnerId,
}

impl NodeGraph {
    pub fn new(owner_id: OwnerId) -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            owner_id,
        }
    }

    pub fn insert_node(&mut self, id: FlowId, spec: NodeSpec, position: Position) {
        self.nodes.insert(
            id,
            NodeEntry {
                spec,
                position,
                computed_id: None,
                build: BuildState::Pending,
            },
        );
    }

    pub fn remove_node(&mut self, id: &FlowId) {
        self.nodes.remove(id);
        self.edges
            .retain(|e| &e.source != id && &e.target != id);
    }

    /// Drop the edge already at `(target, target_socket)`, then add the new
    /// one. Same replace-on-conflict behavior for both exact and variadic
    /// targets — variadic ops grow by dropping into the trailing empty
    /// socket the canvas renders, not by stacking edges on the same socket.
    pub fn add_edge(&mut self, edge: EdgeEntry) {
        self.edges
            .retain(|e| !(e.target == edge.target && e.target_socket == edge.target_socket));
        self.edges.push(edge);
    }

    pub fn remove_edge(&mut self, edge: &EdgeEntry) {
        let target = edge.target.clone();
        let removed_socket = edge.target_socket;
        let is_variadic = self
            .nodes
            .get(&target)
            .map(|n| matches!(descriptor_for(&n.spec).inputs, Arity::Variadic { .. }))
            .unwrap_or(false);
        self.edges.retain(|e| e != edge);
        if is_variadic {
            // Keep variadic socket indices contiguous so the canvas always
            // renders a tight column ending in one trailing "add" slot.
            for e in &mut self.edges {
                if e.target == target && e.target_socket > removed_socket {
                    e.target_socket -= 1;
                }
            }
        }
    }

    /// Parents of `flow_id` in canonical build order (matches what
    /// `compute_node_id` and the constructor expect). For both exact and
    /// variadic ops the order is the `target_socket` index, so users
    /// control input order by which socket they wire to.
    pub fn parents_of(&self, flow_id: &FlowId) -> Vec<FlowId> {
        if !self.nodes.contains_key(flow_id) {
            return Vec::new();
        }
        let mut incoming: Vec<&EdgeEntry> = self
            .edges
            .iter()
            .filter(|e| &e.target == flow_id)
            .collect();
        incoming.sort_by_key(|e| e.target_socket);
        incoming.into_iter().map(|e| e.source.clone()).collect()
    }

    /// Topological order over the graph using Kahn's algorithm. Returns
    /// `(order, cycle_members)` — anything in `cycle_members` couldn't be
    /// reached because it's in or downstream of a cycle.
    fn topo_order(&self) -> (Vec<FlowId>, HashSet<FlowId>) {
        // Build in-degree from the parent map.
        let mut indeg: HashMap<FlowId, usize> =
            self.nodes.keys().map(|k| (k.clone(), 0)).collect();
        let mut children: HashMap<FlowId, Vec<FlowId>> = HashMap::new();
        for flow_id in self.nodes.keys() {
            let parents = self.parents_of(flow_id);
            for p in &parents {
                children.entry(p.clone()).or_default().push(flow_id.clone());
            }
            indeg.insert(flow_id.clone(), parents.len());
        }

        let mut queue: Vec<FlowId> = indeg
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(k, _)| k.clone())
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(n) = queue.pop() {
            order.push(n.clone());
            if let Some(kids) = children.get(&n) {
                for kid in kids {
                    if let Some(d) = indeg.get_mut(kid) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push(kid.clone());
                        }
                    }
                }
            }
        }

        let visited: HashSet<FlowId> = order.iter().cloned().collect();
        let cycle_members: HashSet<FlowId> = self
            .nodes
            .keys()
            .filter(|k| !visited.contains(*k))
            .cloned()
            .collect();
        (order, cycle_members)
    }

    /// Rebuild the graph against `registry`: hash each node's id from current
    /// spec + parents, idempotently insert into the registry, propagate errors
    /// downstream. Returns the set of `NodeId`s the editor wants kept alive.
    /// The caller is responsible for handing this set to the coordinator
    /// (which will union it with other editors' sets and reconcile).
    ///
    /// If `worker` is `Some`, every build closure runs on the worker thread
    /// — required in the live app where the gpui main thread has no
    /// stellarator runtime. Tests pass `None` and call constructors directly
    /// (the test harness installs its own runtime via `#[stellarator::test]`).
    pub fn rebuild_into(
        &mut self,
        db: &Arc<DB>,
        registry: &mut DynamicRegistry,
        worker: Option<&WorkerHandle>,
    ) -> HashSet<NodeId> {
        let (order, cycle_members) = self.topo_order();

        // Mark cycle members up front.
        for cyc in &cycle_members {
            if let Some(entry) = self.nodes.get_mut(cyc) {
                entry.build = BuildState::Error(BuildError::Cycle);
                entry.computed_id = None;
            }
        }

        for flow_id in order {
            let parent_flow_ids = self.parents_of(&flow_id);

            // Resolve parent Arcs and ids. Bail to ParentFailed on any error.
            let mut parent_ids: Vec<NodeId> = Vec::with_capacity(parent_flow_ids.len());
            let mut parent_arcs: Vec<Arc<dyn DynamicNode>> =
                Vec::with_capacity(parent_flow_ids.len());
            let mut parent_failed = false;
            for pid in &parent_flow_ids {
                let Some(entry) = self.nodes.get(pid) else {
                    parent_failed = true;
                    break;
                };
                match &entry.build {
                    BuildState::Built(arc) => {
                        parent_ids.push(arc.id());
                        parent_arcs.push(arc.clone());
                    }
                    _ => {
                        parent_failed = true;
                        break;
                    }
                }
            }

            let entry = self.nodes.get_mut(&flow_id).expect("node exists");
            if parent_failed {
                entry.build = BuildState::Error(BuildError::ParentFailed);
                entry.computed_id = None;
                continue;
            }

            let new_id = compute_node_id(&entry.spec, &parent_ids);

            // Idempotent skip: if we've already built this exact node, leave
            // it alone — keeps in-flight tasks alive across unrelated edits.
            if let (Some(prev), BuildState::Built(arc)) = (entry.computed_id, &entry.build)
                && prev == new_id
                && arc.id() == new_id
            {
                continue;
            }

            let spec = entry.spec.clone();
            let result = if let Some(existing) = registry.get(new_id) {
                Ok(existing)
            } else {
                let build_result = match worker {
                    Some(w) => {
                        let spec_for_worker = spec.clone();
                        let db_for_worker = db.clone();
                        w.run(Box::new(move || {
                            build_spec(&spec_for_worker, parent_arcs, &db_for_worker)
                        }))
                    }
                    None => build_spec(&spec, parent_arcs, db),
                };
                match build_result {
                    Ok(node) => {
                        debug_assert_eq!(
                            node.id(),
                            new_id,
                            "spec::build produced a NodeId that doesn't match compute_node_id"
                        );
                        registry.insert(node.clone());
                        Ok(node)
                    }
                    Err(e) => Err(e),
                }
            };
            let entry = self.nodes.get_mut(&flow_id).expect("node exists");
            match result {
                Ok(arc) => {
                    entry.computed_id = Some(new_id);
                    entry.build = BuildState::Built(arc);
                }
                Err(e) => {
                    entry.computed_id = None;
                    entry.build = BuildState::Error(e);
                }
            }
        }

        self.nodes
            .values()
            .filter_map(|n| n.build.id())
            .collect()
    }

    /// View-layer wrapper around [`rebuild_into`] that pulls the registry
    /// and worker out of the gpui app and submits the resulting alive set to
    /// the [`GraphCoordinator`](crate::node_editor::coordinator::GraphCoordinator).
    /// Use this from inside `Entity<NodeGraph>::update`.
    pub fn rebuild(&mut self, db: &Arc<DB>, cx: &mut gpui::App) {
        // Clone the cheap channel-backed handle out of the worker global
        // first so we can borrow the registry mutably from the same `cx`
        // afterwards.
        let worker = cx.global::<DynamicWorker>().handle().clone();
        let registry = cx.global_mut::<DynamicRegistry>();
        let alive = self.rebuild_into(db, registry, Some(&worker));
        crate::node_editor::coordinator::GraphCoordinator::submit(self.owner_id, alive, cx);
    }
}
