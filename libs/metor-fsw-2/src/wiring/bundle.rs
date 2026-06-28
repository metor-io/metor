//! The relocatable **bundle** — `package`'s output and one of `run`'s inputs
//! (cli-runner.md §4).
//!
//! A bundle is a plain directory carrying everything a mission needs to run with **no
//! source tree and no cargo**: the wiring manifest (`mission.kdl`), a metadata sidecar
//! (`meta.kdl`), and the built `cdylib`s copied in next to them. It is
//! **platform-specific** (it holds compiled `.so`s), so it is built for, and run on, one
//! target arch.
//!
//! [`write_bundle`] produces it from a built [`Wiring`] + the source KDL text;
//! [`load_bundle`] reads one back into a [`Wiring`] whose artifacts point at the copied
//! `.so`s — the cargo-free counterpart to [`build_artifacts`](super::build_artifacts).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kdl::KdlDocument;
use thiserror::Error;

use crate::abi::FSW_ABI_VERSION;

use super::model::Wiring;
use super::parse;

/// The metadata sidecar's file name within a bundle.
const META_FILE: &str = "meta.kdl";
/// The wiring manifest's file name within a bundle.
const MISSION_FILE: &str = "mission.kdl";

/// Options for [`write_bundle`].
#[derive(Clone, Debug, Default)]
pub struct PackageOptions {
    /// Record `profile "release"` in `meta.kdl` (otherwise `"debug"`). Purely
    /// informational — it documents how the bundled `.so`s were built.
    pub release: bool,
}

/// A bundle read/write failure. Each is a clean error — packaging or loading never
/// panics the caller.
#[derive(Debug, Error)]
pub enum BundleError {
    /// A filesystem operation failed.
    #[error("bundle I/O error at {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// An artifact had no resolved `path` at package time (run the build driver first).
    #[error("cannot package artifact `{artifact}`: it has no built `.so` (build it first)")]
    NotBuilt {
        /// The artifact id.
        artifact: String,
    },
    /// A bundle's `meta.kdl` could not be parsed, or was missing a required field.
    #[error("invalid bundle metadata ({META_FILE}): {reason}")]
    BadMeta {
        /// What was wrong.
        reason: String,
    },
    /// The bundle's ABI version does not match this host's [`FSW_ABI_VERSION`].
    #[error(
        "bundle was built against FSW ABI v{found}, but this host speaks v{expected} \
         (rebuild the bundle)"
    )]
    AbiMismatch {
        /// The bundle's recorded ABI version.
        found: u32,
        /// The host's [`FSW_ABI_VERSION`].
        expected: u32,
    },
    /// The bundle's `mission.kdl` failed to parse.
    #[error("invalid bundle manifest ({MISSION_FILE}): {0}")]
    Parse(#[source] Box<super::LoadError>),
    /// A `.so` named by the manifest is absent from the bundle directory.
    #[error("bundle is missing the `.so` for artifact `{artifact}` (expected {path})")]
    MissingSo {
        /// The artifact id.
        artifact: String,
        /// The `.so` path that was expected inside the bundle.
        path: PathBuf,
    },
}

/// Helper: tag an [`std::io::Error`] with the path it happened on.
fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> BundleError {
    let path = path.into();
    move |source| BundleError::Io { path, source }
}

/// Write a relocatable bundle to `dir` (created if absent): copy each artifact's built
/// `.so` in, place the `mission.kdl` manifest (the source KDL, verbatim), and emit the
/// `meta.kdl` sidecar (cli-runner.md §4).
///
/// `wiring` must be **built** — every [`Artifact::path`](crate::Artifact) is `Some` (run
/// [`build_artifacts`](super::build_artifacts) first), else [`BundleError::NotBuilt`].
/// `mission_kdl` is the source wiring text carried into the bundle unchanged; its
/// `artifact` `lib=` stems re-decorate to this platform's file names on
/// [`load_bundle`], matching the copied `.so`s.
pub fn write_bundle(
    wiring: &Wiring,
    mission_kdl: &str,
    opts: &PackageOptions,
    dir: &Path,
) -> Result<(), BundleError> {
    fs::create_dir_all(dir).map_err(io_at(dir))?;

    // Copy each built `.so` in under its produced file name (`Artifact::cdylib`).
    for artifact in &wiring.artifacts {
        let src = artifact.path.as_ref().ok_or_else(|| BundleError::NotBuilt {
            artifact: artifact.id.clone(),
        })?;
        let dst = dir.join(&artifact.cdylib);
        fs::copy(src, &dst).map_err(io_at(&dst))?;
    }

    // The manifest is the source KDL, verbatim — re-parseable, human-readable, and
    // already relocatable (artifact `lib=` are stems, not absolute paths).
    let mission_path = dir.join(MISSION_FILE);
    fs::write(&mission_path, mission_kdl).map_err(io_at(&mission_path))?;

    // The sidecar: the ABI load-guard plus provenance. Schemas are deferred (§4.3).
    let built_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let profile = if opts.release { "release" } else { "debug" };
    let meta = format!(
        "meta {{\n    \
         abi_version {FSW_ABI_VERSION}\n    \
         profile {profile:?}\n    \
         built_at_unix {built_at_unix}\n\
         }}\n"
    );
    let meta_path = dir.join(META_FILE);
    fs::write(&meta_path, meta).map_err(io_at(&meta_path))?;
    Ok(())
}

/// Read a bundle directory back into a runnable [`Wiring`]: parse `meta.kdl` (guarding
/// the ABI version), parse `mission.kdl`, and fill each [`Artifact::path`](crate::Artifact)
/// from the `.so` copied alongside. **Never invokes cargo** — the cargo-free counterpart
/// to [`build_artifacts`](super::build_artifacts) (cli-runner.md §4.4).
pub fn load_bundle(dir: &Path) -> Result<Wiring, BundleError> {
    // --- meta.kdl: the ABI load-guard -----------------------------------
    let meta_path = dir.join(META_FILE);
    let meta_text = fs::read_to_string(&meta_path).map_err(io_at(&meta_path))?;
    let abi = meta_abi_version(&meta_text)?;
    if abi != FSW_ABI_VERSION {
        return Err(BundleError::AbiMismatch {
            found: abi,
            expected: FSW_ABI_VERSION,
        });
    }

    // --- mission.kdl → Wiring, then fill each artifact path from the dir --
    let mission_path = dir.join(MISSION_FILE);
    let mission_text = fs::read_to_string(&mission_path).map_err(io_at(&mission_path))?;
    let mut wiring = parse(&mission_text).map_err(|e| BundleError::Parse(Box::new(e)))?;
    for artifact in &mut wiring.artifacts {
        let so = dir.join(&artifact.cdylib);
        if !so.exists() {
            return Err(BundleError::MissingSo {
                artifact: artifact.id.clone(),
                path: so,
            });
        }
        artifact.path = Some(so);
    }
    Ok(wiring)
}

/// Pull `meta.abi_version` out of a bundle's `meta.kdl`.
fn meta_abi_version(text: &str) -> Result<u32, BundleError> {
    let doc = text
        .parse::<KdlDocument>()
        .map_err(|e| BundleError::BadMeta { reason: e.to_string() })?;
    let meta = doc
        .nodes()
        .iter()
        .find(|n| n.name().value() == "meta")
        .ok_or_else(|| BundleError::BadMeta {
            reason: "no `meta` node".into(),
        })?;
    let children = meta.children().ok_or_else(|| BundleError::BadMeta {
        reason: "`meta` node has no children".into(),
    })?;
    let abi = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "abi_version")
        .and_then(|n| n.entries().iter().find(|e| e.name().is_none()))
        .and_then(|e| e.value().as_integer())
        .ok_or_else(|| BundleError::BadMeta {
            reason: "missing `abi_version`".into(),
        })?;
    u32::try_from(abi).map_err(|_| BundleError::BadMeta {
        reason: format!("`abi_version` {abi} out of range"),
    })
}
