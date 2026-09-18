//! The `metor` binary against the fixture's own target file.

use std::path::Path;
use std::process::Command;

fn metor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_metor"))
}

#[test]
fn a_fixed_cycle_count_runs_the_fixtures_target_and_exits_zero() {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack/target.py");
    let output = metor()
        .args([
            "run",
            &target.display().to_string(),
            "--cycles",
            "3",
            "--print-ports",
        ])
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ports = String::from_utf8_lossy(&output.stdout);
    let links: Vec<&str> = ports
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(link, _)| link)
        .collect();
    assert_eq!(links, vec!["cmds", "pub"], "{ports}");
}

#[test]
fn a_wall_rate_and_a_simulated_step_together_are_a_usage_error() {
    let output = metor()
        .args(["run", "target.py", "--wall", "10", "--sim-dt", "0.1"])
        .output()
        .expect("metor runs");
    assert_eq!(output.status.code(), Some(2));
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("--sim-dt"), "{message}");
}

#[test]
fn a_missing_target_file_fails_without_a_panic() {
    let output = metor()
        .args(["run", "no-such-target.py", "--cycles", "1"])
        .output()
        .expect("metor runs");
    assert_eq!(output.status.code(), Some(1));
    let message = String::from_utf8_lossy(&output.stderr);
    // Python's own traceback reaches this stderr, then the host's one line.
    assert!(
        message.contains("metor: the target file exited"),
        "{message}"
    );
}
