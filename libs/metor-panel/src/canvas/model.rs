//! One graph, two sources.
//!
//! The panel used to have three pictures of the same thing: the node editor's
//! dataflow, the system graph's wiring, and the program projection. They are
//! the same picture — *systems over frames, edges recovered from names* — so
//! this module builds one model from both sources and nothing downstream of it
//! needs to know which half a card came from except to decide whether it can be
//! edited.
//!
//! **Native cards come from the live target's [`Wiring`].** Their source of
//! truth is Rust and `target.py`, so they are viewers: structure, ports, and
//! the position the operator dragged them to. That is unchanged from the
//! system graph, layout engine included.
//!
//! **Python cards come from a compiled [`Manifest`].** Their source of truth
//! is text in this very tile, so every one of them is editable — and their
//! positions live in the text too, as `@node`.
//!
//! ## Cross-source edges are recovered from names, like every other edge
//!
//! A native system's instance name is its telemetry prefix, so a component
//! called `nav.attitude.omega_b` is published by the instance `nav`. A Python
//! port bound to that component is therefore an edge from that native card,
//! found by the same rule wiring uses to match ports: the name. Nothing is
//! registered, and nothing has to agree in advance.
//!
//! The reverse direction does not exist, and that is a fact rather than a gap:
//! native systems are wired in `target.py` against frames the target declares,
//! so one cannot consume a component this panel invented. A Python system's
//! output reaches the vehicle in Phase 3, by being uplinked rather than by
//! being wired to from here.

use std::collections::{BTreeSet, HashMap};

use gpui::SharedString;
use metor_expr::{Binding, Decl, Layout as SourceLayout, Manifest};
use metor_fsw_2::ir::{EdgeKind, Wiring};

use crate::graph_layout::{Direction, EdgeRoute};
use crate::views::system_graph::layout::{self, GraphNodeKind, NODE_WIDTH, card_size};

/// Height of a card's title bar, shared by both sources so the two read as one
/// diagram.
pub const HEADER_HEIGHT: f32 = 24.0;
/// Height of one socket row on a Python card.
pub const SOCKET_ROW_HEIGHT: f32 = 18.0;
/// Where the Python half begins when nothing has been placed by hand: below
/// the native graph, so the two do not overlap on first open.
const PYTHON_GUTTER: f32 = 80.0;
const COLUMN_STRIDE: f32 = 240.0;
const ROW_STRIDE: f32 = 140.0;

/// What a card is, and therefore what may be done to it.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// A system, slot, coordinator, or collapsed scope from the live wiring.
    /// Read-only structure; the position is the panel's own.
    Native {
        kind: GraphNodeKind,
        /// Index into `Wiring::systems` or `::slots`, for the detail rows.
        source_index: Option<usize>,
    },
    /// A declaration in this tile's source. Editable, and its position lives
    /// in the text.
    Python {
        decl: Decl,
        layout: SourceLayout,
    },
}

impl Origin {
    pub fn is_python(&self) -> bool {
        matches!(self, Origin::Python { .. })
    }
}

/// One named connection point on a card.
#[derive(Clone, Debug, PartialEq)]
pub struct Socket {
    pub name: String,
    /// What it carries, for the row's second column.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// A declaration name or an instance name — what edges are recovered from,
    /// and what a rename is a migration of.
    pub id: SharedString,
    pub subtitle: SharedString,
    pub inputs: Vec<Socket>,
    pub outputs: Vec<Socket>,
    pub origin: Origin,
    /// Graph-space top-left.
    pub pos: (f32, f32),
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub from: SharedString,
    pub from_port: SharedString,
    pub to: SharedString,
    pub to_port: SharedString,
    pub kind: EdgeKind,
    pub delayed: bool,
    /// Which of the consumer's ports this fills, when the consumer is Python.
    /// A rebinding gesture rewrites exactly this port's binding.
    pub consumer_port: Option<usize>,
    /// The layout engine's waypoints, for a native-to-native edge that nothing
    /// has been dragged out of. Everything else is a plain two-point wire.
    pub route: Option<EdgeRoute>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub cards: Vec<Card>,
    pub edges: Vec<Edge>,
}

impl Model {
    pub fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }
}

/// What the caller has arranged by hand, for the half whose positions are not
/// in the source.
pub type Overrides = HashMap<SharedString, (f32, f32)>;

/// Build the one model both halves are drawn from.
pub fn build(
    manifest: Option<&Manifest>,
    wiring: Option<&Wiring>,
    collapsed: &BTreeSet<String>,
    direction: Direction,
    overrides: &Overrides,
) -> Model {
    let mut model = Model::default();
    let mut native_bottom = 0.0f32;

    if let Some(wiring) = wiring {
        let graph = layout::layout(wiring, collapsed, direction);
        for node in &graph.nodes {
            let pos = overrides.get(&node.id).copied().unwrap_or(node.pos);
            let height = card_size(node.kind).1;
            native_bottom = native_bottom.max(pos.1 + height);
            model.cards.push(Card {
                id: node.id.clone(),
                subtitle: native_subtitle(wiring, node.kind, node.source_index),
                inputs: Vec::new(),
                outputs: Vec::new(),
                origin: Origin::Native {
                    kind: node.kind,
                    source_index: node.source_index,
                },
                pos,
                height,
            });
        }
        for (i, edge) in graph.edges.iter().enumerate() {
            // The engine's waypoints hold only while both endpoints sit where
            // it put them; a dragged endpoint falls back to a straight wire so
            // the line tracks the card under the pointer.
            let dragged = overrides.contains_key(&edge.from_node)
                || overrides.contains_key(&edge.to_node);
            model.edges.push(Edge {
                from: edge.from_node.clone(),
                from_port: edge.from_port.clone(),
                to: edge.to_node.clone(),
                to_port: edge.to_port.clone(),
                kind: edge.kind,
                delayed: edge.delayed,
                consumer_port: None,
                route: (!dragged).then(|| graph.routes[i].clone()),
            });
        }
    }

    if let Some(manifest) = manifest {
        python_cards(manifest, native_bottom, &mut model);
    }
    model
}

/// One card per declaration, positioned by its own `@node` where it has one.
fn python_cards(manifest: &Manifest, native_bottom: f32, model: &mut Model) {
    let base = match native_bottom > 0.0 {
        true => native_bottom + PYTHON_GUTTER,
        false => 40.0,
    };
    let declarations = manifest.declarations();
    let depths = depths(manifest, &declarations);
    let mut rows: HashMap<usize, usize> = HashMap::new();
    let first = model.cards.len();

    for (at, decl) in declarations.iter().enumerate() {
        let column = depths[at];
        let row = rows.entry(column).or_default();
        let fallback = (
            40.0 + column as f32 * COLUMN_STRIDE,
            base + *row as f32 * ROW_STRIDE,
        );
        *row += 1;

        let (id, subtitle, inputs, outputs, layout) = match *decl {
            Decl::System(i) => {
                let system = &manifest.systems[i];
                let inputs: Vec<Socket> = system
                    .inputs
                    .iter()
                    .map(|port| Socket {
                        name: port.param.clone(),
                        detail: describe(port),
                    })
                    .collect();
                let outputs: Vec<Socket> = system
                    .output
                    .fields
                    .iter()
                    .zip(&system.publishes)
                    .map(|(field, publishes)| Socket {
                        name: field.name.clone(),
                        detail: publishes.clone(),
                    })
                    .collect();
                let subtitle = match system.rate {
                    Some(hz) => format!("{hz} Hz source"),
                    None => system.output.name.clone(),
                };
                (
                    system.name.clone(),
                    subtitle,
                    inputs,
                    outputs,
                    system.layout,
                )
            }
            Decl::Stage(i) => {
                let stage = &manifest.stages[i];
                let kind = match stage.kind {
                    metor_expr::Resample::Zoh => "zero-order hold",
                    metor_expr::Resample::Linear => "linear",
                };
                (
                    stage.name.clone(),
                    format!("{kind} · {} Hz", stage.rate),
                    vec![Socket {
                        name: "in".to_string(),
                        detail: format!("{}", stage.ty),
                    }],
                    vec![Socket {
                        name: stage.name.clone(),
                        detail: format!("{}", stage.ty),
                    }],
                    stage.layout,
                )
            }
        };

        let rowcount = inputs.len().max(outputs.len()).max(1);
        model.cards.push(Card {
            id: SharedString::from(id),
            subtitle: SharedString::from(subtitle),
            inputs,
            outputs,
            origin: Origin::Python {
                decl: *decl,
                layout,
            },
            pos: layout.position.unwrap_or(fallback),
            height: HEADER_HEIGHT + rowcount as f32 * SOCKET_ROW_HEIGHT + 8.0,
            });
    }

    python_edges(manifest, &declarations, first, model);
}

/// Edges into the Python half: from an earlier declaration, or from the native
/// card whose instance name prefixes the component a port reads.
fn python_edges(manifest: &Manifest, declarations: &[Decl], first: usize, model: &mut Model) {
    let at = |decl: Decl| -> Option<SharedString> {
        let index = declarations.iter().position(|d| *d == decl)?;
        Some(model.cards[first + index].id.clone())
    };

    let mut found: Vec<Edge> = Vec::new();
    for (index, decl) in declarations.iter().enumerate() {
        let consumer = model.cards[first + index].id.clone();
        let sources: Vec<(usize, &Binding, String)> = match *decl {
            Decl::System(i) => manifest.systems[i]
                .inputs
                .iter()
                .enumerate()
                .map(|(port, p)| (port, &p.bindings[0], p.param.clone()))
                .collect(),
            Decl::Stage(i) => vec![(0, &manifest.stages[i].source, "in".to_string())],
        };
        for (port, binding, name) in sources {
            let (from, from_port) = match binding {
                Binding::Produced { system, field } => {
                    let Some(from) = at(Decl::System(*system)) else {
                        continue;
                    };
                    (
                        from,
                        SharedString::from(manifest.systems[*system].output.fields[*field].name.clone()),
                    )
                }
                Binding::Resampled { stage } => {
                    let Some(from) = at(Decl::Stage(*stage)) else {
                        continue;
                    };
                    let port = from.clone();
                    (from, port)
                }
                // A component's first segment is the instance that published
                // it, which is the whole of the cross-source rule.
                Binding::Component(path) => {
                    let (instance, rest) = path.split_once('.').unwrap_or((path.as_str(), ""));
                    let Some(card) = model.card(instance) else {
                        continue;
                    };
                    if card.origin.is_python() {
                        continue;
                    }
                    let frame = rest.split_once('.').map_or(rest, |(frame, _)| frame);
                    (card.id.clone(), SharedString::from(frame.to_string()))
                }
            };
            found.push(Edge {
                from,
                from_port,
                to: consumer.clone(),
                to_port: SharedString::from(name),
                kind: EdgeKind::Frame,
                delayed: false,
                consumer_port: Some(port),
                route: None,
            });
        }
    }
    model.edges.extend(found);
}

/// Column per declaration: one past the deepest thing it reads.
///
/// One pass in declaration order is enough, because a binding may only name an
/// earlier declaration — which is also why the Python half cannot contain a
/// cycle.
fn depths(manifest: &Manifest, declarations: &[Decl]) -> Vec<usize> {
    let mut depth = vec![0usize; declarations.len()];
    for (index, decl) in declarations.iter().enumerate() {
        let bindings: Vec<&Binding> = match *decl {
            Decl::System(i) => manifest.systems[i]
                .inputs
                .iter()
                .map(|p| &p.bindings[0])
                .collect(),
            Decl::Stage(i) => vec![&manifest.stages[i].source],
        };
        for binding in bindings {
            let producer = match binding {
                Binding::Produced { system, .. } => Decl::System(*system),
                Binding::Resampled { stage } => Decl::Stage(*stage),
                Binding::Component(_) => continue,
            };
            if let Some(at) = declarations.iter().position(|d| *d == producer) {
                depth[index] = depth[index].max(depth[at] + 1);
            }
        }
    }
    depth
}

fn describe(port: &metor_expr::Port) -> String {
    match &port.bindings[0] {
        Binding::Component(path) => path.clone(),
        Binding::Produced { .. } | Binding::Resampled { .. } => port.frame.name.clone(),
    }
}

fn native_subtitle(wiring: &Wiring, kind: GraphNodeKind, index: Option<usize>) -> SharedString {
    match (kind, index) {
        (GraphNodeKind::System, Some(i)) => SharedString::from(
            wiring.systems[i]
                .ty
                .clone()
                .unwrap_or_else(|| "(default)".into()),
        ),
        (GraphNodeKind::Slot, Some(i)) => {
            let slot = &wiring.slots[i];
            let initial = slot
                .initial
                .as_ref()
                .map(|i| i.occupant.clone())
                .unwrap_or_else(|| "(empty)".to_string());
            SharedString::from(format!("initial: {initial}"))
        }
        (GraphNodeKind::Coordinator, _) => {
            SharedString::from(format!("{} Hz", wiring.coordinator.cycle_rate))
        }
        _ => SharedString::new_static("(collapsed — click to expand)"),
    }
}

/// The width every card is drawn at, native and Python alike.
pub const CARD_WIDTH: f32 = NODE_WIDTH;

#[cfg(test)]
mod tests;
