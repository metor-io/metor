//! The generated pack stubs stay in sync with the packs, and a stale stub is
//! refused at resolve.
//!
//! Stubs are venv-only build artifacts here: the target's PEP 517 backend
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
    WiringBuilder, dev_pack_roots, pack_dev, provision_artifacts, refresh_dev_packs, resolve,
    stubgen,
};

fn target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Generating into an `--out-dir` and re-running with `--check` against the
/// same directory is clean: generation is deterministic, and `--check` still
/// functions against a build directory. The converted target has no
/// `[tool.metor.artifacts]` anymore (both packs are dependencies), so the
/// deprecated target-level path is exercised over a fabricated target dir.
#[test]
fn stubgen_out_dir_roundtrips() {
    let out = tempfile::tempdir().unwrap();
    std::fs::write(
        out.path().join("pyproject.toml"),
        "[tool.metor.artifacts]\nseqs = { crate = \"adcs-sequences\", lib = \"adcs_sequences\" }\n",
    )
    .unwrap();
    let opts = |check| StubgenOptions {
        target_dir: out.path().to_path_buf(),
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
/// pack-side sibling of the target round-trip above, keeping the backend's
/// regenerate-on-sync flow deterministic.
#[test]
fn pack_dev_module_is_deterministic() {
    let pack = target_dir().join("systems").join("adcs-systems");
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

/// `run`'s pre-eval refresh covers exactly this target's two system packs —
/// the `metor-config` path source is not a pack and stays untouched — and a
/// refresh lays out module, lib, and sidecar for each, byte-stable on rerun.
#[test]
fn refresh_covers_exactly_the_dev_packs() {
    let dir = target_dir();
    let systems = dir.join("systems");
    assert_eq!(
        dev_pack_roots(&dir),
        vec![systems.join("adcs-sequences"), systems.join("adcs-systems")]
    );

    let reports = match refresh_dev_packs(&dir, &PackDevOptions::default()) {
        Ok(reports) => reports,
        Err(e) => {
            eprintln!("skipping: building the packs failed: {e}");
            return;
        }
    };
    assert_eq!(reports.len(), 2);
    let modules: Vec<Vec<u8>> = reports
        .iter()
        .map(|r| {
            assert!(r.lib_path.is_file(), "lib laid out for {}", r.triple);
            let mut sidecar = r.lib_path.as_os_str().to_owned();
            sidecar.push(".manifest");
            assert!(PathBuf::from(sidecar).is_file(), "sidecar beside the lib");
            assert!(r.module_dir.join("py.typed").is_file());
            std::fs::read(r.module_dir.join("__init__.py")).expect("module written")
        })
        .collect();

    // The `uv run` shape invokes the backend's `pack dev` and then this
    // refresh; the second pass must be a benign no-op.
    let again = refresh_dev_packs(&dir, &PackDevOptions::default()).expect("rerun is a no-op");
    for (report, module) in again.iter().zip(&modules) {
        assert_eq!(
            &std::fs::read(report.module_dir.join("__init__.py")).unwrap(),
            module,
            "refresh regenerates modules byte-identically"
        );
    }
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
