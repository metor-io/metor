//! The Python toolchain dists' metadata mirrors constants owned by this
//! crate; these tests are the drift guards (see `docs/packaging.md`,
//! §9.1). Plain string checks over the committed `pyproject.toml`s — no
//! TOML parse, so the pins stay greppable exactly as asserted.

use metor_fsw_2::abi::FSW_ABI_VERSION;

const FSW_PYPROJECT: &str = include_str!("../python/metor-fsw/pyproject.toml");
const ABI_PYPROJECT: &str = include_str!("../python/metor-fsw-abi/pyproject.toml");

/// The `metor-fsw` dist version is the crate version.
#[test]
fn fsw_dist_version_matches_crate() {
    let expected = format!("version = \"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(
        FSW_PYPROJECT.contains(&expected),
        "python/metor-fsw/pyproject.toml must declare {expected}"
    );
}

/// The `metor-fsw` dist pins the marker dist to this crate's ABI, and the
/// marker dist's version *is* the ABI number.
#[test]
fn abi_marker_matches_fsw_abi_version() {
    let pin = format!("\"metor-fsw-abi=={FSW_ABI_VERSION}\"");
    assert!(
        FSW_PYPROJECT.contains(&pin),
        "python/metor-fsw/pyproject.toml must depend on {pin}"
    );
    let version = format!("version = \"{FSW_ABI_VERSION}\"");
    assert!(
        ABI_PYPROJECT.contains(&version),
        "python/metor-fsw-abi/pyproject.toml must declare {version}"
    );
}
