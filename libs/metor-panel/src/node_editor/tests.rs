//! Tests for the node editor foundation. The most important test in this
//! module is `node_id_matches_constructor` — drift between `compute_node_id`
//! and the runtime constructors silently breaks reconciliation.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;

use crate::dynamic::{
    BuildError, DynamicNode, DynamicRegistry, NodeId, hash_id, op_tag, ops,
};
use crate::node_editor::config::{NodeEditorConfig, Viewport};
use crate::node_editor::graph::{BuildState, EdgeEntry, NodeGraph, Position};
use crate::node_editor::spec::{NodeSpec, compute_node_id};
use crate::node_editor::validate::{EdgeVerdict, validate_connection};

/// Hash an arbitrary set of args the same way `crate::dynamic::node::hash_id`
/// does — used by tests that don't have a constructor to call (e.g. FromDb,
/// Persist, which need a DB).
fn hash_with(tag: &'static [u8], parents: &[NodeId], f: impl FnOnce(&mut std::collections::hash_map::DefaultHasher)) -> NodeId {
    hash_id(tag, parents, f)
}

/// Per-call unique tempdir path. Tests run in parallel so each call needs a
/// distinct directory.
fn unique_db_path(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "metor-panel-node-editor-{}-{}-{}",
        label,
        std::process::id(),
        n
    ))
}

// Constructors that spawn tasks need a stellarator runtime, so those go inside
// `#[stellarator::test]`. Variants that hash purely from args (FromDb, Persist)
// can be checked synchronously against `hash_id`.
#[stellarator::test]
async fn fixed_rate_id_matches() {
    let built = ops::clock::fixed_rate(123.5).unwrap();
    assert_eq!(built.id(), compute_node_id(&NodeSpec::FixedRate { hz: 123.5 }, &[]));
}

#[stellarator::test]
async fn clock_of_id_matches() {
    let src_clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = ops::generators::constant(src_clock, 1.0).unwrap();
    let src_id = src.id();
    let derived = ops::clock::clock_of(src);
    assert_eq!(derived.id(), compute_node_id(&NodeSpec::ClockOf, &[src_id]));
}

#[stellarator::test]
async fn waveform_id_matches() {
    use crate::dynamic::ops::generators::Waveform;
    for shape in [Waveform::Sin, Waveform::Cos, Waveform::Square, Waveform::Sawtooth] {
        let clock = ops::clock::fixed_rate(200.0).unwrap();
        let clock_id = clock.id();
        let built = ops::generators::waveform(clock, shape, 1.5, 2.0, 0.25).unwrap();
        assert_eq!(
            built.id(),
            compute_node_id(
                &NodeSpec::Waveform { shape, freq: 1.5, amplitude: 2.0, phase: 0.25 },
                &[clock_id],
            ),
            "shape={shape:?}",
        );
    }
}

#[stellarator::test]
async fn random_id_matches() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let clock_id = clock.id();
    let built = ops::generators::random(clock, 42).unwrap();
    assert_eq!(
        built.id(),
        compute_node_id(&NodeSpec::Random { seed: 42 }, &[clock_id]),
    );
}

#[stellarator::test]
async fn constant_id_matches() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let clock_id = clock.id();
    let built = ops::generators::constant(clock, 7.5).unwrap();
    assert_eq!(
        built.id(),
        compute_node_id(&NodeSpec::Constant { value: 7.5 }, &[clock_id]),
    );
}

#[stellarator::test]
async fn scale_id_matches() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = ops::generators::constant(clock, 1.0).unwrap();
    let src_id = src.id();
    let built = ops::derive::scale(src, 2.5).unwrap();
    assert_eq!(built.id(), compute_node_id(&NodeSpec::Scale { k: 2.5 }, &[src_id]));
}

#[stellarator::test]
async fn offset_id_matches() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = ops::generators::constant(clock, 1.0).unwrap();
    let src_id = src.id();
    let built = ops::derive::offset(src, -3.0).unwrap();
    assert_eq!(
        built.id(),
        compute_node_id(&NodeSpec::Offset { k: -3.0 }, &[src_id]),
    );
}

#[stellarator::test]
async fn abs_neg_log_ids_match() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let src = ops::generators::constant(clock, 1.0).unwrap();
    let src_id = src.id();
    let abs = ops::derive::abs(src.clone()).unwrap();
    let neg = ops::derive::neg(src.clone()).unwrap();
    let log = ops::derive::log(src).unwrap();
    assert_eq!(abs.id(), compute_node_id(&NodeSpec::Abs, &[src_id]));
    assert_eq!(neg.id(), compute_node_id(&NodeSpec::Neg, &[src_id]));
    assert_eq!(log.id(), compute_node_id(&NodeSpec::Log, &[src_id]));
}

#[stellarator::test]
async fn add_sub_mul_ids_match() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let a = ops::generators::constant(clock.clone(), 1.0).unwrap();
    let b = ops::generators::constant(clock, 2.0).unwrap();
    let aid = a.id();
    let bid = b.id();
    let add = ops::compose::add(a.clone(), b.clone()).unwrap();
    let sub = ops::compose::sub(a.clone(), b.clone()).unwrap();
    let mul = ops::compose::mul(a, b).unwrap();
    assert_eq!(add.id(), compute_node_id(&NodeSpec::Add, &[aid, bid]));
    assert_eq!(sub.id(), compute_node_id(&NodeSpec::Sub, &[aid, bid]));
    assert_eq!(mul.id(), compute_node_id(&NodeSpec::Mul, &[aid, bid]));
}

#[stellarator::test]
async fn mean_id_matches() {
    let clock = ops::clock::fixed_rate(100.0).unwrap();
    let a = ops::generators::constant(clock.clone(), 1.0).unwrap();
    let b = ops::generators::constant(clock.clone(), 2.0).unwrap();
    let c = ops::generators::constant(clock, 3.0).unwrap();
    let ids: Vec<NodeId> = vec![a.id(), b.id(), c.id()];
    let built = ops::compose::mean(vec![a, b, c]).unwrap();
    assert_eq!(built.id(), compute_node_id(&NodeSpec::Mean, &ids));
}

#[stellarator::test]
async fn resample_ids_match() {
    let slow = ops::clock::fixed_rate(50.0).unwrap();
    let fast = ops::clock::fixed_rate(400.0).unwrap();
    let src = ops::generators::constant(slow, 1.0).unwrap();
    let src_id = src.id();
    let fast_id = fast.id();
    let zoh = ops::resample::zoh(src.clone(), fast.clone()).unwrap();
    let lin = ops::resample::linear(src.clone(), fast.clone()).unwrap();
    let lat = ops::resample::latest_at(src, fast).unwrap();
    assert_eq!(zoh.id(), compute_node_id(&NodeSpec::Zoh, &[src_id, fast_id]));
    assert_eq!(lin.id(), compute_node_id(&NodeSpec::Linear, &[src_id, fast_id]));
    assert_eq!(lat.id(), compute_node_id(&NodeSpec::LatestAt, &[src_id, fast_id]));
}

#[test]
fn from_db_id_matches() {
    // from_db hashes the component_id only (no parents). Verify against a
    // direct hash_id call with the same fold the constructor uses.
    let cid: u64 = 0xCAFEBABEDEADBEEF;
    let direct = hash_with(op_tag::FROM_DB, &[], |h| {
        cid.hash(h);
    });
    let computed = compute_node_id(&NodeSpec::FromDb { component_id: cid }, &[]);
    assert_eq!(direct, computed);
}

#[test]
fn persist_id_matches() {
    // persist hashes the name only (parent provided externally).
    let parent = NodeId(0x1234_5678_9ABC_DEF0);
    let name = "speed".to_string();
    let direct = hash_with(op_tag::PERSIST, &[parent], |h| {
        name.hash(h);
    });
    let computed = compute_node_id(
        &NodeSpec::Persist { name: name.clone() },
        &[parent],
    );
    assert_eq!(direct, computed);
}

fn graph_with_clock_and_constant() -> NodeGraph {
    let mut graph = NodeGraph::new(/*owner_id*/ 1);
    graph.insert_node(
        "clk".into(),
        NodeSpec::FixedRate { hz: 100.0 },
        Position { x: 0.0, y: 0.0 },
    );
    graph.insert_node(
        "k".into(),
        NodeSpec::Constant { value: 1.0 },
        Position { x: 100.0, y: 0.0 },
    );
    graph.add_edge(EdgeEntry {
        source: "clk".into(),
        target: "k".into(),
        target_socket: 0,
    });
    graph
}

#[stellarator::test]
async fn reconcile_adds_and_removes_built_nodes() {
    let mut registry = DynamicRegistry::new();
    let mut graph = graph_with_clock_and_constant();
    let db_path = unique_db_path("reconcile");
    let db = Arc::new(metor_db::DB::create(db_path.clone()).unwrap());

    let alive = graph.rebuild_into(&db, &mut registry, None);
    assert_eq!(alive.len(), 2);
    assert_eq!(registry.len(), 2);

    // Remove the constant; only the clock should survive.
    graph.remove_node(&"k".into());
    let alive2 = graph.rebuild_into(&db, &mut registry, None);
    assert_eq!(alive2.len(), 1);
    // Manually reconcile to mimic what the coordinator would do.
    registry.reconcile(&alive2);
    assert_eq!(registry.len(), 1);

    let _ = std::fs::remove_dir_all(&db_path);
}

#[stellarator::test]
async fn dedup_across_two_graphs_share_node() {
    let mut registry = DynamicRegistry::new();
    let db_path = unique_db_path("dedup");
    let db = Arc::new(metor_db::DB::create(db_path.clone()).unwrap());

    // Two graphs, same fixed-rate clock spec — they should hash to the same id
    // and dedupe to one entry in the registry.
    let mut g1 = NodeGraph::new(1);
    let mut g2 = NodeGraph::new(2);
    g1.insert_node("a".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    g2.insert_node("b".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });

    let alive_a = g1.rebuild_into(&db, &mut registry, None);
    let alive_b = g2.rebuild_into(&db, &mut registry, None);
    assert_eq!(alive_a, alive_b, "same spec must produce the same NodeId");
    assert_eq!(registry.len(), 1, "deduped to a single live instance");

    // Simulate the coordinator's union: dropping one graph should keep the
    // node alive because the other still references it.
    let union: HashSet<NodeId> = alive_a.union(&alive_b).copied().collect();
    registry.reconcile(&union);
    assert_eq!(registry.len(), 1);
    // Drop g1's contribution — g2 still keeps the node.
    registry.reconcile(&alive_b);
    assert_eq!(registry.len(), 1);
    // Drop both — node goes away.
    registry.reconcile(&HashSet::new());
    assert_eq!(registry.len(), 0);

    let _ = std::fs::remove_dir_all(&db_path);
}

#[stellarator::test]
async fn rebuild_propagates_parent_failure_downstream() {
    // A `Persist` with an empty name still works (db inserts use the id, not
    // the name). To force a clock mismatch we use Mul of two different clocks.
    let mut registry = DynamicRegistry::new();
    let db_path = unique_db_path("pf");
    let db = Arc::new(metor_db::DB::create(db_path.clone()).unwrap());

    let mut g = NodeGraph::new(1);
    g.insert_node("clk_a".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    g.insert_node("clk_b".into(), NodeSpec::FixedRate { hz: 200.0 }, Position { x: 0.0, y: 50.0 });
    g.insert_node("a".into(), NodeSpec::Constant { value: 1.0 }, Position { x: 100.0, y: 0.0 });
    g.insert_node("b".into(), NodeSpec::Constant { value: 2.0 }, Position { x: 100.0, y: 50.0 });
    g.insert_node("mul".into(), NodeSpec::Mul, Position { x: 200.0, y: 25.0 });
    g.insert_node("scale".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 300.0, y: 25.0 });

    g.add_edge(EdgeEntry { source: "clk_a".into(), target: "a".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "clk_b".into(), target: "b".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "a".into(), target: "mul".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "b".into(), target: "mul".into(), target_socket: 1 });
    g.add_edge(EdgeEntry { source: "mul".into(), target: "scale".into(), target_socket: 0 });

    g.rebuild_into(&db, &mut registry, None);

    // Mul must fail with ClockMismatch; Scale must inherit ParentFailed.
    let mul = g.nodes.get(&gpui::SharedString::from("mul")).unwrap();
    assert!(
        matches!(mul.build, BuildState::Error(BuildError::ClockMismatch)),
        "expected ClockMismatch on mul, got {:?}",
        mul.build.error()
    );
    let scale = g.nodes.get(&gpui::SharedString::from("scale")).unwrap();
    assert!(
        matches!(scale.build, BuildState::Error(BuildError::ParentFailed)),
        "expected ParentFailed on scale, got {:?}",
        scale.build.error()
    );

    let _ = std::fs::remove_dir_all(&db_path);
}

#[test]
fn validate_rejects_clock_into_value_socket() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    g.insert_node("scale".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 100.0, y: 0.0 });
    let bad = EdgeEntry { source: "clk".into(), target: "scale".into(), target_socket: 0 };
    assert_eq!(validate_connection(&g, &bad), EdgeVerdict::BadSocket);
}

#[test]
fn validate_accepts_value_into_value_socket() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    g.insert_node("k".into(), NodeSpec::Constant { value: 1.0 }, Position { x: 100.0, y: 0.0 });
    g.insert_node("scale".into(), NodeSpec::Scale { k: 2.0 }, Position { x: 200.0, y: 0.0 });
    g.add_edge(EdgeEntry { source: "clk".into(), target: "k".into(), target_socket: 0 });
    let edge = EdgeEntry { source: "k".into(), target: "scale".into(), target_socket: 0 };
    assert_eq!(validate_connection(&g, &edge), EdgeVerdict::Ok);
}

#[test]
fn validate_rejects_self_loop() {
    let mut g = NodeGraph::new(1);
    g.insert_node("k".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 0.0, y: 0.0 });
    let edge = EdgeEntry { source: "k".into(), target: "k".into(), target_socket: 0 };
    assert_eq!(validate_connection(&g, &edge), EdgeVerdict::SelfLoop);
}

#[test]
fn add_edge_replaces_on_exact_arity_target() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    g.insert_node("a".into(), NodeSpec::Constant { value: 1.0 }, Position { x: 100.0, y: 0.0 });
    g.insert_node("b".into(), NodeSpec::Constant { value: 2.0 }, Position { x: 100.0, y: 50.0 });
    g.insert_node("scale".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 200.0, y: 0.0 });
    g.add_edge(EdgeEntry { source: "clk".into(), target: "a".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "clk".into(), target: "b".into(), target_socket: 0 });

    g.add_edge(EdgeEntry { source: "a".into(), target: "scale".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "b".into(), target: "scale".into(), target_socket: 0 });

    let to_scale: Vec<_> = g.edges.iter().filter(|e| e.target == "scale").collect();
    assert_eq!(to_scale.len(), 1, "second connect to same socket should replace");
    assert_eq!(to_scale[0].source, "b");
}

#[test]
fn variadic_edges_use_distinct_sockets() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    for i in 0..3 {
        let id = format!("k{i}");
        g.insert_node(id.clone().into(), NodeSpec::Constant { value: i as f64 }, Position { x: 100.0, y: i as f32 * 20.0 });
        g.add_edge(EdgeEntry { source: "clk".into(), target: id.into(), target_socket: 0 });
    }
    g.insert_node("mean".into(), NodeSpec::Mean, Position { x: 200.0, y: 0.0 });
    for i in 0..3 {
        g.add_edge(EdgeEntry { source: format!("k{i}").into(), target: "mean".into(), target_socket: i });
    }

    let to_mean: Vec<_> = g.edges.iter().filter(|e| e.target == "mean").collect();
    assert_eq!(to_mean.len(), 3, "variadic Mean keeps all parallel edges");

    // Replacing the edge at socket 1 swaps the parent there, leaves the others.
    g.add_edge(EdgeEntry { source: "k0".into(), target: "mean".into(), target_socket: 1 });
    let to_mean: Vec<_> = g.edges.iter().filter(|e| e.target == "mean").collect();
    assert_eq!(to_mean.len(), 3, "drop on existing socket replaces, doesn't append");
}

#[test]
fn variadic_remove_compacts_socket_indices() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 0.0 });
    for i in 0..3 {
        let id = format!("k{i}");
        g.insert_node(id.clone().into(), NodeSpec::Constant { value: i as f64 }, Position { x: 100.0, y: 0.0 });
        g.add_edge(EdgeEntry { source: "clk".into(), target: id.into(), target_socket: 0 });
    }
    g.insert_node("mean".into(), NodeSpec::Mean, Position { x: 200.0, y: 0.0 });
    for i in 0..3 {
        g.add_edge(EdgeEntry { source: format!("k{i}").into(), target: "mean".into(), target_socket: i });
    }

    g.remove_edge(&EdgeEntry { source: "k1".into(), target: "mean".into(), target_socket: 1 });

    let mut to_mean: Vec<_> = g.edges.iter().filter(|e| e.target == "mean").collect();
    to_mean.sort_by_key(|e| e.target_socket);
    let sockets: Vec<usize> = to_mean.iter().map(|e| e.target_socket).collect();
    assert_eq!(sockets, vec![0, 1], "sockets compacted after removal");
    assert_eq!(to_mean[0].source, "k0");
    assert_eq!(to_mean[1].source, "k2");
}

#[test]
fn validate_rejects_cycle() {
    let mut g = NodeGraph::new(1);
    g.insert_node("a".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 0.0, y: 0.0 });
    g.insert_node("b".into(), NodeSpec::Scale { k: 1.0 }, Position { x: 100.0, y: 0.0 });
    g.add_edge(EdgeEntry { source: "a".into(), target: "b".into(), target_socket: 0 });
    let edge = EdgeEntry { source: "b".into(), target: "a".into(), target_socket: 0 };
    assert_eq!(validate_connection(&g, &edge), EdgeVerdict::WouldCycle);
}

#[test]
fn variadic_parent_order_follows_target_socket() {
    let mut g = NodeGraph::new(1);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 100.0 }, Position { x: 0.0, y: 100.0 });
    g.insert_node("a".into(), NodeSpec::Constant { value: 1.0 }, Position { x: 50.0, y: 50.0 });
    g.insert_node("b".into(), NodeSpec::Constant { value: 2.0 }, Position { x: 50.0, y: 0.0 });
    g.insert_node("c".into(), NodeSpec::Constant { value: 3.0 }, Position { x: 50.0, y: 75.0 });
    g.insert_node("mean".into(), NodeSpec::Mean, Position { x: 200.0, y: 50.0 });
    for n in ["a", "b", "c"] {
        g.add_edge(EdgeEntry {
            source: "clk".into(),
            target: n.into(),
            target_socket: 0,
        });
    }
    // Wire to specific socket indices — physical positions are ignored.
    g.add_edge(EdgeEntry { source: "c".into(), target: "mean".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "a".into(), target: "mean".into(), target_socket: 1 });
    g.add_edge(EdgeEntry { source: "b".into(), target: "mean".into(), target_socket: 2 });

    let parents = g.parents_of(&"mean".into());
    let expected: Vec<gpui::SharedString> = vec!["c".into(), "a".into(), "b".into()];
    assert_eq!(parents, expected);
}

#[test]
fn config_round_trips_through_facet_json() {
    let mut g = NodeGraph::new(0xDEAD_BEEF);
    g.insert_node("clk".into(), NodeSpec::FixedRate { hz: 250.0 }, Position { x: 10.0, y: 20.0 });
    g.insert_node(
        "sin".into(),
        NodeSpec::Waveform {
            shape: crate::dynamic::ops::generators::Waveform::Sin,
            freq: 1.5,
            amplitude: 2.0,
            phase: 0.25,
        },
        Position { x: 100.0, y: 20.0 },
    );
    g.insert_node("scale".into(), NodeSpec::Scale { k: -0.5 }, Position { x: 200.0, y: 20.0 });
    g.insert_node("persist".into(), NodeSpec::Persist { name: "speed".into() }, Position { x: 300.0, y: 20.0 });
    g.add_edge(EdgeEntry { source: "clk".into(), target: "sin".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "sin".into(), target: "scale".into(), target_socket: 0 });
    g.add_edge(EdgeEntry { source: "scale".into(), target: "persist".into(), target_socket: 0 });

    let viewport = Viewport { x: -50.0, y: 75.0, zoom: 1.5 };
    let cfg = NodeEditorConfig::from_graph(&g, viewport.clone());
    let json = facet_json::to_string(&cfg).expect("serialize");
    let parsed: NodeEditorConfig = facet_json::from_str(&json).expect("parse");

    assert_eq!(parsed.owner_uuid, 0xDEAD_BEEF);
    assert_eq!(parsed.viewport.zoom, viewport.zoom);
    assert_eq!(parsed.nodes.len(), 4);
    assert_eq!(parsed.edges.len(), 3);

    let g2 = parsed.into_graph();
    assert_eq!(g2.nodes.len(), 4);
    assert_eq!(g2.edges.len(), 3);

    // Hash-identity: a hydrated graph must hash to the same NodeIds as the
    // original. Pick the deepest node and verify its computed_id matches.
    let original_ids: Vec<_> = ["clk", "sin", "scale", "persist"]
        .iter()
        .map(|n| {
            let entry = &g.nodes[&gpui::SharedString::from(*n)];
            let parents: Vec<_> = g
                .parents_of(&(*n).into())
                .iter()
                .map(|p| compute_node_id(&g.nodes[p].spec, &g.parents_of(p).iter().map(|pp| compute_node_id(&g.nodes[pp].spec, &[])).collect::<Vec<_>>()))
                .collect();
            compute_node_id(&entry.spec, &parents)
        })
        .collect();
    let new_ids: Vec<_> = ["clk", "sin", "scale", "persist"]
        .iter()
        .map(|n| {
            let entry = &g2.nodes[&gpui::SharedString::from(*n)];
            let parents: Vec<_> = g2
                .parents_of(&(*n).into())
                .iter()
                .map(|p| compute_node_id(&g2.nodes[p].spec, &g2.parents_of(p).iter().map(|pp| compute_node_id(&g2.nodes[pp].spec, &[])).collect::<Vec<_>>()))
                .collect();
            compute_node_id(&entry.spec, &parents)
        })
        .collect();
    assert_eq!(original_ids, new_ids);
}

#[stellarator::test]
async fn unchanged_subtree_keeps_arc_alive() {
    let mut registry = DynamicRegistry::new();
    let db_path = unique_db_path("idem");
    let db = Arc::new(metor_db::DB::create(db_path.clone()).unwrap());

    let mut g = graph_with_clock_and_constant();
    g.rebuild_into(&db, &mut registry, None);
    let arc_before: Arc<dyn DynamicNode> =
        g.nodes[&gpui::SharedString::from("k")].build.as_built().unwrap().clone();

    // Add an unrelated node; rebuild; the existing constant must keep its Arc.
    g.insert_node(
        "extra".into(),
        NodeSpec::FixedRate { hz: 50.0 },
        Position { x: 0.0, y: 100.0 },
    );
    g.rebuild_into(&db, &mut registry, None);
    let arc_after: Arc<dyn DynamicNode> =
        g.nodes[&gpui::SharedString::from("k")].build.as_built().unwrap().clone();
    assert!(
        Arc::ptr_eq(&arc_before, &arc_after),
        "unchanged node should retain the same Arc across rebuilds",
    );

    let _ = std::fs::remove_dir_all(&db_path);
}
