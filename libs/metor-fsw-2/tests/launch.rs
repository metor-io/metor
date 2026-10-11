//! `run` over a deployment of several members: the members launch as child
//! processes, their output arrives prefixed and in envelope order, a member
//! that fails takes the run down with it, and the single-member path is
//! untouched.
//!
//! One `#[test]` runs every case in sequence: they share the fixture's link
//! ports and the process-global current directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Whether a usable `python3` is on PATH, so the source cases can skip
/// cleanly on a host without one.
fn have_python() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import sys;sys.exit(0 if sys.version_info[:2]>=(3,10) else 1)",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The CLI under test, from the fixture directory, with the interpreter
/// resolved the default way and the members logging so their lines are
/// something to prefix.
fn fsw(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_metor-fsw"));
    command
        .args(args)
        .current_dir(fixtures())
        .env_remove("METOR_PYTHON")
        .env("RUST_LOG", "info");
    command
}

fn run(args: &[&str]) -> Output {
    fsw(args).output().expect("the CLI runs")
}

/// miette wraps a diagnostic at 80 columns and gutters the continuations;
/// undo that so a message asserts as the one string it is.
fn unwrapped(stderr: &str) -> String {
    stderr.replace("\n  │ ", " ")
}

#[test]
fn deployment_members_launch() {
    if !have_python() {
        eprintln!("skipping deployment_members_launch: no python3 >= 3.10 on PATH");
        return;
    }

    // A source deployment runs every member: both pre-flights in envelope
    // order, then nothing but prefixed member lines.
    let output = run(&["run", "launch_target.py", "--cycles", "20"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    let a = stderr
        .find("launch_target.py · a")
        .expect("member `a`'s pre-flight");
    let b = stderr
        .find("launch_target.py · b")
        .expect("member `b`'s pre-flight");
    assert!(a < b, "pre-flights run in envelope order:\n{stderr}");
    assert!(
        stderr
            .lines()
            .filter(|line| line.contains('│'))
            .all(|line| line.starts_with("a │") || line.starts_with("b │")),
        "every piped line carries a member prefix:\n{stderr}"
    );
    assert!(
        stderr.lines().any(|line| line.starts_with("a │"))
            && stderr.lines().any(|line| line.starts_with("b │")),
        "both members speak:\n{stderr}"
    );

    // Two members contending for one address: whichever loses the bind fails,
    // and its failure ends the run rather than leaving half a deployment up.
    let log = tempfile::NamedTempFile::new().expect("a log file");
    let mut child = fsw(&["run", "launch_clash.py"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.as_file().try_clone().expect("the log")))
        .spawn()
        .expect("the CLI spawns");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("the child is waitable") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the clashing deployment did not exit within 30s");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let stderr = std::fs::read_to_string(log.path()).expect("the log reads back");
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.lines().any(|line| {
            (line.starts_with("a │") || line.starts_with("b │"))
                && line.contains("Address already in use")
        }),
        "the losing member's bind error arrives under its prefix:\n{stderr}"
    );
    assert!(
        stderr.contains("member `a` exited with status 1")
            || stderr.contains("member `b` exited with status 1"),
        "the verdict names the member that lost the bind:\n{stderr}"
    );

    // The same members from bundles: the hand-off runs without a source.
    let temp = tempfile::tempdir().expect("a temp dir");
    let mut bundles = Vec::new();
    for ns in ["a", "b"] {
        let bundle = temp.path().join(format!("{ns}.bundle"));
        let output = run(&[
            "package",
            "launch_target.py",
            "--target",
            ns,
            "-o",
            bundle.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        bundles.push(bundle);
    }
    let output = run(&[
        "run",
        bundles[0].to_str().unwrap(),
        bundles[1].to_str().unwrap(),
        "--cycles",
        "20",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.lines().any(|line| line.starts_with("a │"))
            && stderr.lines().any(|line| line.starts_with("b │")),
        "both bundled members speak:\n{stderr}"
    );

    // One selected member is the old single-process path: no children, so
    // nothing is prefixed.
    let output = run(&["run", "launch_target.py", "--target", "a", "--cycles", "20"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains('│'), "a lone member runs here:\n{stderr}");

    // `--serve` names one socket, so it names one member.
    let output = run(&["run", "launch_target.py", "--serve", "127.0.0.1:2260"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        unwrapped(&stderr).contains(
            "`--serve` names one socket; pick the member it applies to with --target (a, b)"
        ),
        "{stderr}"
    );
}
