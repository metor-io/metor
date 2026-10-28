//! Runs JIT-compiled systems and sequences in a real fsw-3 coordinator,
//! with the channel adapter, rings and Rust systems around them.

mod common;

use common::{STAND, Trace, add_rust, config, run};
use metor_code::check::NoImports;
use metor_fsw_3::metor_proto_wkt::SequenceChannelState;

fn stand(fill: f64, leak: f64, params: serde_json::Value) -> Trace {
    let program = match metor_code::jit::compile(STAND, &NoImports) {
        Ok(program) => program,
        Err(error) => panic!("{}", error.render("stand.mc", STAND)),
    };
    let mut table = mc_rt::table(&program).expect("the program registers");
    let trace = Trace::default();
    add_rust(&mut table, (fill, leak), &trace);
    let mut coordinator = config("", params).build(&table).expect("the stand builds");
    run(&mut coordinator, 800);
    trace
}

#[test]
fn test_leak_check_completes_in_a_coordinator() {
    let trace = stand(5.0, 0.0, serde_json::Value::Null);
    let samples = trace.samples();
    assert_eq!(samples[1].open, Some(1), "the valve opens");
    assert_eq!(samples.last().and_then(|s| s.open), Some(0), "and closes");
    assert_eq!(trace.last_state(), Some(SequenceChannelState::Completed.code()));
    assert!(samples.iter().any(|s| s.duty.is_some_and(|d| d > 0.0)), "the heater runs");
}

#[test]
fn test_leak_fails_after_abort_runs() {
    let trace = stand(5.0, 0.05, serde_json::json!({ "hold": 2.0 }));
    assert_eq!(trace.last_state(), Some(SequenceChannelState::Failed.code()));
    assert_eq!(trace.samples().last().and_then(|s| s.open), Some(0));
}

#[test]
fn test_redline_aborts_and_on_abort_closes_the_valve() {
    let trace = stand(40.0, 0.0, serde_json::Value::Null);
    let samples = trace.samples();
    let opened = samples.iter().position(|s| s.open == Some(1)).expect("opens");
    let aborted = samples
        .iter()
        .position(|s| s.state == Some(SequenceChannelState::Aborted.code()))
        .expect("aborts");
    assert!(opened < aborted);
    assert_eq!(samples[aborted].open, Some(0), "on abort closed the valve");
    assert!(samples[opened..aborted].iter().all(|s| s.open == Some(1)));
}

#[test]
fn test_bad_params_refuse_the_build() {
    let program = metor_code::jit::compile(STAND, &NoImports).expect("compiles");
    let mut table = mc_rt::table(&program).expect("registers");
    add_rust(&mut table, (5.0, 0.0), &Trace::default());
    let built = config("", serde_json::json!({ "target": "high" })).build(&table);
    assert!(built.is_err());
}
