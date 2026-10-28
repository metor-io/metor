//! Closes the stand's heater loop around a first-order wall model and
//! checks it settles near its setpoint.

use mc_rt::harness::Harness;
use metor_code::check::NoImports;

const HEATER: &str = r#"
frame Chamber:
    psi: f64
    temp: f64

frame HeaterCmd:
    duty: f64

system heater(setpoint = 40.0, kp = 0.2, ki = 0.01):
    in chamber: Chamber
    out heater: HeaterCmd
    integ = 0.0
    loop:
        err = setpoint - chamber.temp
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(kp * err + ki * integ, 0.0, 1.0)
"#;

#[test]
fn test_heater_settles() {
    let program = metor_code::jit::compile(HEATER, &NoImports).expect("compiles");
    let mut h = Harness::new(&program, "heater", serde_json::Value::Null).expect("makes");
    let (mut temp, mut duty) = (20.0f64, 0.0f64);
    let mut temps = Vec::new();
    for cycle in 0..20_000i64 {
        temp += (2.0 * duty - 0.05 * (temp - 20.0)) * 0.01;
        h.set("chamber", "temp", temp).expect("sets");
        h.step(cycle * 10_000);
        duty = h.get("heater", "duty").expect("reads");
        temps.push(temp);
    }
    let tail = &temps[temps.len() - 1000..];
    let (lo, hi) = tail.iter().fold((f64::MAX, f64::MIN), |(lo, hi), t| (lo.min(*t), hi.max(*t)));
    assert!(hi - lo < 0.5, "oscillates between {lo} and {hi}");
    assert!((hi - 40.0).abs() < 3.0, "settles at {hi}");
}
