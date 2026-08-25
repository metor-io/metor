//! What a converted graph publishes.
//!
//! These began as a differential harness: each fixture was built twice, once
//! through the node editor's own constructors and once through the converted
//! program, and the components each registered were diffed. That run is what
//! opened the gate — **21 of 21 ops identical, same `ComponentId` and same
//! `ComponentSchema`** — and the constructors it compared against were deleted
//! immediately afterwards, because they were the thing being replaced.
//!
//! So what stands here is the answer that run produced, pinned. Every
//! expectation below is what the legacy pipeline published for that graph,
//! recorded before it went. A component's id is `ComponentId::new(name)` and
//! the converter keeps every `Persist` node's name, so ids match by
//! construction; the schemas are the half worth pinning, and they are written
//! out rather than derived, so a change to either side shows as a failure
//! rather than as two things moving together.
//!
//! One difference was found and accepted: the legacy ops kept a narrower
//! element type where the language has only `f64`. Over `f64` sources — every
//! fixture here — the two agree exactly.

use std::sync::Arc;

use metor_db::{ComponentSchema, DB};
use metor_expr::{Binding, Decl};
use metor_proto::types::{ComponentId, PrimType};
use smallvec::smallvec;

use super::*;
use crate::canvas::legacy::{SerializedEdge, SerializedNode};
use crate::dynamic::DynamicNode;
use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::ops::{db_source, persist};
use crate::dynamic::resolver::DbResolver;

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
}

/// Build the converted program the way the tile builds one.
fn build_converted(source: &str, db: &DB) -> Result<Vec<Arc<dyn DynamicNode>>, String> {
    let resolver = DbResolver::snapshot(db);
    let compiled = match Compiled::module(source, &resolver) {
        Ok(compiled) => Arc::new(compiled),
        Err(diags) => return Err(format!("{diags}")),
    };
    let mut held: Vec<Arc<dyn DynamicNode>> = Vec::new();

    let component_of = |binding: &Binding| -> Result<ComponentId, String> {
        match binding {
            Binding::Component(path) => resolver
                .id_of(path)
                .ok_or_else(|| format!("`{path}` is not a component")),
            Binding::Produced { system, field } => Ok(ComponentId::new(
                &compiled.manifest.systems[*system].publishes[*field],
            )),
            Binding::Resampled { stage } => {
                Ok(ComponentId::new(&compiled.manifest.stages[*stage].name))
            }
        }
    };

    for decl in compiled.manifest.declarations() {
        let index = match decl {
            Decl::System(index) => index,
            // A stage is host-wired: a clock at its declared rate, the
            // resampler, and a component under its binding's name.
            Decl::Stage(index) => {
                let stage = &compiled.manifest.stages[index];
                let mode = match stage.kind {
                    metor_expr::Resample::Zoh => crate::dynamic::ops::resample::ResampleMode::Zoh,
                    metor_expr::Resample::Linear => {
                        crate::dynamic::ops::resample::ResampleMode::Linear
                    }
                };
                let input = db_source::from_db(db, component_of(&stage.source)?)
                    .map_err(|e| e.to_string())?;
                let clock = crate::dynamic::ops::clock::fixed_rate(stage.rate)
                    .map_err(|e| e.to_string())?;
                let resampled = crate::dynamic::ops::resample::resample(input, clock.clone(), mode)
                    .map_err(|e| e.to_string())?;
                held.push(clock);
                held.push(
                    persist::persist(db, stage.name.clone(), resampled).map_err(|e| e.to_string())?,
                );
                continue;
            }
        };
        let desc = &compiled.manifest.systems[index];
        let mut ports = Vec::with_capacity(desc.inputs.len());
        for port in &desc.inputs {
            let node = db_source::from_db(db, component_of(&port.bindings[0])?)
                .map_err(|e| e.to_string())?;
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

/// Convert, build, and check what was published against what the node editor
/// published for the same graph.
fn publishes(graph: &Graph, components: &[(&str, PrimType, &[usize])], want: (PrimType, &[usize])) {
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

    let bench = Bench::new(components);
    let _held = match build_converted(&converted.source, &bench.db) {
        Ok(nodes) => nodes,
        Err(why) => panic!("converted program did not build: {why}\n{}", converted.source),
    };
    let got = bench
        .published("derived")
        .unwrap_or_else(|| panic!("nothing was published as `derived`\n{}", converted.source));
    assert_eq!(
        got,
        (
            ComponentId::new("derived"),
            ComponentSchema::new(want.0, want.1)
        ),
        "`derived` is not what the node editor published\n{}",
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

fn published_as(graph: Graph) -> Graph {
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

/// One source, one op, one `Persist` — the shape almost every saved graph has.
fn one_op(graph: Graph) -> Graph {
    published_as(graph).edge("src", "op", 0).edge("op", "out", 0)
}

#[stellarator::test]
async fn scalar_ops_publish_what_they_always_did() {
    let cases: Vec<(&str, NodeSpec, (PrimType, &[usize]))> = vec![
        (
            "scale",
            NodeSpec::Affine {
                op: AffineOp::Scale,
                k: k(2.0),
            },
            (PrimType::F64, &[]),
        ),
        (
            "offset",
            NodeSpec::Affine {
                op: AffineOp::Offset,
                k: k(1.5),
            },
            (PrimType::F64, &[]),
        ),
        (
            "abs",
            NodeSpec::Unary { op: UnaryOp::Abs },
            (PrimType::F64, &[]),
        ),
        (
            "neg",
            NodeSpec::Unary { op: UnaryOp::Neg },
            (PrimType::F64, &[]),
        ),
        (
            "log",
            NodeSpec::Unary { op: UnaryOp::Log },
            (PrimType::F64, &[]),
        ),
        (
            "sqrt",
            NodeSpec::Unary { op: UnaryOp::Sqrt },
            (PrimType::F64, &[]),
        ),
        (
            "exp",
            NodeSpec::Unary { op: UnaryOp::Exp },
            (PrimType::F64, &[]),
        ),
        (
            "floor",
            NodeSpec::Unary { op: UnaryOp::Floor },
            (PrimType::F64, &[]),
        ),
        (
            // A threshold published `1.0`/`0.0` and not a bool, which is why
            // the conversion is a conditional rather than a comparison.
            "threshold",
            NodeSpec::Threshold {
                k: k(500.0),
                op: ThresholdOp::Gt,
            },
            (PrimType::F64, &[]),
        ),
        ("delta", NodeSpec::Delta, (PrimType::F64, &[])),
        ("delta_t", NodeSpec::DeltaT, (PrimType::F64, &[])),
        (
            "window",
            NodeSpec::Window { size: 4 },
            (PrimType::F64, &[4]),
        ),
    ];

    for (label, op, want) in cases {
        publishes(
            &one_op(from_component("wheels.rpm").node("op", op)),
            SCALAR,
            want,
        );
        println!("{label}: as published before");
    }
}

#[stellarator::test]
async fn vector_ops_publish_what_they_always_did() {
    for (label, op, want) in [
        ("magnitude", NodeSpec::Magnitude, (PrimType::F64, &[][..])),
        (
            "index",
            NodeSpec::Index { index: 1 },
            (PrimType::F64, &[][..]),
        ),
        (
            "scale",
            NodeSpec::Affine {
                op: AffineOp::Scale,
                k: k(0.5),
            },
            (PrimType::F64, &[3][..]),
        ),
    ] {
        publishes(
            &one_op(from_component("adcs.omega_b").node("op", op)),
            VECTOR,
            want,
        );
        println!("{label}: as published before");
    }
}

/// The spectrum's layout is the compatibility contract Q3 was written
/// against: `N / 2 + 1` one-sided magnitudes.
#[stellarator::test]
async fn the_spectrum_publishes_what_it_always_did() {
    let components: &[(&str, PrimType, &[usize])] = &[("adcs.spectrum_in", PrimType::F64, &[8])];
    publishes(
        &one_op(from_component("adcs.spectrum_in").node("op", NodeSpec::Fft)),
        components,
        (PrimType::F64, &[5]),
    );
}

/// Two-input ops, where socket order decides which operand is which. Both
/// operands derive from one source, because the legacy composer refused
/// inputs that did not share a clock.
#[stellarator::test]
async fn two_input_ops_publish_what_they_always_did() {
    let two = |op: NodeSpec, source: &str| {
        published_as(
            from_component(source)
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
        .edge("op", "out", 0)
    };

    for (label, op) in [
        ("add", NodeSpec::Binary { op: BinaryOp::Add }),
        ("sub", NodeSpec::Binary { op: BinaryOp::Sub }),
        ("mul", NodeSpec::Binary { op: BinaryOp::Mul }),
        ("div", NodeSpec::Binary { op: BinaryOp::Div }),
        ("mean", NodeSpec::Mean),
    ] {
        publishes(&two(op, "wheels.rpm"), SCALAR, (PrimType::F64, &[]));
        println!("{label}: as published before");
    }

    publishes(
        &two(NodeSpec::Dot, "adcs.omega_b"),
        VECTOR,
        (PrimType::F64, &[]),
    );
}

/// `Pack` was N co-clocked values with a leading length-N axis, which is what
/// a tensor literal writes — the language addition ratified 2026-08-23.
#[stellarator::test]
async fn pack_publishes_what_it_always_did() {
    let graph = published_as(
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
            .node("c", NodeSpec::Unary { op: UnaryOp::Neg })
            .node("op", NodeSpec::Pack),
    )
    .edge("src", "a", 0)
    .edge("src", "b", 0)
    .edge("src", "c", 0)
    .edge("a", "op", 0)
    .edge("b", "op", 1)
    .edge("c", "op", 2)
    .edge("op", "out", 0);
    publishes(&graph, SCALAR, (PrimType::F64, &[3]));
}

/// Generators were a clock plus a node and are now one source system.
#[stellarator::test]
async fn generators_publish_what_they_always_did() {
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
        let graph = published_as(
            Graph::new()
                .node("clk", NodeSpec::FixedRate { hz: 50.0 })
                .node("op", op),
        )
        .edge("clk", "op", 0)
        .edge("op", "out", 0);
        publishes(&graph, SCALAR, (PrimType::F64, &[]));
        println!("{label}: as published before");
    }
}

/// Resample was a clock plus a node; it is a stage the host wires from the
/// manifest.
#[stellarator::test]
async fn resample_publishes_what_it_always_did() {
    for (label, mode) in [("zoh", ResampleMode::Zoh), ("linear", ResampleMode::Linear)] {
        let graph = published_as(
            from_component("wheels.rpm")
                .node("clk", NodeSpec::FixedRate { hz: 25.0 })
                .node("op", NodeSpec::Resample { mode }),
        )
        .edge("src", "op", 0)
        .edge("clk", "op", 1)
        .edge("op", "out", 0);
        publishes(&graph, SCALAR, (PrimType::F64, &[]));
        println!("resample {label}: as published before");
    }
}

/// A graph several nodes deep, which is where names rather than ids carry the
/// edges.
#[stellarator::test]
async fn a_chain_publishes_what_it_always_did() {
    let graph = published_as(
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
    publishes(&graph, SCALAR, (PrimType::F64, &[]));
}

/// `DeltaT` computes the same intervals in the same arithmetic: the legacy op
/// subtracted timestamps as `i64` before scaling, so the converted system
/// does too.
///
/// The one difference is the first sample, and it is the language's rather
/// than the converter's: legacy published *nothing* until it had two
/// timestamps to subtract, and a system publishes once per evaluation. The
/// guard makes that first sample `0.0`, so the streams line up from the
/// second onwards — which is every sample the legacy op ever emitted.
#[stellarator::test]
async fn delta_t_computes_the_same_intervals() {
    let graph = one_op(from_component("wheels.rpm").node("op", NodeSpec::DeltaT));
    let converted = convert_with(&graph.config(), &|_| Some("wheels.rpm".to_string()));

    let bench = Bench::new(SCALAR);
    let component = bench
        .db
        .with_state(|s| s.get_component(ComponentId::new("wheels.rpm")).cloned())
        .unwrap();
    let _held = build_converted(&converted.source, &bench.db).unwrap();

    let out = bench
        .db
        .with_state(|s| s.get_component(ComponentId::new("derived")).cloned())
        .unwrap();
    let mut reader = crate::dynamic::NodeReader::from_disruptor(&out.wal, 8);

    // A microsecond timeline the legacy op would have turned into 0.25 s and
    // 0.5 s.
    for ts in [1_000_000i64, 1_250_000, 1_750_000] {
        component
            .push_buf(metor_proto::types::Timestamp(ts), &1.0f64.to_le_bytes())
            .unwrap();
    }

    let mut seen = Vec::new();
    for _ in 0..300 {
        while let Some(grant) = reader.try_next() {
            for (_, value) in grant.samples() {
                seen.push(f64::from_le_bytes(value.try_into().unwrap()));
            }
        }
        if seen.len() >= 3 {
            break;
        }
        stellarator::sleep(std::time::Duration::from_millis(5)).await;
    }

    assert_eq!(seen.len(), 3, "one publication per evaluation: {seen:?}");
    assert_eq!(seen[0], 0.0, "the guard covers the sample legacy skipped");
    for (at, want) in [(1usize, 250_000i64), (2, 500_000)] {
        assert_eq!(
            seen[at].to_bits(),
            (want as f64 * 1e-6).to_bits(),
            "interval {at} differs"
        );
    }
}

/// Positions come across, because the diagram is part of what was saved.
#[test]
fn positions_become_annotations() {
    let graph = one_op(from_component("wheels.rpm").node(
        "op",
        NodeSpec::Affine {
            op: AffineOp::Scale,
            k: k(2.0),
        },
    ));
    let converted = convert_with(&graph.config(), &|_| Some("wheels.rpm".to_string()));
    assert!(
        converted.source.contains("# @node(x=40, y=20)"),
        "{}",
        converted.source
    );
}

/// Anything the converter could not express rides at the top as a comment, so
/// a conversion is something to read before it is something to keep.
#[test]
fn what_could_not_convert_is_written_where_it_will_be_read() {
    let graph = one_op(from_component("wheels.rpm").node(
        "op",
        NodeSpec::Waveform {
            shape: Waveform::Sin,
            freq: 1.0,
            amplitude: 1.0,
            // A phase offset has no argument in the waveform functions.
            phase: 0.5,
            dtype: PrimType::F64,
            out_shape: smallvec![],
        },
    ));
    let converted = convert_with(&graph.config(), &|_| Some("wheels.rpm".to_string()));
    assert!(!converted.refused.is_empty());
    let annotated = converted.annotated();
    assert!(
        annotated.starts_with("# Converted from a node graph."),
        "{annotated}"
    );
    assert!(annotated.contains("phase offset"), "{annotated}");
}
