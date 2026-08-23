//! P6: what the panel pays for a compiled system.
//!
//! Three numbers the plan asks for, each answering a question someone would
//! reasonably ask before trusting this:
//!
//! - **Keystroke to updated output.** The debounce dominates by design, so
//!   what is measured is everything *else* — compile, instantiate, and the
//!   first sample out the other side.
//! - **Rebuild with state.** What an edit costs mid-flight, and whether the
//!   filter it carries actually arrives.
//! - **A three-system chain against three legacy nodes.** The complaint that
//!   started the design was that arithmetic costs too many nodes; this prices
//!   the replacement against what it replaces.
//!
//! Run with `--nocapture` to print them. What is asserted is only what would
//! be a real problem, so this stays a test rather than a benchmark that fails
//! on a busy machine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use metor_db::{ComponentSchema, DB};
use metor_proto::types::{ComponentId, PrimType, Timestamp};

use super::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::node::{DynamicNode, DynamicNodeExt, NodeReader};
use crate::dynamic::ops;
use crate::dynamic::resolver::DbResolver;
use crate::dynamic::tensor::TypedScalar;

const SOURCE: &str = "wheels.rpm";

struct Bench {
    db: DB,
    _temp: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        let id = ComponentId(ops::persist::component_id_for_name(SOURCE));
        db.with_state_mut(|s| {
            s.insert_component(id, ComponentSchema::new(PrimType::F64, &[]), &db.path)
        })
        .unwrap();
        db.with_state_mut(|s| {
            s.set_component_metadata(
                metor_proto_wkt::ComponentMetadata {
                    component_id: id,
                    name: SOURCE.to_string(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
        Bench { db, _temp: temp }
    }

    fn push(&self, ts: i64, value: f64) {
        let id = ComponentId(ops::persist::component_id_for_name(SOURCE));
        self.db
            .with_state(|s| s.get_component(id).cloned())
            .unwrap()
            .push_buf(Timestamp(ts), &value.to_le_bytes())
            .unwrap();
    }

    fn source(&self) -> Arc<dyn DynamicNode> {
        let id = ComponentId(ops::persist::component_id_for_name(SOURCE));
        ops::db_source::from_db(&self.db, id).unwrap()
    }
}

async fn first(reader: &mut NodeReader) -> Option<f64> {
    for _ in 0..400 {
        while let Some(grant) = reader.try_next() {
            let count = grant.sample_count();
            if count > 0 {
                let (_, value) = grant.sample_at(count - 1);
                return Some(f64::from_le_bytes(value.try_into().unwrap()));
            }
        }
        stellarator::sleep(Duration::from_millis(2)).await;
    }
    None
}

/// Everything between the last keystroke and the first updated sample, minus
/// the debounce the pane deliberately waits out.
#[stellarator::test]
async fn keystroke_to_updated_output() {
    let bench = Bench::new();
    let resolver = DbResolver::snapshot(&bench.db);

    let compile_start = Instant::now();
    let compiled = Arc::new(Compiled::module("scaled = wheels.rpm * 2.0\n", &resolver).unwrap());
    let compile = compile_start.elapsed();

    let build_start = Instant::now();
    let system =
        program::system(&compiled, 0, vec![bench.source()], DEFAULT_FUEL, None).unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let build = build_start.elapsed();

    let mut out = field.subscribe();
    let sample_start = Instant::now();
    bench.push(1, 21.0);
    let value = first(&mut out).await;
    let to_sample = sample_start.elapsed();

    assert_eq!(value, Some(42.0));
    println!(
        "keystroke to plot: compile {:.2} ms + instantiate {:.2} ms + first sample {:.2} ms \
         = {:.2} ms, behind a 200 ms debounce",
        compile.as_secs_f64() * 1e3,
        build.as_secs_f64() * 1e3,
        to_sample.as_secs_f64() * 1e3,
        (compile + build + to_sample).as_secs_f64() * 1e3,
    );

    // The debounce is 200 ms; the work behind it must not be what the
    // operator waits for.
    assert!(
        compile + build < Duration::from_millis(200),
        "compile plus instantiate took {:?}",
        compile + build
    );
}

/// What an edit costs mid-flight, and whether the state it carries arrives.
#[stellarator::test]
async fn rebuild_with_state() {
    let bench = Bench::new();
    let resolver = DbResolver::snapshot(&bench.db);
    let lowpass = |gain: &str| {
        format!(
            "class Lp(State):\n\
             \x20   filtered: f64 = 0.0\n\
             \n\
             @system(\"{SOURCE}\")\n\
             def lp(rpm, s: Lp) -> f64:\n\
             \x20   s.filtered = {gain} * rpm + (1.0 - {gain}) * s.filtered\n\
             \x20   return s.filtered\n"
        )
    };

    let first_build = Arc::new(Compiled::module(&lowpass("0.5"), &resolver).unwrap());
    let running =
        program::system(&first_build, 0, vec![bench.source()], DEFAULT_FUEL, None).unwrap();
    let field = program::field(&first_build, 0, 0, running.node.clone()).unwrap();
    let mut out = field.subscribe();
    bench.push(1, 100.0);
    assert_eq!(first(&mut out).await, Some(50.0));

    let start = Instant::now();
    let snapshot = running.state.snapshot();
    let second = Arc::new(Compiled::module(&lowpass("0.25"), &resolver).unwrap());
    let rebuilt = program::system(
        &second,
        0,
        vec![bench.source()],
        DEFAULT_FUEL,
        Some(&snapshot),
    )
    .unwrap();
    let field = program::field(&second, 0, 0, rebuilt.node.clone()).unwrap();
    let elapsed = start.elapsed();

    let mut out = field.subscribe();
    bench.push(2, 100.0);
    let carried = first(&mut out).await;
    println!(
        "rebuild with state: snapshot + compile + swap {:.2} ms, filter continued from {:?}",
        elapsed.as_secs_f64() * 1e3,
        carried
    );
    assert_eq!(carried, Some(0.25 * 100.0 + 0.75 * 50.0));
}

/// The complaint that started the design, priced: three chained arithmetic
/// steps as one Python system against the same three steps as legacy nodes.
///
/// The feed is delivered in chunks small enough for a consumer to keep up
/// with. That is not a courtesy — a disruptor drops on full, and pushing two
/// thousand samples into a ring nothing has drained yet loses the same
/// fraction for both forms and measures the ring rather than the work.
#[stellarator::test]
async fn a_three_system_chain_against_three_legacy_nodes() {
    let bench = Bench::new();
    let resolver = DbResolver::snapshot(&bench.db);

    // One system doing all three steps — which is the point: in the language
    // this is one line, where the graph it replaces is three nodes and two
    // edges.
    let compiled = Arc::new(
        Compiled::module(
            &format!("chained = ({SOURCE} * 9.81 + 3.0) * 0.5\n"),
            &resolver,
        )
        .unwrap(),
    );
    let system =
        program::system(&compiled, 0, vec![bench.source()], DEFAULT_FUEL, None).unwrap();
    let expression = program::field(&compiled, 0, 0, system.node.clone()).unwrap();

    // The same arithmetic as the node graph it replaces.
    let scale = ops::derive::affine(
        bench.source(),
        ops::derive::AffineOp::Scale,
        TypedScalar::F64(9.81),
    )
    .unwrap();
    let offset =
        ops::derive::affine(scale, ops::derive::AffineOp::Offset, TypedScalar::F64(3.0)).unwrap();
    let legacy =
        ops::derive::affine(offset, ops::derive::AffineOp::Scale, TypedScalar::F64(0.5)).unwrap();

    let (expression_ns, seen) = drain_cost(&bench, &expression, 1).await;
    assert_eq!(seen, SAMPLES, "the system dropped samples");
    let (legacy_ns, seen) = drain_cost(&bench, &legacy, SAMPLES as i64 + 1).await;
    assert_eq!(seen, SAMPLES, "the legacy chain dropped samples");

    println!(
        "three-step chain, {SAMPLES} samples: one Python system {expression_ns:.0} ns/sample, \
         three legacy nodes {legacy_ns:.0} ns/sample"
    );
}

const SAMPLES: usize = 2048;
/// Small enough that a consumer drains it before the ring wraps.
const CHUNK: usize = 64;

/// Feed `SAMPLES` through one node in drainable chunks, returning the cost per
/// sample and how many came out.
///
/// Only the drain is timed: the pushes and the yields between them are the
/// harness, not the work being priced.
async fn drain_cost(
    bench: &Bench,
    node: &Arc<dyn DynamicNode>,
    first_stamp: i64,
) -> (f64, usize) {
    let mut reader = node.subscribe();
    let mut seen = 0usize;
    let mut elapsed = Duration::ZERO;

    for chunk in 0..SAMPLES / CHUNK {
        for i in 0..CHUNK {
            bench.push(first_stamp + (chunk * CHUNK + i) as i64, i as f64);
        }
        let want = (chunk + 1) * CHUNK;
        let start = Instant::now();
        for _ in 0..400 {
            while let Some(grant) = reader.try_next() {
                seen += grant.sample_count();
            }
            if seen >= want {
                break;
            }
            stellarator::sleep(Duration::from_micros(200)).await;
        }
        elapsed += start.elapsed();
        if seen < want {
            break;
        }
    }
    (elapsed.as_secs_f64() * 1e9 / seen.max(1) as f64, seen)
}
