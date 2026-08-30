//! Replaying history through a system must give what the live loop would
//! have published.

use std::time::Duration;

use metor_db::Component;
use metor_proto::types::{PrimType, Timestamp};

use super::program::{self, DEFAULT_FUEL};
use super::program_tests::{Bench, settle, take, watch, wire};
use super::replay::{ReplayPlan, ReplayStats, replay, ticks};
use crate::dynamic::ops;

impl Bench {
    fn component(&self, name: &str) -> Component {
        self.db
            .with_state(|s| s.get_component(self.id(name)).cloned())
            .expect("a component the bench created")
    }

    /// Wait for the persist task to land `name`'s sample at `ts`, which is
    /// what a replay reads — the WAL is not enough.
    async fn persisted(&self, name: &str, ts: i64) {
        let component = self.component(name);
        for _ in 0..300 {
            if component
                .time_series
                .latest()
                .is_some_and(|latest| latest.timestamp() >= Timestamp(ts))
            {
                return;
            }
            settle().await;
        }
        panic!("{name} never persisted its sample at {ts}");
    }

    fn plan(&self, source: &str) -> ReplayPlan {
        let compiled = self.compile(source);
        let ports = compiled.manifest.systems[0]
            .inputs
            .iter()
            .map(|port| match &port.bindings[0] {
                metor_expr::Binding::Component(path) => self.component(path),
                other => panic!("this helper wires components, not {other:?}"),
            })
            .collect();
        ReplayPlan {
            compiled,
            system: 0,
            ports,
            outputs: Vec::new(),
        }
    }
}

/// Replay a plan, collecting each frame's first f64 by timestamp.
fn run(plan: &ReplayPlan, range: std::ops::Range<i64>) -> (Vec<(i64, f64)>, ReplayStats) {
    let mut out = Vec::new();
    let mut field = Vec::new();
    let stats = replay(
        plan,
        Timestamp(range.start)..Timestamp(range.end),
        DEFAULT_FUEL,
        &mut |ts, frame| {
            plan.field(0, frame, &mut field);
            out.push((ts.0, f64::from_le_bytes(field[..8].try_into().unwrap())));
            true
        },
    )
    .expect("the replay runs");
    (out, stats)
}

/// Every value the persisted component holds, oldest first.
fn history(component: &Component) -> Vec<(i64, f64)> {
    let mut nodes: Vec<_> = component.time_series.iter_node_slices().collect();
    nodes.reverse();
    nodes
        .iter()
        .flat_map(|node| {
            node.iter_values(&component.schema)
                .map(|(ts, view)| (ts.0, view.to_f64()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[stellarator::test]
async fn a_replay_reproduces_what_the_live_loop_computed() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    let source = "scaled = wheels.rpm * 2.0 + 1.0\n";
    let compiled = bench.compile(source);
    let system = wire(&bench, &compiled, 0);
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let published = ops::persist::persist(&bench.db, "scaled".to_string(), field).unwrap();
    let mut out = watch(&published);

    for step in 1..=10 {
        bench.push("wheels.rpm", step, &[f64::from(step as i32)]);
    }
    assert_eq!(take(&mut out, 10).await.len(), 10);
    bench.persisted("wheels.rpm", 10).await;
    bench.persisted("scaled", 10).await;

    let (replayed, stats) = run(&bench.plan(source), 1..11);
    assert_eq!(replayed, history(&bench.component("scaled")));
    assert_eq!(stats.read, 10);
    assert_eq!(stats.emitted, 10);
    assert!(!stats.stopped);
}

#[stellarator::test]
async fn a_replay_fires_on_the_driving_port_and_holds_the_rest() {
    let bench = Bench::new(&[
        ("adcs.rate", PrimType::F64, &[]),
        ("wheels.rpm", PrimType::F64, &[]),
    ]);
    bench.push("wheels.rpm", 1, &[7.0]);
    bench.push("adcs.rate", 2, &[3.0]);
    bench.push("wheels.rpm", 3, &[8.0]);
    bench.push("adcs.rate", 4, &[1.0]);
    bench.persisted("wheels.rpm", 3).await;
    bench.persisted("adcs.rate", 4).await;

    let plan = bench.plan(
        "@system(\"adcs.rate\", \"wheels.rpm\")\ndef both(rate, rpm) -> f64:\n    return rate * 100.0 + rpm\n",
    );
    let (replayed, _) = run(&plan, 0..10);
    assert_eq!(replayed, vec![(2, 307.0), (4, 108.0)]);
}

#[stellarator::test]
async fn a_replay_seeds_held_ports_from_before_the_gap() {
    let bench = Bench::new(&[
        ("adcs.rate", PrimType::F64, &[]),
        ("wheels.rpm", PrimType::F64, &[]),
    ]);
    bench.push("wheels.rpm", 1, &[7.0]);
    bench.push("wheels.rpm", 2, &[9.0]);
    bench.push("adcs.rate", 5, &[3.0]);
    bench.persisted("wheels.rpm", 2).await;
    bench.persisted("adcs.rate", 5).await;

    let plan = bench.plan(
        "@system(\"adcs.rate\", \"wheels.rpm\")\ndef both(rate, rpm) -> f64:\n    return rate * 100.0 + rpm\n",
    );
    // The rpm samples sit before the range; the newest of them is what the
    // evaluation at 5 must see.
    let (replayed, stats) = run(&plan, 5..6);
    assert_eq!(replayed, vec![(5, 309.0)]);
    assert_eq!(stats.read, 1, "history before the range is a seed, not a read");
}

#[stellarator::test]
async fn a_replay_skips_until_every_held_port_has_a_value() {
    let bench = Bench::new(&[
        ("adcs.rate", PrimType::F64, &[]),
        ("wheels.rpm", PrimType::F64, &[]),
    ]);
    bench.push("adcs.rate", 1, &[3.0]);
    bench.push("adcs.rate", 2, &[4.0]);
    bench.push("wheels.rpm", 3, &[7.0]);
    bench.push("adcs.rate", 4, &[5.0]);
    bench.persisted("adcs.rate", 4).await;
    bench.persisted("wheels.rpm", 3).await;

    let plan = bench.plan(
        "@system(\"adcs.rate\", \"wheels.rpm\")\ndef both(rate, rpm) -> f64:\n    return rate * 100.0 + rpm\n",
    );
    let (replayed, _) = run(&plan, 0..10);
    assert_eq!(replayed, vec![(4, 507.0)]);
}

#[stellarator::test]
async fn a_rate_clocked_system_replays_on_an_aligned_grid() {
    let bench = Bench::new(&[]);
    let plan = bench.plan("@system(rate=10.0)\ndef sig() -> f64:\n    return 2.0\n");
    let (replayed, stats) = run(&plan, 1_000_001..2_000_000);
    let grid: Vec<i64> = (1..10).map(|k| 1_000_000 + k * 100_000).collect();
    assert_eq!(replayed.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(), grid);
    assert!(replayed.iter().all(|(_, v)| *v == 2.0));
    assert_eq!(stats.read, 0);

    // The grid is the epoch's, so a stretch starting on a tick includes it.
    let on_tick: Vec<_> = ticks(10.0, Timestamp(1_000_000)..Timestamp(1_200_000)).collect();
    assert_eq!(on_tick, vec![Timestamp(1_000_000), Timestamp(1_100_000)]);
}

#[stellarator::test]
async fn stateful_expressions_cold_start_at_the_gap() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    for step in 1..=4 {
        bench.push("wheels.rpm", step, &[100.0]);
    }
    bench.persisted("wheels.rpm", 4).await;

    let plan = bench.plan(
        "class Lp(State):\n\
         \x20   filtered: f64 = 0.0\n\
         \n\
         @system(\"wheels.rpm\")\n\
         def lp(rpm, s: Lp) -> f64:\n\
         \x20   s.filtered = 0.5 * rpm + 0.5 * s.filtered\n\
         \x20   return s.filtered\n",
    );
    // Whatever came before 3 is not carried: the filter starts from its
    // declared default at the gap.
    let (replayed, _) = run(&plan, 3..5);
    assert_eq!(replayed, vec![(3, 50.0), (4, 75.0)]);
}

#[stellarator::test]
async fn a_replay_stops_when_the_sink_says_so() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    for step in 1..=10 {
        bench.push("wheels.rpm", step, &[1.0]);
    }
    bench.persisted("wheels.rpm", 10).await;

    let plan = bench.plan("scaled = wheels.rpm * 2.0\n");
    let mut seen = 0;
    let stats = replay(
        &plan,
        Timestamp(1)..Timestamp(11),
        DEFAULT_FUEL,
        &mut |_, _| {
            seen += 1;
            seen < 3
        },
    )
    .unwrap();
    assert_eq!(seen, 3);
    assert_eq!(stats.emitted, 3);
    assert!(stats.stopped);
}

#[stellarator::test]
async fn a_faulting_body_ends_the_replay_with_an_error() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    bench.push("wheels.rpm", 1, &[1.0]);
    bench.persisted("wheels.rpm", 1).await;

    let plan = bench.plan(
        "@system(\"wheels.rpm\")\ndef spin(rpm) -> f64:\n    x = rpm\n    while True:\n        x = x + 1.0\n    return x\n",
    );
    let err = replay(&plan, Timestamp(0)..Timestamp(10), DEFAULT_FUEL, &mut |_, _| true)
        .expect_err("a runaway body must fault");
    assert!(format!("{err}").contains("fuel"), "{err}");
}

#[stellarator::test]
async fn a_replay_reads_only_the_range_it_was_given() {
    let bench = Bench::new(&[("wheels.rpm", PrimType::F64, &[])]);
    for step in 1..=10 {
        bench.push("wheels.rpm", step, &[f64::from(step as i32)]);
    }
    bench.persisted("wheels.rpm", 10).await;
    stellarator::sleep(Duration::from_millis(5)).await;

    let plan = bench.plan("scaled = wheels.rpm * 2.0\n");
    let (replayed, _) = run(&plan, 4..8);
    assert_eq!(
        replayed,
        vec![(4, 8.0), (5, 10.0), (6, 12.0), (7, 14.0)],
        "half-open: the end is excluded"
    );
}
