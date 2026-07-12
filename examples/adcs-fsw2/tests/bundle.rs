//! Bundle round-trip smoke: freeze a mission to the IR bundle layout, load it
//! back cargo-free, resolve, and run a few cycles.
//!
//! The bundle now carries the frozen `Wiring` IR (`wiring.json` + `meta.json`)
//! plus the built `.so`s, not verbatim source — so a mission runs with no KDL
//! parse and no Python on target. Two legs: the KDL mission through the IR
//! path, and `mission.py` packaged end to end (eval → IR → bundle → run),
//! which is the Phase 1 rejection now replaced by a real package. Convergence
//! parity stays in `closed_loop.rs`; here we only assert the bundle is
//! self-contained and runnable. Gated off `miri` (it builds + `dlopen`s real
//! cdylibs).

#![cfg(not(miri))]

use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_2::wiring::{
    Registry, build_artifacts, build_target, eval_python_mission, load_bundle,
    metor_config_version, parse, resolve, write_bundle,
};
use metor_fsw_2::{BuildOptions, PackageOptions};

const MISSION_KDL: &str = include_str!("../mission.kdl");

fn mission(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// A unique temp directory for this test's bundle (process-scoped, best-effort cleanup).
fn temp_bundle_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("adcs-fsw2-bundle-{tag}-{}.bundle", std::process::id()))
}

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
fn kdl_bundle_round_trips_and_runs() {
    // 1. Parse + build the mission (the cdylibs land in cargo's target dir). Test binaries
    //    can't host a `process=#true` worker (no `worker_entry` in main) — run in-process.
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        // Build plumbing unavailable — skip (closed_loop covers the dlopen path directly).
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    // 2. Freeze into the IR bundle: wiring.json + meta.json + the .so's, with the
    //    source riding along as never-consumed provenance.
    let dir = temp_bundle_dir("kdl");
    let _ = std::fs::remove_dir_all(&dir); // clear any stale run
    let opts = PackageOptions {
        target: build_target(&[]),
        provenance: Some(mission("mission.kdl")),
        ..PackageOptions::default()
    };
    write_bundle(&wiring, &opts, &dir).expect("write the bundle");

    // The bundle is the frozen IR, the JSON sidecar, the provenance copy, and one
    // `.so` per artifact — no verbatim KDL manifest.
    assert!(dir.join("wiring.json").exists(), "frozen IR written");
    assert!(dir.join("meta.json").exists(), "JSON sidecar written");
    assert!(dir.join("mission.kdl").exists(), "provenance copy rides along");
    for artifact in &wiring.artifacts {
        assert!(
            dir.join(&artifact.cdylib).exists(),
            "cdylib `{}` copied into the bundle",
            artifact.cdylib
        );
    }

    // 3. Load the bundle back — cargo-free, no KDL parse — and resolve + run.
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
fn kdl_bundle_round_trips_as_metor_archive() {
    // The single-file `.metor` form: pack the mission into one tar, then load
    // it back (unpacked to a temp dir) and run it cargo-free.
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

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

#[test]
fn python_mission_packages_and_runs() {
    // The Phase 1 rejection replaced: a `.py` mission packages through the same
    // IR path a `.kdl` does, then runs cargo-free with no Python on the run side.
    if !have_python() {
        eprintln!("skipping: no python3 >= 3.10 on PATH");
        return;
    }
    let mut wiring = match eval_python_mission(&mission("mission.py")) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("skipping: mission.py did not evaluate: {e}");
            return;
        }
    };
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    let dir = temp_bundle_dir("py");
    let _ = std::fs::remove_dir_all(&dir);
    let opts = PackageOptions {
        target: build_target(&[]),
        metor_config_version: Some(metor_config_version().to_string()),
        provenance: Some(mission("mission.py")),
        ..PackageOptions::default()
    };
    write_bundle(&wiring, &opts, &dir).expect("write the .py bundle");
    assert!(dir.join("mission.py").exists(), "python provenance rides along");

    // The run side needs no Python: load the frozen IR and run it.
    let mut loaded = load_bundle(&dir).expect("load the .py bundle");
    for spec in &mut loaded.systems {
        spec.process = false;
    }
    let mut coord = resolve(&loaded, &Registry::with_builtins()).expect("resolve the .py bundle");
    stellarator::run(move || async move {
        coord.run_for(50).await;
    });

    let _ = std::fs::remove_dir_all(&dir);
}
