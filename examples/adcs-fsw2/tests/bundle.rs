//! Bundle round-trip smoke: freeze `mission.py` to the IR bundle layout, load it
//! back cargo-free, resolve, and run a few cycles.
//!
//! The bundle carries the frozen `Wiring` IR (`wiring.json` + `meta.json`) plus
//! the built `.so`s, not verbatim source — so a mission runs with no Python and
//! no config parse on target. Two legs, both packaged from `mission.py` (evaluate
//! → IR → bundle → run): the directory bundle and the single-file `.metor`
//! archive. Convergence parity stays in `closed_loop.rs`; here we only assert the
//! bundle is self-contained and runnable. Gated off `miri` (it builds + `dlopen`s
//! real cdylibs) and skipped without a CPython ≥ 3.10 to evaluate `mission.py`.

#![cfg(not(miri))]

use std::path::{Path, PathBuf};

use metor_fsw_2::wiring::{
    Registry, Wiring, build_artifacts, build_target, eval_python_mission, load_bundle, resolve,
    write_bundle,
};
use metor_fsw_2::{BuildOptions, PackageOptions};

mod common;

fn mission(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// A unique temp directory for this test's bundle (process-scoped, best-effort cleanup).
fn temp_bundle_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("adcs-fsw2-bundle-{tag}-{}.bundle", std::process::id()))
}

/// Evaluate `mission.py` into a built `Wiring`, in-process (test binaries can't host a
/// `process=#true` worker). `None` — skip, not fail — when Python or the build plumbing is
/// unavailable (offline/sandboxed cargo, no CPython ≥ 3.10).
fn eval_and_build() -> Option<Wiring> {
    if !common::ensure_stubs() {
        return None;
    }
    let mut wiring = match eval_python_mission(&mission("mission.py")) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("skipping: mission.py did not evaluate: {e}");
            return None;
        }
    };
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return None;
    }
    Some(wiring)
}

#[test]
fn python_mission_packages_and_runs() {
    // A `.py` mission packages through the IR path, then runs cargo-free with no Python on
    // the run side: load the frozen IR and run it.
    let Some(wiring) = eval_and_build() else {
        return;
    };

    let dir = temp_bundle_dir("py");
    let _ = std::fs::remove_dir_all(&dir);
    let opts = PackageOptions {
        target: build_target(&[]),
        provenance: Some(mission("mission.py")),
        ..PackageOptions::default()
    };
    write_bundle(&wiring, &opts, &dir).expect("write the bundle");

    // The bundle is the frozen IR, the JSON sidecar, the provenance copy, and one `.so` per
    // artifact — no verbatim source manifest.
    assert!(dir.join("wiring.json").exists(), "frozen IR written");
    assert!(dir.join("meta.json").exists(), "JSON sidecar written");
    assert!(dir.join("mission.py").exists(), "python provenance rides along");
    for artifact in &wiring.artifacts {
        assert!(
            dir.join(&artifact.cdylib).exists(),
            "cdylib `{}` copied into the bundle",
            artifact.cdylib
        );
    }

    let mut loaded = load_bundle(&dir).expect("load the bundle");
    for spec in &mut loaded.systems {
        spec.process = false;
    }
    assert_eq!(loaded.artifacts.len(), wiring.artifacts.len());
    for artifact in &loaded.artifacts {
        assert!(
            artifact.path.as_deref().is_some_and(|p| p.starts_with(&dir)),
            "bundle artifact path points inside the bundle dir"
        );
    }
    let mut coord = resolve(&loaded, &Registry::with_builtins()).expect("resolve the bundle");
    assert!(
        coord.output_instances().len() >= 3,
        "plant/nav/ctrl outputs registered from the bundle"
    );
    stellarator::run(move || async move {
        coord.run_for(50).await;
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_mission_round_trips_as_metor_archive() {
    // The single-file `.metor` form: pack the mission into one tar, then load it back
    // (unpacked to a temp dir) and run it cargo-free.
    let Some(wiring) = eval_and_build() else {
        return;
    };

    let archive = std::env::temp_dir().join(format!("adcs-fsw2-{}.metor", std::process::id()));
    let _ = std::fs::remove_file(&archive);
    let opts = PackageOptions {
        target: build_target(&[]),
        ..PackageOptions::default()
    };
    write_bundle(&wiring, &opts, &archive).expect("write the .metor archive");
    assert!(archive.is_file(), "single-file bundle written");

    let mut loaded = load_bundle(&archive).expect("load the .metor archive");
    for spec in &mut loaded.systems {
        spec.process = false;
    }
    let mut coord = resolve(&loaded, &Registry::with_builtins()).expect("resolve the .metor bundle");
    assert!(coord.output_instances().len() >= 3, "systems registered from the archive");
    stellarator::run(move || async move {
        coord.run_for(50).await;
    });

    let _ = std::fs::remove_file(&archive);
}
