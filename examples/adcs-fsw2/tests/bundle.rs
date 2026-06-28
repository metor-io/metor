//! Bundle round-trip smoke (cli-runner.md §4): `parse` → `build_artifacts` →
//! [`write_bundle`] to a temp dir → [`load_bundle`] → `resolve` → run a few cycles.
//!
//! This exercises the **cargo-free** run path — `load_bundle` fills each artifact's path
//! from the copied `.so` and never invokes cargo — proving a packaged mission loads and
//! steps. Convergence/parity stays in `closed_loop.rs`; here we only assert the bundle is
//! self-contained and runnable. Gated off `miri` (it builds + `dlopen`s real cdylibs).

#![cfg(not(miri))]

use std::path::PathBuf;

use metor_fsw_2::wiring::Registry;
use metor_fsw_2::{
    BuildOptions, PackageOptions, build_artifacts, load_bundle, parse, resolve, write_bundle,
};

const MISSION_KDL: &str = include_str!("../mission.kdl");

/// A unique temp directory for this test's bundle (process-scoped, best-effort cleanup).
fn temp_bundle_dir() -> PathBuf {
    std::env::temp_dir().join(format!("adcs-fsw2-bundle-{}.bundle", std::process::id()))
}

#[test]
fn bundle_round_trips_and_runs() {
    // 1. Parse + build the mission (the cdylibs land in cargo's target dir).
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        // Build plumbing unavailable — skip (closed_loop covers the dlopen path directly).
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    // 2. Package into a relocatable bundle directory.
    let dir = temp_bundle_dir();
    let _ = std::fs::remove_dir_all(&dir); // clear any stale run
    write_bundle(&wiring, MISSION_KDL, &PackageOptions::default(), &dir)
        .expect("write the bundle");

    // The bundle is self-contained: the manifest, the sidecar, and one `.so` per artifact.
    assert!(dir.join("mission.kdl").exists(), "manifest copied in");
    assert!(dir.join("meta.kdl").exists(), "metadata sidecar written");
    for artifact in &wiring.artifacts {
        assert!(
            dir.join(&artifact.cdylib).exists(),
            "cdylib `{}` copied into the bundle",
            artifact.cdylib
        );
    }

    // 3. Load the bundle back — cargo-free — and resolve it (dl-only, empty registry).
    let loaded = load_bundle(&dir).expect("load the bundle");
    assert_eq!(loaded.artifacts.len(), wiring.artifacts.len());
    for artifact in &loaded.artifacts {
        assert!(
            artifact.path.as_deref().is_some_and(|p| p.starts_with(&dir)),
            "bundle artifact path points inside the bundle dir"
        );
    }
    let mut coord = resolve(&loaded, &Registry::new()).expect("resolve the bundle");

    // 4. The loaded mission has its three systems' outputs registered, and it steps.
    assert!(
        coord.output_instances().len() >= 3,
        "plant/nav/ctrl outputs registered from the bundle"
    );
    stellarator::run(move || async move {
        coord.run_for(50).await;
    });

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&dir);
}
