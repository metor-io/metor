//! Frames and `Option`s of frames taken by value: a cyclic fn with a bare
//! frame is skipped until the frame's first record, and every such input is
//! read each cycle whether or not the fn runs.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_proto::types::Timestamp;

use crate::SystemParams;
use crate::coordinator::{CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
use crate::def::DefCx;
use crate::port::{Input, Output};
use crate::tests::utils::Imu;

use super::def_of;

/// Writes the cycle count from cycle `from` on, and counts failed writes.
struct Late {
    tick: i64,
    from: i64,
    failed: Rc<Cell<usize>>,
}

#[crate::system]
fn late(#[state] s: &mut Late, imu: &mut Output<Imu>) {
    s.tick += 1;
    if s.tick >= s.from && imu.write(&Imu::new(s.tick, s.tick as f64)).is_err() {
        s.failed.set(s.failed.get() + 1);
    }
}

type Seen = Rc<RefCell<Vec<(f64, Option<f64>)>>>;

/// Keeps each run's values for the test.
struct Gated(Seen);

#[crate::system]
fn gated(#[state] s: &mut Gated, req: Imu, opt: Option<Imu>) {
    s.0.borrow_mut()
        .push((req.sample, opt.map(|imu| imu.sample)));
}

/// The pair of `Late` producers feeding `gated`, writing from `req_from` and
/// `opt_from`. Returns what `gated` saw and each producer's failed writes
/// after `cycles` cycles.
fn run_gated(req_from: i64, opt_from: i64, cycles: i64) -> (Vec<(f64, Option<f64>)>, usize, usize) {
    let seen = Seen::default();
    let state = seen.clone();
    let (req_failed, opt_failed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let mut table = SystemTable::new();
    let (rf, of) = (req_failed.clone(), opt_failed.clone());
    table
        .register("late_req", late(), move || Late {
            tick: 0,
            from: req_from,
            failed: rf.clone(),
        })
        .expect("valid records");
    table
        .register("late_opt", late(), move || Late {
            tick: 0,
            from: opt_from,
            failed: of.clone(),
        })
        .expect("valid records");
    table
        .register("gated", gated(), move || Gated(state.clone()))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("req", "late_req"),
            SystemConfig::new("opt", "late_opt"),
            SystemConfig {
                inputs: vec![
                    InputConfig::new("req", "req", "imu"),
                    InputConfig::new("opt", "opt", "imu"),
                ],
                ..SystemConfig::new("gated", "gated")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    for cycle in 1..=cycles {
        coordinator.step(Timestamp(cycle));
    }
    let seen = seen.borrow().clone();
    (seen, req_failed.get(), opt_failed.get())
}

/// A bare frame skips the fn until its first record; an `Option` of one
/// does not, and both hold the newest record each cycle.
#[test]
fn test_bare_frame_skips_until_first_record() {
    let (seen, req_failed, opt_failed) = run_gated(3, 5, 6);
    assert_eq!(
        seen,
        [(3.0, None), (4.0, None), (5.0, Some(5.0)), (6.0, Some(6.0))]
    );
    assert_eq!((req_failed, opt_failed), (0, 0));
}

/// While a bare frame never arrives the fn never runs, yet its other inputs
/// are still read each cycle, so their producers never fill a ring.
#[test]
fn test_skipped_fn_still_drains_its_inputs() {
    let (seen, _, opt_failed) = run_gated(i64::MAX, 1, 200);
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(
        opt_failed, 0,
        "the skipped fn's `opt` input stalled its ring"
    );
}

/// By-value frames declare the same ports an `Input` would.
#[test]
fn test_by_value_frames_declare_input_ports() {
    let cx = DefCx::empty();
    assert_eq!(
        def_of(gated()).inputs,
        vec![Input::<Imu>::def(&cx, "req"), Input::<Imu>::def(&cx, "opt")]
    );
}

/// A test calls the fn directly with plain values.
#[test]
fn test_direct_call_takes_values() {
    let mut state = Gated(Seen::default());
    gated()(&mut state, Imu::new(1, 1.0), None);
    assert_eq!(*state.0.borrow(), [(1.0, None)]);
}

#[derive(SystemParams)]
struct Pair {
    both: [Imu; 2],
    maybe: Option<Imu>,
}

#[crate::system]
fn pairs(#[state] s: &mut Gated, pair: Pair) {
    let [a, b] = pair.both;
    s.0.borrow_mut()
        .push((a.sample + b.sample, pair.maybe.map(|imu| imu.sample)));
}

/// An array of bare frames, inside a `SystemParams` struct, runs the fn only
/// once every element has a record.
#[test]
fn test_array_in_struct_waits_for_every_element() {
    let seen = Seen::default();
    let state = seen.clone();
    let mut table = SystemTable::new();
    for (name, from) in [("a", 1), ("b", 4)] {
        table
            .register(name, late(), move || Late {
                tick: 0,
                from,
                failed: Rc::default(),
            })
            .expect("valid records");
    }
    table
        .register("pairs", pairs(), move || Gated(state.clone()))
        .expect("valid records");
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("a", "a"),
            SystemConfig::new("b", "b"),
            SystemConfig {
                inputs: vec![
                    InputConfig::new("both.0", "a", "imu"),
                    InputConfig::new("both.1", "b", "imu"),
                    InputConfig::new("maybe", "a", "imu"),
                ],
                ..SystemConfig::new("pairs", "pairs")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("valid");
    for cycle in 1..=5 {
        coordinator.step(Timestamp(cycle));
    }
    assert_eq!(*seen.borrow(), [(8.0, Some(4.0)), (10.0, Some(5.0))]);
    let names: Vec<_> = def_of(pairs())
        .inputs
        .iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(names, ["both.0", "both.1", "maybe"]);
}
