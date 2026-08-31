//! Q2: source systems and the generators.
//!
//! A system with nothing to wait on says how often it wants to run, and the
//! host supplies the timer. The signal it produces comes from the timestamp it
//! is handed — the waveforms are pure functions of `now()` — or from a state
//! word the host seeded, which is `random()`. Both are checked here against
//! the definitions written out, since neither is a nox operation.

use crate::{Ty, compile_module};

use super::systems::{Table, build, imu_table, refuse};

/// The reference `random()` is defined as: splitmix64, advanced once per call
/// and taken from the top 53 bits.
fn splitmix(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

#[test]
fn a_rate_makes_a_system_its_own_driver() {
    let source = "\
@system(rate=50.0)
def tick() -> f64:
    return sine(1.0, 2.0)
";
    let program = compile_module(source, &imu_table()).unwrap();
    let system = program.manifest.system("tick").unwrap();
    assert_eq!(system.rate, Some(50.0));
    assert_eq!(system.driving, None);
    assert!(system.inputs.is_empty());
    assert_eq!(system.publishes, vec!["tick"]);

    // An input-driven system says nothing about rate, and vice versa.
    let program = compile_module(
        "@system(\"wheels.rpm\")\ndef scaled(rpm) -> f64:\n    return rpm * 2.0\n",
        &imu_table(),
    )
    .unwrap();
    let system = program.manifest.system("scaled").unwrap();
    assert_eq!(system.rate, None);
    assert_eq!(system.driving, Some(0));
}

/// A system is clocked by one thing. Saying both is a question with no answer.
#[test]
fn rate_and_on_are_exclusive() {
    let text = refuse(
        "@system(\"wheels.rpm\", rate=10.0, on=\"rpm\")\ndef f(rpm) -> f64:\n    return rpm\n",
        &imu_table(),
    );
    assert!(text.contains("source-clocked"), "{text}");

    for bad in ["rate=0.0", "rate=-1.0", "rate=\"fast\""] {
        let text = refuse(
            &format!("@system({bad})\ndef f() -> f64:\n    return 1.0\n"),
            &imu_table(),
        );
        assert!(text.contains("positive number of hertz"), "{bad}: {text}");
    }
}

/// The four shapes, as functions of the timestamp the system was handed.
/// Phase is whole cycles, so the fractional part is what the two ramps read.
#[test]
fn the_waveforms_are_functions_of_now() {
    let table = imu_table();
    // `libm` directly, not `std`: the guest's `sin` is libm's, and the two
    // differ in the last place — the same reason the differential harness
    // draws its transcendentals from libm.
    type Shape = fn(f64) -> f64;
    let cases: [(&str, Shape); 4] = [
        ("sine", |cycles| libm::sin(cycles * std::f64::consts::TAU)),
        ("cosine", |cycles| libm::cos(cycles * std::f64::consts::TAU)),
        ("square", |cycles| {
            let frac = cycles - cycles.floor();
            if frac <= 0.5 { 1.0 } else { -1.0 }
        }),
        ("sawtooth", |cycles| {
            let frac = cycles - cycles.floor();
            2.0 * frac - 1.0
        }),
    ];

    for (kind, shape) in cases {
        let source =
            format!("@system(rate=100.0)\ndef sig() -> f64:\n    return {kind}(2.0, 3.0)\n");
        let mut run = build(&source, &table, "sig");
        // Microseconds, the unit a `Timestamp` carries.
        for micros in [0i64, 1_000, 125_000, 250_000, 700_000, 3_333_333] {
            assert_eq!(run.eval(micros), 0);
            let cycles = 2.0 * (micros as f64 * 1e-6);
            let want = 3.0 * shape(cycles);
            assert_eq!(
                run.scalar("sig").to_bits(),
                want.to_bits(),
                "{kind} at {micros} µs"
            );
        }
    }
}

/// `constant(v)` is `v` — it exists so the palette has a name for a source
/// that does not vary, and so the migration has one target per legacy op.
#[test]
fn a_constant_is_its_argument() {
    let mut run = build(
        "@system(rate=10.0)\ndef bias() -> f64:\n    return constant(9.81)\n",
        &imu_table(),
        "bias",
    );
    assert_eq!(run.eval(0), 0);
    assert_eq!(run.scalar("bias"), 9.81);
    assert_eq!(run.eval(1_000_000), 0);
    assert_eq!(run.scalar("bias"), 9.81);
}

/// The generator's state is a state field like any other: it is in the
/// manifest, it is snapshotted, and the host seeds it because zero is a legal
/// seed but a shared one.
#[test]
fn random_keeps_its_state_where_the_host_can_seed_it() {
    let source = "@system(rate=10.0)\ndef noise() -> f64:\n    return random()\n";
    let program = compile_module(source, &imu_table()).unwrap();
    let system = program.manifest.system("noise").unwrap();
    assert_eq!(system.state.len(), 1);
    assert_eq!(system.state[0].name, crate::state::RNG_FIELD);
    assert_eq!(system.state[0].ty, Ty::I64);

    // A system that never calls it carries no state at all.
    let quiet = compile_module(
        "@system(rate=10.0)\ndef flat() -> f64:\n    return 1.0\n",
        &imu_table(),
    )
    .unwrap();
    assert!(quiet.manifest.system("flat").unwrap().state.is_empty());

    for seed in [0u64, 1, 0xDEAD_BEEF_CAFE_F00D] {
        let mut run = build(source, &imu_table(), "noise");
        let at = run.address("state_ptr", Some(0));
        let memory = run.instance.get_memory(&run.store, "memory").unwrap();
        memory
            .write(&mut run.store, at as usize, &seed.to_le_bytes())
            .unwrap();

        let mut reference = seed;
        for step in 0..8 {
            assert_eq!(run.eval(step * 100_000), 0);
            let want = splitmix(&mut reference);
            assert_eq!(
                run.scalar("noise").to_bits(),
                want.to_bits(),
                "seed {seed}, step {step}"
            );
            assert!((0.0..1.0).contains(&run.scalar("noise")));
        }
    }
}

/// Two seeds, two sequences — which is the whole reason the host writes one.
#[test]
fn a_different_seed_draws_a_different_sequence() {
    let source = "@system(rate=10.0)\ndef noise() -> f64:\n    return random()\n";
    let draw = |seed: u64| {
        let mut run = build(source, &imu_table(), "noise");
        let at = run.address("state_ptr", Some(0));
        let memory = run.instance.get_memory(&run.store, "memory").unwrap();
        memory
            .write(&mut run.store, at as usize, &seed.to_le_bytes())
            .unwrap();
        (0..4)
            .map(|i| {
                run.eval(i);
                run.scalar("noise")
            })
            .collect::<Vec<_>>()
    };
    assert_ne!(draw(1), draw(2));
    assert_eq!(draw(7), draw(7));
}

#[test]
fn the_generators_need_a_system_around_them() {
    for source in [
        "def f() -> f64:\n    return random()\n",
        "def f() -> f64:\n    return sine(1.0, 1.0)\n",
    ] {
        let text = refuse(source, &Table::new(&[]));
        assert!(text.contains("inside a system"), "{source}: {text}");
    }
}

/// Q4: a resample is a top-level binding and nothing else — the one construct
/// the compiler recognises and deliberately does not compile.
#[test]
fn a_resample_binding_becomes_a_host_stage() {
    let program = compile_module(
        "slow = resample_zoh(wheels.rpm, 10.0)\nsmooth = resample_linear(adcs.omega_b, 5.0)\n",
        &imu_table(),
    )
    .unwrap();
    assert!(
        program.manifest.systems.is_empty(),
        "a stage is not compiled"
    );
    assert_eq!(program.manifest.stages.len(), 2);

    let zoh = &program.manifest.stages[0];
    assert_eq!(zoh.name, "slow");
    assert_eq!(zoh.kind, crate::Resample::Zoh);
    assert_eq!(zoh.rate, 10.0);
    assert_eq!(zoh.source, crate::Binding::Component("wheels.rpm".into()));
    assert_eq!(zoh.ty, Ty::F64);

    let linear = &program.manifest.stages[1];
    assert_eq!(linear.kind, crate::Resample::Linear);
    assert_eq!(
        linear.ty,
        Ty::Tensor {
            dtype: crate::Dtype::F64,
            shape: vec![3]
        },
        "a stage carries what its input carried"
    );
}

/// A stage is an ordinary producer: what reads it is an edge, not a lookup.
#[test]
fn a_stage_feeds_what_comes_after_it() {
    let program = compile_module(
        "slow = resample_zoh(wheels.rpm, 10.0)\nscaled = slow * 2.0\nslower = resample_zoh(slow, 1.0)\n",
        &imu_table(),
    )
    .unwrap();
    let scaled = program.manifest.system("scaled").unwrap();
    assert_eq!(
        scaled.inputs[0].bindings,
        vec![crate::Binding::Resampled { stage: 0 }]
    );
    assert_eq!(scaled.inputs[0].frame.fields[0].ty, Ty::F64);
    assert_eq!(
        program.manifest.stages[1].source,
        crate::Binding::Resampled { stage: 0 }
    );

    // A host builds in declaration order, and the spans are what say it.
    assert_eq!(
        program.manifest.declarations(),
        vec![
            crate::Decl::Stage(0),
            crate::Decl::System(0),
            crate::Decl::Stage(1)
        ]
    );
}

/// A system's output can be resampled, which is why stages and systems are
/// checked in one pass rather than two.
#[test]
fn a_stage_can_read_a_system() {
    let program = compile_module(
        "fast = wheels.rpm * 2.0\nslow = resample_linear(fast, 4.0)\n",
        &imu_table(),
    )
    .unwrap();
    assert_eq!(
        program.manifest.stages[0].source,
        crate::Binding::Produced {
            system: 0,
            field: 0
        }
    );
    assert_eq!(program.manifest.stages[0].ty, Ty::F64);
}

#[test]
fn a_resample_anywhere_else_says_where_it_belongs() {
    for source in [
        "scaled = resample_zoh(wheels.rpm, 10.0) * 2.0\n",
        "@system(\"wheels.rpm\")\ndef f(rpm) -> f64:\n    return resample_linear(rpm, 10.0)\n",
    ] {
        let text = refuse(source, &imu_table());
        assert!(
            text.contains("top-level binding of its own"),
            "{source}: {text}"
        );
    }

    for (source, needle) in [
        ("slow = resample_zoh(wheels.rpm)\n", "a source and a rate"),
        (
            "slow = resample_zoh(wheels.rpm, 0.0)\n",
            "positive number of hertz",
        ),
        ("slow = resample_zoh(nothing.here, 10.0)\n", "no component"),
    ] {
        let text = refuse(source, &imu_table());
        assert!(text.contains(needle), "{source}: {text}");
    }
}
