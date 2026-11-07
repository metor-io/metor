//! Tests the `metor` binary against the fixture's target files.

mod common;

use std::path::Path;

use common::{fixture_dir, fixture_wasm, metor, panicked};
use metor_fsw_3::cli::run::{Run, load};
use metor_fsw_3::coordinator::CoordinatorConfig;
use metor_fsw_3::deploy::TargetConfig;
use metor_fsw_3::dl::{PackKind, PackRef, WASM_TRIPLE, wasm_name};
use metor_fsw_3::{InputConfig, SystemConfig, Timestamp};

#[test]
fn test_target_exits_after_fixed_cycles() {
    let target = fixture_dir().join("target.py");
    let output = metor()
        .args(["run", &target.display().to_string(), "--cycles", "3"])
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_composites_run_and_show_by_dotted_id() {
    let target = fixture_dir().join("composite.py");
    let target = target.display().to_string();
    let run = metor()
        .args(["run", &target, "--cycles", "3"])
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    for name in ["rack.a.gain", "rack.b.echo", "solo.gain"] {
        let show = metor()
            .args(["show", &target, name])
            .output()
            .expect("metor runs");
        let stderr = String::from_utf8_lossy(&show.stderr);
        assert!(show.status.success(), "{name}: {stderr}");
    }
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
    assert!(
        message.contains("no target file at no-such-target.py"),
        "{message}"
    );
}

/// Writes a deployment of two empty targets into `dir`. `body` is inserted
/// before the deployment is created.
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

/// A deploy error names the line of the target file that made the system.
#[test]
fn test_build_error_cites_the_target_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(
        dir.path(),
        "from metor_config import Connect, Peer, Publish, Subscribe\n\
         out = a.add(\"out\", Publish(transport=Connect(\"127.0.0.1:1\")))\n\
         b.add(\"mirror\", Subscribe(transport=Peer(out)))",
    );
    let output = metor()
        .args(["build", "deploy.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("`b.mirror` names link `a.out`, which does not listen on a fixed port")
            && stderr.contains("[deploy.py:6:"),
        "{stderr}"
    );
}

/// The target file names itself relative to the working directory, so its
/// line shows from a parent directory too.
#[test]
fn test_build_error_cites_the_target_file_from_a_parent_dir() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).expect("makes sub");
    two_targets(
        &sub,
        "from metor_config import Connect, Peer, Publish, Subscribe\n\
         out = a.add(\"out\", Publish(transport=Connect(\"127.0.0.1:1\")))\n\
         b.add(\"mirror\", Subscribe(transport=Peer(out)))",
    );
    let output = metor()
        .args(["build", "sub/deploy.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("[sub/deploy.py:6:"), "{stderr}");
    assert!(stderr.contains("b.add(\"mirror\""), "{stderr}");
}

#[test]
fn test_target_without_a_deployment_says_so() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(dir.path().join("target.py"), "x = 1\n").expect("writes");
    let output = metor()
        .args(["build", "target.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("M0207"), "{stderr}");
}

/// With `--format json`, stdout is one versioned object per line, even when
/// the target file prints.
#[test]
fn test_json_stdout_is_one_object_per_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(dir.path(), "print(\"hello from stdout\")");
    for command in [
        vec!["build", "deploy.py"],
        vec!["run", "deploy.py", "--cycles", "2"],
    ] {
        let output = metor()
            .args(&command)
            .args(["--format", "json"])
            .current_dir(dir.path())
            .output()
            .expect("metor runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{command:?}: {stdout}");
        for line in stdout.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
            assert_eq!(value["v"], 1, "{line}");
        }
        assert!(stdout.contains("hello from stdout"), "{stdout}");
    }
    let ended: serde_json::Value = {
        let output = metor()
            .args(["run", "deploy.py", "--cycles", "2", "--format", "json"])
            .current_dir(dir.path())
            .output()
            .expect("metor runs");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let line = stdout
            .lines()
            .find(|line| line.contains(r#""type":"ended""#))
            .unwrap_or_else(|| panic!("an ended event: {stdout}"))
            .to_string();
        serde_json::from_str(&line).expect("JSON")
    };
    assert_eq!(
        ended["targets"].as_array().map(Vec::len),
        Some(2),
        "{ended}"
    );
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
    assert_eq!(stderr.matches("Resolved").count(), 1, "{stderr}");
    let expected = [
        "Evaluated",
        "Resolved",
        "Running",
        "Finished after",
        "TARGET",
    ];
    let row = |line: &str| {
        ["a ", "b "]
            .iter()
            .any(|name| line.trim_start().starts_with(name))
    };
    assert!(
        stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| row(line) || expected.iter().any(|phase| line.contains(phase))),
        "a clean deployment prints only its phases and summary: {stderr}"
    );
    let rows: Vec<&str> = stderr.lines().filter(|line| row(line)).collect();
    assert_eq!(rows.len(), 2, "one summary row per target: {stderr}");
}

/// A child's lines name its target, since the parent relays them as they
/// are.
#[test]
fn test_every_child_names_its_target() {
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
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    let failed = stderr
        .lines()
        .find_map(|line| line.split_once("these targets did not exit cleanly: "))
        .map(|(_, failed)| failed)
        .unwrap_or_default();
    assert!(failed.contains('a') && failed.contains('b'), "{stderr}");
    assert!(
        stderr.contains("these systems latched off: a.boom")
            || stderr.contains("these systems latched off: b.boom"),
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
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("did not exit cleanly: b"), "{stderr}");
    assert!(
        stderr.contains("these systems latched off: b.boom"),
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

    // The parent only watches for SIGINT after it evaluates the target file.
    std::thread::sleep(std::time::Duration::from_secs(3));
    // SAFETY: `kill` takes a pid and a signal number and does not touch
    // memory.
    unsafe { libc::kill(parent.id() as i32, libc::SIGINT) };

    let status = wait_bounded(&mut parent).expect("the deployment ends on SIGINT");
    let output = parent.wait_with_output().expect("its output is ours");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status.code(), Some(130), "{stderr}");
    assert!(
        !stderr.contains("did not exit cleanly"),
        "an interrupted child is not a failed one: {stderr}"
    );
}

/// Builds the chain `sampler -> meter -> echo`, all from the fixture pack.
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
            kind: PackKind::Wasm(Default::default()),
            ..PackRef::dylib("echo".into(), "echo_pack".into(), libs.path().into())
        }],
        coordinator: chain(),
    };
    let (mut coordinator, _) = load(&config, &Run::default(), None).expect("loads");
    for cycle in 1..=3 {
        coordinator.step(Timestamp(cycle));
    }
    assert_eq!(coordinator.cycle(), 3);
    assert_eq!(panicked(&coordinator).len(), 0);
}

#[test]
fn test_check_builds_every_target_and_runs_none() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(dir.path(), "");
    let output = metor()
        .args(["check", "deploy.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr.matches("Checked").count(), 2, "{stderr}");
    assert!(!stderr.contains("Running"), "{stderr}");
}

#[test]
fn test_check_fails_with_the_errors_code() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(
        dir.path(),
        "from metor_config import Connect, Peer, Publish, Subscribe\n\
         out = a.add(\"out\", Publish(transport=Connect(\"127.0.0.1:1\")))\n\
         b.add(\"mirror\", Subscribe(transport=Peer(out)))",
    );
    let output = metor()
        .args(["check", "deploy.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("M0212"), "{stderr}");
}

#[test]
fn test_check_writes_the_names_completion_reads() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(dir.path(), "");
    let output = metor()
        .args(["check", "deploy.py"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names = std::fs::read_to_string(dir.path().join(".metor/names")).expect("check wrote it");
    assert!(names.starts_with("target a\ntarget b\n"), "{names}");
}

/// A link's two ends are joined, in the picture and in Mermaid.
#[test]
fn test_graph_joins_a_link_across_targets() {
    let dir = tempfile::tempdir().expect("a temp dir");
    two_targets(
        dir.path(),
        "from metor_config import Listen, Peer, Publish, Subscribe\n\
         out = a.add(\"out\", Publish(transport=Listen(\"127.0.0.1:47811\")))\n\
         b.add(\"mirror\", Subscribe(transport=Peer(out)))",
    );
    let graph = |args: &[&str]| {
        let output = metor()
            .arg("graph")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("metor runs");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        String::from_utf8(output.stdout).expect("utf-8")
    };
    let picture = graph(&["deploy.py"]);
    assert!(
        picture.contains("b.mirror") && picture.contains("a.out"),
        "{picture}"
    );
    let mermaid = graph(&["deploy.py", "--as", "mermaid"]);
    assert!(mermaid.contains("t1_0 <-.-> t0_0"), "{mermaid}");
    let one = graph(&["deploy.py", "b"]);
    assert!(!one.contains("  a  "), "one target: {one}");
    assert!(
        picture.contains("io_submit, io_complete  fsw.io"),
        "{picture}"
    );
    let ports = graph(&["deploy.py", "--ports"]);
    assert!(
        ports.contains("b.mirror") && ports.contains("io_submit, io_complete  fsw.io"),
        "idle systems are listed by kind: {ports}"
    );
}

/// `metor new target` makes a project that checks as written. The
/// workspace's lockfile and a shared target dir keep the pack build short.
#[test]
fn test_new_target_checks() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let new = metor()
        .args(["new", "target", "my-sat"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    assert!(
        new.status.success(),
        "{}",
        String::from_utf8_lossy(&new.stderr)
    );
    let project = dir.path().join("my-sat");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::copy(
        workspace.join("Cargo.lock"),
        project.join("systems/my-sat/Cargo.lock"),
    )
    .expect("copies the lockfile");
    let check = metor()
        .arg("check")
        .current_dir(&project)
        .env("CARGO_TARGET_DIR", workspace.join("target/scaffold"))
        .output()
        .expect("metor runs");
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check.status.success(), "{stderr}");
    assert!(stderr.contains("Checked"), "{stderr}");
    let again = metor()
        .args(["new", "pack", "my-sat"])
        .current_dir(dir.path())
        .output()
        .expect("metor runs");
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("M0108"));
}
