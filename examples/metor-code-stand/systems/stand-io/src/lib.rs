//! The `stand_io` pack: the Rust half of the metor-code stand.
//!
//! `chamber_sim` stands in for the pressure transducer, valve driver and
//! heater of a test stand. `monitor` prints what the stand's metor-code
//! programs do, so a `metor run` shows the sequence's progress. `stand.mc`
//! imports this pack's frames with `use frames from "stand_io"`.

use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{ChannelStatus, Frame, Input, JsonSchema, Output, SystemTable, Timestamp, system};
use serde::Deserialize;

/// The chamber's pressure and wall temperature.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
pub struct Chamber {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub psi: f64,
    pub temp: f64,
}

/// The fill valve's command: open when `open` is nonzero.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
pub struct ValveCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub open: u8,
    pub _pad: [u8; 7],
}

/// The wall heater's duty cycle, from 0 to 1.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
pub struct HeaterCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub duty: f64,
}

/// [`chamber_sim`]'s params.
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ChamberParams {
    /// psi/s gained while the valve is open.
    #[serde(default = "default_fill")]
    pub fill: f64,
    /// psi/s lost to a leak.
    #[serde(default)]
    pub leak: f64,
    /// Whether the valve sticks open once opened.
    #[serde(default)]
    pub stuck: bool,
    /// degC/s at full heater duty.
    #[serde(default = "default_heat")]
    pub heat: f64,
}

fn default_fill() -> f64 {
    10.0
}

fn default_heat() -> f64 {
    2.0
}

/// The chamber's state between cycles.
pub struct ChamberSim {
    params: ChamberParams,
    psi: f64,
    temp: f64,
    open: bool,
    last: Option<Timestamp>,
}

impl ChamberSim {
    fn new(params: ChamberParams) -> Self {
        Self {
            params,
            psi: 14.7,
            temp: 20.0,
            open: false,
            last: None,
        }
    }

    /// Advances the chamber by `dt` seconds.
    fn advance(&mut self, dt: f64, valve_open: bool, duty: f64) {
        self.open = valve_open || (self.params.stuck && self.open);
        if self.open {
            self.psi += self.params.fill * dt;
        }
        self.psi = (self.psi - self.params.leak * dt).max(0.0);
        let cooling = 0.05 * (self.temp - 20.0);
        self.temp += (self.params.heat * duty.clamp(0.0, 1.0) - cooling) * dt;
    }
}

/// A simulated chamber: the valve fills it, a leak drains it, and the
/// heater warms its wall against ambient.
#[system]
fn chamber_sim(
    #[state] s: &mut ChamberSim,
    now: Timestamp,
    valve: &mut Input<ValveCmd>,
    heater: &mut Input<HeaterCmd>,
    chamber: &mut Output<Chamber>,
) {
    let dt = s.last.map_or(0.0, |last| (now.0 - last.0) as f64 * 1e-6);
    s.last = Some(now);
    let open = valve.latest().ok().flatten().is_some_and(|v| v.open != 0);
    let duty = heater.latest().ok().flatten().map_or(0.0, |h| h.duty);
    s.advance(dt, open, duty);
    let _ = chamber.write(&Chamber {
        timestamp: now,
        psi: s.psi,
        temp: s.temp,
    });
}

/// [`monitor`]'s params.
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MonitorParams {
    /// Seconds between chamber reports.
    #[serde(default = "default_report")]
    pub report: f64,
}

fn default_report() -> f64 {
    10.0
}

/// What [`monitor`] last printed.
pub struct Monitor {
    state: Option<u32>,
    open: Option<u8>,
    next_report: i64,
    period: i64,
    start: Option<i64>,
}

impl Monitor {
    fn new(params: MonitorParams) -> Self {
        Self {
            state: None,
            open: None,
            next_report: 0,
            period: (params.report * 1e6) as i64,
            start: None,
        }
    }
}

const STATES: [&str; 8] = [
    "running", "stopped", "completed", "aborted", "failed", "panicked", "unloaded", "quiescing",
];

/// Prints the leak check's state and valve whenever they change, and the
/// chamber every `report` seconds, in seconds since the first cycle.
///
/// It reads every input every cycle: a reader that falls behind fills its
/// ring, and a full ring refuses its producer's writes.
#[system]
fn monitor(
    #[state] m: &mut Monitor,
    now: Timestamp,
    status: &mut Input<ChannelStatus>,
    valve: &mut Input<ValveCmd>,
    chamber: &mut Input<Chamber>,
    heater: &mut Input<HeaterCmd>,
) {
    let start = *m.start.get_or_insert(now.0);
    let t = (now.0 - start) as f64 * 1e-6;
    let chamber = chamber.latest().ok().flatten().map(|c| *c);
    let duty = heater.latest().ok().flatten().map_or(0.0, |h| h.duty);
    let state = status.latest().ok().flatten().map(|s| s.state);
    if state != m.state
        && let Some(state) = state
    {
        let name = STATES.get(state as usize).unwrap_or(&"unknown");
        let psi = chamber.map_or(0.0, |c| c.psi);
        println!("[{t:8.2} s] leak_check {name} (chamber {psi:.1} psi)");
        m.state = Some(state);
    }
    let open = valve.latest().ok().flatten().map(|v| v.open);
    if open != m.open
        && let Some(open) = open
    {
        let psi = chamber.map_or(0.0, |c| c.psi);
        let word = if open != 0 { "opened" } else { "closed" };
        println!("[{t:8.2} s] valve {word} at {psi:.1} psi");
        m.open = Some(open);
    }
    if now.0 >= m.next_report
        && let Some(c) = chamber
    {
        println!("[{t:8.2} s] chamber {:.1} psi, wall {:.1} C, heater {:.0}%", c.psi, c.temp, duty * 100.0);
        m.next_report = now.0 + m.period;
    }
}

/// Returns the pack's table: `chamber_sim` and `monitor`.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    let registered = table
        .register("chamber_sim", chamber_sim(), ChamberSim::new)
        .and_then(|()| table.register("monitor", monitor(), Monitor::new));
    if let Err(error) = registered {
        metor_fsw_3::tracing::error!("stand_io: {error}");
    }
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(fill: f64, leak: f64, stuck: bool) -> ChamberSim {
        ChamberSim::new(ChamberParams {
            fill,
            leak,
            stuck,
            heat: 2.0,
        })
    }

    #[test]
    fn test_fills_only_while_open() {
        let mut s = sim(10.0, 0.0, false);
        s.advance(1.0, true, 0.0);
        assert!((s.psi - 24.7).abs() < 1e-9);
        s.advance(1.0, false, 0.0);
        assert!((s.psi - 24.7).abs() < 1e-9);
    }

    #[test]
    fn test_stuck_valve_keeps_filling() {
        let mut s = sim(10.0, 0.0, true);
        s.advance(1.0, true, 0.0);
        s.advance(1.0, false, 0.0);
        assert!((s.psi - 34.7).abs() < 1e-9);
    }

    #[test]
    fn test_leak_never_goes_negative_and_heater_warms() {
        let mut s = sim(0.0, 100.0, false);
        s.advance(1.0, false, 1.0);
        assert_eq!(s.psi, 0.0);
        assert!(s.temp > 20.0);
    }
}
