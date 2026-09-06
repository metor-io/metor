//! Runtime tests for compiled streaming systems.

use std::sync::Arc;
use std::time::Duration;

use metor_db::{ComponentSchema, DB};
use metor_expr::Ty;
use metor_proto::types::{ComponentId, PrimType, Timestamp};

use super::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::node::{DynamicNode, DynamicNodeExt, NodeReader};
use crate::dynamic::ops;
use crate::dynamic::resolver::DbResolver;

/// A db holding the components a test's expression names.
pub(super) struct Bench {
    pub(super) db: DB,
    _temp: tempfile::TempDir,
}

impl Bench {
    pub(super) fn new(components: &[(&str, PrimType, &[usize])]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        for (name, prim, dim) in components {
            let id = ComponentId::new(name);
            db.with_state_mut(|state| {
                state.insert_component(id, ComponentSchema::new(*prim, *dim), &db.path)
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

    pub(super) fn id(&self, name: &str) -> ComponentId {
        ComponentId::new(name)
    }

    pub(super) fn push(&self, name: &str, ts: i64, values: &[f64]) {
        let component = self
            .db
            .with_state(|s| s.get_component(self.id(name)).cloned())
            .unwrap();
        let prim = component.schema.prim_type;
        let mut bytes = Vec::new();
        for v in values {
            crate::dynamic::tensor::write_f64_as(&mut bytes, prim, *v);
        }
        component.push_buf(Timestamp(ts), &bytes).unwrap();
    }

    pub(super) fn source(&self, name: &str) -> Arc<dyn DynamicNode> {
        ops::db_source::from_db(&self.db, self.id(name)).unwrap()
    }

    pub(super) fn compile(&self, source: &str) -> Arc<Compiled> {
        let resolver = DbResolver::snapshot(&self.db);
        match Compiled::module(source, &resolver) {
            Ok(compiled) => Arc::new(compiled),
            Err(diags) => panic!("expected {source:?} to compile, got:\n{diags}"),
        }
    }
}

/// Start watching a node.
///
/// Always before the samples are pushed: a disruptor reader begins at the
/// current write position, so one made afterwards never sees what it missed.
pub(super) fn watch(node: &Arc<dyn DynamicNode>) -> NodeReader {
    node.subscribe()
}

/// Read `count` scalars off a watched node, giving up rather than hanging.
pub(super) async fn take(reader: &mut NodeReader, count: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..300 {
        while let Some(grant) = reader.try_next() {
            for (_, value) in grant.samples() {
                out.push(f64::from_le_bytes(value.try_into().unwrap()));
            }
        }
        if out.len() >= count {
            out.truncate(count);
            return out;
        }
        settle().await;
    }
    panic!("expected {count} samples, saw {}: {out:?}", out.len());
}

/// Let the spawned tasks run. Nothing here is timing-dependent — this is only
/// how a test yields to the nodes it is driving.
pub(super) async fn settle() {
    stellarator::sleep(Duration::from_millis(5)).await;
}

/// Build one system over db-backed ports, in the manifest's port order.
pub(super) fn wire(bench: &Bench, compiled: &Arc<Compiled>, index: usize) -> program::System {
    let ports: Vec<program::PortSource> = compiled.manifest.systems[index]
        .inputs
        .iter()
        .map(|port| match &port.bindings[0] {
            metor_expr::Binding::Component(path) => program::PortSource::live(bench.source(path)),
            other => panic!("this helper wires components, not {other:?}"),
        })
        .collect();
    program::system(compiled, index, ports, DEFAULT_FUEL, None).unwrap()
}

#[stellarator::test]
async fn an_expression_publishes_what_it_computes() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("scaled = wheels.rpm * 2.0 + 1.0\n");
    let system = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    for step in 0..4 {
        bench.push("wheels.rpm", step + 1, &[f64::from(step as i32)]);
    }
    assert_eq!(take(&mut out, 4).await, vec![1.0, 3.0, 5.0, 7.0]);
    assert_eq!(system.health.fault(), None);
}

/// A component's own element type is not the language's. Everything numeric
/// reads as `f64`, which is what lets one expression span an `f32` channel
/// and an `i32` counter without saying so.
#[stellarator::test]
async fn narrower_components_widen_on_the_way_in() {
    let bench = Bench::new(&[
        ("sensor.temp", PrimType::F32, &[]),
        ("counter.ticks", PrimType::I32, &[]),
    ]);
    let compiled = bench.compile("total = sensor.temp + counter.ticks\n");
    let system = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    bench.push("counter.ticks", 1, &[10.0]);
    settle().await;
    bench.push("sensor.temp", 2, &[0.5]);
    assert_eq!(take(&mut out, 1).await, vec![10.5]);
}

/// The run rule: fire on the driving input, read the latest of the rest, and
/// skip the cycle while anything else has never published.
#[stellarator::test]
async fn a_system_fires_on_its_driving_input_and_holds_the_rest() {
    let bench = Bench::new(&[
        ("adcs.rate", PrimType::F64, &[]),
        ("wheels.rpm", PrimType::F64, &[]),
    ]);
    let compiled = bench.compile(
        "@system(\"adcs.rate\", \"wheels.rpm\")\ndef both(rate, rpm) -> f64:\n    return rate * 100.0 + rpm\n",
    );
    let system = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    // `wheels.rpm` has never published, so these are skipped rather than
    // evaluated against a zero nobody wrote.
    bench.push("adcs.rate", 1, &[1.0]);
    bench.push("adcs.rate", 2, &[2.0]);
    settle().await;

    bench.push("wheels.rpm", 3, &[7.0]);
    settle().await;
    bench.push("adcs.rate", 4, &[3.0]);
    bench.push("adcs.rate", 5, &[4.0]);
    // Zero-order hold: one rpm sample serves both rate samples.
    assert_eq!(take(&mut out, 2).await, vec![307.0, 407.0]);

    bench.push("wheels.rpm", 6, &[9.0]);
    settle().await;
    bench.push("adcs.rate", 7, &[5.0]);
    assert_eq!(take(&mut out, 1).await, vec![509.0]);
}

/// The output frame is several fields of several types, and each becomes an
/// ordinary value node with the schema it really has.
#[stellarator::test]
async fn every_output_field_becomes_its_own_node() {
    let bench = Bench::new(&[("imu.omega", PrimType::F64, &[3])]);
    let compiled = bench.compile(
        "class Omega(Frame):\n\
         \x20   omega: Tensor[f64, 3]\n\
         \n\
         class Rate(Frame):\n\
         \x20   magnitude: f64\n\
         \x20   spinning: bool\n\
         \n\
         @system(bind={\"o\": \"imu\"})\n\
         def rate(o: Omega) -> Rate:\n\
         \x20   return Rate(magnitude=o.omega @ o.omega, spinning=o.omega[0] > 0.5)\n",
    );
    let system = &compiled.manifest.systems[0];
    assert_eq!(system.publishes, vec!["rate.magnitude", "rate.spinning"]);
    assert_eq!(system.output.fields[0].ty, Ty::F64);
    assert_eq!(system.output.fields[1].ty, Ty::Bool);

    let running = wire(&bench, &compiled, 0);
    let magnitude = program::field(&compiled, 0, 0, running.node.clone()).unwrap();
    let spinning = program::field(&compiled, 0, 1, running.node.clone()).unwrap();
    assert_eq!(
        spinning.value_type().schema().unwrap(),
        &ComponentSchema::new(PrimType::Bool, &[][..])
    );

    let mut flags = watch(&spinning);
    let mut out = watch(&magnitude);
    bench.push("imu.omega", 1, &[1.0, 2.0, 3.0]);
    assert_eq!(take(&mut out, 1).await, vec![14.0]);
    let grant = flags.next().await;
    assert_eq!(grant.sample_at(0).1, &[1u8]);
}

/// An output field is a value node like any other, so publishing it is the
/// existing `persist` and nothing new.
#[stellarator::test]
async fn an_output_field_persists_as_a_real_component() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("doubled = wheels.rpm * 2.0\n");
    let running = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, running.node.clone()).unwrap();
    let name = compiled.manifest.systems[0].publishes[0].clone();
    let published = ops::persist::persist(&bench.db, name.clone(), field).unwrap();
    let mut out = watch(&published);

    bench.push("wheels.rpm", 1, &[21.0]);
    assert_eq!(take(&mut out, 1).await, vec![42.0]);
    assert!(
        bench
            .db
            .with_state(|s| s.get_component(bench.id(&name)).is_some())
    );
}

/// A runaway loop burns its grant and the system parks — it does not stall the
/// panel, and it does not take anything else down with it.
#[stellarator::test]
async fn a_runaway_body_parks_the_system() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile(
        "@system(\"wheels.rpm\")\ndef spin(rpm) -> f64:\n    x = rpm\n    while True:\n        x = x + 1.0\n    return x\n",
    );
    let system = wire(&bench, &compiled, 0);
    bench.push("wheels.rpm", 1, &[1.0]);
    for _ in 0..200 {
        if system.health.fault().is_some() {
            break;
        }
        settle().await;
    }
    let fault = system.health.fault().expect("a runaway body must park");
    assert!(fault.contains("fuel"), "{fault}");

    // Its inputs keep being read, so nothing upstream backs up behind it.
    for step in 2..40 {
        bench.push("wheels.rpm", step, &[1.0]);
    }
    settle().await;
}

/// The rebuild contract: an edit swaps one instance and the filter picks up
/// where it left off, because state is keyed by what it means, not by which
/// instance held it.
#[stellarator::test]
async fn a_rebuild_carries_state_across_the_swap() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let lowpass = |gain: &str| {
        format!(
            "class Lp(State):\n\
             \x20   filtered: f64 = 0.0\n\
             \n\
             @system(\"wheels.rpm\")\n\
             def lp(rpm, s: Lp) -> f64:\n\
             \x20   s.filtered = {gain} * rpm + (1.0 - {gain}) * s.filtered\n\
             \x20   return s.filtered\n"
        )
    };

    let first = bench.compile(&lowpass("0.5"));
    let running = wire(&bench, &first, 0);
    let field = program::field(&first, 0, 0, running.node.clone()).unwrap();
    let mut out = watch(&field);
    bench.push("wheels.rpm", 1, &[100.0]);
    assert_eq!(take(&mut out, 1).await, vec![50.0]);
    let carried = running.state.snapshot();
    assert_eq!(carried.entries.len(), 1);

    // A gain edit changes the body, so this system rebuilds — and its memory
    // of the last sample has to come with it.
    let second = bench.compile(&lowpass("0.25"));
    assert_ne!(
        first.system_hash(0, &[]),
        second.system_hash(0, &[]),
        "an edited body must hash differently"
    );
    let rebuilt = program::system(
        &second,
        0,
        vec![program::PortSource::live(bench.source("wheels.rpm"))],
        DEFAULT_FUEL,
        Some(&carried),
    )
    .unwrap();
    let field = program::field(&second, 0, 0, rebuilt.node.clone()).unwrap();
    let mut out = watch(&field);
    bench.push("wheels.rpm", 2, &[100.0]);
    assert_eq!(take(&mut out, 1).await, vec![0.25 * 100.0 + 0.75 * 50.0]);
}

/// An edit to one system leaves the others' identities alone, which is what
/// makes "rebuild only what changed" a property rather than an intention.
#[stellarator::test]
async fn an_edit_only_changes_the_system_it_touched() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let before = bench.compile("a = wheels.rpm * 2.0\nb = wheels.rpm * 3.0\n");
    let after = bench.compile("a = wheels.rpm * 2.0\nb = wheels.rpm * 4.0\n");
    let port = vec![bench.source("wheels.rpm").id()];
    assert_eq!(before.system_hash(0, &port), after.system_hash(0, &port));
    assert_ne!(before.system_hash(1, &port), after.system_hash(1, &port));
}

#[stellarator::test]
async fn editing_a_helper_changes_each_dependent_system() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let before =
        bench.compile("def gain(x: f64) -> f64:\n    return x * 2.0\n\nout = gain(wheels.rpm)\n");
    let after =
        bench.compile("def gain(x: f64) -> f64:\n    return x * 3.0\n\nout = gain(wheels.rpm)\n");
    let port = vec![bench.source("wheels.rpm").id()];
    assert_ne!(before.system_hash(0, &port), after.system_hash(0, &port));
}

#[stellarator::test]
async fn source_positions_and_canvas_layout_do_not_change_system_identity() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let before = bench.compile("# short\nout = wheels.rpm * 2.0  # @node(x=1, y=2)\n");
    let after = bench
        .compile("# a longer unrelated comment\nout = wheels.rpm * 2.0  # @node(x=30, y=40)\n");
    let port = vec![bench.source("wheels.rpm").id()];
    assert_eq!(before.system_hash(0, &port), after.system_hash(0, &port));
}

/// A channel that has published and gone quiet must still show its value.
///
/// A disruptor reader begins at the write head, so without seeding, a system
/// over a slow channel waits for a sample that may be minutes off and the plot
/// of it looks broken rather than idle. The host hands the last committed
/// sample in, and the system fires from it once.
#[stellarator::test]
async fn a_quiet_channel_still_yields_its_current_value() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("scaled = wheels.rpm * 2.0\n");

    // Everything this channel will ever say, said before the system exists.
    bench.push("wheels.rpm", 1, &[21.0]);
    stellarator::sleep(Duration::from_millis(20)).await;

    let id = bench.id("wheels.rpm");
    let seed = program::latest_sample(&bench.db, id);
    assert!(
        seed.is_some(),
        "the component must have history to seed from"
    );

    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: bench.source("wheels.rpm"),
            seed,
        }],
        DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    // Nothing more is ever pushed, and a value arrives anyway.
    assert_eq!(take(&mut out, 1).await, vec![42.0]);
}

#[stellarator::test]
async fn a_fault_in_the_opening_sample_parks_the_system() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("out = [1.0, 2.0][int(wheels.rpm)]\n");
    bench.push("wheels.rpm", 1, &[2.0]);
    settle().await;
    let id = bench.id("wheels.rpm");
    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: bench.source("wheels.rpm"),
            seed: program::latest_sample(&bench.db, id),
        }],
        DEFAULT_FUEL,
        None,
    )
    .unwrap();
    for _ in 0..200 {
        if system.health.fault().is_some() {
            break;
        }
        settle().await;
    }
    assert!(system.health.fault().is_some());
}

/// The same seeding, for an input the system merely holds: a system whose
/// driving channel is live must not sit skipping cycles because a *second*
/// channel published before it existed.
#[stellarator::test]
async fn a_held_input_is_seeded_from_what_it_already_published() {
    let bench = Bench::new(&[
        ("adcs.rate", PrimType::F64, &[]),
        ("wheels.rpm", PrimType::F64, &[]),
    ]);
    let compiled = bench.compile(
        "@system(\"adcs.rate\", \"wheels.rpm\")\ndef both(rate, rpm) -> f64:\n    return rate * 100.0 + rpm\n",
    );

    // The held input speaks once, before anything is watching it.
    bench.push("wheels.rpm", 1, &[7.0]);
    stellarator::sleep(Duration::from_millis(20)).await;

    let ports: Vec<program::PortSource> = ["adcs.rate", "wheels.rpm"]
        .iter()
        .map(|name| program::PortSource {
            node: bench.source(name),
            seed: program::latest_sample(&bench.db, bench.id(name)),
        })
        .collect();
    let system = program::system(&compiled, 0, ports, DEFAULT_FUEL, None).unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    // Without the seed this cycle would be skipped as "rpm has never
    // published" and nothing would ever come out.
    bench.push("adcs.rate", 2, &[3.0]);
    assert_eq!(take(&mut out, 1).await, vec![307.0]);
}

/// Seed *then* tail: the case the seeding work did not cover.
///
/// An expression over a channel that had already published must show its
/// current value immediately AND keep following the channel afterwards. One
/// point and then silence is the symptom this pins.
#[stellarator::test]
async fn a_seeded_system_keeps_following_its_input() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("scaled = wheels.rpm + 1.0\n");

    // History, before anything is watching.
    bench.push("wheels.rpm", 1, &[10.0]);
    stellarator::sleep(Duration::from_millis(20)).await;

    let id = bench.id("wheels.rpm");
    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: bench.source("wheels.rpm"),
            seed: program::latest_sample(&bench.db, id),
        }],
        DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    // The seed, then a live tail.
    for step in 2..=5 {
        bench.push("wheels.rpm", step, &[f64::from(step as i32) * 10.0]);
    }
    assert_eq!(
        take(&mut out, 5).await,
        vec![11.0, 21.0, 31.0, 41.0, 51.0],
        "the seed must be followed by every later sample"
    );
    assert_eq!(system.health.fault(), None);
}

/// The same, through the whole panel path: the expression's own hidden
/// component must accumulate the seed and the tail, with advancing
/// timestamps, because that is what a plot reads.
#[stellarator::test]
async fn a_seeded_expression_accumulates_history() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("scaled = wheels.rpm + 1.0\n");

    bench.push("wheels.rpm", 1, &[10.0]);
    stellarator::sleep(Duration::from_millis(20)).await;

    let id = bench.id("wheels.rpm");
    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: bench.source("wheels.rpm"),
            seed: program::latest_sample(&bench.db, id),
        }],
        DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let published = ops::persist::persist(&bench.db, "scaled".to_string(), field).unwrap();
    let mut out = watch(&published);

    for step in 2..=5 {
        bench.push("wheels.rpm", step, &[f64::from(step as i32) * 10.0]);
    }
    assert_eq!(take(&mut out, 5).await, vec![11.0, 21.0, 31.0, 41.0, 51.0]);

    // What a plot actually reads: distinct, advancing entries.
    let component = bench
        .db
        .with_state(|s| s.get_component(ComponentId::new("scaled")).cloned())
        .expect("the expression's component");
    let latest = component.time_series.latest().expect("history");
    assert_eq!(
        latest.timestamp(),
        Timestamp(5),
        "the newest entry must be the newest sample, not the seed"
    );
}

/// A system feeding another system, tailing live.
///
/// The downstream system reads what the upstream *publishes*, so this is the
/// whole chain — system, field, persist, back through `from_db` into the next
/// system — and every link has to keep following rather than stopping after
/// the first sample.
#[stellarator::test]
async fn a_chain_of_systems_keeps_following_its_input() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let compiled = bench.compile("doubled = wheels.rpm * 2.0\nplus = doubled + 1.0\n");
    assert_eq!(compiled.manifest.systems.len(), 2);

    // Upstream, published so the downstream port can read it.
    let first = wire(&bench, &compiled, 0);
    let first_field = program::field(&compiled, 0, 0, first.node.clone()).unwrap();
    let upstream = ops::persist::persist(
        &bench.db,
        compiled.manifest.systems[0].publishes[0].clone(),
        first_field,
    )
    .unwrap();

    // Downstream, reading the component the upstream just created.
    let upstream_id = ComponentId::new(&compiled.manifest.systems[0].publishes[0]);
    let second = program::system(
        &compiled,
        1,
        vec![program::PortSource {
            node: ops::db_source::from_db(&bench.db, upstream_id).unwrap(),
            seed: program::latest_sample(&bench.db, upstream_id),
        }],
        DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let second_field = program::field(&compiled, 1, 0, second.node.clone()).unwrap();

    let mut out = watch(&second_field);
    let mut mid = watch(&upstream);
    for step in 1..=4 {
        bench.push("wheels.rpm", step, &[f64::from(step as i32)]);
    }

    assert_eq!(take(&mut mid, 4).await, vec![2.0, 4.0, 6.0, 8.0]);
    assert_eq!(
        take(&mut out, 4).await,
        vec![3.0, 5.0, 7.0, 9.0],
        "the downstream system stopped following its producer"
    );
    assert_eq!(first.health.fault(), None);
    assert_eq!(second.health.fault(), None);
}

/// The user's case, end to end: a rank-1 channel plus a scalar literal
/// broadcasts, and the *shape* survives all the way to the output component.
///
/// The schema matters as much as the values — a rank-1 result published under
/// a scalar schema would give every plot the wrong idea about what it is
/// holding.
#[stellarator::test]
async fn a_scalar_broadcasts_over_a_vector_component() {
    let bench = Bench::new(&[("xyz", PrimType::F64, &[3])]);
    let compiled = bench.compile("out = xyz + 1.0\n");
    assert_eq!(
        compiled.manifest.systems[0].output.fields[0].ty,
        Ty::Tensor {
            dtype: metor_expr::Dtype::F64,
            shape: vec![3],
        },
        "the inferred output must keep the broadcast shape"
    );

    let system = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    assert_eq!(
        field.value_type().schema().unwrap(),
        &ComponentSchema::new(PrimType::F64, &[3][..]),
        "and so must the component it publishes as"
    );

    let published = ops::persist::persist(&bench.db, "out".to_string(), field).unwrap();
    let mut reader = published.subscribe();
    bench.push("xyz", 1, &[1.0, 2.0, 3.0]);

    for _ in 0..300 {
        if let Some(grant) = reader.try_next() {
            let (_, value) = grant.sample_at(grant.sample_count() - 1);
            let got: Vec<f64> = value
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| f64::from_le_bytes(*c))
                .collect();
            assert_eq!(got, vec![2.0, 3.0, 4.0]);
            return;
        }
        stellarator::sleep(Duration::from_millis(5)).await;
    }
    panic!("nothing published; fault = {:?}", system.health.fault());
}

/// A source system is driven by its declared clock.
#[stellarator::test]
async fn a_source_system_clocks_itself() {
    let bench = Bench::new(&[]);
    let compiled =
        bench.compile("@system(rate=200.0)\ndef sig() -> f64:\n    return sine(1.0, 2.0)\n");
    assert_eq!(compiled.manifest.systems[0].rate, Some(200.0));

    let system = program::system(&compiled, 0, Vec::new(), DEFAULT_FUEL, None).unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut reader = watch(&field);
    let seen = take(&mut reader, 4).await;
    assert!(
        seen.iter().all(|v| (-2.0..=2.0).contains(v)),
        "a 2.0-amplitude sine stays inside its amplitude: {seen:?}"
    );
    assert!(
        seen.windows(2).any(|w| w[0] != w[1]),
        "a sine of the timestamp varies: {seen:?}"
    );
}

/// The generator's state is seeded per instance, so two sources drawing at
/// once do not draw the same numbers.
#[stellarator::test]
async fn two_random_sources_draw_different_sequences() {
    let bench = Bench::new(&[]);
    let compiled = bench.compile(
        "@system(rate=200.0)\ndef a() -> f64:\n    return random()\n\
         @system(rate=200.0)\ndef b() -> f64:\n    return random()\n",
    );

    let mut drawn = Vec::new();
    let mut held = Vec::new();
    for index in 0..2 {
        let system = program::system(&compiled, index, Vec::new(), DEFAULT_FUEL, None).unwrap();
        let field = program::field(&compiled, index, 0, system.node.clone()).unwrap();
        let mut reader = watch(&field);
        held.push(system);
        drawn.push(take(&mut reader, 6).await);
    }
    assert!(
        drawn[0].iter().all(|v| (0.0..1.0).contains(v)),
        "random() is uniform in [0, 1): {:?}",
        drawn[0]
    );
    assert_ne!(drawn[0], drawn[1], "two generators must not share a seed");
}

/// A system with no inputs and no rate has nothing to fire it, and says so
/// rather than sitting silent.
#[stellarator::test]
async fn an_unclocked_system_with_no_inputs_is_refused() {
    let bench = Bench::new(&[]);
    let compiled = bench.compile("@system\ndef sig() -> f64:\n    return 1.0\n");
    let Err(err) = program::system(&compiled, 0, Vec::new(), DEFAULT_FUEL, None) else {
        panic!("nothing fires it, so it must not build");
    };
    assert!(format!("{err}").contains("rate="), "{err}");
}

/// Two seeded vector inputs contracted with `@`: the product must follow
/// both channels after the opening sample, not stop at one.
#[stellarator::test]
async fn a_dot_of_two_seeded_inputs_keeps_following_both() {
    let bench = Bench::new(&[
        ("imu.a", PrimType::F64, &[3]),
        ("imu.b", PrimType::F64, &[3]),
    ]);
    let compiled = bench.compile("d = imu.a @ imu.b\n");

    bench.push("imu.a", 1, &[1.0, 0.0, 0.0]);
    bench.push("imu.b", 1, &[2.0, 0.0, 0.0]);
    stellarator::sleep(Duration::from_millis(20)).await;

    let ports: Vec<program::PortSource> = ["imu.a", "imu.b"]
        .iter()
        .map(|name| program::PortSource {
            node: bench.source(name),
            seed: program::latest_sample(&bench.db, bench.id(name)),
        })
        .collect();
    let system = program::system(&compiled, 0, ports, DEFAULT_FUEL, None).unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let mut out = watch(&field);

    for step in 2..=5 {
        let v = f64::from(step as i32);
        bench.push("imu.a", step, &[v, 0.0, 0.0]);
        bench.push("imu.b", step, &[2.0, 0.0, 0.0]);
    }
    assert_eq!(take(&mut out, 5).await, vec![2.0, 4.0, 6.0, 8.0, 10.0]);
    assert_eq!(system.health.fault(), None);
}
