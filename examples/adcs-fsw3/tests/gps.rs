//! The GPS driver against the `gps_receiver` model over a sim UART, as
//! `sim.py` wires them.

use core::cell::{Cell, RefCell};
use core::time::Duration;
use std::rc::Rc;

use adcs_contracts::{BodyState, Gps, GpsFault, Quat, V3};
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::coordinator::{Clock, Coordinator, CoordinatorConfig, SystemTable};
use metor_fsw_3::{Input, Output, Timestamp, system};
use serde_json::json;

/// The cycle period (µs).
const DT: i64 = 10_000;

/// A body on a straight line through a 400 km orbit radius, at `now`.
fn body(now: Timestamp) -> BodyState {
    let t = now.0 as f64 * 1e-6;
    let (pos, vel) = (
        V3::from_buf([6.78e6, 0.0, 0.0]),
        V3::from_buf([0.0, 7670.0, 0.0]),
    );
    BodyState {
        timestamp: now,
        q_b_eci: Quat::identity(),
        omega_b: V3::zeros(),
        pos_eci: pos + vel * t,
        vel_eci: vel,
    }
}

#[system]
fn truth(now: Timestamp, body: &mut Output<BodyState>) {
    let _ = body.write(&self::body(now));
}

/// Writes the outage the test sets, every cycle.
#[system]
fn outage(#[state] on: &mut Rc<Cell<bool>>, now: Timestamp, fault: &mut Output<GpsFault>) {
    let outage = u64::from(on.get());
    let _ = fault.write(&GpsFault {
        timestamp: now,
        outage,
    });
}

type Fixes = Rc<RefCell<Vec<(Timestamp, Gps)>>>;

#[system]
fn fixes(#[state] seen: &mut Fixes, now: Timestamp, fix: &mut Input<Gps>) {
    let got = fix.drain().flatten().map(|f| (now, *f));
    seen.borrow_mut().extend(got);
}

/// The target, the fixes it records, and the outage it reads.
struct Rig {
    c: Coordinator,
    fixes: Fixes,
    outage: Rc<Cell<bool>>,
    now: Timestamp,
}

impl Rig {
    fn new(pos_sigma: f64) -> Self {
        let (fixes, outage) = (Fixes::default(), Rc::<Cell<bool>>::default());
        let mut table: SystemTable = adcs_systems::pack();
        register_builtins(&mut table).unwrap();
        table.register("truth", truth(), || ()).unwrap();
        let on = outage.clone();
        table
            .register("outage", self::outage(), move || on.clone())
            .unwrap();
        let seen = fixes.clone();
        table
            .register("fixes", self::fixes(), move || seen.clone())
            .unwrap();
        let c = config(pos_sigma).build(&table).unwrap();
        Self {
            c,
            fixes,
            outage,
            now: Timestamp(0),
        }
    }

    /// Runs for `seconds` and returns the fixes written meanwhile.
    fn run(&mut self, seconds: f64) -> Vec<(Timestamp, Gps)> {
        for _ in 0..(seconds * 1e6 / DT as f64).round() as usize {
            self.now = Timestamp(self.now.0 + DT);
            self.c.step(self.now);
        }
        let ended: Vec<_> = self.c.systems().filter(|(_, end)| end.is_some()).collect();
        assert!(ended.is_empty(), "{ended:?}");
        self.fixes.take()
    }
}

/// `truth` and `outage`, the driver, `io_submit`, the receiver,
/// `io_complete`, then the fix sink.
fn config(pos_sigma: f64) -> CoordinatorConfig {
    let input = |port: &str, system: &str, from: &str| json!({ "port": port, "from": [{ "system": system, "port": from }] });
    let params = json!({ "seed": 7, "pos_sigma": pos_sigma, "vel_sigma": 0.0 });
    let systems = json!([
        { "id": "truth", "ty": "truth" },
        { "id": "outage", "ty": "outage" },
        { "id": "gps", "ty": "gps_driver" },
        { "id": "io_submit", "ty": "fsw.io" },
        { "id": "rx", "ty": "gps_receiver", "params": params,
          "inputs": [input("truth", "truth", "body"), input("faults", "outage", "fault")] },
        { "id": "io_complete", "ty": "fsw.io", "params": { "status": true } },
        { "id": "fixes", "ty": "fixes", "inputs": [input("fix", "gps", "fix")] },
    ]);
    let sim = json!({
        "kind": "uart",
        "host": { "system": "gps", "param": "uart" },
        "device": { "system": "rx", "param": "uart" },
    });
    let resources = json!({ "gps_uart": { "sim": sim } });
    CoordinatorConfig {
        clock: Clock::Simulated {
            dt: Duration::from_micros(DT as u64),
            epoch: None,
        },
        systems: serde_json::from_value(systems).unwrap(),
        resources: serde_json::from_value(resources).unwrap(),
        ..Default::default()
    }
}

#[test]
fn sets_ten_hertz_and_fixes_match_truth() {
    let fixes = Rig::new(0.0).run(2.0);
    assert!((19..=22).contains(&fixes.len()), "{} fixes", fixes.len());
    // With no noise, each fix is the truth of a recent cycle, through ECEF
    // and back.
    let err = |(at, fix): &(Timestamp, Gps)| {
        let off = |k: i64| -> f64 {
            let then = body(Timestamp(at.0 - k * DT));
            (fix.pos_eci - then.pos_eci).norm().into_buf()
        };
        (0..5).map(off).fold(f64::MAX, f64::min)
    };
    let worst = fixes.iter().map(err).fold(0.0, f64::max);
    assert!(worst < 0.01, "{worst} m");
    let (at, first) = fixes[0];
    let vel: f64 = (first.vel_eci - body(at).vel_eci).norm().into_buf();
    assert!(vel < 1e-3, "{vel} m/s");
}

#[test]
fn an_outage_stops_fixes_until_it_clears() {
    let mut rig = Rig::new(5.0);
    assert!(!rig.run(1.0).is_empty());
    rig.outage.set(true);
    rig.run(0.2);
    assert!(rig.run(1.0).is_empty());
    rig.outage.set(false);
    assert!(!rig.run(1.0).is_empty());
}
