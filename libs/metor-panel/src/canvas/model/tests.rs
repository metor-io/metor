//! The model is the part of the canvas that has no window in it, so this is
//! where the projection rules are pinned: what becomes a card, what becomes an
//! edge, and where a card sits when nobody has said.

use std::collections::BTreeSet;

use metor_expr::{CompSchema, Dtype, FrameSchema, Manifest, Resolver, Ty};

use super::*;

/// A host that knows a few components, one of which a native instance
/// publishes.
struct Table;

impl Resolver for Table {
    fn component(&self, path: &str) -> Option<CompSchema> {
        let ty = match path {
            "wheels.rpm" | "nav.attitude.rate" => Ty::F64,
            "nav.attitude.omega_b" => Ty::Tensor {
                dtype: Dtype::F64,
                shape: vec![3],
            },
            _ => return None,
        };
        Some(CompSchema { ty })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        ["wheels.rpm", "nav.attitude.rate", "nav.attitude.omega_b"]
            .into_iter()
            .filter(|path| path.rsplit('.').next() == Some(name))
            .map(str::to_string)
            .collect()
    }

    fn frame(&self, _: &str) -> Option<FrameSchema> {
        None
    }
}

fn compile(source: &str) -> Manifest {
    metor_expr::compile_module(source, &Table)
        .unwrap_or_else(|d| panic!("expected {source:?} to compile:\n{d}"))
        .manifest
}

fn model_of(source: &str) -> Model {
    build(
        Some(&compile(source)),
        None,
        &BTreeSet::new(),
        Direction::LeftRight,
        &Overrides::new(),
    )
}

#[test]
fn every_declaration_is_a_card_and_every_binding_is_an_edge() {
    let model = model_of(
        "scaled = wheels.rpm * 2.0\nslow = resample_zoh(scaled, 10.0)\nfinal = slow + 1.0\n",
    );
    assert_eq!(
        model
            .cards
            .iter()
            .map(|c| c.id.as_ref())
            .collect::<Vec<_>>(),
        vec!["scaled", "slow", "final"]
    );
    assert!(model.cards.iter().all(|c| c.origin.is_python()));

    // `wheels.rpm` has no card here, so the only edges are the two in-program
    // ones — a binding to a component is a label, not a wire.
    let wires: Vec<(&str, &str)> = model
        .edges
        .iter()
        .map(|e| (e.from.as_ref(), e.to.as_ref()))
        .collect();
    assert_eq!(wires, vec![("scaled", "slow"), ("slow", "final")]);
    assert!(model.edges.iter().all(|e| e.consumer_port == Some(0)));
}

/// A stage is a card like any other, and says what it does.
#[test]
fn a_stage_reads_as_a_card() {
    let model = model_of("slow = resample_linear(wheels.rpm, 5.0)\n");
    let card = model.card("slow").unwrap();
    assert_eq!(card.subtitle.as_ref(), "linear · 5 Hz");
    assert_eq!(card.outputs[0].name, "slow");
    assert!(matches!(
        card.origin,
        Origin::Python {
            decl: Decl::Stage(0),
            ..
        }
    ));
}

/// A source system says its rate where an input-driven one says its frame.
#[test]
fn a_source_system_says_so() {
    let model = model_of("@system(rate=50.0)\ndef sig() -> f64:\n    return sine(1.0, 1.0)\n");
    let card = model.card("sig").unwrap();
    assert_eq!(card.subtitle.as_ref(), "50 Hz source");
    assert!(card.inputs.is_empty());
    assert_eq!(card.outputs.len(), 1);
}

/// Declaration order sets the columns and a card that has never been placed
/// gets a deterministic one — a program nobody has arranged still reads.
#[test]
fn unplaced_cards_lay_out_by_depth() {
    let model = model_of("a = wheels.rpm * 2.0\nb = a + 1.0\nc = b + a\n");
    let x = |id: &str| model.card(id).unwrap().pos.0;
    assert!(x("a") < x("b"), "a feeds b");
    assert!(x("b") < x("c"), "b feeds c");

    // And a placed one keeps exactly what its source says.
    let model = model_of("a = wheels.rpm * 2.0  # @node(x=500, y=300)\n");
    assert_eq!(model.card("a").unwrap().pos, (500.0, 300.0));
}

/// The cross-source rule, which is the whole point of one canvas: a component
/// is published by the instance its name starts with, so a Python port reading
/// it is an edge from that native card.
#[test]
fn a_component_binds_to_the_instance_that_publishes_it() {
    let mut model = Model::default();
    model.cards.push(Card {
        id: "nav".into(),
        subtitle: "estimator".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        origin: Origin::Native {
            kind: GraphNodeKind::System,
            source_index: Some(0),
        },
        pos: (0.0, 0.0),
        height: 62.0,
    });
    let manifest = compile("rate = nav.attitude.rate * 2.0\n");
    let declarations = manifest.declarations();
    let first = model.cards.len();
    python_cards(&manifest, 62.0, &mut model);
    assert_eq!(model.cards.len(), first + 1);

    let edge = model
        .edges
        .iter()
        .find(|e| e.to == "rate")
        .expect("the port reads a native instance");
    assert_eq!(edge.from.as_ref(), "nav");
    assert_eq!(
        edge.from_port.as_ref(),
        "attitude",
        "the frame is the second segment, the field the third"
    );
    assert_eq!(declarations.len(), 1);

    // The Python half starts below the native one, so nothing overlaps on
    // first open.
    assert!(model.card("rate").unwrap().pos.1 > 62.0);
}

/// A component nothing on the canvas publishes is a label on the port, not a
/// dangling wire.
#[test]
fn an_unknown_component_is_not_an_edge() {
    let model = model_of("scaled = wheels.rpm * 2.0\n");
    assert!(model.edges.is_empty());
    assert_eq!(model.card("scaled").unwrap().inputs[0].detail, "wheels.rpm");
}

/// A native card sits where the IR's declaration-site layout says, unless a
/// hand-drag overrides it; without either, the auto-layout decides.
#[test]
fn ir_layout_places_a_native_card_and_overrides_still_win() {
    use metor_fsw_2::ir::{
        ClockSpec, CoordinatorSpec, IR_VERSION, ParamSource, SystemSpec, Wiring,
    };
    let wiring = Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
            namespace: None,
            wasm_fuel_per_poll: None,
            wasm_memory_limit_bytes: None,
        },
        artifacts: Vec::new(),
        states: Vec::new(),
        systems: vec![
            SystemSpec {
                name: "placed".into(),
                ty: Some("Demo".into()),
                artifact: None,
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
                attach: None,
                layout: Some((420.0, 180.0)),
            },
            SystemSpec {
                name: "auto".into(),
                ty: Some("Demo".into()),
                artifact: None,
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
                attach: None,
                layout: None,
            },
        ],
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
        program: None,
    };
    let model = build(
        None,
        Some(&wiring),
        &BTreeSet::new(),
        Direction::LeftRight,
        &Overrides::new(),
    );
    assert_eq!(model.card("placed").unwrap().pos, (420.0, 180.0));
    assert_ne!(model.card("auto").unwrap().pos, (420.0, 180.0));

    let mut overrides = Overrides::new();
    overrides.insert("placed".into(), (10.0, 20.0));
    let model = build(
        None,
        Some(&wiring),
        &BTreeSet::new(),
        Direction::LeftRight,
        &overrides,
    );
    assert_eq!(
        model.card("placed").unwrap().pos,
        (10.0, 20.0),
        "a hand-drag beats the declaration-site position"
    );
}
