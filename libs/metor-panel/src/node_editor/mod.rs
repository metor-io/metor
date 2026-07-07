//! Node editor for dynamic components.
//!
//! Phase 2 of the dynamic component system (Phase 1 is the runtime toolkit
//! at `crate::dynamic`). This module owns:
//!
//! - [`spec::NodeSpec`] — serializable per-node op + args.
//! - [`graph::NodeGraph`] — the editor's data model (nodes, edges, build state).
//! - [`coordinator::GraphCoordinator`] — multi-editor alive-set aggregator.
//! - [`registry::OpDescriptor`] — palette/canvas/validator metadata.
//! - [`validate`] — connection validation + edge coloring.
//! - [`pane::NodeEditor`] — the visual canvas pane that renders the graph
//!   and routes edits back into it.

pub mod config;
pub mod coordinator;
pub mod graph;
pub mod inspector_rows;
pub mod palette_provider;
pub mod pane;
pub mod registry;
pub mod spec;
pub mod validate;
pub mod worker;

#[cfg(test)]
mod tests;

pub use config::{NodeEditorConfig, SerializedEdge, SerializedNode, Viewport};
pub use coordinator::GraphCoordinator;
pub use graph::{BuildState, EdgeEntry, FlowId, NodeEntry, NodeGraph, Position};
pub use pane::{ActiveNodeEditor, DeleteSelected, NodeEditor};
pub use registry::{Arity, OpDescriptor, SocketKind, descriptor, descriptor_for};
pub use spec::{NodeSpec, NodeSpecKind, build, compute_node_id};
pub use validate::{EdgeColor, EdgeVerdict, edge_color, validate_connection};
pub use worker::{DynamicWorker, WorkerHandle};
