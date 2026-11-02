use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use metor_fsw_3_ring::{NoWake, RingBuffer};
use metor_proto_wkt::LogEvent;
use zerocopy::{FromBytes, IntoBytes};

use super::*;
use crate::log::{LogPort, with_log_port};
use crate::mux::params::{OutParam, StampParam, TriggerParam};
use crate::mux::plan::tests::{def, map, output, records, split, system};
use crate::port::{Input, Output};
use crate::record::Bytes;
use crate::system::{InputBinding, OutputBinding, SystemDef};
use crate::tests::utils::{Pair, PairA, PairB, ring, ring_of};

/// A mux wired to rings: a writer per input and a reader per output.
struct Rig {
    mux: Mux,
    ins: DynInputs,
    outs: DynOutputs,
    sources: Vec<Output<Bytes>>,
    sinks: Vec<Input<Bytes>>,
    dropped: Arc<AtomicU32>,
    log: LogPort,
    logs: Input<LogEvent>,
    _rings: Vec<RingBuffer>,
}

impl Rig {
    fn new(params: MuxParams, def: SystemDef) -> Self {
        Self::with_depth(params, def, 8)
    }

    /// A rig whose output rings hold `depth` records.
    fn with_depth(params: MuxParams, def: SystemDef, depth: usize) -> Self {
        let records = records();
        let mux = Mux::new(
            params,
            &Bound {
                def: &def,
                records: &records,
            },
        )
        .expect("resolves");
        let mut rings = Vec::new();
        let (mut sources, mut bindings) = (Vec::new(), Vec::new());
        for port in &def.inputs {
            let ring = ring_of(port.max_len, 8, 1);
            let writer = ring.writer(NoWake).expect("a writer");
            sources.push(Output::with_dropped_counter(
                writer,
                port.max_len,
                Arc::default(),
            ));
            let views = vec![ring.view(NoWake).expect("a view")];
            bindings.push(InputBinding {
                def: port.clone(),
                views,
            });
            rings.push(ring);
        }
        let (mut sinks, mut writers) = (Vec::new(), Vec::new());
        for port in &def.outputs {
            let ring = ring_of(port.max_len, depth, 1);
            let writer = ring.writer(NoWake).expect("a writer");
            writers.push(OutputBinding {
                def: port.clone(),
                writer,
            });
            sinks.push(Input::new(vec![ring.view(NoWake).expect("a view")]));
            rings.push(ring);
        }
        let log_ring = ring::<LogEvent>(8, 1);
        let log = LogPort::new(crate::tests::utils::output(&log_ring));
        let logs = crate::tests::utils::input(&log_ring);
        rings.push(log_ring);
        let dropped = Arc::default();
        Self {
            mux,
            ins: DynInputs::new(bindings),
            outs: DynOutputs::new(writers, &dropped),
            sources,
            sinks,
            dropped,
            log,
            logs,
            _rings: rings,
        }
    }

    fn send(&mut self, input: usize, bytes: &[u8]) {
        self.sources[input].write(bytes).expect("room");
    }

    fn step(&mut self, now: i64) {
        let Self {
            mux,
            ins,
            outs,
            log,
            ..
        } = self;
        with_log_port(log, Timestamp(now), || mux.step(Timestamp(now), ins, outs));
    }

    fn take<T: FromBytes>(&mut self, out: usize) -> Vec<T> {
        let records = self.sinks[out].drain().filter_map(Result::ok);
        records
            .filter_map(|b| T::read_from_prefix(b).ok().map(|(t, _)| t))
            .collect()
    }

    fn faults(&mut self) -> usize {
        let lines = self.logs.drain().filter_map(Result::ok);
        lines
            .filter(|line| line.fields.iter().any(|(_, v)| v == "mux"))
            .count()
    }
}

fn pair_a(stamp: i64, a: f64) -> PairA {
    PairA {
        timestamp: Timestamp(stamp),
        a,
    }
}

fn pair_b(stamp: i64, b: [f64; 3]) -> PairB {
    PairB {
        timestamp: Timestamp(stamp),
        b: crate::FrameArray(b),
    }
}

/// A split of `src.pair`, triggered by and stamped from it.
fn split_rig() -> Rig {
    let (mut params, def) = split();
    for out in &mut params.outputs {
        out.on = TriggerParam::Input("src.pair".into());
        out.stamp = StampParam::Input("src.pair".into());
    }
    Rig::new(params, def)
}

/// `pa.a` and `pb.b` combined into a `Pair`, on `on`.
fn combine(on: TriggerParam, stamp: StampParam, max_age: Option<u64>) -> Rig {
    let (params, def) = combined(on, stamp, max_age);
    Rig::new(params, def)
}

fn combined(on: TriggerParam, stamp: StampParam, max_age: Option<u64>) -> (MuxParams, SystemDef) {
    let params = MuxParams {
        inputs: vec!["pa".into(), "pb".into()],
        outputs: vec![OutParam {
            on,
            stamp,
            max_age: max_age.map(core::time::Duration::from_micros),
            ..output("pair", vec![map("a", "pa", "a"), map("b", "pb", "b")])
        }],
    };
    let def = system(
        vec![def::<PairA>("pa"), def::<PairB>("pb")],
        vec![def::<Pair>("pair")],
    );
    (params, def)
}

fn on_pb() -> Rig {
    combine(
        TriggerParam::Input("pb".into()),
        StampParam::Input("pb".into()),
        None,
    )
}

fn stamps(pairs: &[Pair]) -> Vec<i64> {
    pairs.iter().map(|p| p.timestamp.0).collect()
}

#[test]
fn test_split_emits_per_record() {
    let mut rig = split_rig();
    for t in [5, 6, 7] {
        rig.send(0, Pair::new(t, t as f64, [1.0, 2.0, t as f64]).as_bytes());
    }
    rig.step(100);
    let a: Vec<PairA> = rig.take(0);
    let b: Vec<PairB> = rig.take(1);
    assert_eq!(a, [pair_a(5, 5.0), pair_a(6, 6.0), pair_a(7, 7.0)]);
    assert_eq!(b[2], pair_b(7, [1.0, 2.0, 7.0]));
    rig.step(101);
    assert!(rig.take::<PairA>(0).is_empty());
}

#[test]
fn test_combine_holds_the_latest_of_the_other_input() {
    let mut rig = on_pb();
    rig.send(1, pair_b(1, [1.0; 3]).as_bytes());
    rig.step(10);
    assert!(rig.take::<Pair>(0).is_empty(), "pa has not delivered");
    rig.send(0, pair_a(2, 6.0).as_bytes());
    rig.send(0, pair_a(2, 7.0).as_bytes());
    rig.send(1, pair_b(3, [3.0; 3]).as_bytes());
    rig.step(11);
    rig.send(1, pair_b(4, [4.0; 3]).as_bytes());
    rig.step(12);
    assert_eq!(rig.faults(), 0);
    let out: Vec<Pair> = rig.take(0);
    assert_eq!(
        out,
        [Pair::new(3, 7.0, [3.0; 3]), Pair::new(4, 7.0, [4.0; 3])]
    );
}

#[test]
fn test_any_all_cycle() {
    let mut any = combine(TriggerParam::Any, StampParam::Now, None);
    let mut all = combine(TriggerParam::All, StampParam::Now, None);
    let mut cycle = combine(TriggerParam::Cycle, StampParam::Now, None);
    for rig in [&mut any, &mut all, &mut cycle] {
        rig.send(0, pair_a(1, 1.0).as_bytes());
        rig.step(10);
        assert!(rig.take::<Pair>(0).is_empty(), "pb has not delivered");
        rig.send(1, pair_b(2, [2.0; 3]).as_bytes());
        rig.step(11);
        rig.send(0, pair_a(3, 3.0).as_bytes());
        rig.step(12);
        rig.step(13);
    }
    assert_eq!(stamps(&any.take(0)), [11, 12]);
    assert_eq!(stamps(&all.take(0)), [11]);
    assert_eq!(stamps(&cycle.take(0)), [11, 12, 13]);
}

#[test]
fn test_unstamped_input_ages_from_arrival() {
    let mut rig = combine(
        TriggerParam::Input("pb".into()),
        StampParam::Input("pa".into()),
        Some(5),
    );
    rig.mux.plan.ins[0].stamp_at = None;
    rig.send(0, pair_a(1, 1.0).as_bytes());
    for now in [100, 105, 106] {
        rig.send(1, pair_b(now, [0.0; 3]).as_bytes());
        rig.step(now);
    }
    assert_eq!(
        stamps(&rig.take(0)),
        [100, 100],
        "max_age exactly equal emits"
    );
}

#[test]
fn test_now_stamp_on_a_port_trigger() {
    let mut rig = combine(TriggerParam::Input("pb".into()), StampParam::Now, None);
    rig.send(0, pair_a(1, 1.0).as_bytes());
    rig.send(1, pair_b(2, [0.0; 3]).as_bytes());
    rig.step(50);
    assert_eq!(stamps(&rig.take(0)), [50]);
}

#[test]
fn test_short_record_skipped_with_a_fault() {
    let mut rig = split_rig();
    rig.send(0, &[0u8; 4]);
    rig.step(10);
    assert!(rig.take::<PairA>(0).is_empty());
    assert_eq!(rig.faults(), 1);
}

#[test]
fn test_full_output_ring_counts_a_drop() {
    let (params, def) = combined(TriggerParam::Cycle, StampParam::Now, None);
    let mut rig = Rig::with_depth(params, def, 2);
    rig.send(0, pair_a(1, 1.0).as_bytes());
    rig.send(1, pair_b(1, [0.0; 3]).as_bytes());
    let mut now = 10;
    while rig.dropped.load(Ordering::Relaxed) == 0 {
        rig.step(now);
        now += 1;
        assert!(now < 100, "the ring never filled");
    }
    assert_eq!(rig.dropped.swap(0, Ordering::Relaxed), 1);
    let kept = rig.take::<Pair>(0).len();
    assert_eq!(kept as i64, now - 11);
    // A reader frees what it drained on its next read.
    assert!(rig.take::<Pair>(0).is_empty());
    rig.step(now);
    assert_eq!(stamps(&rig.take(0)), [now]);
}

#[test]
fn test_max_age_withholds_until_fresh() {
    let mut rig = combine(TriggerParam::Cycle, StampParam::Now, Some(10));
    rig.send(0, pair_a(100, 1.0).as_bytes());
    rig.send(1, pair_b(100, [0.0; 3]).as_bytes());
    rig.step(105);
    rig.step(111);
    assert_eq!(stamps(&rig.take(0)), [105]);
    rig.send(0, pair_a(110, 2.0).as_bytes());
    rig.step(112);
    assert!(rig.take::<Pair>(0).is_empty(), "pb is still stale");
    rig.send(1, pair_b(112, [0.0; 3]).as_bytes());
    rig.step(113);
    assert_eq!(rig.take::<Pair>(0)[0].a, 2.0);
}

#[test]
fn test_held_input_drains_before_its_trigger() {
    let mut rig = combine(
        TriggerParam::Input("pa".into()),
        StampParam::Input("pa".into()),
        None,
    );
    rig.send(0, pair_a(1, 1.0).as_bytes());
    rig.send(1, pair_b(1, [1.0; 3]).as_bytes());
    rig.step(10);
    assert_eq!(rig.take::<Pair>(0), [Pair::new(1, 1.0, [1.0; 3])]);
}

/// Overwrites the length of the first record in `ring` so it cannot be read.
fn corrupt_first_record(ring: &RingBuffer) {
    let (base, len) = ring.region();
    let header = len - ring.config().capacity;
    // SAFETY: `header..header + 4` lies in the live region, and no handle
    // touches it during the write.
    unsafe { core::ptr::write_bytes(base.add(header), 0xff, 4) };
}

#[test]
fn test_unreadable_records_fault_once_a_cycle() {
    let mut rig = combine(TriggerParam::Any, StampParam::Now, None);
    rig.send(0, pair_a(1, 1.0).as_bytes());
    rig.send(1, pair_b(1, [1.0; 3]).as_bytes());
    corrupt_first_record(&rig._rings[0]);
    corrupt_first_record(&rig._rings[1]);
    rig.step(10);
    assert_eq!(rig.faults(), 1);
    assert!(rig.take::<Pair>(0).is_empty());
}
