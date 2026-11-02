//! Drives `health` from the linked pack with scripted sensor samples.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use adcs_contracts::{Gps, Magnetometer, SunSensors, V3, epoch_at, sun_dir_eci};
use metor_fsw_3::alarm::{ClearedMsg, RaisedMsg};
use metor_fsw_3::coordinator::{Coordinator, CoordinatorConfig, InputConfig, SystemConfig};
use metor_fsw_3::zerocopy::FromZeros;
use metor_fsw_3::{Input, Output, Severity, Timestamp, system};

/// The samples the feeder writes this cycle; `None` writes none.
#[derive(Clone, Default)]
struct Script {
    css: Rc<Cell<Option<[f64; 6]>>>,
    /// Field magnitude (T), along body X.
    mag: Rc<Cell<Option<f64>>>,
    pos_eci: Rc<Cell<Option<[f64; 3]>>>,
}

/// An alarm edge: its id, and the severity of a raise or `None` for a clear.
type Edge = (String, Option<Severity>);

/// Every alarm edge `health` published, and each raise's message.
#[derive(Clone, Default)]
struct Edges {
    edges: Rc<RefCell<Vec<Edge>>>,
    messages: Rc<RefCell<Vec<String>>>,
}

#[system]
fn feeder(
    #[state] script: &mut Script,
    now: Timestamp,
    css: &mut Output<SunSensors>,
    mag: &mut Output<Magnetometer>,
    gps: &mut Output<Gps>,
) {
    if let Some(heads) = script.css.get() {
        let _ = css.write(&SunSensors {
            timestamp: now,
            css: heads,
        });
    }
    if let Some(b) = script.mag.get() {
        let _ = mag.write(&Magnetometer {
            timestamp: now,
            mag_b: V3::from_buf([b, 0.0, 0.0]),
        });
    }
    if let Some(pos) = script.pos_eci.get() {
        let mut fix = Gps::new_zeroed();
        fix.timestamp = now;
        fix.pos_eci = V3::from_buf(pos);
        let _ = gps.write(&fix);
    }
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

struct Rig {
    coordinator: Coordinator,
    script: Script,
    edges: Edges,
    cycle: i64,
}

impl Rig {
    fn new() -> Self {
        let (script, edges) = (Script::default(), Edges::default());
        let mut table = adcs_systems_3::pack();
        let s = script.clone();
        table
            .register("feeder", feeder(), move || s.clone())
            .expect("valid records");
        let e = edges.clone();
        table
            .register("alarm_recorder", alarm_recorder(), move || e.clone())
            .expect("valid records");
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("feeder", "feeder"),
                SystemConfig {
                    inputs: vec![
                        InputConfig::new("css", "feeder", "css"),
                        InputConfig::new("mag", "feeder", "mag"),
                        InputConfig::new("gps", "feeder", "gps"),
                    ],
                    ..SystemConfig::new("health", "health")
                },
                SystemConfig {
                    inputs: vec![
                        InputConfig::new("raised", "health", "alarm_raised"),
                        InputConfig::new("cleared", "health", "alarm_cleared"),
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
            edges,
            cycle: 0,
        }
    }

    /// Steps `n` cycles at 120 Hz.
    fn run(&mut self, n: i64) {
        for _ in 0..n {
            self.coordinator
                .step(Timestamp(self.cycle * 1_000_000 / 120));
            self.cycle += 1;
        }
    }

    fn edges(&self) -> Vec<Edge> {
        self.edges.edges.take()
    }

    fn messages(&self) -> Vec<String> {
        self.edges.messages.take()
    }
}

const CSS_DARK: &str = "health.css_dark";
const MAG_FIELD: &str = "health.mag_field";
const MAG_DEAD: &str = "health.mag_dead";

/// Sun sensor readings with every head dark, and with the +X head lit.
const DARK: [f64; 6] = [0.01; 6];
const LIT: [f64; 6] = [0.9, 0.01, 0.01, 0.01, 0.01, 0.01];

/// A position 7000 km from Earth's centre toward the sun, or away from it.
fn orbit(sunward: bool) -> [f64; 3] {
    let r = if sunward { 7.0e6 } else { -7.0e6 };
    sun_dir_eci(epoch_at(0.0)).into_buf().map(|s| s * r)
}

fn raised(id: &str, severity: Severity) -> Edge {
    (id.into(), Some(severity))
}

fn cleared(id: &str) -> Edge {
    (id.into(), None)
}

#[test]
fn test_dark_sun_sensors_in_sunlight_raise_then_clear() {
    let mut rig = Rig::new();
    rig.script.pos_eci.set(Some(orbit(true)));
    rig.script.css.set(Some(DARK));
    rig.run(239);
    assert_eq!(rig.edges(), [], "debounced");
    rig.run(1);
    assert_eq!(rig.edges(), [raised(CSS_DARK, Severity::Warning)]);
    assert_eq!(rig.messages(), ["all sun sensors dark in sunlight"]);
    rig.script.css.set(Some(LIT));
    rig.run(240);
    assert_eq!(rig.edges(), [cleared(CSS_DARK)]);
}

#[test]
fn test_dark_sun_sensors_in_eclipse_are_nominal() {
    let mut rig = Rig::new();
    rig.script.pos_eci.set(Some(orbit(false)));
    rig.script.css.set(Some(DARK));
    rig.run(480);
    assert_eq!(rig.edges(), []);
}

#[test]
fn test_no_fix_or_no_sample_holds_the_debounce() {
    let mut rig = Rig::new();
    rig.script.css.set(Some(DARK));
    rig.run(480);
    assert_eq!(rig.edges(), [], "no fix: no sunlight to compare against");
    rig.script.pos_eci.set(Some(orbit(true)));
    rig.run(200);
    rig.script.css.set(None);
    rig.run(100);
    assert_eq!(rig.edges(), [], "a gap pauses the count");
    rig.script.css.set(Some(DARK));
    rig.run(39);
    assert_eq!(rig.edges(), []);
    rig.run(1);
    assert_eq!(rig.edges(), [raised(CSS_DARK, Severity::Warning)]);
}

#[test]
fn test_mag_field_out_of_band_raises_then_recovers() {
    let mut rig = Rig::new();
    rig.script.mag.set(Some(30e-6));
    rig.run(20);
    rig.script.mag.set(Some(80e-6));
    rig.run(11);
    assert_eq!(rig.edges(), [], "debounced");
    rig.run(1);
    assert_eq!(rig.edges(), [raised(MAG_FIELD, Severity::Warning)]);
    assert_eq!(rig.messages(), ["|B| 80.0 uT"]);
    rig.script.mag.set(Some(45e-6));
    rig.run(12);
    assert_eq!(rig.edges(), [cleared(MAG_FIELD)]);
}

#[test]
fn test_dead_magnetometer_raises_both() {
    let mut rig = Rig::new();
    rig.script.mag.set(Some(0.0));
    rig.run(12);
    let want = [
        raised(MAG_FIELD, Severity::Warning),
        raised(MAG_DEAD, Severity::Critical),
    ];
    assert_eq!(rig.edges(), want);
    assert_eq!(rig.messages(), ["|B| 0.0 uT", "|B| 0.0 uT"]);
}
