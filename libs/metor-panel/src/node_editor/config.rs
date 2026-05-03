//! Persistence shape for a node editor pane. Round-trips through
//! `facet_json` alongside the rest of the dashboard preset (the editor's
//! `NodeEditor::to_config` produces this; `NodeEditor::from_config` parses
//! it back). Per-preset: each `NodeEditorConfig` is embedded inside a
//! `SerializedItem`'s `state` blob, so swapping presets swaps the graph.

use crate::node_editor::graph::{EdgeEntry, NodeGraph, Position};
use crate::node_editor::spec::NodeSpec;

#[derive(Clone, Debug, Default, facet::Facet)]
pub struct NodeEditorConfig {
    pub owner_uuid: u64,
    pub viewport: Viewport,
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}

#[derive(Clone, Debug, Default, facet::Facet)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

#[derive(Clone, Debug, facet::Facet)]
pub struct SerializedNode {
    pub flow_id: String,
    pub spec: NodeSpec,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, facet::Facet)]
pub struct SerializedEdge {
    pub source: String,
    pub target: String,
    pub target_socket: u32,
}

impl NodeEditorConfig {
    /// Snapshot a graph (without the build state, which is recomputed on load).
    pub fn from_graph(graph: &NodeGraph, viewport: Viewport) -> Self {
        let mut nodes: Vec<SerializedNode> = graph
            .nodes
            .iter()
            .map(|(flow_id, entry)| SerializedNode {
                flow_id: flow_id.to_string(),
                spec: entry.spec.clone(),
                x: entry.position.x,
                y: entry.position.y,
            })
            .collect();
        // Stable order so on-disk diffs are minimal when nothing meaningful changed.
        nodes.sort_by(|a, b| a.flow_id.cmp(&b.flow_id));

        let edges: Vec<SerializedEdge> = graph
            .edges
            .iter()
            .map(|e| SerializedEdge {
                source: e.source.to_string(),
                target: e.target.to_string(),
                target_socket: e.target_socket as u32,
            })
            .collect();

        Self {
            owner_uuid: graph.owner_id,
            viewport,
            nodes,
            edges,
        }
    }

    /// Hydrate into a fresh graph. Call `rebuild_into` afterwards to populate
    /// build state.
    pub fn into_graph(self) -> NodeGraph {
        let mut graph = NodeGraph::new(self.owner_uuid);
        for n in self.nodes {
            graph.insert_node(
                n.flow_id.into(),
                n.spec,
                Position { x: n.x, y: n.y },
            );
        }
        for e in self.edges {
            let source = e.source.into();
            let target = e.target.into();
            // Skip edges whose endpoints aren't present in the hydrated
            // node set — `add_edge` indexes `nodes` directly, and a corrupt
            // or schema-drifted preset (e.g. a node renamed/removed in a
            // newer build with stale edges in old preset JSON) would panic
            // the panel at startup.
            if !graph.nodes.contains_key(&source) || !graph.nodes.contains_key(&target) {
                continue;
            }
            graph.add_edge(EdgeEntry {
                source,
                target,
                target_socket: e.target_socket as usize,
            });
        }
        graph
    }
}
