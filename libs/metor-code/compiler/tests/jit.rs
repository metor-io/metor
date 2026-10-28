//! Compiles programs with the JIT and steps them with mc-rt's harness.

use mc_rt::harness::{Ended, Harness};
use metor_code::check::NoImports;

const MS: i64 = 1_000;

fn compile(src: &str) -> mc_rt::Program {
    match metor_code::jit::compile(src, &NoImports) {
        Ok(program) => program,
        Err(error) => panic!("{}", error.render("test.mc", src)),
    }
}

fn errors(src: &str) -> String {
    match metor_code::jit::compile(src, &NoImports) {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.render("test.mc", src),
    }
}

const HEATER: &str = r#"
frame Thermal:
    c: f64

frame HeaterCmd:
    duty: f64
    on: bool

const KP = 0.2

def gain(err: f64, k: f64) -> f64:
    return err * k

system heater(setpoint = 40.0, ki: f64 = 0.01):
    "Holds a plate at its setpoint."
    in temp: Thermal
    out heater: HeaterCmd
    integ = 0.0
    alert overtemp critical: temp.c > 80.0 for 2.0
    loop:
        err = setpoint - temp.c
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(gain(err, KP) + ki * integ, 0.0, 1.0)
        heater.on = heater.duty > 0.0
"#;

#[test]
fn test_heater_controls_toward_setpoint() {
    let program = compile(HEATER);
    let mut h =
        Harness::new(&program, "heater", serde_json::json!({ "setpoint": 50.0 })).expect("makes");
    h.set("temp", "c", 20.0).expect("sets");
    h.step(0);
    assert!(h.wrote("heater"));
    assert_eq!(h.get("heater", "duty").expect("reads"), 1.0);
    assert_eq!(h.get("heater", "on").expect("reads"), 1.0);
    h.set("temp", "c", 49.0).expect("sets");
    h.step(10 * MS);
    let duty = h.get("heater", "duty").expect("reads");
    // 0.2 * 1 + 0.01 * clamp(30 * 0 + 1 * 0.01) ...
    let expected = 0.2 * 1.0 + 0.01 * (0.0 + 30.0 * 0.0 + 1.0 * 0.01);
    assert!((duty - expected).abs() < 1e-12, "{duty} vs {expected}");
    h.set("temp", "c", 60.0).expect("sets");
    h.step(20 * MS);
    assert_eq!(h.get("heater", "duty").expect("reads"), 0.0);
    assert_eq!(h.get("heater", "on").expect("reads"), 0.0);
}

#[test]
fn test_alert_fires_after_hold_on_rising_edge() {
    let program = compile(HEATER);
    let mut h = Harness::new(&program, "heater", serde_json::Value::Null).expect("makes");
    h.set("temp", "c", 90.0).expect("sets");
    let mut now = 0;
    for _ in 0..150 {
        h.step(now);
        now += 10 * MS;
    }
    assert_eq!(
        h.get("alerts", "overtemp").expect("reads"),
        0.0,
        "1.5 s is under the hold"
    );
    for _ in 0..60 {
        h.step(now);
        now += 10 * MS;
    }
    assert_eq!(h.get("alerts", "overtemp").expect("reads"), 1.0);
    let rises = h.log().iter().filter(|e| e.kind == "alert").count();
    assert_eq!(rises, 1, "{:?}", h.log());
    h.set("temp", "c", 20.0).expect("sets");
    h.step(now);
    assert_eq!(h.get("alerts", "overtemp").expect("reads"), 0.0);
}

#[test]
fn test_params_refused_and_defaulted() {
    let program = compile(HEATER);
    assert!(Harness::new(&program, "heater", serde_json::json!({ "setpoint": "hot" })).is_err());
    assert!(Harness::new(&program, "heater", serde_json::json!({ "gain": 1.0 })).is_err());
    let desc = program.desc.ty("heater").expect("heater");
    assert_eq!(desc.params.len(), 2);
    assert_eq!(desc.params[0].default, serde_json::json!(40.0));
    assert_eq!(desc.doc, "Holds a plate at its setpoint.");
    assert!(desc.cost > 0);
}

const FAULTS: &str = r#"
frame In:
    n: i32
    x: f64
    i: i64

frame Out:
    q: i32
    y: f64
    arr: [f64; 3]

system faulty:
    in a: In
    out b: Out
    loop:
        b.q = 100 / a.n
        b.y = a.x
        b.arr[a.i] = 7.0
"#;

#[test]
fn test_runtime_faults_never_trap() {
    let program = compile(FAULTS);
    let mut h = Harness::new(&program, "faulty", serde_json::Value::Null).expect("makes");
    h.set("a", "n", 4.0).expect("sets");
    h.set("a", "x", 1.5).expect("sets");
    h.set("a", "i", 1.0).expect("sets");
    h.step(0);
    assert_eq!(h.get("b", "q").expect("reads"), 25.0);
    assert_eq!(h.get("b", "arr.1").expect("reads"), 7.0);
    assert!(h.log().is_empty(), "{:?}", h.log());

    h.set("a", "n", 0.0).expect("sets");
    h.set("a", "x", f64::NAN).expect("sets");
    h.set("a", "i", 9.0).expect("sets");
    h.step(10 * MS);
    assert_eq!(h.get("b", "q").expect("reads"), 0.0);
    assert_eq!(
        h.get("b", "y").expect("reads"),
        1.5,
        "NaN keeps the last value"
    );
    assert_eq!(
        h.get("b", "arr.2").expect("reads"),
        7.0,
        "index clamps to the end"
    );
    let kinds: Vec<&str> = h.log().iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["div_by_zero", "index_clamped", "nan_write"]);
}

const LEAK: &str = r#"
frame Chamber:
    psi: f64

frame ValveCmd:
    open: u8

sequence leak_check(target = 150.0):
    in chamber: Chamber
    out valve: ValveCmd

    redline "chamber over": chamber.psi > 165.0
    on abort:
        valve.open = False

    valve = ValveCmd(open = True)
    wait until chamber.psi >= target timeout 30.0
    valve.open = False
    wait 5 min
    if chamber.psi < target - 2.0:
        fail "leak"
"#;

#[test]
fn test_leak_check_completes() {
    let program = compile(LEAK);
    let mut h = Harness::new(&program, "leak_check", serde_json::Value::Null).expect("makes");
    h.set("chamber", "psi", 14.7).expect("sets");
    h.step(0);
    assert_eq!(h.get("valve", "open").expect("reads"), 1.0);
    let mut now = 0;
    for _ in 0..10 {
        now += 100 * MS;
        h.step(now);
    }
    assert!(h.ended().is_none());
    h.set("chamber", "psi", 151.0).expect("sets");
    now += 100 * MS;
    h.step(now);
    assert_eq!(h.get("valve", "open").expect("reads"), 0.0);
    now += 299_000 * MS;
    h.step(now);
    assert!(h.ended().is_none(), "still holding");
    now += 1_000 * MS;
    h.step(now);
    assert_eq!(h.ended(), Some(&Ended::Completed));
}

#[test]
fn test_leak_check_fails_on_leak_and_runs_on_abort() {
    let program = compile(LEAK);
    let mut h = Harness::new(&program, "leak_check", serde_json::Value::Null).expect("makes");
    h.set("chamber", "psi", 152.0).expect("sets");
    h.step(0);
    assert_eq!(
        h.get("valve", "open").expect("reads"),
        0.0,
        "the wait holds at once"
    );
    h.set("chamber", "psi", 140.0).expect("sets");
    h.step(300_000 * MS);
    assert_eq!(h.ended(), Some(&Ended::Failed("leak".into())));
    assert!(h.wrote("valve"));
    assert!(h.logged("failed"));
}

#[test]
fn test_leak_check_times_out() {
    let program = compile(LEAK);
    let mut h = Harness::new(&program, "leak_check", serde_json::Value::Null).expect("makes");
    h.set("chamber", "psi", 10.0).expect("sets");
    h.run(0, 1_000 * MS, 31);
    match h.ended() {
        Some(Ended::Failed(message)) => assert!(message.contains("timed out"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        h.get("valve", "open").expect("reads"),
        0.0,
        "on abort closed the valve"
    );
}

#[test]
fn test_redline_aborts() {
    let program = compile(LEAK);
    let mut h = Harness::new(&program, "leak_check", serde_json::Value::Null).expect("makes");
    h.set("chamber", "psi", 10.0).expect("sets");
    h.step(0);
    assert_eq!(h.get("valve", "open").expect("reads"), 1.0);
    h.set("chamber", "psi", 170.0).expect("sets");
    h.step(10 * MS);
    assert_eq!(h.ended(), Some(&Ended::Aborted));
    assert!(h.logged("redline"));
    assert_eq!(h.get("valve", "open").expect("reads"), 0.0);
}

#[test]
fn test_waits_in_loops_resume() {
    let src = r#"
frame Count:
    n: i64

sequence counter:
    out c: Count keep
    total = 0
    for i in range(3):
        total += i + 1
        c.n = total
        wait 10 ms
"#;
    let program = compile(src);
    let mut h = Harness::new(&program, "counter", serde_json::Value::Null).expect("makes");
    let mut seen = Vec::new();
    for cycle in 0..8 {
        h.step(cycle * 5 * MS);
        if h.wrote("c") {
            seen.push(h.get("c", "n").expect("reads"));
        }
    }
    assert_eq!(seen, [1.0, 3.0, 6.0]);
    assert_eq!(h.ended(), Some(&Ended::Completed));
}

#[test]
fn test_abort_coverage_is_checked() {
    let src = r#"
frame ValveCmd:
    open: u8

sequence s:
    out valve: ValveCmd
    out other: ValveCmd
    on abort:
        valve.open = 0
    valve.open = 1
    other.open = 1
"#;
    let text = errors(src);
    assert!(
        text.contains("writes `other`, but `on abort:` does not"),
        "{text}"
    );
    assert!(!text.contains("writes `valve`"), "{text}");
}

#[test]
fn test_totality_errors() {
    let recursive =
        "def f(x: f64) -> f64:\n    return g(x)\ndef g(x: f64) -> f64:\n    return f(x)\n";
    assert!(errors(recursive).contains("calls itself"));
    let unbounded = "frame F:\n    a: i64\nsystem s:\n    in x: F\n    out y: F\n    loop:\n        for i in range(x.a):\n            y.a = i\n";
    assert!(errors(unbounded).contains("must be a constant"));
    let div = "frame F:\n    a: i64\nsystem s:\n    out y: F\n    loop:\n        y.a = 1 / 0\n";
    assert!(errors(div).contains("division by a constant zero"));
    let index =
        "frame F:\n    a: [f64; 2]\nsystem s:\n    out y: F\n    loop:\n        y.a[2] = 1.0\n";
    assert!(errors(index).contains("out of range"));
}

#[test]
fn test_type_errors_are_reported_with_spans() {
    let src = "frame F:\n    a: i64\n    b: f64\nsystem s:\n    in x: F\n    out y: F\n    loop:\n        y.b = x.a\n        y.a = x.nope\n        x.a = 1\n";
    let text = errors(src);
    assert!(text.contains("expected `f64`, found `i64`"), "{text}");
    assert!(text.contains("has no field `nope`"), "{text}");
    assert!(text.contains("inputs are read-only"), "{text}");
    assert!(text.contains("test.mc:8"), "{text}");
}

#[test]
fn test_math_and_conversions() {
    let src = r#"
frame V:
    x: f64
    n: i32
    s: u8

system m:
    in a: V
    out b: V
    loop:
        b.x = atan2(a.x, 1.0) + sqrt(4.0) + floor(2.7) + abs(-1.5) + 2.0 ** 3.0
        b.n = i32(a.x * 10.0)
        b.s = u8(300.0)
"#;
    let program = compile(src);
    let mut h = Harness::new(&program, "m", serde_json::Value::Null).expect("makes");
    h.set("a", "x", 1.0).expect("sets");
    h.step(0);
    let expected = libm::atan2(1.0, 1.0) + 2.0 + 2.0 + 1.5 + 8.0;
    assert_eq!(h.get("b", "x").expect("reads"), expected);
    assert_eq!(h.get("b", "n").expect("reads"), 10.0);
    assert_eq!(
        h.get("b", "s").expect("reads"),
        255.0,
        "float to int saturates"
    );
}
