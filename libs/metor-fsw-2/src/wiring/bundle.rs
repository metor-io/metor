//! Relocatable bundles, directories that carry everything a mission needs to
//! run without a source tree or a cargo install.
//!
//! A bundle holds three things side by side: the wiring manifest
//! (`mission.kdl`, the source KDL text carried over verbatim), a metadata
//! sidecar (`meta.kdl`), and a copy of every artifact's built `cdylib`. The
//! manifest stays relocatable because its `artifact` entries name library
//! stems rather than absolute paths; loading re-decorates each stem into this
//! platform's file name and expects the matching `.so` next to the manifest.
//! Since the shared objects are compiled code, a bundle is tied to the target
//! architecture it was built for. Each `.so`'s `<cdylib>.manifest` sidecar,
//! when the build driver produced one, is copied in alongside it (the
//! forward-compat carrier for manifest-hash checks; unread today).
//!
//! The sidecar records the ABI version the bundle was built against and the
//! wiring IR version of its manifest; [`load_bundle`] refuses any bundle
//! whose versions differ from this host's [`FSW_ABI_VERSION`] and
//! [`IR_VERSION`]. It also records the build profile and a timestamp for
//! provenance; those are informational only.
//!
//! [`write_bundle`] produces a bundle from a built [`Wiring`] plus the source
//! KDL text. [`load_bundle`] reads one back into a [`Wiring`] whose artifact
//! paths point at the copied `.so`s, ready to run without invoking cargo.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kdl::KdlDocument;
use thiserror::Error;

use crate::abi::FSW_ABI_VERSION;

use super::model::{IR_VERSION, Wiring};
use super::parse;

/// File name of the metadata sidecar within a bundle.
const META_FILE: &str = "meta.kdl";
/// File name of the wiring manifest within a bundle.
const MISSION_FILE: &str = "mission.kdl";

/// Caller-supplied details that [`write_bundle`] records in the `meta.kdl`
/// sidecar.
#[derive(Clone, Debug, Default)]
pub struct PackageOptions {
    /// Record `profile "release"` in `meta.kdl` instead of `"debug"`. Purely
    /// informational; it documents how the bundled `.so`s were built.
    pub release: bool,
}

/// A failure from [`write_bundle`] or [`load_bundle`], ranging from plain
/// filesystem trouble to a bundle built against an incompatible ABI.
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
    /// An artifact had no built `.so` at package time.
    #[error("cannot package artifact `{artifact}`: it has no built `.so` (build it first)")]
    NotBuilt {
        /// The artifact id.
        artifact: String,
    },
    /// The bundle's `meta.kdl` could not be parsed, or was missing a required
    /// field.
    #[error("invalid bundle metadata ({META_FILE}): {reason}")]
    BadMeta {
        /// What was wrong.
        reason: String,
    },
    /// The bundle's ABI version does not match this host's
    /// [`FSW_ABI_VERSION`].
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
    /// The bundle's recorded wiring IR version does not match this host's
    /// [`IR_VERSION`]. A bundle that records no version predates IR
    /// versioning and is rejected the same way; bundles are rebuildable.
    #[error(
        "bundle {} wiring IR, but this host speaks v{expected} (rebuild the bundle)",
        match found {
            Some(v) => format!("was built against v{v}"),
            None => "predates the versioned".to_string(),
        }
    )]
    IrMismatch {
        /// The bundle's recorded IR version, `None` for a pre-versioning
        /// bundle.
        found: Option<u32>,
        /// The host's [`IR_VERSION`].
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

/// Tag an [`std::io::Error`] with the path it happened on.
fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> BundleError {
    let path = path.into();
    move |source| BundleError::Io { path, source }
}

/// Write a bundle to `dir`, creating the directory if needed.
///
/// Copies each artifact's built `.so` in under its produced file name, writes
/// `mission_kdl` verbatim as the manifest, and emits the `meta.kdl` sidecar
/// described by [`PackageOptions`].
/// Every artifact in `wiring` must already have a built path, otherwise this
/// returns [`BundleError::NotBuilt`].
pub fn write_bundle(
    wiring: &Wiring,
    mission_kdl: &str,
    opts: &PackageOptions,
    dir: &Path,
) -> Result<(), BundleError> {
    fs::create_dir_all(dir).map_err(io_at(dir))?;

    for artifact in &wiring.artifacts {
        let src = artifact
            .path
            .as_ref()
            .ok_or_else(|| BundleError::NotBuilt {
                artifact: artifact.id.clone(),
            })?;
        let dst = dir.join(&artifact.cdylib);
        fs::copy(src, &dst).map_err(io_at(&dst))?;
        // The manifest sidecar rides along when the build driver wrote one
        // (`BuildOptions::manifest_sidecar`); a bundle without it stays valid.
        let sidecar = crate::dl::manifest_sidecar_path(src);
        if sidecar.exists() {
            let sidecar_dst = crate::dl::manifest_sidecar_path(&dst);
            fs::copy(&sidecar, &sidecar_dst).map_err(io_at(&sidecar_dst))?;
        }
    }

    let mission_path = dir.join(MISSION_FILE);
    fs::write(&mission_path, mission_kdl).map_err(io_at(&mission_path))?;

    let built_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let profile = if opts.release { "release" } else { "debug" };
    let meta = format!(
        "meta {{\n    \
         abi_version {FSW_ABI_VERSION}\n    \
         ir_version {IR_VERSION}\n    \
         profile {profile:?}\n    \
         built_at_unix {built_at_unix}\n\
         }}\n"
    );
    let meta_path = dir.join(META_FILE);
    fs::write(&meta_path, meta).map_err(io_at(&meta_path))?;
    Ok(())
}

/// Read a bundle directory back into a runnable [`Wiring`].
///
/// Checks the sidecar's ABI version against [`FSW_ABI_VERSION`], parses the
/// manifest, and fills each artifact's path from the `.so` copied alongside.
/// Fails with [`BundleError::MissingSo`] if a `.so` the manifest names is not
/// in the directory.
pub fn load_bundle(dir: &Path) -> Result<Wiring, BundleError> {
    let meta_path = dir.join(META_FILE);
    let meta_text = fs::read_to_string(&meta_path).map_err(io_at(&meta_path))?;
    let abi = meta_abi_version(&meta_text)?;
    if abi != FSW_ABI_VERSION {
        return Err(BundleError::AbiMismatch {
            found: abi,
            expected: FSW_ABI_VERSION,
        });
    }
    let ir = meta_version(&meta_text, "ir_version")?;
    if ir != Some(IR_VERSION) {
        return Err(BundleError::IrMismatch {
            found: ir,
            expected: IR_VERSION,
        });
    }

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

/// Pull `meta.abi_version` out of a bundle's `meta.kdl`. Required: every
/// bundle has recorded it since bundles existed.
fn meta_abi_version(text: &str) -> Result<u32, BundleError> {
    meta_version(text, "abi_version")?.ok_or_else(|| BundleError::BadMeta {
        reason: "missing `abi_version`".into(),
    })
}

/// Pull the `meta.<field>` version integer out of a bundle's `meta.kdl`,
/// `None` when the node is absent.
fn meta_version(text: &str, field: &str) -> Result<Option<u32>, BundleError> {
    let doc = text
        .parse::<KdlDocument>()
        .map_err(|e| BundleError::BadMeta {
            reason: e.to_string(),
        })?;
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
    let Some(node) = children.nodes().iter().find(|n| n.name().value() == field) else {
        return Ok(None);
    };
    let value = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_integer())
        .ok_or_else(|| BundleError::BadMeta {
            reason: format!("missing `{field}`"),
        })?;
    u32::try_from(value).map(Some).map_err(|_| BundleError::BadMeta {
        reason: format!("`{field}` {value} out of range"),
    })
}
