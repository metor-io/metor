use std::collections::BTreeSet;

use metor_fsw_2::ir::{
    ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, IR_VERSION, ParamSource, ScopeSpec, SlotSpec,
    SystemSpec, Wiring,
};

use crate::graph_layout::Direction;

use super::layout::{GraphNodeKind, layout};

fn base_wiring() -> Wiring {
    Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
            namespace: None,
        },
        artifacts: Vec::new(),
        states: Vec::new(),
        systems: Vec::new(),
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
    }
}

fn system(name: &str, scope: Option<usize>) -> SystemSpec {
    SystemSpec {
        name: name.to_string(),
        ty: Some("Demo".to_string()),
        artifact: None,
        params: ParamSource::None,
        process: false,
        src: None,
        scope,
        attach: None,
    }
}

fn frame_edge(from: &str, to: &str, delayed: bool) -> EdgeSpec {
    EdgeSpec {
        from: from.to_string(),
        out: "out".to_string(),
        to: to.to_string(),
        in_: "in".to_string(),
        delayed,
        kind: EdgeKind::Frame,
        src: None,
    }
}

fn no_collapse() -> BTreeSet<String> {
    BTreeSet::new()
}

/// A chain a → b → c lays out over three consecutive layers.
#[test]
fn frame_chain_layers_left_to_right() {
    let mut w = base_wiring();
    w.systems = vec![system("a", None), system("b", None), system("c", None)];
    w.edges = vec![frame_edge("a", "b", false), frame_edge("b", "c", false)];

    let g = layout(&w, &no_collapse(), Direction::LeftRight);
    assert_eq!(g.node("a").unwrap().layer, 0);
    assert_eq!(g.node("b").unwrap().layer, 1);
    assert_eq!(g.node("c").unwrap().layer, 2);
    // Layers advance in +x.
    assert!(g.node("a").unwrap().pos.0 < g.node("b").unwrap().pos.0);
    assert!(g.node("b").unwrap().pos.0 < g.node("c").unwrap().pos.0);
}

/// A cycle a → b → a where the back edge is delayed: the delayed edge is
/// excluded from layering, so the remaining skeleton is acyclic and layers
/// deterministically. Re-running yields identical layers.
#[test]
fn cycle_broken_by_delayed_edge_is_deterministic() {
    let mut w = base_wiring();
    w.systems = vec![system("a", None), system("b", None)];
    w.edges = vec![frame_edge("a", "b", false), frame_edge("b", "a", true)];

    let g1 = layout(&w, &no_collapse(), Direction::LeftRight);
    assert_eq!(g1.node("a").unwrap().layer, 0);
    assert_eq!(g1.node("b").unwrap().layer, 1);

    let g2 = layout(&w, &no_collapse(), Direction::LeftRight);
    for id in ["a", "b"] {
        assert_eq!(g1.node(id).unwrap().layer, g2.node(id).unwrap().layer);
        assert_eq!(g1.node(id).unwrap().pos, g2.node(id).unwrap().pos);
    }
    // Both edges survive as drawable edges (the delayed one is drawn, just not
    // layered).
    assert_eq!(g1.edges.len(), 2);
    assert!(g1.edges.iter().any(|e| e.delayed));
}

/// Collapsing a scope replaces its members with one group node and reroutes
/// edges crossing the boundary to that group.
#[test]
fn collapsing_scope_aggregates_members_and_reroutes_edges() {
    let mut w = base_wiring();
    w.scopes = vec![ScopeSpec {
        path: "adcs".to_string(),
        parent: None,
        src: None,
    }];
    // inner_a, inner_b live in scope 0; outside is unscoped.
    w.systems = vec![
        system("inner_a", Some(0)),
        system("inner_b", Some(0)),
        system("outside", None),
    ];
    w.edges = vec![
        frame_edge("inner_a", "inner_b", false), // internal to the scope
        frame_edge("inner_b", "outside", false), // crosses the boundary
    ];

    // Expanded: three real nodes, both edges.
    let expanded = layout(&w, &no_collapse(), Direction::LeftRight);
    assert_eq!(expanded.nodes.len(), 3);
    assert_eq!(expanded.edges.len(), 2);

    // Collapsed: one group node ("adcs") + "outside".
    let mut collapsed = BTreeSet::new();
    collapsed.insert("adcs".to_string());
    let g = layout(&w, &collapsed, Direction::LeftRight);

    assert!(g.node("inner_a").is_none());
    assert!(g.node("inner_b").is_none());
    let group = g.node("adcs").expect("group node exists");
    assert_eq!(group.kind, GraphNodeKind::ScopeGroup);
    assert!(g.node("outside").is_some());

    // The internal edge is dropped; the crossing edge now runs adcs → outside.
    assert_eq!(g.edges.len(), 1);
    let e = &g.edges[0];
    assert_eq!(e.from_node.as_str(), "adcs");
    assert_eq!(e.to_node.as_str(), "outside");
}

/// The coordinator node appears only when an edge references it.
#[test]
fn coordinator_node_only_when_referenced() {
    let mut w = base_wiring();
    w.systems = vec![system("slotless", None)];
    let without = layout(&w, &no_collapse(), Direction::LeftRight);
    assert!(without.node("coordinator").is_none());

    w.slots = vec![SlotSpec {
        name: "seq".to_string(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: Vec::new(),
        initial: None,
        process: false,
        src: None,
        scope: None,
    }];
    w.edges = vec![EdgeSpec {
        from: "coordinator".to_string(),
        out: "SlotControlOut".to_string(),
        to: "seq".to_string(),
        in_: "SlotControlIn".to_string(),
        delayed: false,
        kind: EdgeKind::Frame,
        src: None,
    }];
    let with = layout(&w, &no_collapse(), Direction::LeftRight);
    let coord = with.node("coordinator").expect("coordinator referenced");
    assert_eq!(coord.kind, GraphNodeKind::Coordinator);
}

/// Top→bottom direction advances the chain in +y instead of +x.
#[test]
fn top_bottom_flows_down() {
    let mut w = base_wiring();
    w.systems = vec![system("a", None), system("b", None), system("c", None)];
    w.edges = vec![frame_edge("a", "b", false), frame_edge("b", "c", false)];

    let g = layout(&w, &no_collapse(), Direction::TopBottom);
    assert!(g.node("a").unwrap().pos.1 < g.node("b").unwrap().pos.1);
    assert!(g.node("b").unwrap().pos.1 < g.node("c").unwrap().pos.1);
}

/// A config document saved before the direction field existed still loads,
/// defaulting to left→right.
#[test]
fn config_without_direction_defaults() {
    let json = r#"{"viewport":{"x":1.0,"y":2.0},"overrides":[],"collapsed":[]}"#;
    let cfg: super::config::SystemGraphConfig = serde_json::from_str(json).expect("deserialize");
    assert_eq!(cfg.direction, Direction::LeftRight);
    assert_eq!(cfg.viewport.x, 1.0);
}

/// Message edges do not drive layering (excluded from the skeleton) but are
/// still drawn.
#[test]
fn msg_edges_excluded_from_layering() {
    let mut w = base_wiring();
    w.systems = vec![system("a", None), system("b", None)];
    w.edges = vec![EdgeSpec {
        from: "a".to_string(),
        out: "Cmd".to_string(),
        to: "b".to_string(),
        in_: "Cmd".to_string(),
        delayed: false,
        kind: EdgeKind::Msg,
        src: None,
    }];
    let g = layout(&w, &no_collapse(), Direction::LeftRight);
    // No frame skeleton edge, so both stay at layer 0.
    assert_eq!(g.node("a").unwrap().layer, 0);
    assert_eq!(g.node("b").unwrap().layer, 0);
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].kind, EdgeKind::Msg);
}
