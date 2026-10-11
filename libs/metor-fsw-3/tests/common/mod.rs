//! Records and fixtures shared by the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use metor_fsw_3::Record;
use metor_fsw_3::cli::cargo::cargo_build;
use serde::{Deserialize, Serialize};

/// The fixture pack's `Ping` record, declared again here.
///
/// The host matches records by a hash of the name, so this copy is the same
/// record as the pack's.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

/// Returns the `echo-pack` fixture's crate directory.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack")
}

/// Builds the fixture once per test binary and returns its cdylib.
pub fn fixture_lib() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        cargo_build("echo-pack", root).expect("the fixture builds")
    })
}
