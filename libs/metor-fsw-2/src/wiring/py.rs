//! Evaluate a `.py` mission by running it under a subprocess CPython.
//!
//! The mission file imports the `metor_config` recorder, builds a mission, and
//! at exit writes the serialized [`Wiring`] IR. This module resolves an
//! interpreter, materializes the embedded recorder, runs the file, and ingests
//! the JSON it produced — landing a `Wiring` the shared
//! [`resolve`](super::resolve) consumes, exactly like the Rust
//! [`WiringBuilder`](super::WiringBuilder).
//!
//! Errors keep their native surface: a Python-level failure prints CPython's
//! own traceback (the mission file is just a script, so `pdb` and IDE debuggers
//! work), and the host's [`LoadError`] anchors resolve-time failures via the
//! `src` fields the recorder fills.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use miette::{IntoDiagnostic, miette};

use super::model::{IR_VERSION, Wiring};

/// The `metor_config` package, embedded file-by-file so the `metor-fsw` binary
/// carries its own recorder. Materialized to a temp dir at eval time unless
/// `$METOR_CONFIG_PY` points at a live checkout.
const EMBEDDED_PACKAGE: &[(&str, &str)] = &[
    (
        "metor_config/__init__.py",
        include_str!("../../python/metor_config/__init__.py"),
    ),
    // The `py.typed` marker travels too, so a materialized recorder is a typed
    // package pyright checks against.
    (
        "metor_config/py.typed",
        include_str!("../../python/metor_config/py.typed"),
    ),
];

/// The embedded recorder's `__version__`, compared against the version the
/// emitted IR carries. Kept in lockstep with `python/metor_config/__init__.py`.
const EMBEDDED_METOR_CONFIG_VERSION: &str = "0.3.0";

/// `true` if `path` is a Python mission (a `.py` file the CLI evaluates).
pub fn is_python_mission(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "py")
}

/// The embedded `metor_config` recorder version this host evaluates Python
/// missions with, recorded as bundle provenance.
pub fn metor_config_version() -> &'static str {
    EMBEDDED_METOR_CONFIG_VERSION
}

/// Evaluate a `.py` mission into a [`Wiring`].
///
/// Resolves an interpreter (`$METOR_PYTHON` → `$VIRTUAL_ENV/bin/python` →
/// `python3`, requiring ≥ 3.10), runs the file with the recorder on
/// `PYTHONPATH` and `$METOR_IR_OUT` pointed at a temp file, then reads back the
/// IR. A non-zero exit passes the child's stderr through verbatim.
pub fn eval_python_mission(path: &Path) -> miette::Result<Wiring> {
    let python = resolve_interpreter()?;

    // Recorder source: a live checkout via $METOR_CONFIG_PY, else the embedded
    // copy materialized to a per-run temp dir (dropped when this returns).
    let materialized;
    let pythonpath_root: PathBuf = match std::env::var_os("METOR_CONFIG_PY") {
        Some(dir) => PathBuf::from(dir),
        None => {
            materialized = materialize_recorder()?;
            materialized.path().to_path_buf()
        }
    };

    let ir_file = tempfile::Builder::new()
        .prefix("metor-ir-")
        .suffix(".json")
        .tempfile()
        .into_diagnostic()?;

    let status = Command::new(&python)
        .arg(path)
        .env("PYTHONPATH", prepend_pythonpath(&pythonpath_root))
        .env("METOR_IR_OUT", ir_file.path())
        .status()
        .map_err(|e| miette!("failed to run `{}`: {e}", python.display()))?;

    if !status.success() {
        // The child wrote its traceback straight to our stderr (inherited);
        // that native surface is the tier-1 error, so add nothing to it.
        return Err(miette!(
            "evaluating `{}` failed (exit {})",
            path.display(),
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
        ));
    }

    let json = std::fs::read_to_string(ir_file.path())
        .map_err(|e| miette!("mission `{}` produced no IR: {e}", path.display()))?;
    ingest_ir(&json, path)
}

/// Deserialize the emitted IR, checking versions before handing off to resolve.
fn ingest_ir(json: &str, path: &Path) -> miette::Result<Wiring> {
    let raw: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| miette!("mission `{}` emitted invalid JSON: {e}", path.display()))?;

    // A friendlier message than the resolve-time `IrVersionMismatch`, naming
    // both versions; resolve's own check still fires as the backstop.
    if let Some(v) = raw.get("ir_version").and_then(serde_json::Value::as_u64)
        && v != u64::from(IR_VERSION)
    {
        return Err(miette!(
            "mission `{}` was emitted for IR version {v}, but this host speaks {IR_VERSION}; \
             update `metor_config`",
            path.display(),
        ));
    }

    // Version skew is a warning, not an error: $METOR_CONFIG_PY makes it
    // legitimate in development.
    if let Some(v) = raw
        .get("metor_config_version")
        .and_then(serde_json::Value::as_str)
        && v != EMBEDDED_METOR_CONFIG_VERSION
    {
        eprintln!(
            "warning: mission emitted by metor_config {v}, host embeds \
             {EMBEDDED_METOR_CONFIG_VERSION}"
        );
    }

    serde_json::from_str(json).map_err(|e| {
        miette!(
            "mission `{}` emitted an IR this host cannot read: {e}",
            path.display()
        )
    })
}

/// Resolve and validate a CPython ≥ 3.10.
fn resolve_interpreter() -> miette::Result<PathBuf> {
    let candidate = if let Some(explicit) = std::env::var_os("METOR_PYTHON") {
        PathBuf::from(explicit)
    } else if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        let p = PathBuf::from(venv).join("bin").join("python");
        if p.exists() {
            p
        } else {
            PathBuf::from("python3")
        }
    } else {
        PathBuf::from("python3")
    };

    let out = Command::new(&candidate)
        .args([
            "-c",
            "import sys;print('%d.%d'%sys.version_info[:2]);\
             sys.exit(0 if sys.version_info[:2]>=(3,10) else 1)",
        ])
        .output()
        .map_err(|e| {
            miette!(
                "could not run Python interpreter `{}`: {e}\n\
                 set $METOR_PYTHON to a CPython ≥ 3.10",
                candidate.display()
            )
        })?;

    if !out.status.success() {
        let found = String::from_utf8_lossy(&out.stdout);
        return Err(miette!(
            "Python interpreter `{}` is too old ({}); metor_config needs ≥ 3.10",
            candidate.display(),
            found.trim(),
        ));
    }
    Ok(candidate)
}

/// Write the embedded recorder into a fresh temp dir.
fn materialize_recorder() -> miette::Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("metor-config-")
        .tempdir()
        .into_diagnostic()?;
    for (rel, contents) in EMBEDDED_PACKAGE {
        let target = dir.path().join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).into_diagnostic()?;
        }
        let mut f = std::fs::File::create(&target).into_diagnostic()?;
        f.write_all(contents.as_bytes()).into_diagnostic()?;
    }
    Ok(dir)
}

/// `root` prepended to any inherited `PYTHONPATH`, so the recorder wins.
fn prepend_pythonpath(root: &Path) -> std::ffi::OsString {
    match std::env::var_os("PYTHONPATH") {
        Some(existing) => {
            let mut paths = vec![root.to_path_buf()];
            paths.extend(std::env::split_paths(&existing));
            std::env::join_paths(paths).expect("valid PYTHONPATH")
        }
        None => root.as_os_str().to_owned(),
    }
}
