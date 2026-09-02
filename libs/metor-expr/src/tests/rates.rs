//! `delta`, `deltat`, and `mean(x, n)`: the one-sample memories.

use super::systems::{IMU, Table, build, imu_table, refuse};
use crate::Ty;

/// The first sample has nothing to differ from, so it reads 0 rather than
/// itself — and every one after is the difference from its predecessor.
#[test]
fn delta_is_the_change_since_the_previous_sample() {
    let source = "\
@system(\"wheels.rpm\")
def step(rpm) -> f64:
    return delta(rpm)
";
    let mut run = build(source, &imu_table(), "step");
    let seen: Vec<f64> = [5.0, 7.0, 7.0, 2.5]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            run.set("rpm", "rpm", &[*v]);
            run.eval(i as i64);
            run.scalar("step")
        })
        .collect();
    assert_eq!(seen, vec![0.0, 2.0, 0.0, -4.5]);
}

/// An input's block carries its sample's timestamp in microseconds; the gap
/// between one stamp and the one before comes back in seconds, the unit the
/// waveforms already measure the clock in. An evaluation that sees the same
/// stamp again is not a new sample, so the gap holds.
#[test]
fn deltat_is_seconds_between_an_inputs_samples() {
    let source = "\
@system(\"wheels.rpm\")
def gap(rpm) -> f64:
    return deltat(rpm)
";
    let mut run = build(source, &imu_table(), "gap");
    run.set("rpm", "rpm", &[1.0]);
    let seen: Vec<f64> = [1_000_000, 1_250_000, 1_250_000, 3_000_000]
        .iter()
        .map(|t| {
            run.stamp("rpm", *t);
            run.eval(*t);
            run.scalar("gap")
        })
        .collect();
    assert_eq!(seen, vec![0.0, 0.25, 0.25, 1.75]);
}

/// A held input keeps its newest sample across many evaluations of the
/// driving one, so its gap is measured between *its* arrivals, whatever the
/// system's own clock is doing.
#[test]
fn deltat_of_a_held_input_follows_its_own_samples() {
    let source = "\
@system(\"wheels.rpm\", \"adcs.omega_b\")
def gap(rpm, omega_b) -> f64:
    return deltat(omega_b)
";
    let mut run = build(source, &imu_table(), "gap");
    run.set("rpm", "rpm", &[1.0]);
    run.set("omega_b", "omega_b", &[0.0, 0.0, 0.0]);
    run.stamp("omega_b", 0);
    for t in [100_000, 200_000, 300_000] {
        run.stamp("rpm", t);
        run.eval(t);
        assert_eq!(run.scalar("gap"), 0.0);
    }
    run.stamp("omega_b", 2_000_000);
    for t in [2_100_000, 2_200_000] {
        run.stamp("rpm", t);
        run.eval(t);
        assert_eq!(run.scalar("gap"), 2.0);
    }
    run.stamp("omega_b", 2_500_000);
    run.stamp("rpm", 2_600_000);
    run.eval(2_600_000);
    assert_eq!(run.scalar("gap"), 0.5);
}

/// A frame parameter is one input, so it is stamped once — by its own name
/// or through any of its fields.
#[test]
fn deltat_of_a_frame_parameter_or_its_field() {
    let source = format!(
        "{IMU}
@system
def gap(imu: Imu) -> f64:
    return deltat(imu) + deltat(imu.omega)
"
    );
    let mut run = build(&source, &imu_table(), "gap");
    run.set("imu", "omega", &[0.0, 0.0, 0.0]);
    run.set("imu", "accel", &[0.0, 0.0, 0.0]);
    run.stamp("imu", 1_000_000);
    run.eval(1_000_000);
    assert_eq!(run.scalar("gap"), 0.0);
    run.stamp("imu", 1_500_000);
    run.eval(1_500_000);
    assert_eq!(run.scalar("gap"), 1.0);
}

/// A rate is the two together, which is the pipeline they exist for. `delta`
/// counts evaluations and `deltat` counts the input's arrivals, which agree
/// on the driving input; the floor keeps the first tick's `0 / 0` out.
#[test]
fn a_rate_is_delta_over_deltat() {
    let source = "\
@system(\"wheels.rpm\")
def accel(rpm) -> f64:
    return delta(rpm) / max(deltat(rpm), 1e-6)
";
    let mut run = build(source, &imu_table(), "accel");
    run.set("rpm", "rpm", &[10.0]);
    run.stamp("rpm", 0);
    run.eval(0);
    assert_eq!(run.scalar("accel"), 0.0);
    run.set("rpm", "rpm", &[20.0]);
    run.stamp("rpm", 500_000);
    run.eval(500_000);
    assert_eq!(run.scalar("accel"), 20.0);
}

/// The previous sample is state like any other: named, typed, one per call
/// site, and restored with the rest on a rebuild.
#[test]
fn a_delta_is_ordinary_state() {
    let program = crate::compile_module(
        "@system(\"wheels.rpm\")\ndef both(rpm) -> f64:\n    return delta(rpm) + delta(rpm * 2.0) + deltat(rpm)\n",
        &imu_table(),
    )
    .unwrap();
    let state = &program.manifest.system("both").unwrap().state;
    assert_eq!(
        state.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        vec!["@delta0", "@delta1", "@deltat2.prev", "@deltat2.last"]
    );
    assert!(state.iter().all(|f| f.ty == Ty::F64));
    assert!(
        state
            .iter()
            .all(|f| matches!(f.default, crate::Init::F64(v) if v.is_nan()))
    );
}

/// A bare `=` field is a system that never needed a name, so it differences
/// like one.
#[test]
fn a_one_liner_can_difference() {
    let program =
        crate::compile_expr("delta(wheels.rpm) / deltat(wheels.rpm)", &imu_table()).unwrap();
    assert_eq!(program.manifest.systems.len(), 1);
    assert_eq!(program.manifest.systems[0].state.len(), 3);
}

#[test]
fn differencing_needs_a_system_and_deltat_an_input() {
    for (source, needle) in [
        (
            "def f(x: f64) -> f64:\n    return delta(x)\n",
            "`delta` exists inside a system",
        ),
        (
            "def f(x: f64) -> f64:\n    return deltat(x)\n",
            "`deltat` exists inside a system",
        ),
        (
            "def f(x: f64) -> f64:\n    return mean(x, 4)\n",
            "inside a system",
        ),
    ] {
        let text = refuse(source, &Table::new(&[]));
        assert!(text.contains(needle), "{source}: {text}");
    }
    for source in [
        "@system(\"wheels.rpm\")\ndef f(rpm) -> f64:\n    return deltat(rpm * 2.0)\n",
        "@system(\"wheels.rpm\")\ndef f(rpm) -> f64:\n    return deltat(0.0)\n",
    ] {
        let text = refuse(source, &imu_table());
        assert!(text.contains("`deltat` needs an input"), "{source}: {text}");
    }
}

/// `mean(x, n)` is `mean(window(x, n))` and nothing more: the same ring, the
/// same leading zeros, so the average ramps in over the first `n` samples.
#[test]
fn mean_of_the_last_n_samples_is_a_windowed_mean() {
    let sugar = "\
@system(\"wheels.rpm\")
def smooth(rpm) -> f64:
    return mean(rpm, 4)
";
    let spelled = "\
@system(\"wheels.rpm\")
def smooth(rpm) -> f64:
    return mean(window(rpm, 4))
";
    let mut a = build(sugar, &imu_table(), "smooth");
    let mut b = build(spelled, &imu_table(), "smooth");
    let mut seen = Vec::new();
    for i in 1..=6 {
        for run in [&mut a, &mut b] {
            run.set("rpm", "rpm", &[i as f64]);
            run.eval(i);
        }
        seen.push(a.scalar("smooth"));
        assert_eq!(a.scalar("smooth"), b.scalar("smooth"));
    }
    assert_eq!(seen, vec![0.25, 0.75, 1.5, 2.5, 3.5, 4.5]);

    let text = refuse(
        "@system(\"wheels.rpm\")\ndef f(rpm) -> f64:\n    return mean(rpm, rpm)\n",
        &imu_table(),
    );
    assert!(text.contains("positive integer literal"), "{text}");
}
