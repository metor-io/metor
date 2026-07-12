//! The generated pack stubs stay in sync with the packs, and a stale stub is
//! refused at resolve.
//!
//! `stubgen --check` is the CI gate: it rebuilds the packs, re-renders their
//! `packs/<id>.py`, and byte-compares against the checked-in modules. The
//! staleness test then tampers the recorded manifest hash and confirms
//! `resolve` fails before any dlopen with the regen hint.
//!
//! Both build the pack cdylibs through cargo, so they skip (with a note) where
//! that is unavailable — the same convention as the other tracked suites — and
//! never run under miri.

#![cfg(not(miri))]

use std::path::PathBuf;

use metor_fsw_2::wiring::{
    BuildOptions, LoadError, Registry, StubgenError, StubgenOptions, WiringBuilder,
    build_artifacts, resolve, stubgen,
};

fn mission_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The checked-in `packs/adcs.py` and `packs/seqs.py` are exactly what
/// `metor-fsw stubgen` regenerates — the acceptance idempotence gate.
#[test]
fn stubgen_check_is_clean() {
    let opts = StubgenOptions {
        mission_dir: mission_dir(),
        check: true,
        build: true,
        release: false,
        cargo_args: Vec::new(),
    };
    match stubgen(&opts) {
        Ok(_) => {}
        Err(StubgenError::Build(e)) => {
            eprintln!("skipping: building the packs failed: {e}");
        }
        Err(e) => panic!("checked-in stubs are stale (regenerate with `metor-fsw stubgen`): {e}"),
    }
}

/// A recorded manifest hash that no longer matches the built pack is a stale
/// stub, refused at resolve with [`LoadError::StaleStubs`]. Driven over a
/// systemless wiring so the check fires before the (process) systems pass.
#[test]
fn tampered_hash_is_refused_at_resolve() {
    let mut wiring = WiringBuilder::new()
        .artifact("adcs", "adcs-systems", "adcs_systems")
        .build();
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: building the pack failed: {e}");
        return;
    }
    wiring.artifacts[0].manifest_hash = Some("sha256:not-the-real-hash".to_string());
    match resolve(&wiring, &Registry::with_builtins()) {
        Err(LoadError::StaleStubs { artifact }) => assert_eq!(artifact, "adcs"),
        Err(other) => panic!("expected StaleStubs, got {other:?}"),
        Ok(_) => panic!("expected StaleStubs, resolve succeeded"),
    }
}
