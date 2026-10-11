//! The `metor` binary against the fixture's own target file.

mod common;

use std::path::Path;

use common::{fixture_dir, fixture_wasm, metor};
use metor_fsw_3::cli::cargo::{WASM_TRIPLE, wasm_name};
use metor_fsw_3::cli::config::{PackKind, PackRef, TargetConfig};
use metor_fsw_3::cli::run::{Run, load};
use metor_fsw_3::coordinator::CoordinatorConfig;
use metor_fsw_3::{InputConfig, SystemConfig, Timestamp};

#[test]
fn test_target_exits_after_fixed_cycles() {
    let target = fixture_dir().join("target.py");
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

/// A deployment of two empty targets, written into a temp dir.
fn two_targets(dir: &Path, body: &str) -> std::path::PathBuf {
    let target = dir.join("deploy.py");
    std::fs::write(
        &target,
        format!(
            "from metor_config import Deployment, Target\n\
             a = Target(cycle_rate=1000.0, namespace=\"a\")\n\
             b = Target(cycle_rate=1000.0, namespace=\"b\")\n\
             {body}\n\
             Deployment(targets=[a, b])\n"
        ),
    )
    .expect("writes");
    target
}

#[test]
fn test_deployment_runs_every_target_as_a_child() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(dir.path(), "");
    let output = metor()
        .args(["run", &target.display().to_string(), "--cycles", "3"])
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.is_empty(), "a clean target prints nothing: {stderr}");
}

/// A system's log lines stay on its log port; what a child prints is its
/// own CLI's last word, which the parent prefixes with the child's name.
#[test]
fn test_every_child_is_prefixed() {
    let fixture = fixture_dir();
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(
        dir.path(),
        "from echo_pack import Boom\na.add(\"boom\", Boom())\nb.add(\"boom\", Boom())",
    );
    let output = metor()
        .args(["run", &target.display().to_string(), "--cycles", "5"])
        .env("PYTHONPATH", fixture.join(".metor"))
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let failed = stderr
        .lines()
        .find_map(|line| line.strip_prefix("metor: these targets did not exit cleanly: "))
        .unwrap_or_default();
    assert!(failed.contains('a') && failed.contains('b'), "{stderr}");
    assert!(
        stderr.contains("a │ metor: these systems latched off: boom")
            || stderr.contains("b │ metor: these systems latched off: boom"),
        "{stderr}"
    );
}

#[test]
fn test_latched_child_fails_the_deployment() {
    let fixture = fixture_dir();
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(
        dir.path(),
        "from echo_pack import Boom\nb.add(\"boom\", Boom())",
    );
    let output = metor()
        .args(["run", &target.display().to_string(), "--cycles", "5"])
        .env("PYTHONPATH", fixture.join(".metor"))
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("did not exit cleanly: b"), "{stderr}");
    assert!(
        stderr.contains("b │ metor: these systems latched off: boom"),
        "{stderr}"
    );
}

#[test]
fn test_print_ports_needs_one_target() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(dir.path(), "");
    let output = metor()
        .args([
            "run",
            &target.display().to_string(),
            "--cycles",
            "1",
            "--print-ports",
        ])
        .output()
        .expect("metor runs");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--print-ports") && stderr.contains("--target"),
        "{stderr}"
    );
}

#[test]
fn test_select_target_runs_in_process() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(dir.path(), "");
    let output = metor()
        .args([
            "run",
            &target.display().to_string(),
            "--cycles",
            "2",
            "--target",
            "b",
        ])
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        !stderr.contains("│"),
        "no prefix for an in-process target: {stderr}"
    );
}

/// Waits for `child` up to two seconds, killing it if it outlasts that.
fn wait_bounded(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("the child is ours") {
            return Some(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

#[test]
fn test_sigint_ends_every_child() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = two_targets(dir.path(), "");
    let mut parent = metor()
        .args([
            "run",
            &target.display().to_string(),
            "--cycles",
            "100000000",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("metor runs");

    // The parent evaluates the target file before it watches for SIGINT.
    std::thread::sleep(std::time::Duration::from_secs(3));
    // SAFETY: `kill` takes a pid and a signal number and touches no memory.
    unsafe { libc::kill(parent.id() as i32, libc::SIGINT) };

    let status = wait_bounded(&mut parent).expect("the deployment ends on SIGINT");
    let output = parent.wait_with_output().expect("its output is ours");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        !stderr.contains("did not exit cleanly"),
        "an interrupted child is not a failed one: {stderr}"
    );
}

/// `sampler -> meter -> echo`, every system from the pack.
fn chain() -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("sampler", "echo.sampler"),
            SystemConfig {
                inputs: vec![InputConfig::new("sample", "sampler", "sample")],
                ..SystemConfig::new("meter", "echo.meter")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("input", "meter", "output")],
                ..SystemConfig::new("echo", "echo.echo")
            },
        ],
        ..Default::default()
    }
}

#[test]
fn test_wasm_pack_loads_and_runs() {
    let libs = tempfile::tempdir().expect("a temp dir");
    let dir = libs.path().join(WASM_TRIPLE);
    std::fs::create_dir_all(&dir).expect("creates");
    std::fs::write(dir.join(wasm_name("echo_pack")), fixture_wasm()).expect("writes");
    let config = TargetConfig {
        packs: vec![PackRef {
            kind: PackKind::Wasm,
            ..PackRef::dylib("echo".into(), "echo_pack".into(), libs.path().into())
        }],
        coordinator: chain(),
    };
    let mut coordinator = load(&config, &Run::default()).expect("loads");
    for cycle in 1..=3 {
        coordinator.step(Timestamp(cycle));
    }
    assert_eq!(coordinator.cycle(), 3);
    assert_eq!(coordinator.latched().count(), 0);
}
