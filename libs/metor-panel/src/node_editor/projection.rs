//! A program, seen as a graph.
//!
//! The design's claim is that a canvas can be a *projection* of source rather
//! than a second source of truth, and that this is tractable because the
//! projectable surface is declaration-shaped: a flat set of named units whose
//! connections are recoverable from names, with layout out of band. This
//! module is that claim, written down.
//!
//! Everything here is derived. One card per system, its input sockets from the
//! manifest's ports and its output sockets from the output frame's fields, and
//! an edge wherever one system's port reads what another publishes. Nothing is
//! stored that the source could not say again — except position, which the
//! source has no business knowing and which therefore lives in the pane.
//!
//! It is read-only in this phase, deliberately. Enso spent years on text↔graph
//! reconciliation, and the mitigation that matters is structural: prove the
//! projection against real programs *before* the gestures that rewrite them
//! exist. Phase 2 turns [`Edge`]s into rebinding gestures and [`Socket`]s into
//! drop targets; what it must not have to change is what a card *is*.
//!
//! ## What Phase 2 inherits
//!
//! - A card is identified by its **system name**, not by an index. Names are
//!   what edges are recovered from and what layout is keyed by, so a rename is
//!   a real migration and an index would have made it invisible.
//! - An edge is `(producer, producer field) -> (consumer, consumer port)`. It
//!   carries the frame name it matched on, because a rebinding gesture has to
//!   rewrite exactly that.
//! - Layout is a `name -> position` sidecar with a deterministic fallback, so
//!   a program that has never been laid out still reads correctly and a
//!   program that has keeps what the operator arranged.

use std::collections::HashMap;

use metor_expr::{Binding, Manifest};

/// Where a card sits, in graph coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

/// One socket on a card: a port a system reads, or a field it publishes.
#[derive(Clone, Debug, PartialEq)]
pub struct Socket {
    /// The parameter or field name, as the source writes it.
    pub name: String,
    /// What flows through it, for the row's second line.
    pub detail: String,
}

/// One system, as a card.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub name: String,
    pub inputs: Vec<Socket>,
    pub outputs: Vec<Socket>,
    pub position: Position,
}

/// One frame edge, recovered from names.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub producer: usize,
    pub producer_field: usize,
    pub consumer: usize,
    pub consumer_port: usize,
}

/// A whole program, projected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    pub cards: Vec<Card>,
    pub edges: Vec<Edge>,
}

/// Project a compiled program onto the canvas.
///
/// `placed` supplies whatever the pane remembers about where cards go; a
/// system it has never seen is laid out by [`column_layout`].
pub fn project(manifest: &Manifest, placed: &HashMap<String, Position>) -> Projection {
    let depths = depths(manifest);
    let mut per_column: HashMap<usize, usize> = HashMap::new();

    let cards = manifest
        .systems
        .iter()
        .enumerate()
        .map(|(index, system)| {
            let column = depths[index];
            let row = per_column.entry(column).or_default();
            let position = placed
                .get(&system.name)
                .copied()
                .unwrap_or_else(|| column_layout(column, *row));
            *row += 1;

            Card {
                name: system.name.clone(),
                inputs: system
                    .inputs
                    .iter()
                    .map(|port| Socket {
                        name: port.param.clone(),
                        detail: describe_port(port),
                    })
                    .collect(),
                outputs: system
                    .output
                    .fields
                    .iter()
                    .zip(&system.publishes)
                    .map(|(field, published)| Socket {
                        name: field.name.clone(),
                        detail: published.clone(),
                    })
                    .collect(),
                position,
            }
        })
        .collect();

    Projection {
        cards,
        edges: edges(manifest),
    }
}

/// Edges are recovered from names, the same way wiring recovers ports: a
/// binding that names another system's output *is* the fact that they are
/// connected. Nothing else records the connection, which is why the source
/// alone is enough to rebuild the graph.
fn edges(manifest: &Manifest) -> Vec<Edge> {
    let mut out = Vec::new();
    for (consumer, system) in manifest.systems.iter().enumerate() {
        for (consumer_port, port) in system.inputs.iter().enumerate() {
            for binding in &port.bindings {
                if let Binding::Produced { system, field } = binding {
                    let edge = Edge {
                        producer: *system,
                        producer_field: *field,
                        consumer,
                        consumer_port,
                    };
                    // A multi-field frame read from one producer is one edge,
                    // not one per field: the canvas connects frames.
                    if !out.contains(&edge) {
                        out.push(edge);
                    }
                }
            }
        }
    }
    out
}

/// How far each system sits from the components it ultimately reads.
///
/// A binding may only name an earlier declaration, so one pass in declaration
/// order settles every depth — the graph cannot contain a cycle for the same
/// reason.
fn depths(manifest: &Manifest) -> Vec<usize> {
    let mut depths = vec![0usize; manifest.systems.len()];
    for (index, system) in manifest.systems.iter().enumerate() {
        let mut depth = 0;
        for port in &system.inputs {
            for binding in &port.bindings {
                if let Binding::Produced { system, .. } = binding {
                    depth = depth.max(depths[*system] + 1);
                }
            }
        }
        depths[index] = depth;
    }
    depths
}

/// Width one card occupies, plus the gap to the next column.
const COLUMN_STRIDE: f32 = 240.0;
/// Height one card occupies, plus the gap to the next row.
const ROW_STRIDE: f32 = 140.0;

/// Where a card goes when nobody has moved it: dataflow left to right, one
/// column per step away from the raw components.
pub fn column_layout(column: usize, row: usize) -> Position {
    Position {
        x: 40.0 + column as f32 * COLUMN_STRIDE,
        y: 40.0 + row as f32 * ROW_STRIDE,
    }
}

/// What a port reads, said the way a socket row should say it.
fn describe_port(port: &metor_expr::Port) -> String {
    match port.bindings.as_slice() {
        [Binding::Component(path)] => path.clone(),
        [] => port.frame.name.clone(),
        bindings if bindings.iter().all(|b| matches!(b, Binding::Produced { .. })) => {
            port.frame.name.clone()
        }
        _ => format!("{} ({} fields)", port.frame.name, port.bindings.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_expr::{CompSchema, FrameSchema, Resolver, Ty};

    struct Table(Vec<(&'static str, Ty)>);

    impl Resolver for Table {
        fn component(&self, path: &str) -> Option<CompSchema> {
            self.0
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, ty)| CompSchema { ty: ty.clone() })
        }
        fn suffix(&self, name: &str) -> Vec<String> {
            self.0
                .iter()
                .filter(|(path, _)| path.rsplit('.').next() == Some(name))
                .map(|(path, _)| (*path).to_string())
                .collect()
        }
        fn frame(&self, _name: &str) -> Option<FrameSchema> {
            None
        }
    }

    fn table() -> Table {
        Table(vec![
            ("wheels.rpm", Ty::F64),
            ("adcs.omega_b", Ty::Tensor {
                dtype: metor_expr::Dtype::F64,
                shape: vec![3],
            }),
        ])
    }

    /// One system, one card; the sockets say what the manifest says.
    #[test]
    fn a_system_projects_to_a_card_with_its_ports_and_its_publications() {
        let program = metor_expr::compile_module(
            "class Rate(Frame):\n\
             \x20   fast: f64\n\
             \x20   hot: bool\n\
             \n\
             @system(\"wheels.rpm\")\n\
             def watch(rpm) -> f64:\n\
             \x20   return rpm * 2.0\n",
            &table(),
        )
        .expect("compiles");
        let projection = project(&program.manifest, &HashMap::new());

        assert_eq!(projection.cards.len(), 1);
        let card = &projection.cards[0];
        assert_eq!(card.name, "watch");
        assert_eq!(card.inputs.len(), 1);
        assert_eq!(card.inputs[0].name, "rpm");
        assert_eq!(card.inputs[0].detail, "wheels.rpm");
        assert_eq!(card.outputs.len(), 1);
        assert_eq!(card.outputs[0].detail, "watch");
        assert!(projection.edges.is_empty());
    }

    /// The projection rule that matters: an edge exists because two systems
    /// name the same frame, and nothing else records it.
    #[test]
    fn edges_are_recovered_from_names_and_lay_out_left_to_right() {
        let program = metor_expr::compile_module(
            "scaled = adcs.omega_b * 100.0\n\
             total = scaled[0] + scaled[1] + scaled[2]\n\
             also = total * 2.0\n",
            &table(),
        )
        .expect("compiles");
        let projection = project(&program.manifest, &HashMap::new());

        assert_eq!(projection.cards.len(), 3);
        assert_eq!(
            projection.edges,
            vec![
                Edge {
                    producer: 0,
                    producer_field: 0,
                    consumer: 1,
                    consumer_port: 0
                },
                Edge {
                    producer: 1,
                    producer_field: 0,
                    consumer: 2,
                    consumer_port: 0
                },
            ]
        );

        // Each step away from the raw components is one column further right.
        let xs: Vec<f32> = projection.cards.iter().map(|c| c.position.x).collect();
        assert!(xs[0] < xs[1] && xs[1] < xs[2], "{xs:?}");
        assert_eq!(projection.cards[0].position.y, projection.cards[1].position.y);
    }

    /// Layout is a sidecar keyed by name: what the operator arranged survives,
    /// and what they have not seen yet is placed for them.
    #[test]
    fn remembered_positions_win_over_the_automatic_layout() {
        let program = metor_expr::compile_module(
            "scaled = adcs.omega_b * 100.0\ntotal = scaled[0]\n",
            &table(),
        )
        .expect("compiles");
        let mut placed = HashMap::new();
        placed.insert("total".to_string(), Position { x: 7.0, y: 9.0 });

        let projection = project(&program.manifest, &placed);
        assert_eq!(projection.cards[1].position, Position { x: 7.0, y: 9.0 });
        assert_eq!(projection.cards[0].position, column_layout(0, 0));
    }

    /// A program with no systems projects to an empty graph rather than to
    /// anything that has to be special-cased downstream.
    #[test]
    fn an_empty_program_projects_to_an_empty_graph() {
        let program = metor_expr::compile_module("", &table()).expect("compiles");
        assert_eq!(
            project(&program.manifest, &HashMap::new()),
            Projection::default()
        );
    }
}
