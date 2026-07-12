//! Relocatable bundles, directories that carry everything a mission needs to
//! run without a source tree, a cargo install, or any config front-end.
//!
//! A bundle holds the frozen mission IR side by side with the code it names:
//! `wiring.json` is the versioned [`Wiring`] serialized as JSON (source
//! anchors, scopes, and per-artifact manifest hashes intact, but artifact
//! `path`s stripped so the bundle stays relocatable and byte-reproducible),
//! `meta.json` is a plain-serde [`BundleMeta`] sidecar, and every artifact's
//! built `cdylib` — plus its `<cdylib>.manifest` sidecar when the build driver
//! wrote one — is copied in alongside. The source file that produced the
//! mission (`mission.py` or `mission.kdl`) rides along as verbatim provenance
//! and is never consumed on load: the run path needs no Python and no KDL
//! parser, strictly more hermetic than re-parsing source on target.
//!
//! [`BundleMeta`] records the ABI version and IR version the bundle was built
//! against, the target triple its `.so`s were compiled for, the build profile,
//! a timestamp, the `sha256` of the `wiring.json` bytes (the determinism
//! backstop CI diffs), and the `metor_config` recorder version when the
//! mission was Python. [`load_bundle`] refuses any bundle whose ABI, IR, or
//! target does not match this host — a triple mismatch is a clean
//! [`BundleError::TargetMismatch`] before any dlopen, where an arch mismatch
//! used to surface as a dlopen mystery — and verifies each artifact's recorded
//! manifest hash against its copied sidecar, so a tampered bundle fails before
//! resolve.
//!
//! [`write_bundle`] produces a bundle from a built [`Wiring`] plus a
//! [`PackageOptions`]. [`load_bundle`] reads one back into a [`Wiring`] whose
//! artifact paths point at the copied `.so`s, ready to run without invoking
//! cargo.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::abi::FSW_ABI_VERSION;

use super::model::{IR_VERSION, Wiring};

/// File name of the metadata sidecar within a bundle.
const META_FILE: &str = "meta.json";
/// File name of the frozen wiring IR within a bundle.
const WIRING_FILE: &str = "wiring.json";
/// Base name of the optional provenance copy of the source file; the real name
/// keeps the source's extension (`mission.py` / `mission.kdl`).
const PROVENANCE_STEM: &str = "mission";

/// The pre-Phase-3 bundle's metadata file, still detected so an old-layout
/// bundle is rejected with a clear message instead of a confusing parse error.
const LEGACY_META_FILE: &str = "meta.kdl";

/// The plain-serde metadata sidecar (`meta.json`) written beside a bundle's
/// `wiring.json`. Everything here is either a compatibility gate checked at
/// load ([`abi_version`](Self::abi_version), [`ir_version`](Self::ir_version),
/// [`target`](Self::target)) or provenance the run path never depends on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleMeta {
    /// The FSW ABI version the bundled `.so`s were built against; must equal
    /// this host's [`FSW_ABI_VERSION`] at load.
    pub abi_version: u32,
    /// The wiring IR version of `wiring.json`; must equal this host's
    /// [`IR_VERSION`] at load.
    pub ir_version: u32,
    /// The target triple the `.so`s were compiled for, when the packager could
    /// determine it. `None` skips the load-time triple check.
    pub target: Option<String>,
    /// The build profile the `.so`s were compiled under (`"debug"` /
    /// `"release"`). Informational.
    pub profile: String,
    /// Seconds since the Unix epoch at package time. Provenance only, and
    /// deliberately excluded from [`ir_sha256`](Self::ir_sha256) so it does
    /// not perturb reproducibility.
    pub built_at_unix: u64,
    /// `sha256:<hex>` of the `wiring.json` bytes — the determinism backstop CI
    /// re-evaluates and diffs (`metor-fsw package --check-ir`).
    pub ir_sha256: String,
    /// The `metor_config` recorder version, when the mission was authored in
    /// Python. `None` for a KDL mission. Provenance only.
    pub metor_config_version: Option<String>,
}

/// Caller-supplied inputs [`write_bundle`] records in `meta.json` and uses to
/// copy the provenance source.
#[derive(Clone, Debug, Default)]
pub struct PackageOptions {
    /// Record `profile "release"` instead of `"debug"`. Documents how the
    /// bundled `.so`s were built.
    pub release: bool,
    /// The target triple the `.so`s were built for, recorded in `meta.json`
    /// and checked against the host at load. `None` records no target.
    pub target: Option<String>,
    /// The `metor_config` recorder version for a Python mission, recorded as
    /// provenance. `None` for a KDL mission.
    pub metor_config_version: Option<String>,
    /// The source file to copy in verbatim as provenance (`mission.py` /
    /// `mission.kdl`), never consumed on load. `None` writes no provenance
    /// copy.
    pub provenance: Option<PathBuf>,
}

/// A failure from [`write_bundle`] or [`load_bundle`], ranging from plain
/// filesystem trouble to a bundle built for the wrong architecture.
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
    /// The bundle's `meta.json` could not be read or parsed.
    #[error("invalid bundle metadata ({META_FILE}): {reason}")]
    BadMeta {
        /// What was wrong.
        reason: String,
    },
    /// The bundle uses the retired pre-Phase-3 layout (verbatim `mission.kdl`
    /// with a `meta.kdl` sidecar). Bundles are rebuildable by design, so there
    /// is no migration shim.
    #[error(
        "bundle uses the retired layout (`mission.kdl` + `{LEGACY_META_FILE}`); the bundle now \
         carries the frozen IR (`{WIRING_FILE}` + `{META_FILE}`) — repackage it with \
         `metor-fsw package`"
    )]
    OldLayout,
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
    /// [`IR_VERSION`]. Bundles are rebuildable.
    #[error("bundle was built against wiring IR v{found}, but this host speaks v{expected} (rebuild the bundle)")]
    IrMismatch {
        /// The bundle's recorded IR version.
        found: u32,
        /// The host's [`IR_VERSION`].
        expected: u32,
    },
    /// The bundle's `.so`s were built for a different target triple than this
    /// host runs — the clean load-time verdict that used to surface as an
    /// opaque dlopen failure.
    #[error(
        "bundle was built for target `{found}`, but this host is `{expected}` \
         (rebuild the bundle for this target)"
    )]
    TargetMismatch {
        /// The bundle's recorded target triple.
        found: String,
        /// The host triple.
        expected: String,
    },
    /// The bundle's `wiring.json` failed to deserialize.
    #[error("invalid bundle wiring ({WIRING_FILE}): {reason}")]
    BadWiring {
        /// The deserialize failure.
        reason: String,
    },
    /// A `.so` named by the wiring is absent from the bundle directory.
    #[error("bundle is missing the `.so` for artifact `{artifact}` (expected {path})")]
    MissingSo {
        /// The artifact id.
        artifact: String,
        /// The `.so` path that was expected inside the bundle.
        path: PathBuf,
    },
    /// An artifact's recorded manifest hash does not match the sidecar copied
    /// into the bundle: the bundle was tampered with or assembled from
    /// mismatched parts.
    #[error(
        "bundle artifact `{artifact}` fails its manifest-hash check (the `.so`/sidecar does not \
         match the frozen IR); rebuild the bundle"
    )]
    ManifestHashMismatch {
        /// The artifact id.
        artifact: String,
    },
}

/// Tag an [`std::io::Error`] with the path it happened on.
fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> BundleError {
    let path = path.into();
    move |source| BundleError::Io { path, source }
}

/// Serialize `wiring` path-stripped to the canonical `wiring.json` bytes. The
/// same compact serde rendering the IR contract pins and the `WiringManifest`
/// telemetry carries, so the frozen file, the emitted manifest, and a CI
/// re-evaluation are all byte-comparable.
fn wiring_json(wiring: &Wiring) -> String {
    serde_json::to_string(&wiring.path_stripped()).expect("a built Wiring serializes to JSON")
}

/// The `sha256:<hex>` of `bytes`, the same format the manifest-hash check uses.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Write a bundle to `dir`, creating the directory if needed.
///
/// Copies each artifact's built `.so` (and its manifest sidecar when present)
/// under its produced file name, writes the frozen, path-stripped IR to
/// `wiring.json`, emits the `meta.json` sidecar, and copies the provenance
/// source file verbatim when [`PackageOptions::provenance`] names one. Every
/// artifact in `wiring` must already have a built path, otherwise this returns
/// [`BundleError::NotBuilt`].
pub fn write_bundle(
    wiring: &Wiring,
    opts: &PackageOptions,
    dir: &Path,
) -> Result<(), BundleError> {
    fs::create_dir_all(dir).map_err(io_at(dir))?;

    for artifact in &wiring.artifacts {
        let src = artifact.path.as_ref().ok_or_else(|| BundleError::NotBuilt {
            artifact: artifact.id.clone(),
        })?;
        let dst = dir.join(&artifact.cdylib);
        fs::copy(src, &dst).map_err(io_at(&dst))?;
        // The manifest sidecar rides along when the build driver wrote one; a
        // bundle without it stays valid (the manifest-hash check is skipped).
        let sidecar = crate::dl::manifest_sidecar_path(src);
        if sidecar.exists() {
            let sidecar_dst = crate::dl::manifest_sidecar_path(&dst);
            fs::copy(&sidecar, &sidecar_dst).map_err(io_at(&sidecar_dst))?;
        }
    }

    let json = wiring_json(wiring);
    let wiring_path = dir.join(WIRING_FILE);
    fs::write(&wiring_path, &json).map_err(io_at(&wiring_path))?;

    let built_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = BundleMeta {
        abi_version: FSW_ABI_VERSION,
        ir_version: IR_VERSION,
        target: opts.target.clone(),
        profile: if opts.release { "release" } else { "debug" }.to_string(),
        built_at_unix,
        // Hash the exact bytes written, excluding the timestamp above.
        ir_sha256: sha256_hex(json.as_bytes()),
        metor_config_version: opts.metor_config_version.clone(),
    };
    let meta_json = serde_json::to_string_pretty(&meta).expect("BundleMeta serializes to JSON");
    let meta_path = dir.join(META_FILE);
    fs::write(&meta_path, meta_json).map_err(io_at(&meta_path))?;

    if let Some(source) = &opts.provenance {
        let name = provenance_name(source);
        let dst = dir.join(name);
        fs::copy(source, &dst).map_err(io_at(&dst))?;
    }
    Ok(())
}

/// The provenance copy's file name: `mission.<ext>` keeping the source's
/// extension so a consumer (and `--check-ir`) can tell Python from KDL, or
/// bare `mission` when the source has none.
fn provenance_name(source: &Path) -> String {
    match source.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{PROVENANCE_STEM}.{ext}"),
        None => PROVENANCE_STEM.to_string(),
    }
}

/// Read a bundle directory back into a runnable [`Wiring`].
///
/// Reads `meta.json`, checks the ABI version, IR version, and target triple
/// against this host, deserializes `wiring.json`, fills each artifact's path
/// from the `.so` copied alongside, and verifies every recorded manifest hash
/// against the copied sidecar. Fails before any dlopen with the matching
/// [`BundleError`] on any mismatch.
pub fn load_bundle(dir: &Path) -> Result<Wiring, BundleError> {
    let meta_path = dir.join(META_FILE);
    if !meta_path.exists() {
        // A pre-Phase-3 bundle is a clean, named rejection, not a missing-file
        // surprise.
        if dir.join(LEGACY_META_FILE).exists() {
            return Err(BundleError::OldLayout);
        }
        return Err(BundleError::BadMeta {
            reason: format!("no `{META_FILE}`"),
        });
    }
    let meta_text = fs::read_to_string(&meta_path).map_err(io_at(&meta_path))?;
    let meta: BundleMeta = serde_json::from_str(&meta_text).map_err(|e| BundleError::BadMeta {
        reason: e.to_string(),
    })?;

    if meta.abi_version != FSW_ABI_VERSION {
        return Err(BundleError::AbiMismatch {
            found: meta.abi_version,
            expected: FSW_ABI_VERSION,
        });
    }
    if meta.ir_version != IR_VERSION {
        return Err(BundleError::IrMismatch {
            found: meta.ir_version,
            expected: IR_VERSION,
        });
    }
    // The triple check needs both a recorded target and a determinable host;
    // absent either, it cannot render a verdict and is skipped (the dlopen
    // path stays the backstop, as before Phase 3).
    if let (Some(found), Some(expected)) =
        (&meta.target, super::build_driver::current_host_triple())
        && found != &expected
    {
        return Err(BundleError::TargetMismatch {
            found: found.clone(),
            expected,
        });
    }

    let wiring_path = dir.join(WIRING_FILE);
    let wiring_text = fs::read_to_string(&wiring_path).map_err(io_at(&wiring_path))?;
    let mut wiring: Wiring =
        serde_json::from_str(&wiring_text).map_err(|e| BundleError::BadWiring {
            reason: e.to_string(),
        })?;

    for artifact in &mut wiring.artifacts {
        let so = dir.join(&artifact.cdylib);
        if !so.exists() {
            return Err(BundleError::MissingSo {
                artifact: artifact.id.clone(),
                path: so,
            });
        }
        // Verify the frozen manifest hash against the copied sidecar (the same
        // hash function stubgen and resolve use) before filling the path, so a
        // tampered `.so`/sidecar fails here rather than at dlopen.
        if let Some(recorded) = artifact.manifest_hash.as_deref()
            && let Some(bytes) = crate::dl::manifest_sidecar_bytes(&so)
            && super::stubgen::manifest_hash(&bytes) != recorded
        {
            return Err(BundleError::ManifestHashMismatch {
                artifact: artifact.id.clone(),
            });
        }
        artifact.path = Some(so);
    }
    Ok(wiring)
}
