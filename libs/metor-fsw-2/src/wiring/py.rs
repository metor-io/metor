//! Evaluate a `.py` target by running it under a subprocess CPython.
//!
//! The target file imports the `metor_config` recorder, builds a target, and
//! at exit writes the serialized [`Wiring`] IR. This module resolves an
//! interpreter, materializes the embedded recorder, runs the file, and ingests
//! the JSON it produced — landing a `Wiring` the shared
//! [`resolve`](super::resolve) consumes, exactly like the Rust
//! [`WiringBuilder`](super::WiringBuilder).
//!
//! Errors keep their native surface: a Python-level failure prints CPython's
//! own traceback (the target file is just a script, so `pdb` and IDE debuggers
//! work), and the host's [`LoadError`] anchors resolve-time failures via the
//! `src` fields the recorder fills.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use miette::{IntoDiagnostic, miette};

use super::model::Wiring;

/// The `metor_config` package, embedded file-by-file so the `metor-fsw` binary
/// carries its own recorder. Materialized to a temp dir at eval time unless
/// `$METOR_CONFIG_PY` points at a live checkout.
const EMBEDDED_PACKAGE: &[(&str, &str)] = &[
    (
        "metor_config/__init__.py",
        include_str!("../../python/metor-config/metor_config/__init__.py"),
    ),
    // The `py.typed` marker travels too, so a materialized recorder is a typed
    // package pyright checks against.
    (
        "metor_config/py.typed",
        include_str!("../../python/metor-config/metor_config/py.typed"),
    ),
];

/// `true` if `path` is a Python target (a `.py` file the CLI evaluates).
pub fn is_python_target(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "py")
}

/// The embedded recorder's `__version__`, scanned from the embedded source —
/// the version this binary pairs with, used to derive the `metor-config`
/// compatible-range pin stamped into pack wheels.
pub(super) fn metor_config_version() -> &'static str {
    let source = EMBEDDED_PACKAGE[0].1;
    source
        .lines()
        .find_map(|line| line.strip_prefix("__version__ = \""))
        .and_then(|rest| rest.split('"').next())
        .expect("embedded metor_config declares __version__")
}

/// Evaluate a `.py` target into a [`Wiring`].
///
/// Resolves an interpreter (`$METOR_PYTHON` → `$VIRTUAL_ENV/bin/python` →
/// `python3`, requiring ≥ 3.10), runs the file with the recorder on
/// `PYTHONPATH` and `$METOR_IR_OUT` pointed at a temp file, then reads back the
/// IR. A non-zero exit passes the child's stderr through verbatim.
pub fn eval_python_target(path: &Path) -> miette::Result<Wiring> {
    let python = resolve_interpreter()?;

    // Recorder source, in preference order: a live checkout via
    // `$METOR_CONFIG_PY`; the interpreter's own environment (a venv that
    // installed `metor_config` — anything on `PYTHONPATH` would shadow
    // site-packages, so the embedded copy must stay off the path); else the
    // embedded copy materialized to a per-run temp dir (dropped when this
    // returns) so a bare `metor-fsw run` needs no venv.
    let materialized;
    let mut roots: Vec<PathBuf> = Vec::new();
    match std::env::var_os("METOR_CONFIG_PY") {
        Some(dir) => roots.push(PathBuf::from(dir)),
        None if imports_metor_config(&python) => {}
        None => {
            materialized = materialize_recorder()?;
            roots.push(materialized.path().to_path_buf());
        }
    }

    let ir_file = tempfile::Builder::new()
        .prefix("metor-ir-")
        .suffix(".json")
        .tempfile()
        .into_diagnostic()?;

    // A target whose stubs are venv-only build artifacts keeps them in
    // `.metor/packs` beside the target file, and each path-source pack
    // dependency keeps its module in `<pack>/.metor`; putting those build
    // dirs on the path lets a bare `metor-fsw run` (or a Rust test) evaluate
    // the target without the venv active. In a venv they resolve there
    // first-equal — the contents are the same artifacts the backends expose.
    if let Some(target_dir) = path.parent() {
        let stubs = target_dir.join(".metor");
        if stubs.is_dir() {
            roots.push(stubs);
        }
        roots.extend(pack_stub_roots(target_dir));
    }

    let status = Command::new(&python)
        .arg(path)
        .env("PYTHONPATH", prepend_pythonpath(&roots))
        .env("METOR_IR_OUT", ir_file.path())
        .env(
            "METOR_EXPECTED_ABI",
            crate::abi::FSW_ABI_VERSION.to_string(),
        )
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
        .map_err(|e| miette!("target `{}` produced no IR: {e}", path.display()))?;
    ingest_ir(&json, path)
}

/// Deserialize the emitted IR. The IR version is checked at resolve time.
fn ingest_ir(json: &str, path: &Path) -> miette::Result<Wiring> {
    let raw: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| miette!("target `{}` emitted invalid JSON: {e}", path.display()))?;

    serde_json::from_value(raw).map_err(|e| {
        miette!(
            "target `{}` emitted an IR this host cannot read: {e}",
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

/// `true` if `python` can already import `metor_config` from its own
/// environment (a venv install), in which case the embedded copy stays off
/// `PYTHONPATH` so the environment's version wins.
fn imports_metor_config(python: &Path) -> bool {
    Command::new(python)
        .args(["-c", "import metor_config"])
        // The probe asks about the interpreter's own environment; an
        // inherited PYTHONPATH would answer for someone else.
        .env_remove("PYTHONPATH")
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The path-source dependency roots of the target-adjacent
/// `pyproject.toml`: each `[tool.uv.sources] <dist> = {{ path = … }}` entry,
/// target-relative, sorted. Direct sources only — a pack's own path-source
/// dependencies are not chased, the same limitation as the `PYTHONPATH` scan,
/// so refresh and import visibility stay symmetric. Best-effort by design —
/// no pyproject or no sources means "no roots".
pub(super) fn path_source_roots(target_dir: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(target_dir.join("pyproject.toml")) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(sources) = doc
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("sources"))
        .and_then(|s| s.as_table())
    else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = sources
        .values()
        .filter_map(|entry| entry.get("path")?.as_str())
        .map(|rel| target_dir.join(rel))
        .collect();
    roots.sort();
    roots
}

/// The `.metor` build dirs of the target's path-source pack dependencies:
/// each [`path_source_roots`] entry whose target carries one. Best-effort by
/// design — a real dependency problem surfaces as Python's own
/// `ModuleNotFoundError` with the target traceback.
fn pack_stub_roots(target_dir: &Path) -> Vec<PathBuf> {
    path_source_roots(target_dir)
        .into_iter()
        .map(|root| root.join(".metor"))
        .filter(|dir| dir.is_dir())
        .collect()
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

/// `roots` prepended (in order) to any inherited `PYTHONPATH`, so the
/// recorder wins.
fn prepend_pythonpath(roots: &[PathBuf]) -> std::ffi::OsString {
    let mut paths = roots.to_vec();
    if let Some(existing) = std::env::var_os("PYTHONPATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).expect("valid PYTHONPATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path-source scan keeps exactly the `path = …` entries, sorted, and
    /// degrades to empty on a missing or garbled pyproject.
    #[test]
    fn path_source_roots_reads_uv_sources() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(path_source_roots(tmp.path()).is_empty(), "no pyproject");

        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(&pyproject, "[project\n").unwrap();
        assert!(
            path_source_roots(tmp.path()).is_empty(),
            "garbled pyproject"
        );

        std::fs::write(
            &pyproject,
            "[project]\nname = \"m\"\nversion = \"0.0.0\"\n\
             [tool.uv.sources]\nb-pack = { path = \"systems/b\" }\n\
             a-pack = { path = \"systems/a\" }\npinned = { version = \"1.0\" }\n",
        )
        .unwrap();
        assert_eq!(
            path_source_roots(tmp.path()),
            vec![tmp.path().join("systems/a"), tmp.path().join("systems/b"),]
        );
    }

    /// The stub roots are the path sources' `.metor` dirs, existing ones only.
    #[test]
    fn pack_stub_roots_wants_metor_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"m\"\nversion = \"0.0.0\"\n\
             [tool.uv.sources]\na = { path = \"a\" }\nb = { path = \"b\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("a/.metor")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        assert_eq!(
            pack_stub_roots(tmp.path()),
            vec![tmp.path().join("a/.metor")]
        );
    }
}
