//! Drives `commission` from the linked pack with a scripted rate estimate.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use adcs_contracts::{AttitudeEstimate, Mode, ModeCmd, V3};
use metor_fsw_3::alarm::{ClearedMsg, RaisedMsg};
use metor_fsw_3::coordinator::{
    Coordinator, CoordinatorConfig, EndReason, InputConfig, SystemConfig,
};
use metor_fsw_3::zerocopy::FromZeros;
use metor_fsw_3::{AbortRequest, Input, Output, Severity, Timestamp, system};
use serde_json::json;

/// The rate the estimator reports and whether the aborter fires, set by the test.
#[derive(Clone, Default)]
struct Script {
    rate: Rc<Cell<f64>>,
    abort: Rc<Cell<bool>>,
}

/// Every mode the sequence commanded.
#[derive(Clone, Default)]
struct Commands(Rc<RefCell<Vec<Mode>>>);

/// An alarm edge: its id, and the severity of a raise or `None` for a clear.
type Edge = (String, Option<Severity>);

/// Every alarm edge the sequence published, and each raise's message.
#[derive(Clone, Default)]
struct Edges {
    edges: Rc<RefCell<Vec<Edge>>>,
    messages: Rc<RefCell<Vec<String>>>,
}

#[system]
fn estimator(
    #[state] script: &mut Script,
    now: Timestamp,
    estimate: &mut Output<AttitudeEstimate>,
) {
    let mut e = AttitudeEstimate::new_zeroed();
    e.timestamp = now;
    e.omega_b = V3::from_buf([script.rate.get(), 0.0, 0.0]);
    let _ = estimate.write(&e);
}

#[system]
fn aborter(#[state] script: &mut Script, abort: &mut Output<AbortRequest>) {
    if script.abort.get() {
        let _ = abort.write(&AbortRequest {});
    }
}

#[system]
fn recorder(#[state] commands: &mut Commands, mode_cmd: &mut Input<ModeCmd>) {
    let mut commands = commands.0.borrow_mut();
    commands.extend(mode_cmd.drain().flatten().filter_map(|m| m.mode().ok()));
}

#[system]
fn alarm_recorder(
    #[state] edges: &mut Edges,
    raised: &mut Input<RaisedMsg>,
    cleared: &mut Input<ClearedMsg>,
) {
    let (mut log, mut messages) = (edges.edges.borrow_mut(), edges.messages.borrow_mut());
    for r in raised.drain().flatten() {
        log.push((r.def_id.to_string(), Some(r.severity)));
        messages.push(r.message.as_str().to_string());
    }
    log.extend(
        cleared
            .drain()
            .flatten()
            .map(|c| (c.def_id.to_string(), None)),
    );
}

/// A rig of the sequence, its feeds, and its recorders.
struct Rig {
    coordinator: Coordinator,
    script: Script,
    commands: Commands,
    edges: Edges,
}

impl Rig {
    /// `commission` settling under 0.05 rad/s, or moving on after `timeout_s`,
    /// then watching the wheels for `handover_s`.
    fn new(timeout_s: f64, handover_s: f64) -> Self {
        let (script, commands) = (Script::default(), Commands::default());
        let edges = Edges::default();
        let mut table = adcs_seqs_3::pack();
        let s = script.clone();
        table
            .register("estimator", estimator(), move || s.clone())
            .expect("valid records");
        let s = script.clone();
        table
            .register("aborter", aborter(), move || s.clone())
            .expect("valid records");
        let c = commands.clone();
        table
            .register("recorder", recorder(), move || c.clone())
            .expect("valid records");
        let e = edges.clone();
        table
            .register("alarm_recorder", alarm_recorder(), move || e.clone())
            .expect("valid records");
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("estimator", "estimator"),
                SystemConfig::new("aborter", "aborter"),
                SystemConfig {
                    params: json!({
                        "rate": 0.05,
                        "detumble_timeout_s": timeout_s,
                        "handover_s": handover_s,
                    }),
                    inputs: vec![
                        InputConfig::new("abort", "aborter", "abort"),
                        InputConfig::new("estimate", "estimator", "estimate"),
                    ],
                    ..SystemConfig::new("commission", "commission")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("mode_cmd", "commission", "mode_cmd")],
                    ..SystemConfig::new("recorder", "recorder")
                },
                SystemConfig {
                    inputs: vec![
                        InputConfig::new("raised", "commission", "alarm_raised"),
                        InputConfig::new("cleared", "commission", "alarm_cleared"),
                    ],
                    ..SystemConfig::new("alarms", "alarm_recorder")
                },
            ],
            ..Default::default()
        };
        let coordinator = config.build(&table).expect("the rig builds");
        Self {
            coordinator,
            script,
            commands,
            edges,
        }
    }

    /// Steps one cycle at `ms` milliseconds.
    fn step(&mut self, ms: i64) {
        self.coordinator.step(Timestamp(ms * 1_000));
    }

    fn ended(&self) -> Option<&EndReason> {
        self.coordinator.systems().find_map(|(_, reason)| reason)
    }

    fn commands(&self) -> Vec<Mode> {
        self.commands.0.take()
    }

    fn edges(&self) -> Vec<Edge> {
        self.edges.edges.take()
    }

    fn messages(&self) -> Vec<String> {
        self.edges.messages.take()
    }

    /// Steps each millisecond in `ms`.
    fn run(&mut self, ms: core::ops::Range<i64>) {
        ms.for_each(|ms| self.step(ms));
    }
}

const DETUMBLING: &str = "commission.detumbling";
const REBOUND: &str = "commission.rate_rebound";

fn raised(id: &str, severity: Severity) -> Edge {
    (id.into(), Some(severity))
}

fn cleared(id: &str) -> Edge {
    (id.into(), None)
}

const DETUMBLE: Mode = Mode::Detumble;
const NADIR: Mode = Mode::Idle;
const SAFE: Mode = Mode::Safe;

#[test]
fn test_settled_rate_goes_to_nadir() {
    let mut rig = Rig::new(60.0, 0.01);
    rig.script.rate.set(0.2);
    for ms in 0..5 {
        rig.step(ms);
    }
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
    rig.script.rate.set(0.01);
    rig.step(5);
    rig.step(6);
    assert_eq!((rig.commands(), rig.ended()), (vec![NADIR], None));
    rig.run(7..20);
    assert_eq!(rig.ended(), Some(&EndReason::Completed));
}

#[test]
fn test_rate_at_the_bound_holds_detumble() {
    let mut rig = Rig::new(60.0, 0.01);
    rig.script.rate.set(0.05);
    for ms in 0..5 {
        rig.step(ms);
    }
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
}

#[test]
fn test_timeout_goes_to_nadir() {
    let mut rig = Rig::new(0.01, 0.01);
    rig.script.rate.set(0.2);
    rig.step(0);
    rig.step(5);
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
    rig.step(20);
    rig.step(21);
    assert_eq!(rig.commands(), [NADIR]);
    rig.run(22..40);
    assert_eq!(rig.ended(), Some(&EndReason::Completed));
}

#[test]
fn test_abort_commands_safe() {
    let mut rig = Rig::new(60.0, 0.01);
    rig.script.rate.set(0.2);
    rig.step(0);
    rig.step(1);
    rig.script.abort.set(true);
    rig.step(2);
    rig.step(3);
    assert_eq!(rig.commands(), [DETUMBLE, SAFE]);
    assert_eq!(rig.ended(), Some(&EndReason::Aborted));
}

#[test]
fn test_bad_params_are_refused() {
    let table = adcs_seqs_3::pack();
    for params in [
        json!({ "rate": -1.0, "detumble_timeout_s": 1.0, "handover_s": 1.0 }),
        json!({ "rate": 0.05, "detumble_timeout_s": -1.0, "handover_s": 1.0 }),
        json!({ "rate": 0.05, "detumble_timeout_s": 1.0, "handover_s": -1.0 }),
        json!({ "rate": 0.05, "detumble_timeout_s": 1.0 }),
    ] {
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                params,
                ..SystemConfig::new("commission", "commission")
            }],
            ..Default::default()
        };
        assert!(config.build(&table).is_err());
    }
}

#[test]
fn test_detumble_alarm_raises_then_clears() {
    let mut rig = Rig::new(60.0, 0.01);
    rig.script.rate.set(0.2);
    for ms in 0..5 {
        rig.step(ms);
    }
    assert_eq!(rig.edges(), [raised(DETUMBLING, Severity::Warning)]);
    rig.script.rate.set(0.01);
    rig.step(5);
    rig.step(6);
    assert_eq!(rig.edges(), [cleared(DETUMBLING)]);
    assert_eq!(rig.messages(), ["rate above 0.05 rad/s"]);
}

#[test]
fn test_runaway_rate_trips_the_redline() {
    let mut rig = Rig::new(60.0, 0.01);
    rig.script.rate.set(1.0);
    rig.run(0..3);
    let edges = rig.edges();
    let redline = |(id, _): &&Edge| id == "commission.redline.rate runaway";
    let redlines: Vec<_> = edges.iter().filter(redline).map(|(_, s)| *s).collect();
    assert_eq!(redlines, [Some(Severity::Critical), None], "{edges:?}");
    assert!(edges.contains(&cleared(DETUMBLING)), "{edges:?}");
    assert_eq!(rig.commands(), [DETUMBLE, SAFE]);
    assert_eq!(rig.ended(), Some(&EndReason::Aborted));
}

#[test]
fn test_rebound_on_the_wheels_raises_then_recovers() {
    let mut rig = Rig::new(60.0, 1.0);
    rig.script.rate.set(0.01);
    rig.run(0..3);
    assert_eq!(rig.commands(), [DETUMBLE, NADIR]);
    rig.script.rate.set(0.2);
    rig.run(3..14);
    assert_eq!(rig.edges(), [], "debounced");
    rig.step(14);
    assert_eq!(rig.edges(), [raised(REBOUND, Severity::Warning)]);
    assert_eq!(
        rig.messages(),
        ["rate 0.200 rad/s, above 0.1 on the wheels"]
    );
    rig.script.rate.set(0.01);
    rig.run(15..40);
    assert_eq!(rig.edges(), [cleared(REBOUND)]);
    assert_eq!(rig.ended(), None);
}
