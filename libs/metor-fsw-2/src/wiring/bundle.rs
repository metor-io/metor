//! Relocatable bundles, directories that carry everything a target needs to
//! run without a source tree, a cargo install, or any config front-end.
//!
//! A bundle holds the frozen target IR side by side with the code it names:
//! `wiring.json` is the versioned [`Wiring`] serialized as JSON (source
//! anchors, scopes, and per-artifact manifest hashes intact, but artifact
//! `path`s stripped so the bundle stays relocatable and byte-reproducible),
//! `meta.json` is a plain-serde [`BundleMeta`] sidecar, and every artifact's
//! built `cdylib`, plus its `<cdylib>.manifest` sidecar when the build driver
//! wrote one, is copied in alongside. The `target.py` that produced the
//! target rides along as verbatim provenance and is never consumed on load:
//! the run path needs no Python and no config parse, strictly more hermetic
//! than re-evaluating source on target.
//!
//! [`BundleMeta`] records the ABI version and IR version the bundle was built
//! against, the target triple its `.so`s were compiled for, the build profile,
//! a timestamp, the `sha256` of the `wiring.json` bytes (the determinism
//! backstop CI diffs), and the `metor_config` recorder version the target was
//! evaluated with. [`load_bundle`] checks the ABI and target. The later
//! resolve pass checks the IR version. A triple mismatch is a clean
//! [`BundleError::TargetMismatch`] before any dlopen, rather than an opaque
//! dlopen failure. It verifies the frozen IR digest and each recorded
//! manifest hash. A manifest hash checks interface compatibility; it is not
//! a digest of the shared-object bytes.
//!
//! [`write_bundle`] produces a bundle from a built [`Wiring`] plus a
//! [`PackageOptions`]. [`load_bundle`] reads one back into a [`Wiring`] whose
//! artifact paths point at the copied `.so`s, ready to run without invoking
//! cargo.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use metor_fsw_2_core::abi::FSW_ABI_VERSION;

use super::model::Wiring;

/// File name of the metadata sidecar within a bundle.
const META_FILE: &str = "meta.json";
/// File name of the frozen wiring IR within a bundle.
const WIRING_FILE: &str = "wiring.json";
/// Base name of the optional provenance copy of the source file; the real name
/// keeps the source's `.py` extension.
const PROVENANCE_STEM: &str = "target";

/// Extension of the single-file bundle form: an uncompressed tar of the
/// directory layout.
pub const METOR_EXTENSION: &str = "metor";

/// Maximum member-name bytes a ustar header's name field holds.
const NAME_CAP: usize = 100;

/// The plain-serde metadata sidecar (`meta.json`) written beside a bundle's
/// `wiring.json`. Everything here is either a compatibility gate checked at
/// load ([`abi_version`](Self::abi_version), [`target`](Self::target)) or
/// provenance the run path never depends on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleMeta {
    /// The FSW ABI version the bundled `.so`s were built against; must equal
    /// this host's [`FSW_ABI_VERSION`] at load.
    pub abi_version: u32,
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
    /// `sha256:<hex>` of the `wiring.json` bytes, the determinism backstop CI
    /// re-evaluates and diffs (`metor-fsw package --check-ir`).
    pub ir_sha256: String,
    /// Per-artifact provenance: what was actually packaged, hashed from the
    /// exact bytes copied into the bundle. Recording only; the load gates
    /// (ABI, IR hash, triple, manifest hash) cover integrity. Serde-defaulted
    /// so pre-provenance bundles load unchanged.
    #[serde(default)]
    pub packs: Vec<PackProvenance>,
}

/// One artifact's `meta.json` provenance entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackProvenance {
    /// The [`Artifact::id`](super::model::Artifact::id).
    pub artifact_id: String,
    /// The publishing distribution, when the artifact came from one.
    pub dist: Option<super::model::DistRef>,
    /// How the artifact was provisioned.
    pub source: PackSourceKind,
    /// `sha256:<hex>` of the shared-object bytes copied into the bundle: a
    /// digest of the code itself, where `manifest_hash` digests only the
    /// interface.
    pub cdylib_sha256: String,
    /// The recorded pack-manifest hash, when the artifact carried one.
    pub manifest_hash: Option<String>,
}

/// Where a packaged artifact's shared object came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackSourceKind {
    /// Selected from a prebuilt per-triple payload (an installed pack wheel,
    /// or a local pack's `pack dev` layout).
    Prebuilt,
    /// Compiled from the crate by the build driver at package time.
    CrateBuilt,
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
    /// The `target.py` to copy in verbatim as provenance, never consumed on
    /// load. `None` writes no provenance copy.
    pub provenance: Option<PathBuf>,
    /// The package timestamp recorded as `built_at_unix`; `None` uses the
    /// current time. Pin it to make a `.metor` archive byte-reproducible for
    /// identical inputs (the timestamp is provenance, excluded from
    /// `ir_sha256`, but it is content, so it must be fixed for bit-exact
    /// output).
    pub built_at_unix: Option<u64>,
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
    /// The bundle's `.so`s were built for a different target triple than this
    /// host runs, caught here instead of surfacing as an opaque dlopen
    /// failure.
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
    /// A bundle member is not a single, portable file name.
    #[error("invalid bundle member name `{name}`: {reason}")]
    InvalidMemberName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: String,
    },
    /// An archive contains the same member more than once.
    #[error("bundle archive contains duplicate member `{name}`")]
    DuplicateMember {
        /// The repeated name.
        name: String,
    },
    /// The frozen wiring bytes do not match `meta.json`.
    #[error("bundle wiring fails its ir_sha256 integrity check")]
    IrHashMismatch,
    /// A `.so` named by the wiring is absent from the bundle directory.
    #[error("bundle is missing the `.so` for artifact `{artifact}` (expected {path})")]
    MissingSo {
        /// The artifact id.
        artifact: String,
        /// The `.so` path that was expected inside the bundle.
        path: PathBuf,
    },
    /// A recorded manifest hash requires a sidecar to verify it against.
    #[error("bundle is missing the manifest sidecar for artifact `{artifact}` (expected {path})")]
    MissingManifest {
        /// The artifact id.
        artifact: String,
        /// The sidecar path expected inside the bundle.
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

fn validate_member_name(name: &str) -> Result<(), BundleError> {
    let mut components = Path::new(name).components();
    let one_normal =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if name.is_empty() || name.contains(['/', '\\']) || !one_normal {
        return Err(BundleError::InvalidMemberName {
            name: name.to_string(),
            reason: "expected one normal path component".to_string(),
        });
    }
    if name.len() > NAME_CAP {
        return Err(BundleError::InvalidMemberName {
            name: name.to_string(),
            reason: format!("name exceeds the {NAME_CAP}-byte archive limit"),
        });
    }
    Ok(())
}

fn validate_member_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), BundleError> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        validate_member_name(name)?;
        if !seen.insert(name) {
            return Err(BundleError::DuplicateMember {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Serialize `wiring` path-stripped to the canonical `wiring.json` bytes. The
/// same compact serde rendering the IR contract pins and the `WiringManifest`
/// telemetry carries, so the frozen file, the emitted manifest, and a CI
/// re-evaluation are all byte-comparable.
fn wiring_json(wiring: &Wiring) -> String {
    serde_json::to_string(&wiring.path_stripped()).expect("a built Wiring serializes to JSON")
}

/// Write a bundle to `dir`, creating the directory if needed.
///
/// Copies each artifact's built `.so` (and its manifest sidecar when present)
/// under its produced file name, writes the frozen, path-stripped IR to
/// `wiring.json`, emits the `meta.json` sidecar, and copies the provenance
/// source file verbatim when [`PackageOptions::provenance`] names one. Every
/// artifact in `wiring` must already have a built path, otherwise this returns
/// [`BundleError::NotBuilt`].
pub fn write_bundle(wiring: &Wiring, opts: &PackageOptions, out: &Path) -> Result<(), BundleError> {
    let members = bundle_members(wiring, opts)?;
    if out.extension().is_some_and(|e| e == METOR_EXTENSION) {
        write_archive(&members, out)
    } else {
        write_dir(&members, out)
    }
}

/// One bundle member: its file name and where its bytes come from, inline
/// (the freshly built `meta.json` / `wiring.json`) or a source file to copy
/// (`.so`s, sidecars, the provenance source).
enum MemberSource {
    Inline(Vec<u8>),
    Path(PathBuf),
}

/// The bundle's members in canonical order: `meta.json`, `wiring.json`, then
/// each artifact's `.so` and `.manifest` sorted by artifact id, then the
/// provenance copy. One ordering both the directory and single-file writers
/// share, so the two forms carry identical content, and the `.metor` tar is
/// byte-stable for identical inputs.
fn bundle_members(
    wiring: &Wiring,
    opts: &PackageOptions,
) -> Result<Vec<(String, MemberSource)>, BundleError> {
    let json = wiring_json(wiring);
    let built_at_unix = opts.built_at_unix.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    // Artifacts sorted by id so entry order is stable regardless of the order
    // the front-end recorded them in. Provenance is hashed from the exact
    // bytes being copied, so `meta.json` describes this bundle's members.
    let mut artifacts: Vec<&super::model::Artifact> = wiring.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.id.cmp(&b.id));
    let mut artifact_members: Vec<(String, MemberSource)> = Vec::new();
    let mut packs: Vec<PackProvenance> = Vec::new();
    for artifact in artifacts {
        let src = artifact
            .path
            .as_ref()
            .ok_or_else(|| BundleError::NotBuilt {
                artifact: artifact.id.clone(),
            })?;
        let cdylib = member_artifact_name(opts.target.as_deref(), artifact);
        let so_bytes = fs::read(src).map_err(io_at(src))?;
        packs.push(PackProvenance {
            artifact_id: artifact.id.clone(),
            dist: artifact.dist.clone(),
            source: if artifact.prebuilt_dir.is_some() {
                PackSourceKind::Prebuilt
            } else {
                PackSourceKind::CrateBuilt
            },
            cdylib_sha256: super::pack_module::manifest_hash(&so_bytes),
            manifest_hash: artifact.manifest_hash.clone(),
        });
        artifact_members.push((cdylib.clone(), MemberSource::Inline(so_bytes)));
        // The manifest sidecar rides along when the build driver wrote one; a
        // bundle without it stays valid (the manifest-hash check is skipped).
        let sidecar = crate::dl::manifest_sidecar_path(src);
        if sidecar.exists() {
            let name = format!("{cdylib}.manifest");
            artifact_members.push((name, MemberSource::Path(sidecar)));
        }
    }

    let meta = BundleMeta {
        abi_version: FSW_ABI_VERSION,
        target: opts.target.clone(),
        profile: if opts.release { "release" } else { "debug" }.to_string(),
        built_at_unix,
        // Hash the exact wiring.json bytes, excluding the timestamp above.
        ir_sha256: super::pack_module::manifest_hash(json.as_bytes()),
        packs,
    };
    let meta_json = serde_json::to_string_pretty(&meta).expect("BundleMeta serializes to JSON");

    let mut members = vec![
        (
            META_FILE.to_string(),
            MemberSource::Inline(meta_json.into_bytes()),
        ),
        (
            WIRING_FILE.to_string(),
            MemberSource::Inline(json.into_bytes()),
        ),
    ];
    members.extend(artifact_members);

    if let Some(source) = &opts.provenance {
        members.push((provenance_name(source), MemberSource::Path(source.clone())));
    }
    validate_member_names(members.iter().map(|(name, _)| name.as_str()))?;
    Ok(members)
}

/// The member file name of an artifact's loadable: a cdylib's is derived
/// from the bundle's recorded target triple, or the host's convention when
/// the bundle records none (the load-time triple check is skipped by the
/// same fallback, so writer and reader agree); a wasm module is one
/// arch-neutral `<id>.wasm` regardless of triple.
fn member_artifact_name(target: Option<&str>, artifact: &super::model::Artifact) -> String {
    match artifact.kind {
        crate::ir::ArtifactKind::Wasm => format!("{}.wasm", artifact.id),
        crate::ir::ArtifactKind::Cdylib => match target {
            Some(triple) => super::cdylib_file_name_for(triple, &artifact.lib),
            None => super::cdylib_file_name(&artifact.lib),
        },
    }
}

/// Read a member's bytes, inline or copied from its source file.
fn member_bytes(source: &MemberSource) -> Result<Vec<u8>, BundleError> {
    match source {
        MemberSource::Inline(bytes) => Ok(bytes.clone()),
        MemberSource::Path(path) => fs::read(path).map_err(io_at(path)),
    }
}

/// Write the members as the directory bundle layout.
fn write_dir(members: &[(String, MemberSource)], dir: &Path) -> Result<(), BundleError> {
    fs::create_dir_all(dir).map_err(io_at(dir))?;
    for (name, source) in members {
        let dst = dir.join(name);
        match source {
            MemberSource::Inline(bytes) => fs::write(&dst, bytes).map_err(io_at(&dst))?,
            MemberSource::Path(src) => {
                // Fresh-inode replacement: macOS kill-caches dylib signatures
                // by inode, and bundles are repackaged over themselves.
                super::build_driver::copy_atomic(src, &dst).map_err(io_at(&dst))?;
            }
        }
    }
    Ok(())
}

/// Write the members as a single uncompressed `.metor` tar with zeroed entry
/// mode/ids/timestamps, so identical inputs produce byte-identical archives.
fn write_archive(members: &[(String, MemberSource)], path: &Path) -> Result<(), BundleError> {
    let file = fs::File::create(path).map_err(io_at(path))?;
    let mut builder = tar::Builder::new(file);
    for (name, source) in members {
        let bytes = member_bytes(source)?;
        let mut header = tar::Header::new_ustar();
        header.set_path(name).map_err(io_at(path))?;
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .map_err(io_at(path))?;
    }
    builder.finish().map_err(io_at(path))
}

/// The provenance copy's file name: `target.<ext>` keeping the source's
/// extension (`target.py`), or bare `target` when the source has none.
fn provenance_name(source: &Path) -> String {
    match source.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{PROVENANCE_STEM}.{ext}"),
        None => PROVENANCE_STEM.to_string(),
    }
}

/// Read a bundle back into a runnable [`Wiring`], dispatched by shape: a
/// `.metor` file is unpacked to a temp directory first, a directory is read in
/// place.
///
/// Reads `meta.json`, checks the ABI version and target triple against this
/// host, deserializes `wiring.json`, fills each artifact's path
/// from the `.so` copied alongside, and verifies every recorded manifest hash
/// against the copied sidecar. Fails before any dlopen with the matching
/// [`BundleError`] on any mismatch.
///
/// A `.metor` bundle unpacks to a temp directory that outlives this call (the
/// returned `Wiring`'s artifact paths point into it, dlopened later at
/// resolve); the OS reclaims it. A directory bundle is read where it sits.
pub fn load_bundle(path: &Path) -> Result<Wiring, BundleError> {
    if path.is_file() && path.extension().is_some_and(|e| e == METOR_EXTENSION) {
        let dir = unpack_archive(path)?;
        // Keep the unpacked files alive for the process: resolve dlopens the
        // `.so`s after this returns. Leaking the temp dir is the cost of a
        // cargo-free single-file run; the OS temp cleanup reclaims it.
        let dir = dir.keep();
        return load_bundle_dir(&dir);
    }
    load_bundle_dir(path)
}

/// The directory-form loader behind [`load_bundle`].
fn load_bundle_dir(dir: &Path) -> Result<Wiring, BundleError> {
    let meta_path = dir.join(META_FILE);
    if !meta_path.exists() {
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
    // The triple check needs both a recorded target and a determinable host;
    // absent either, it cannot render a verdict and is skipped, leaving
    // dlopen as the backstop.
    if let (Some(found), Some(expected)) = (&meta.target, super::build_driver::host_triple())
        && found != &expected
    {
        return Err(BundleError::TargetMismatch {
            found: found.clone(),
            expected,
        });
    }

    let wiring_path = dir.join(WIRING_FILE);
    let wiring_bytes = fs::read(&wiring_path).map_err(io_at(&wiring_path))?;
    if super::pack_module::manifest_hash(&wiring_bytes) != meta.ir_sha256 {
        return Err(BundleError::IrHashMismatch);
    }
    let mut wiring: Wiring =
        serde_json::from_slice(&wiring_bytes).map_err(|e| BundleError::BadWiring {
            reason: e.to_string(),
        })?;

    for artifact in &mut wiring.artifacts {
        let cdylib = member_artifact_name(meta.target.as_deref(), artifact);
        validate_member_name(&cdylib)?;
        let so = dir.join(&cdylib);
        if !so.exists() {
            return Err(BundleError::MissingSo {
                artifact: artifact.id.clone(),
                path: so,
            });
        }
        if let Some(recorded) = artifact.manifest_hash.as_deref() {
            let sidecar = crate::dl::manifest_sidecar_path(&so);
            let bytes = match fs::read(&sidecar) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(BundleError::MissingManifest {
                        artifact: artifact.id.clone(),
                        path: sidecar,
                    });
                }
                Err(e) => return Err(io_at(&sidecar)(e)),
            };
            if super::pack_module::manifest_hash(&bytes) != recorded {
                return Err(BundleError::ManifestHashMismatch {
                    artifact: artifact.id.clone(),
                });
            }
        }
        artifact.path = Some(so);
    }
    Ok(wiring)
}

/// The frozen `wiring.json` file name, for consumers that read a bundle's IR
/// directly (the `--check-ir` determinism gate).
pub const WIRING_FILE_NAME: &str = WIRING_FILE;

/// Unpack a `.metor` archive into a fresh temp directory, writing every member
/// as a real file so a directory-shaped consumer can proceed unchanged.
/// Exposed for the `--check-ir` gate, which reads a bundle's `wiring.json` and
/// provenance copy off disk.
pub fn unpack_metor(path: &Path) -> Result<tempfile::TempDir, BundleError> {
    unpack_archive(path)
}

/// Unpack a `.metor` archive into a fresh temp directory, writing every member
/// as a real file so the directory loader (and dlopen) can proceed unchanged.
fn unpack_archive(path: &Path) -> Result<tempfile::TempDir, BundleError> {
    let bad = |reason: String| BundleError::BadMeta {
        reason: format!("malformed `.{METOR_EXTENSION}` archive: {reason}"),
    };
    let file = fs::File::open(path).map_err(io_at(path))?;
    let dir = tempfile::Builder::new()
        .prefix("metor-bundle-")
        .tempdir()
        .map_err(io_at(path))?;
    let mut archive = tar::Archive::new(file);
    let mut members = Vec::new();
    for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(bad(format!(
                "unsupported tar entry type {:?}",
                entry.header().entry_type()
            )));
        }
        let name = entry
            .path()
            .map_err(|e| bad(e.to_string()))?
            .to_str()
            .ok_or_else(|| bad("tar member name is not UTF-8".to_string()))?
            .to_string();
        let size = usize::try_from(entry.size())
            .map_err(|_| bad(format!("entry `{name}` size does not fit this platform")))?;
        let mut content = Vec::with_capacity(size);
        std::io::Read::read_to_end(&mut entry, &mut content).map_err(|e| bad(e.to_string()))?;
        members.push((name, content));
    }
    validate_member_names(members.iter().map(|(name, _)| name.as_str()))?;
    for (name, content) in members {
        let dst = dir.path().join(&name);
        fs::write(&dst, content).map_err(io_at(&dst))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw `.metor`-shaped archive from possibly-invalid entries, to
    /// exercise [`unpack_archive`]'s own validation rather than the `tar`
    /// crate's.
    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut out);
            for (name, content) in entries {
                let mut header = tar::Header::new_ustar();
                // `set_path` rejects `..` components; write the raw name field
                // directly so the escaping-member case below can be built.
                let name_field = &mut header.as_ustar_mut().unwrap().name;
                name_field[..name.len()].copy_from_slice(name.as_bytes());
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, *content).unwrap();
            }
            builder.finish().unwrap();
        }
        out
    }

    #[test]
    fn member_names_are_single_portable_components() {
        for name in ["../escape", "/absolute", "a/b", r"a\b", ".", "..", ""] {
            assert!(
                matches!(
                    validate_member_name(name),
                    Err(BundleError::InvalidMemberName { .. })
                ),
                "accepted {name:?}"
            );
        }
        validate_member_name("libflight.so").unwrap();
    }

    #[test]
    fn archive_rejects_duplicate_and_escaping_members() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("escape.metor", archive(&[("../escape", b"bad")])),
            (
                "duplicate.metor",
                archive(&[("wiring.json", b"one"), ("wiring.json", b"two")]),
            ),
        ] {
            let path = tmp.path().join(name);
            fs::write(&path, bytes).unwrap();
            assert!(unpack_archive(&path).is_err(), "accepted {name}");
        }
        assert!(!tmp.path().join("escape").exists());
    }
}
