use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn requested_target_parses_both_forms() {
    assert_eq!(requested_target(&args(&[])), None);
    assert_eq!(requested_target(&args(&["--release"])), None);
    assert_eq!(
        requested_target(&args(&["--target", "thumbv7em-none-eabihf"])),
        Some("thumbv7em-none-eabihf")
    );
    assert_eq!(
        requested_target(&args(&["--target=thumbv7em-none-eabihf"])),
        Some("thumbv7em-none-eabihf")
    );
    // The last occurrence wins, matching cargo.
    assert_eq!(
        requested_target(&args(&["--target=a", "--target", "b"])),
        Some("b")
    );
    // A dangling `--target` names no triple.
    assert_eq!(requested_target(&args(&["--target"])), None);
}

#[test]
fn strip_target_removes_flag_and_value() {
    assert_eq!(
        strip_target_args(&args(&[
            "--features",
            "x",
            "--target",
            "a",
            "--target=b",
            "-v"
        ])),
        args(&["--features", "x", "-v"])
    );
    assert_eq!(
        strip_target_args(&args(&["--target"])),
        Vec::<String>::new()
    );
}

#[test]
fn cross_is_a_non_host_target() {
    assert!(!is_cross(&args(&[])), "no --target is a native build");
    // No real toolchain targets this triple, so it can never be the host.
    assert!(is_cross(&args(&["--target", "thumbv7em-none-eabihf"])));
    if let Some(host) = host_triple() {
        assert!(
            !is_cross(&args(&["--target", &host])),
            "--target naming the host is still a native build"
        );
    }
}

/// The freshness gate for the divergence comparison: an existing sidecar
/// counts only while it is at least as new as the library.
#[test]
#[cfg(not(miri))]
fn fresh_sidecar_respects_mtimes() {
    let dir = tempfile::tempdir().unwrap();
    let so = dir.path().join("libpack.so");
    std::fs::write(&so, b"library").unwrap();
    let sidecar = crate::dl::manifest_sidecar_path(&so);

    assert_eq!(fresh_sidecar(&sidecar, &so), None, "no sidecar yet");

    // Written after the library: fresh, so its bytes are compared.
    std::fs::write(&sidecar, b"manifest").unwrap();
    assert_eq!(fresh_sidecar(&sidecar, &so), Some(b"manifest".to_vec()));

    // Predating the library: stale, so it is overwritten, not compared.
    let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&sidecar)
        .unwrap()
        .set_modified(earlier)
        .unwrap();
    assert_eq!(fresh_sidecar(&sidecar, &so), None);
}

// -----------------------------------------------------------------------
// End-to-end over the dl fixture pack. These share the fixture crate's
// sidecar in the target dir, so they serialize on one lock. A nested
// cargo build failure fails the test.
// -----------------------------------------------------------------------

// Shared with the dl and pack-codegen fixture tests: all three build and
// describe the same fixture pack into one target-dir sidecar.
#[cfg(not(miri))]
use crate::dl::FIXTURE_LOCK;

#[cfg(not(miri))]
fn fixture_wiring() -> Wiring {
    super::super::WiringBuilder::new()
        .artifact(
            "fixture",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
        .build()
}

/// The sidecar is the exact bytes `fsw_pack_describe` reports, and
/// decodes to the fixture's entries.
#[test]
#[cfg(not(miri))]
fn sidecar_matches_describe() {
    let _guard = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wiring = fixture_wiring();
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("required fixture builds");
    let so = wiring.artifacts[0].path.clone().expect("path filled");
    let sidecar = crate::dl::manifest_sidecar_path(&so);
    let bytes = std::fs::read(&sidecar).expect("sidecar written next to the .so");
    let described = crate::dl::describe_raw(&so).expect("in-process describe");
    assert_eq!(bytes, described, "sidecar bytes ≡ fsw_pack_describe bytes");
}

/// The cross arm verifies arch-independence rather than assuming it: an
/// up-to-date target sidecar whose bytes differ from the host-described
/// manifest is a hard error, and matching bytes pass. Driven with
/// `cross = true` over the host build itself, since a real foreign
/// `--target` is not buildable in this test environment, but the code
/// path is the same: build the host twin, describe it, compare, write.
#[test]
#[cfg(not(miri))]
fn cross_divergence_is_a_hard_error() {
    let _guard = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wiring = fixture_wiring();
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("required fixture builds");
    let artifact = &wiring.artifacts[0];
    let so = artifact.path.clone().expect("path filled");
    let sidecar = crate::dl::manifest_sidecar_path(&so);
    let opts = BuildOptions::default();

    // Plant a fresh-but-divergent "target" sidecar; the host-described
    // bytes must refuse to replace it.
    std::fs::write(&sidecar, b"divergent").unwrap();
    let err = write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &so, &opts, true)
        .expect_err("a fresh divergent sidecar is refused");
    assert!(
        matches!(err, BuildError::ManifestDivergence { .. }),
        "{err}"
    );

    // Identical bytes pass the comparison and are rewritten in place.
    let described = crate::dl::describe_raw(&so).expect("in-process describe");
    std::fs::write(&sidecar, &described).unwrap();
    write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &so, &opts, true)
        .expect("a matching sidecar is accepted");
    assert_eq!(std::fs::read(&sidecar).unwrap(), described);
}

/// `manifest_sidecar: false` builds the library and writes nothing next
/// to it.
#[test]
#[cfg(not(miri))]
fn sidecar_opt_out_writes_nothing() {
    let _guard = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let opts = BuildOptions {
        manifest_sidecar: false,
        ..BuildOptions::default()
    };
    let mut wiring = fixture_wiring();
    provision_artifacts(&mut wiring, &opts).expect("required fixture builds");
    let so = wiring.artifacts[0].path.clone().expect("path filled");
    let sidecar = crate::dl::manifest_sidecar_path(&so);
    // Another test (or an earlier run) may have left one; the assertion
    // is that *this* build does not produce it.
    let _ = std::fs::remove_file(&sidecar);
    let mut again = fixture_wiring();
    provision_artifacts(&mut again, &opts).expect("rebuild is a cargo no-op");
    assert!(!sidecar.exists(), "opted out, so no sidecar is written");
}

/// A prebuilt artifact is a selection, not a build: the host triple picks
/// `<dir>/<triple>/<cdylib>`, the sidecar rides adjacent for the resolve
/// gate, and a triple the directory does not ship errors naming the ones
/// it does. Laid out from the fixture's host build, exactly the shape
/// `pack dev` and an installed pack wheel produce.
#[test]
#[cfg(not(miri))]
fn prebuilt_selects_by_triple() {
    let _guard = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut built = fixture_wiring();
    provision_artifacts(&mut built, &BuildOptions::default()).expect("required fixture builds");
    let so = built.artifacts[0].path.clone().expect("path filled");
    let host = host_triple().expect("host triple determinable");

    // Lay out `<libs>/<host-triple>/{cdylib, sidecar}`.
    let tmp = tempfile::tempdir().unwrap();
    let libs = tmp.path().join("libs");
    let triple_dir = libs.join(&host);
    std::fs::create_dir_all(&triple_dir).unwrap();
    let dst = triple_dir.join(so.file_name().unwrap());
    std::fs::copy(&so, &dst).unwrap();
    std::fs::copy(
        crate::dl::manifest_sidecar_path(&so),
        crate::dl::manifest_sidecar_path(&dst),
    )
    .unwrap();

    let mut wiring = fixture_wiring();
    wiring.artifacts[0].prebuilt_dir = Some(libs.clone());
    provision_artifacts(&mut wiring, &BuildOptions::default())
        .expect("prebuilt selection needs no cargo");
    assert_eq!(wiring.artifacts[0].path.as_deref(), Some(dst.as_path()));

    // A triple the directory does not ship is a clean error listing the
    // triples it does.
    let mut cross = fixture_wiring();
    cross.artifacts[0].prebuilt_dir = Some(libs);
    let opts = BuildOptions {
        extra_args: vec!["--target".into(), "riscv64gc-unknown-linux-gnu".into()],
        ..BuildOptions::default()
    };
    let err = provision_artifacts(&mut cross, &opts).expect_err("missing triple is refused");
    match err {
        BuildError::PrebuiltMissing {
            artifact,
            triple,
            available,
            ..
        } => {
            assert_eq!(artifact, "fixture");
            assert_eq!(triple, "riscv64gc-unknown-linux-gnu");
            assert_eq!(available, vec![host]);
        }
        other => panic!("expected PrebuiltMissing, got {other}"),
    }
}

/// `locate_artifacts` finds an already-built library without cargo, and a
/// never-built one is a hard `NotBuilt`, the `run --no-build` contract.
#[test]
#[cfg(not(miri))]
fn locate_artifacts_finds_built_and_refuses_missing() {
    let _guard = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut built = fixture_wiring();
    provision_artifacts(&mut built, &BuildOptions::default()).expect("required fixture builds");
    let so = built.artifacts[0].path.clone().expect("path filled");

    // Locating from the crate dir walks up to the same workspace target
    // dir the build wrote into.
    let mut wiring = fixture_wiring();
    locate_artifacts(&mut wiring, Path::new("."), false).expect("built library is found");
    assert_eq!(
        wiring.artifacts[0].path.as_ref().unwrap().file_name(),
        so.file_name()
    );

    // A library that was never built is a hard error naming the crate.
    let mut missing = super::super::WiringBuilder::new()
        .artifact("missing", "no-such-crate", "metor_fsw_2_never_built")
        .build();
    let err = locate_artifacts(&mut missing, Path::new("."), false)
        .expect_err("a never-built library is refused");
    assert!(matches!(err, BuildError::NotBuilt { .. }), "{err}");
}

/// The compile-time triple and cargo's own view of the host agree, so the
/// cargo-free fallback never changes what a bundle records or a prebuilt
/// selection picks.
#[test]
fn compiled_triple_matches_cargo() {
    let (Some(compiled), Some(cargo)) = (compiled_triple(), cargo_host_triple()) else {
        eprintln!("skipping: platform unlisted or cargo unavailable");
        return;
    };
    assert_eq!(compiled, cargo);
}
