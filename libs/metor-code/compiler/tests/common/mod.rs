//! A test stand shared by the coordinator and AOT tests: a Rust plant that
//! pressurises a chamber through a valve, the leak check sequence and a
//! heater system from `STAND`, and a recorder.

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3::coordinator::{
    Adapter, ChannelConfig, Coordinator, CoordinatorConfig, InputConfig, RunState, SystemConfig,
    SystemTable,
};
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{ChannelName, ChannelStatus, Frame, Input, Output, Timestamp, system};

pub const STAND: &str = r#"
frame Chamber:
    psi: f64

frame ValveCmd:
    open: u8

frame HeaterCmd:
    duty: f64

sequence leak_check(target = 150.0, hold = 5.0):
    "Pressurises the chamber, holds it, and checks for a leak."
    in chamber: Chamber
    out valve: ValveCmd

    redline "chamber over": chamber.psi > 165.0
    on abort:
        valve.open = False

    valve = ValveCmd(open = True)
    wait until chamber.psi >= target timeout 30.0
    valve.open = False
    wait hold
    if chamber.psi < target - 2.0:
        fail "leak"

system heater(setpoint = 40.0):
    in chamber: Chamber
    out heater: HeaterCmd
    integ = 0.0
    alert overpressure warning: chamber.psi > 160.0 for 0.05
    loop:
        err = setpoint - chamber.psi
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(0.1 * err + 0.01 * integ, 0.0, 1.0)
"#;

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct Chamber {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub psi: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct ValveCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub open: u8,
    pub _pad: [u8; 7],
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct HeaterCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub duty: f64,
}

/// A chamber that fills by `fill` psi a cycle while the valve is open, and
/// leaks `leak` psi a cycle.
pub struct Plant {
    pub psi: f64,
    pub fill: f64,
    pub leak: f64,
}

#[system]
fn plant(#[state] s: &mut Plant, now: Timestamp, valve: &mut Input<ValveCmd>, chamber: &mut Output<Chamber>) {
    let open = valve.latest().ok().flatten().is_some_and(|v| v.open != 0);
    if open {
        s.psi += s.fill;
    }
    s.psi = (s.psi - s.leak).max(0.0);
    let _ = chamber.write(&Chamber { timestamp: now, psi: s.psi });
}

/// One cycle as the recorder saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub open: Option<u8>,
    pub state: Option<u32>,
    pub duty: Option<f64>,
}

#[derive(Clone, Default)]
pub struct Trace(pub Rc<RefCell<Vec<Sample>>>);

impl Trace {
    pub fn samples(&self) -> Vec<Sample> {
        self.0.borrow().clone()
    }

    /// The channel state of the last cycle.
    pub fn last_state(&self) -> Option<u32> {
        self.0.borrow().iter().rev().find_map(|s| s.state)
    }
}

#[system]
fn recorder(
    #[state] t: &mut Trace,
    valve: &mut Input<ValveCmd>,
    status: &mut Input<ChannelStatus>,
    heater: &mut Input<HeaterCmd>,
) {
    let sample = Sample {
        open: valve.latest().ok().flatten().map(|v| v.open),
        state: status.latest().ok().flatten().map(|s| s.state),
        duty: heater.latest().ok().flatten().map(|h| h.duty),
    };
    t.0.borrow_mut().push(sample);
}

/// Adds the Rust systems to a table that already holds the compiled ones.
pub fn add_rust(table: &mut SystemTable, plant_state: (f64, f64), trace: &Trace) {
    let (fill, leak) = plant_state;
    table
        .register("plant", plant(), move || Plant { psi: 14.7, fill, leak })
        .expect("plant registers");
    let trace = trace.clone();
    table
        .register("recorder", recorder(), move || trace.clone())
        .expect("recorder registers");
}

/// The stand's config. `prefix` names the compiled types, such as `stand.`
/// for a pack registered as `stand`.
pub fn config(prefix: &str, params: serde_json::Value) -> CoordinatorConfig {
    let channel = Adapter::Channel(ChannelConfig {
        name: ChannelName::try_from("leak_check".to_string()).expect("a short name"),
        start: RunState::Running,
    });
    CoordinatorConfig {
        systems: vec![
            SystemConfig {
                inputs: vec![InputConfig::new("valve", "leak", "valve")],
                ..SystemConfig::new("plant", "plant")
            },
            SystemConfig {
                params,
                inputs: vec![InputConfig::new("chamber", "plant", "chamber")],
                adapter: Some(channel),
                ..SystemConfig::new("leak", format!("{prefix}leak_check"))
            },
            SystemConfig {
                params: serde_json::json!({ "setpoint": 100.0 }),
                inputs: vec![InputConfig::new("chamber", "plant", "chamber")],
                ..SystemConfig::new("heater", format!("{prefix}heater"))
            },
            SystemConfig {
                inputs: vec![
                    InputConfig::new("valve", "leak", "valve"),
                    InputConfig::new("status", "leak", "channel_status"),
                    InputConfig::new("heater", "heater", "heater"),
                ],
                ..SystemConfig::new("recorder", "recorder")
            },
        ],
        ..Default::default()
    }
}

/// Steps `coordinator` for `cycles` cycles of 10 ms.
pub fn run(coordinator: &mut Coordinator, cycles: usize) {
    for cycle in 0..cycles {
        coordinator.step(Timestamp(1_000_000 + cycle as i64 * 10_000));
    }
}
