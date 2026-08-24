//! Both pipelines, side by side, on the same graphs.
//!
//! The plan's bar for deleting the node editor is that a converted graph
//! publishes the *same components* — same ids, same schemas. So these tests do
//! not inspect the generated Python at all. They build the legacy graph
//! against one database, build the converted program against another, and
//! compare what each registered.
//!
//! Every op the node editor had appears below, which is more coverage than the
//! repo's own presets could give: the shipped presets are dashboards and plots
//! and contain no node graphs at all, so "run it on the examples" would have
//! exercised nothing.

use std::collections::HashMap;
use std::sync::Arc;

use metor_db::{ComponentSchema, DB};
use metor_expr::{Binding, Decl};
use metor_proto::types::{ComponentId, PrimType};
use smallvec::smallvec;

use super::*;
use crate::dynamic::DynamicNode;
use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::ops::{db_source, persist};
use crate::dynamic::resolver::DbResolver;
use crate::node_editor::config::{SerializedEdge, SerializedNode};
use crate::node_editor::spec;

struct Bench {
    db: DB,
    _temp: tempfile::TempDir,
}

impl Bench {
    fn new(components: &[(&str, PrimType, &[usize])]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        for (name, prim, dim) in components {
            let id = ComponentId::new(name);
            db.with_state_mut(|state| {
                state.insert_component(id, ComponentSchema::new(*prim, dim), &db.path)
            })
            .unwrap();
            let mut metadata = metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: (*name).to_string(),
                metadata: Default::default(),
            };
            use metor_proto_wkt::MetadataExt;
            metadata.set("source", "test");
            db.with_state_mut(|state| state.set_component_metadata(metadata, &db.path))
                .unwrap();
        }
        Bench { db, _temp: temp }
    }

    /// What a component is, as the two pipelines must agree on it.
    fn published(&self, name: &str) -> Option<(ComponentId, ComponentSchema)> {
        let id = ComponentId::new(name);
        self.db
            .with_state(|s| s.get_component(id).map(|c| (id, c.schema.clone())))
    }
}

/// A graph, written the way the editor saved one.
struct Graph {
    nodes: Vec<SerializedNode>,
    edges: Vec<SerializedEdge>,
}

impl Graph {
    fn new() -> Self {
        Graph {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    fn node(mut self, id: &str, spec: NodeSpec) -> Self {
        let at = self.nodes.len() as f32;
        self.nodes.push(SerializedNode {
            flow_id: id.to_string(),
            spec,
            x: 40.0 * at,
            y: 20.0 * at,
        });
        self
    }

    fn edge(mut self, from: &str, to: &str, socket: u32) -> Self {
        self.edges.push(SerializedEdge {
            source: from.to_string(),
            target: to.to_string(),
            target_socket: socket,
        });
        self
    }

    fn config(&self) -> NodeEditorConfig {
        NodeEditorConfig {
            viewport: Default::default(),
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
        }
    }

    /// Build the graph the way the node editor built it.
    fn build_legacy(&self, db: &DB) -> Vec<Arc<dyn DynamicNode>> {
        let mut built: HashMap<&str, Arc<dyn DynamicNode>> = HashMap::new();
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.edges {
            let slot = parents.entry(edge.target.as_str()).or_default();
            let at = edge.target_socket as usize;
            if slot.len() <= at {
                slot.resize(at + 1, "");
            }
            slot[at] = edge.source.as_str();
        }
        // Each fixture writes its nodes producer-first, so declaration order
        // is topological.
        for node in &self.nodes {
            let inputs: Vec<Arc<dyn DynamicNode>> = parents
                .get(node.flow_id.as_str())
                .map(|list| list.iter().filter_map(|id| built.get(id).cloned()).collect())
                .unwrap_or_default();
            match spec::build(&node.spec, inputs, db) {
                Ok(node_built) => {
                    built.insert(node.flow_id.as_str(), node_built);
                }
                Err(err) => panic!("legacy build of `{}` failed: {err}", node.flow_id),
            }
        }
        built.into_values().collect()
    }
}

/// Build the converted program the way the tile builds one.
fn build_converted(source: &str, db: &DB) -> Result<Vec<Arc<dyn DynamicNode>>, String> {
    let resolver = DbResolver::snapshot(db);
    let compiled = match Compiled::module(source, &resolver) {
        Ok(compiled) => Arc::new(compiled),
        Err(diags) => return Err(format!("{diags}")),
    };
    let mut held: Vec<Arc<dyn DynamicNode>> = Vec::new();

    for decl in compiled.manifest.declarations() {
        let Decl::System(index) = decl else {
            continue;
        };
        let desc = &compiled.manifest.systems[index];
        let mut ports = Vec::with_capacity(desc.inputs.len());
        for port in &desc.inputs {
            let id = match &port.bindings[0] {
                Binding::Component(path) => resolver
                    .id_of(path)
                    .ok_or_else(|| format!("`{path}` is not a component"))?,
                Binding::Produced { system, field } => {
                    ComponentId::new(&compiled.manifest.systems[*system].publishes[*field])
                }
                Binding::Resampled { stage } => {
                    ComponentId::new(&compiled.manifest.stages[*stage].name)
                }
            };
            let node = db_source::from_db(db, id).map_err(|e| e.to_string())?;
            ports.push(program::PortSource::live(node));
        }
        let system = program::system(&compiled, index, ports, DEFAULT_FUEL, None)
            .map_err(|e| e.to_string())?;
        held.push(system.node.clone());
        for (field, name) in desc.publishes.iter().enumerate() {
            let node = program::field(&compiled, index, field, system.node.clone())
                .map_err(|e| e.to_string())?;
            held.push(persist::persist(db, name.clone(), node).map_err(|e| e.to_string())?);
        }
    }
    Ok(held)
}

/// The whole check for one graph: both pipelines, then a diff.
fn agree(graph: &Graph, components: &[(&str, PrimType, &[usize])], published: &str) {
    let legacy = Bench::new(components);
    let _legacy_nodes = graph.build_legacy(&legacy.db);
    let want = legacy
        .published(published)
        .unwrap_or_else(|| panic!("the legacy graph published nothing called `{published}`"));

    let converted = convert_with(&graph.config(), &|id| {
        components
            .iter()
            .map(|(name, _, _)| *name)
            .find(|name| ComponentId::new(name) == id)
            .map(str::to_string)
    });
    assert!(
        converted.refused.is_empty(),
        "conversion refused: {:?}\n{}",
        converted.refused,
        converted.source
    );

    let fresh = Bench::new(components);
    let _new_nodes = match build_converted(&converted.source, &fresh.db) {
        Ok(nodes) => nodes,
        Err(why) => panic!("converted program did not build: {why}\n{}", converted.source),
    };
    let got = fresh.published(published).unwrap_or_else(|| {
        panic!(
            "the converted program published nothing called `{published}`\n{}",
            converted.source
        )
    });

    assert_eq!(
        got, want,
        "`{published}` differs after conversion\n{}",
        converted.source
    );
}

const SCALAR: &[(&str, PrimType, &[usize])] = &[("wheels.rpm", PrimType::F64, &[])];
const VECTOR: &[(&str, PrimType, &[usize])] = &[("adcs.omega_b", PrimType::F64, &[3])];

fn from_component(name: &str) -> Graph {
    Graph::new().node(
        "src",
        NodeSpec::FromDb {
            component_id: ComponentId::new(name).0,
        },
    )
}

fn published(graph: Graph) -> Graph {
    graph.node(
        "out",
        NodeSpec::Persist {
            name: "derived".to_string(),
        },
    )
}

fn k(v: f64) -> TypedScalar {
    TypedScalar::F64(v)
}

/// Every single-input op over an `f64` scalar, which is the shape almost every
/// saved graph has.
#[stellarator::test]
async fn scalar_ops_publish_the_same_component() {
    let cases: Vec<(&str, NodeSpec)> = vec![
        (
            "scale",
            NodeSpec::Affine {
                op: AffineOp::Scale,
                k: k(2.0),
            },
        ),
        (
            "offset",
            NodeSpec::Affine {
                op: AffineOp::Offset,
                k: k(1.5),
            },
        ),
        ("abs", NodeSpec::Unary { op: UnaryOp::Abs }),
        ("neg", NodeSpec::Unary { op: UnaryOp::Neg }),
        ("log", NodeSpec::Unary { op: UnaryOp::Log }),
        ("sqrt", NodeSpec::Unary { op: UnaryOp::Sqrt }),
        ("exp", NodeSpec::Unary { op: UnaryOp::Exp }),
        ("floor", NodeSpec::Unary { op: UnaryOp::Floor }),
        (
            "threshold",
            NodeSpec::Threshold {
                k: k(500.0),
                op: ThresholdOp::Gt,
            },
        ),
        ("delta", NodeSpec::Delta),
        ("window", NodeSpec::Window { size: 4 }),
    ];

    for (label, op) in cases {
        let graph = published(from_component("wheels.rpm").node("op", op))
            .edge("src", "op", 0)
            .edge("op", "out", 0);
        agree(&graph, SCALAR, "derived");
        println!("{label}: identical");
    }
}

/// The ops that reduce or reshape a vector.
#[stellarator::test]
async fn vector_ops_publish_the_same_component() {
    for (label, op) in [
        ("magnitude", NodeSpec::Magnitude),
        ("index", NodeSpec::Index { index: 1 }),
        (
            "scale",
            NodeSpec::Affine {
                op: AffineOp::Scale,
                k: k(0.5),
            },
        ),
    ] {
        let graph = published(from_component("adcs.omega_b").node("op", op))
            .edge("src", "op", 0)
            .edge("op", "out", 0);
        agree(&graph, VECTOR, "derived");
        println!("{label}: identical");
    }
}

/// An FFT over a power-of-two vector, which is the compatibility contract Q3
/// was written against.
#[stellarator::test]
async fn the_spectrum_publishes_the_same_component() {
    let components: &[(&str, PrimType, &[usize])] = &[("adcs.spectrum_in", PrimType::F64, &[8])];
    let graph = published(from_component("adcs.spectrum_in").node("op", NodeSpec::Fft))
        .edge("src", "op", 0)
        .edge("op", "out", 0);
    agree(&graph, components, "derived");
}

/// Two-input ops, where socket order decides which operand is which.
#[stellarator::test]
async fn two_input_ops_publish_the_same_component() {
    let components: &[(&str, PrimType, &[usize])] = &[("wheels.rpm", PrimType::F64, &[])];
    for (label, op) in [
        ("add", NodeSpec::Binary { op: BinaryOp::Add }),
        ("sub", NodeSpec::Binary { op: BinaryOp::Sub }),
        ("mul", NodeSpec::Binary { op: BinaryOp::Mul }),
        ("div", NodeSpec::Binary { op: BinaryOp::Div }),
        ("mean", NodeSpec::Mean),
    ] {
        let graph = published(
            from_component("wheels.rpm")
                .node(
                    "a",
                    NodeSpec::Affine {
                        op: AffineOp::Scale,
                        k: k(2.0),
                    },
                )
                .node(
                    "b",
                    NodeSpec::Affine {
                        op: AffineOp::Offset,
                        k: k(1.0),
                    },
                )
                .node("op", op),
        )
        .edge("src", "a", 0)
        .edge("src", "b", 0)
        .edge("a", "op", 0)
        .edge("b", "op", 1)
        .edge("op", "out", 0);
        agree(&graph, components, "derived");
        println!("{label}: identical");
    }

    let vectors: &[(&str, PrimType, &[usize])] = &[("adcs.omega_b", PrimType::F64, &[3])];
    let graph = published(
        from_component("adcs.omega_b")
            .node(
                "a",
                NodeSpec::Affine {
                    op: AffineOp::Scale,
                    k: k(2.0),
                },
            )
            .node(
                "b",
                NodeSpec::Affine {
                    op: AffineOp::Offset,
                    k: k(1.0),
                },
            )
            .node("op", NodeSpec::Dot),
    )
    .edge("src", "a", 0)
    .edge("src", "b", 0)
    .edge("a", "op", 0)
    .edge("b", "op", 1)
    .edge("op", "out", 0);
    agree(&graph, vectors, "derived");
}

/// Generators, which were a clock plus a node and are now one source system.
#[stellarator::test]
async fn generators_publish_the_same_component() {
    for (label, op) in [
        (
            "waveform",
            NodeSpec::Waveform {
                shape: Waveform::Sin,
                freq: 2.0,
                amplitude: 3.0,
                phase: 0.0,
                dtype: PrimType::F64,
                out_shape: smallvec![],
            },
        ),
        (
            "random",
            NodeSpec::Random {
                seed: 7,
                dtype: PrimType::F64,
                out_shape: smallvec![],
            },
        ),
        (
            "constant",
            NodeSpec::Constant {
                value: k(9.81),
                out_shape: smallvec![],
            },
        ),
    ] {
        let graph = published(
            Graph::new()
                .node("clk", NodeSpec::FixedRate { hz: 50.0 })
                .node("op", op),
        )
        .edge("clk", "op", 0)
        .edge("op", "out", 0);
        agree(&graph, SCALAR, "derived");
        println!("{label}: identical");
    }
}

/// A graph several nodes deep, which is where names rather than ids carry the
/// edges.
#[stellarator::test]
async fn a_chain_publishes_the_same_component() {
    let graph = published(
        from_component("wheels.rpm")
            .node(
                "a",
                NodeSpec::Affine {
                    op: AffineOp::Scale,
                    k: k(9.81),
                },
            )
            .node(
                "b",
                NodeSpec::Affine {
                    op: AffineOp::Offset,
                    k: k(3.0),
                },
            )
            .node("c", NodeSpec::Unary { op: UnaryOp::Sqrt }),
    )
    .edge("src", "a", 0)
    .edge("a", "b", 0)
    .edge("b", "c", 0)
    .edge("c", "out", 0);
    agree(&graph, SCALAR, "derived");
}

/// Positions come across, because the diagram is part of what was saved.
#[test]
fn positions_become_annotations() {
    let graph = published(from_component("wheels.rpm").node(
        "op",
        NodeSpec::Affine {
            op: AffineOp::Scale,
            k: k(2.0),
        },
    ))
    .edge("src", "op", 0)
    .edge("op", "out", 0);
    let converted = convert_with(&graph.config(), &|_| Some("wheels.rpm".to_string()));
    assert!(
        converted.source.contains("# @node(x=40, y=20)"),
        "{}",
        converted.source
    );
}

/// The two ops with no expression in the language say so, by name, instead of
/// converting into something that merely looks right.
#[test]
fn what_cannot_convert_is_named() {
    for (label, op, needle) in [
        ("pack", NodeSpec::Pack, "no tensor literal"),
        ("delta_t", NodeSpec::DeltaT, "state field"),
    ] {
        let graph = published(from_component("wheels.rpm").node("op", op))
            .edge("src", "op", 0)
            .edge("op", "out", 0);
        let converted = convert_with(&graph.config(), &|_| Some("wheels.rpm".to_string()));
        assert!(
            converted.refused.iter().any(|why| why.contains(needle)),
            "{label}: {:?}",
            converted.refused
        );
    }
}

/// Where the two vocabularies genuinely differ, and it is not the converter's
/// to hide: the legacy ops kept a narrower element type, the language has one.
#[stellarator::test]
async fn a_narrower_source_widens_and_the_report_says_so() {
    let components: &[(&str, PrimType, &[usize])] = &[("sensor.count", PrimType::I32, &[])];
    let graph = published(from_component("sensor.count").node("op", NodeSpec::Window { size: 4 }))
        .edge("src", "op", 0)
        .edge("op", "out", 0);

    let legacy = Bench::new(components);
    let _held = graph.build_legacy(&legacy.db);
    let (legacy_id, legacy_schema) = legacy.published("derived").unwrap();

    let converted = convert_with(&graph.config(), &|_| Some("sensor.count".to_string()));
    let fresh = Bench::new(components);
    let _new = build_converted(&converted.source, &fresh.db).unwrap();
    let (new_id, new_schema) = fresh.published("derived").unwrap();

    assert_eq!(new_id, legacy_id, "the id is the name, and the name is kept");
    assert_eq!(legacy_schema.prim_type, PrimType::I32);
    assert_eq!(new_schema.prim_type, PrimType::F64);
    assert_eq!(new_schema.dim, legacy_schema.dim, "the shape is unchanged");
    assert!(widens(legacy_schema.prim_type), "and the report flags it");
}
