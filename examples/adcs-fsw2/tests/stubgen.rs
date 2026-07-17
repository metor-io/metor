//! The generated pack stubs stay in sync with the packs, and a stale stub is
//! refused at resolve.
//!
//! Stubs are venv-only build artifacts here: the mission's PEP 517 backend
//! (`_backend/metor_build`) regenerates them into `.metor/packs` on every
//! `uv sync`, so there is no checked-in copy to byte-diff. The round-trip test
//! keeps the contract stubgen's `--check` relies on — generation into an
//! `--out-dir` is deterministic and immediately check-clean — and the
//! staleness test tampers the recorded manifest hash and confirms `resolve`
//! fails before any dlopen with the regen hint.
//!
//! Both build the pack cdylibs through cargo, so they skip (with a note) where
//! that is unavailable — the same convention as the other tracked suites — and
//! never run under miri.

#![cfg(not(miri))]

use std::path::PathBuf;

use metor_fsw_2::wiring::{
    BuildOptions, LoadErrorKind, PackDevOptions, Registry, StubgenError, StubgenOptions,
    WiringBuilder, pack_dev, provision_artifacts, resolve, stubgen,
};

fn mission_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Generating into an `--out-dir` and re-running with `--check` against the
/// same directory is clean: the backend's regenerate-on-sync flow is
/// deterministic, and `--check` still functions against a build directory.
/// Mission-level stubgen covers the legacy `seqs` artifact only; `adcs` is a
/// path-source pack (see `pack_dev_module_is_deterministic`).
#[test]
fn stubgen_out_dir_roundtrips() {
    let out = tempfile::tempdir().unwrap();
    let opts = |check| StubgenOptions {
        mission_dir: mission_dir(),
        out_dir: Some(out.path().join("packs")),
        check,
        build: true,
        release: false,
        cargo_args: Vec::new(),
    };
    match stubgen(&opts(false)) {
        Ok(report) => {
            let expected = ["__init__.py", "py.typed", "seqs.py"];
            for name in expected {
                assert!(
                    out.path().join("packs").join(name).exists(),
                    "missing generated {name}"
                );
            }
            assert_eq!(report.modules.len(), expected.len());
        }
        Err(StubgenError::Build(e)) => {
            eprintln!("skipping: building the packs failed: {e}");
            return;
        }
        Err(e) => panic!("stubgen into out-dir failed: {e}"),
    }
    stubgen(&opts(true)).expect("check clean immediately after generate");
}

/// The adcs pack's `pack dev` layout regenerates byte-identically — the
/// pack-side sibling of the mission round-trip above, keeping the backend's
/// regenerate-on-sync flow deterministic.
#[test]
fn pack_dev_module_is_deterministic() {
    let pack = mission_dir().join("systems").join("adcs-systems");
    let module = pack.join(".metor").join("adcs_pack").join("__init__.py");
    let first = match pack_dev(&pack, &PackDevOptions::default()) {
        Ok(_) => std::fs::read(&module).expect("module written"),
        Err(e) => {
            eprintln!("skipping: pack dev failed: {e}");
            return;
        }
    };
    pack_dev(&pack, &PackDevOptions::default()).expect("relayout is a cargo no-op");
    assert_eq!(
        first,
        std::fs::read(&module).unwrap(),
        "pack dev regenerates the module byte-identically"
    );
}

/// A recorded manifest hash that no longer matches the built pack is a stale
/// stub, refused at resolve with [`LoadErrorKind::StaleStubs`]. Driven over a
/// systemless wiring so the check fires before the (process) systems pass.
#[test]
fn tampered_hash_is_refused_at_resolve() {
    let mut wiring = WiringBuilder::new()
        .artifact("adcs", "adcs-systems", "adcs_systems")
        .build();
    if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: building the pack failed: {e}");
        return;
    }
    wiring.artifacts[0].manifest_hash = Some("sha256:not-the-real-hash".to_string());
    match resolve(&wiring, &Registry::with_builtins()) {
        Err(e) => match e.kind {
            LoadErrorKind::StaleStubs { artifact } => assert_eq!(artifact, "adcs"),
            other => panic!("expected StaleStubs, got {other:?}"),
        },
        Ok(_) => panic!("expected StaleStubs, resolve succeeded"),
    }
}
