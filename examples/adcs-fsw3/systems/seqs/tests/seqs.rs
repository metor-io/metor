//! Drives `commission` from the linked pack with a scripted rate estimate.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use adcs_contracts::{AttitudeEstimate, ModeCmd, V3};
use metor_fsw_3::coordinator::{
    Coordinator, CoordinatorConfig, EndReason, InputConfig, SystemConfig,
};
use metor_fsw_3::zerocopy::FromZeros;
use metor_fsw_3::{AbortRequest, Input, Output, Timestamp, system};
use serde_json::json;

/// The rate the estimator reports and whether the aborter fires, set by the test.
#[derive(Clone, Default)]
struct Script {
    rate: Rc<Cell<f64>>,
    abort: Rc<Cell<bool>>,
}

/// Every `(mode, law)` the sequence commanded.
#[derive(Clone, Default)]
struct Commands(Rc<RefCell<Vec<(u8, u8)>>>);

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
    commands.extend(mode_cmd.drain().flatten().map(|m| (m.mode, m.law)));
}

/// A rig of the sequence, its feeds, and its recorder.
struct Rig {
    coordinator: Coordinator,
    script: Script,
    commands: Commands,
}

impl Rig {
    /// `commission` settling under 0.05 rad/s, or moving on after `timeout_s`.
    fn new(timeout_s: f64) -> Self {
        let (script, commands) = (Script::default(), Commands::default());
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
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("estimator", "estimator"),
                SystemConfig::new("aborter", "aborter"),
                SystemConfig {
                    params: json!({ "rate": 0.05, "detumble_timeout_s": timeout_s }),
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
            ],
            ..Default::default()
        };
        let coordinator = config.build(&table).expect("the rig builds");
        Self {
            coordinator,
            script,
            commands,
        }
    }

    /// Steps one cycle at `ms` milliseconds.
    fn step(&mut self, ms: i64) {
        self.coordinator.step(Timestamp(ms * 1_000));
    }

    fn ended(&self) -> Option<&EndReason> {
        self.coordinator.systems().find_map(|(_, reason)| reason)
    }

    fn commands(&self) -> Vec<(u8, u8)> {
        self.commands.0.take()
    }
}

const DETUMBLE: (u8, u8) = (ModeCmd::DETUMBLE, ModeCmd::LAW_DETUMBLE);
const NADIR: (u8, u8) = (ModeCmd::IDLE, ModeCmd::LAW_NADIR);
const SAFE: (u8, u8) = (ModeCmd::SAFE, ModeCmd::LAW_NADIR);

#[test]
fn test_settled_rate_goes_to_nadir() {
    let mut rig = Rig::new(60.0);
    rig.script.rate.set(0.2);
    for ms in 0..5 {
        rig.step(ms);
    }
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
    rig.script.rate.set(0.01);
    rig.step(5);
    rig.step(6);
    assert_eq!(rig.commands(), [NADIR]);
    assert_eq!(rig.ended(), Some(&EndReason::Completed));
}

#[test]
fn test_rate_at_the_bound_holds_detumble() {
    let mut rig = Rig::new(60.0);
    rig.script.rate.set(0.05);
    for ms in 0..5 {
        rig.step(ms);
    }
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
}

#[test]
fn test_timeout_goes_to_nadir() {
    let mut rig = Rig::new(0.01);
    rig.script.rate.set(0.2);
    rig.step(0);
    rig.step(5);
    assert_eq!((rig.commands(), rig.ended()), (vec![DETUMBLE], None));
    rig.step(20);
    rig.step(21);
    assert_eq!(rig.commands(), [NADIR]);
    assert_eq!(rig.ended(), Some(&EndReason::Completed));
}

#[test]
fn test_abort_commands_safe() {
    let mut rig = Rig::new(60.0);
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
        json!({ "rate": -1.0, "detumble_timeout_s": 1.0 }),
        json!({ "rate": 0.05, "detumble_timeout_s": -1.0 }),
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
