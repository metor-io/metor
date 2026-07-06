//! CLI arg-parsing + override-mapping tests. These drive the clap structs and the pure
//! helpers (`apply_overrides`, `load_run_wiring`) directly — no process, no cargo, no
//! runtime — so the command surface is pinned without running a mission.

use super::*;
use crate::wiring::WiringBuilder;

/// Parse an argv into the [`Cli`], panicking on a parse error (the happy path).
fn parse_args(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).expect("parse argv")
}

/// Extract a [`RunArgs`] from a parsed `run` invocation.
fn run_args(argv: &[&str]) -> RunArgs {
    match parse_args(argv).command {
        Command::Run(a) => a,
        other => panic!("expected `run`, got {other:?}"),
    }
}

#[test]
fn build_args_parse() {
    let cli = parse_args(&[
        "metor-fsw",
        "build",
        "mission.kdl",
        "--release",
        "--cargo-arg",
        "--target",
        "--cargo-arg",
        "aarch64-unknown-linux-gnu",
    ]);
    match cli.command {
        Command::Build(a) => {
            assert_eq!(a.kdl, PathBuf::from("mission.kdl"));
            assert!(a.release);
            assert_eq!(a.cargo_arg, vec!["--target", "aarch64-unknown-linux-gnu"]);
        }
        other => panic!("expected `build`, got {other:?}"),
    }
}

#[test]
fn package_args_parse() {
    let cli = parse_args(&["metor-fsw", "package", "m.kdl", "-o", "dist/x.bundle"]);
    match cli.command {
        Command::Package(a) => {
            assert_eq!(a.kdl, PathBuf::from("m.kdl"));
            assert_eq!(a.out, PathBuf::from("dist/x.bundle"));
            assert!(!a.release);
        }
        other => panic!("expected `package`, got {other:?}"),
    }
}

#[test]
fn run_args_parse_overrides() {
    let a = run_args(&[
        "metor-fsw",
        "run",
        "m.kdl",
        "--build",
        "--wall",
        "--cycle-rate",
        "200",
        "--telemetry",
        "127.0.0.1:2240",
        "--cycles",
        "4000",
    ]);
    assert!(a.build);
    assert!(a.wall);
    assert_eq!(a.cycle_rate, Some(200.0));
    assert_eq!(a.telemetry, Some("127.0.0.1:2240".parse().unwrap()));
    assert_eq!(a.cycles, Some(4000));
}

#[test]
fn clock_flags_mutually_exclusive() {
    let r = Cli::try_parse_from(["metor-fsw", "run", "m.kdl", "--wall", "--sim-dt", "0.01"]);
    assert!(r.is_err(), "--wall and --sim-dt must not coexist");
}

#[test]
fn telemetry_flags_mutually_exclusive() {
    let r = Cli::try_parse_from([
        "metor-fsw",
        "run",
        "m.kdl",
        "--telemetry",
        "127.0.0.1:2240",
        "--no-telemetry",
    ]);
    assert!(r.is_err(), "--telemetry and --no-telemetry must not coexist");
}

#[test]
fn overrides_applied_to_wiring() {
    let mut wiring = WiringBuilder::new()
        .coordinator(120.0, ClockSpec::Simulated { dt_secs: 1.0 / 120.0 })
        .build();
    let args = run_args(&[
        "metor-fsw", "run", "m.kdl", "--build", "--wall", "--cycle-rate", "200", "--telemetry",
        "127.0.0.1:2240",
    ]);
    apply_overrides(&mut wiring, &args).unwrap();
    assert!(matches!(wiring.coordinator.clock, ClockSpec::Wall));
    assert_eq!(wiring.coordinator.cycle_rate, 200.0);
    let downlink = wiring
        .systems
        .iter()
        .find(|s| s.ty.as_deref() == Some(crate::wiring::TCP_DOWNLINK_TYPE))
        .expect("--telemetry pushed a TcpDownlink spec");
    assert_eq!(downlink.name, "telemetry");
}

#[test]
fn no_telemetry_override_disables_kdl_telemetry() {
    let mut wiring = WiringBuilder::new()
        .coordinator(120.0, ClockSpec::Wall)
        .telemetry("127.0.0.1:2240".parse().unwrap())
        .build();
    let args = run_args(&["metor-fsw", "run", "m.kdl", "--build", "--no-telemetry"]);
    apply_overrides(&mut wiring, &args).unwrap();
    assert!(
        !wiring
            .systems
            .iter()
            .any(|s| s.ty.as_deref() == Some(crate::wiring::TCP_DOWNLINK_TYPE)),
        "--no-telemetry removed the KDL-declared TcpDownlink spec"
    );
}

#[test]
fn uplink_override_pushes_tcp_uplink_spec() {
    let mut wiring = WiringBuilder::new()
        .coordinator(120.0, ClockSpec::Wall)
        .build();
    assert!(wiring.systems.is_empty(), "no uplink by default");
    let args = run_args(&[
        "metor-fsw", "run", "m.kdl", "--build", "--uplink", "127.0.0.1:2241",
    ]);
    apply_overrides(&mut wiring, &args).unwrap();
    assert_eq!(
        wiring.systems,
        vec![crate::wiring::SystemSpec::tcp_uplink(
            "uplink",
            "127.0.0.1:2241".parse().unwrap()
        )]
    );
}

#[test]
fn sim_dt_override_sets_simulated_clock() {
    let mut wiring = WiringBuilder::new().coordinator(120.0, ClockSpec::Wall).build();
    let args = run_args(&["metor-fsw", "run", "m.kdl", "--build", "--sim-dt", "0.005"]);
    apply_overrides(&mut wiring, &args).unwrap();
    assert!(matches!(
        wiring.coordinator.clock,
        ClockSpec::Simulated { dt_secs } if (dt_secs - 0.005).abs() < 1e-12
    ));
}

#[test]
fn source_kdl_run_without_build_is_rejected() {
    // Not a bundle (a `.kdl` path), and no `--build`: errors before touching the fs.
    let args = run_args(&["metor-fsw", "run", "nonexistent_mission.kdl"]);
    let err = load_run_wiring(&args).expect_err("must require --build");
    assert!(format!("{err:?}").contains("--build"), "{err:?}");
}

#[test]
fn unknown_telemetry_mode_is_rejected() {
    let mut wiring = WiringBuilder::new().coordinator(1.0, ClockSpec::Wall).build();
    let args = run_args(&[
        "metor-fsw", "run", "m.kdl", "--build", "--telemetry", "127.0.0.1:2240",
        "--telemetry-mode", "subset",
    ]);
    let err = apply_overrides(&mut wiring, &args).expect_err("subset not a CLI mode");
    assert!(format!("{err:?}").contains("telemetry-mode"), "{err:?}");
}
