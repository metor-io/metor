//! The `metor` binary against the fixture's own target file.

use std::path::Path;
use std::process::Command;

fn metor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_metor"))
}

#[test]
fn test_target_exits_after_fixed_cycles() {
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
fn test_reject_conflicting_clock_options() {
    let output = metor()
        .args(["run", "target.py", "--wall", "10", "--sim-dt", "0.1"])
        .output()
        .expect("metor runs");
    assert_eq!(output.status.code(), Some(2));
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("--sim-dt"), "{message}");
}

#[test]
fn test_missing_target_returns_error() {
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
