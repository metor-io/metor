//! The subprocess CPython eval path: extension dispatch, a real interpreter
//! running a fixture target against the embedded recorder, native-traceback
//! passthrough on a raising target, and `$METOR_PYTHON` honored.
//!
//! The subprocess cases need a CPython ≥ 3.10 on PATH; they skip with a clear
//! message when none is found. They share one `#[test]` so their `$METOR_PYTHON`
//! mutations never race the other cases.

use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_2::wiring::eval_python_target;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Whether a usable `python3` is on PATH, so the subprocess cases can skip
/// cleanly on a host without one.
fn have_python() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import sys;sys.exit(0 if sys.version_info[:2]>=(3,10) else 1)",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn subprocess_eval() {
    if !have_python() {
        eprintln!("skipping subprocess_eval: no python3 >= 3.10 on PATH");
        return;
    }

    // A trivial two-system static target evaluates to the expected Wiring.
    // SAFETY: single-threaded within this test; no other case races the env.
    unsafe { std::env::remove_var("METOR_PYTHON") };
    let wiring =
        eval_python_target(&fixture("trivial_target.py")).expect("the trivial target evaluates");
    assert_eq!(wiring.systems.len(), 2);
    assert_eq!(wiring.systems[0].name, "a");
    assert_eq!(wiring.systems[1].name, "b");
    assert_eq!(wiring.systems[0].ty.as_deref(), Some("Alarms"));
    assert_eq!(wiring.edges.len(), 1);
    assert_eq!(wiring.edges[0].from, "a");
    assert_eq!(wiring.edges[0].in_, "in_");
    assert_eq!(wiring.coordinator.cycle_rate, 100.0);
    // The recorder anchored the systems back to the target file.
    let src = wiring.systems[0]
        .src
        .as_ref()
        .expect("system carries a source anchor");
    assert!(src.file.as_deref().unwrap().ends_with("trivial_target.py"));
    assert!(src.line > 0);

    // A raising target fails; its native traceback is the (stderr) surface.
    let err =
        eval_python_target(&fixture("raising_target.py")).expect_err("the raising target fails");
    assert!(err.to_string().contains("raising_target.py"));

    // $METOR_PYTHON is honored: a bogus interpreter is a clean error.
    unsafe { std::env::set_var("METOR_PYTHON", "/nonexistent/python-xyz") };
    let err =
        eval_python_target(&fixture("trivial_target.py")).expect_err("a bogus $METOR_PYTHON fails");
    assert!(err.to_string().contains("python-xyz"));
    unsafe { std::env::remove_var("METOR_PYTHON") };
}
