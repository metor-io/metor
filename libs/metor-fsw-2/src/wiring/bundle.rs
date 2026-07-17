//! Relocatable bundles, directories that carry everything a mission needs to
//! run without a source tree, a cargo install, or any config front-end.
//!
//! A bundle holds the frozen mission IR side by side with the code it names:
//! `wiring.json` is the versioned [`Wiring`] serialized as JSON (source
//! anchors, scopes, and per-artifact manifest hashes intact, but artifact
//! `path`s stripped so the bundle stays relocatable and byte-reproducible),
//! `meta.json` is a plain-serde [`BundleMeta`] sidecar, and every artifact's
//! built `cdylib` — plus its `<cdylib>.manifest` sidecar when the build driver
//! wrote one — is copied in alongside. The `mission.py` that produced the
//! mission rides along as verbatim provenance and is never consumed on load:
//! the run path needs no Python and no config parse, strictly more hermetic
//! than re-evaluating source on target.
//!
//! [`BundleMeta`] records the ABI version and IR version the bundle was built
//! against, the target triple its `.so`s were compiled for, the build profile,
//! a timestamp, the `sha256` of the `wiring.json` bytes (the determinism
//! backstop CI diffs), and the `metor_config` recorder version the mission was
//! evaluated with. [`load_bundle`] refuses any bundle whose ABI, IR, or
//! target does not match this host — a triple mismatch is a clean
//! [`BundleError::TargetMismatch`] before any dlopen, where an arch mismatch
//! used to surface as a dlopen mystery. It verifies the frozen IR digest and
//! each recorded manifest hash. A manifest hash checks interface compatibility;
//! it is not a digest of the shared-object bytes.
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

use crate::abi::FSW_ABI_VERSION;

use super::model::Wiring;

/// File name of the metadata sidecar within a bundle.
const META_FILE: &str = "meta.json";
/// File name of the frozen wiring IR within a bundle.
const WIRING_FILE: &str = "wiring.json";
/// Base name of the optional provenance copy of the source file; the real name
/// keeps the source's `.py` extension.
const PROVENANCE_STEM: &str = "mission";

/// Extension of the single-file bundle form: an uncompressed tar of the
/// directory layout.
pub const METOR_EXTENSION: &str = "metor";

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
    /// `sha256:<hex>` of the `wiring.json` bytes — the determinism backstop CI
    /// re-evaluates and diffs (`metor-fsw package --check-ir`).
    pub ir_sha256: String,
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
    /// The `mission.py` to copy in verbatim as provenance, never consumed on
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
    if name.len() > tar::NAME_CAP {
        return Err(BundleError::InvalidMemberName {
            name: name.to_string(),
            reason: format!("name exceeds the {}-byte archive limit", tar::NAME_CAP),
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

/// One bundle member: its file name and where its bytes come from — inline
/// (the freshly built `meta.json` / `wiring.json`) or a source file to copy
/// (`.so`s, sidecars, the provenance source).
enum MemberSource {
    Inline(Vec<u8>),
    Path(PathBuf),
}

/// The bundle's members in canonical order — `meta.json`, `wiring.json`, then
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
    let meta = BundleMeta {
        abi_version: FSW_ABI_VERSION,
        target: opts.target.clone(),
        profile: if opts.release { "release" } else { "debug" }.to_string(),
        built_at_unix,
        // Hash the exact wiring.json bytes, excluding the timestamp above.
        ir_sha256: super::stubgen::manifest_hash(json.as_bytes()),
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

    // Artifacts sorted by id so entry order is stable regardless of the order
    // the front-end recorded them in.
    let mut artifacts: Vec<&super::model::Artifact> = wiring.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.id.cmp(&b.id));
    for artifact in artifacts {
        let src = artifact
            .path
            .as_ref()
            .ok_or_else(|| BundleError::NotBuilt {
                artifact: artifact.id.clone(),
            })?;
        let cdylib = member_cdylib_name(opts.target.as_deref(), &artifact.lib);
        members.push((cdylib.clone(), MemberSource::Path(src.clone())));
        // The manifest sidecar rides along when the build driver wrote one; a
        // bundle without it stays valid (the manifest-hash check is skipped).
        let sidecar = crate::dl::manifest_sidecar_path(src);
        if sidecar.exists() {
            let name = format!("{cdylib}.manifest");
            members.push((name, MemberSource::Path(sidecar)));
        }
    }

    if let Some(source) = &opts.provenance {
        members.push((provenance_name(source), MemberSource::Path(source.clone())));
    }
    validate_member_names(members.iter().map(|(name, _)| name.as_str()))?;
    Ok(members)
}

/// The member file name of an artifact's shared object: derived from the
/// bundle's recorded target triple, or the host's convention when the bundle
/// records none (in which case the load-time triple check is skipped too, so
/// writer and reader agree by the same fallback).
fn member_cdylib_name(target: Option<&str>, lib: &str) -> String {
    match target {
        Some(triple) => super::cdylib_file_name_for(triple, lib),
        None => super::cdylib_file_name(lib),
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
                fs::copy(src, &dst).map_err(io_at(&dst))?;
            }
        }
    }
    Ok(())
}

/// Write the members as a single uncompressed `.metor` tar with zeroed entry
/// timestamps, so identical inputs produce byte-identical archives.
fn write_archive(members: &[(String, MemberSource)], path: &Path) -> Result<(), BundleError> {
    let mut out = Vec::new();
    for (name, source) in members {
        let bytes = member_bytes(source)?;
        tar::write_entry(&mut out, name, &bytes);
    }
    // Two zero blocks mark end-of-archive.
    out.extend_from_slice(&[0u8; tar::BLOCK * 2]);
    fs::write(path, out).map_err(io_at(path))
}

/// The provenance copy's file name: `mission.<ext>` keeping the source's
/// extension (`mission.py`), or bare `mission` when the source has none.
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
/// Reads `meta.json`, checks the ABI version, IR version, and target triple
/// against this host, deserializes `wiring.json`, fills each artifact's path
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
    // absent either, it cannot render a verdict and is skipped (the dlopen
    // path stays the backstop, as before Phase 3).
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
    if super::stubgen::manifest_hash(&wiring_bytes) != meta.ir_sha256 {
        return Err(BundleError::IrHashMismatch);
    }
    let mut wiring: Wiring =
        serde_json::from_slice(&wiring_bytes).map_err(|e| BundleError::BadWiring {
            reason: e.to_string(),
        })?;

    for artifact in &mut wiring.artifacts {
        let cdylib = member_cdylib_name(meta.target.as_deref(), &artifact.lib);
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
            if super::stubgen::manifest_hash(&bytes) != recorded {
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
    let bytes = fs::read(path).map_err(io_at(path))?;
    let dir = tempfile::Builder::new()
        .prefix("metor-bundle-")
        .tempdir()
        .map_err(io_at(path))?;
    let members = tar::read(&bytes).map_err(|reason| BundleError::BadMeta {
        reason: format!("malformed `.{METOR_EXTENSION}` archive: {reason}"),
    })?;
    validate_member_names(members.iter().map(|(name, _)| name.as_str()))?;
    for (name, content) in members {
        let dst = dir.path().join(&name);
        fs::write(&dst, content).map_err(io_at(&dst))?;
    }
    Ok(dir)
}

/// A byte-exact, dependency-free uncompressed `ustar` reader/writer, just
/// enough for the flat bundle layout (short names, regular files). Reproducible
/// by construction: zeroed timestamps, ids, and mode, so identical inputs
/// produce identical bytes.
mod tar {
    /// The tar block size; headers and padded payloads are multiples of it.
    pub(super) const BLOCK: usize = 512;
    /// Maximum name bytes in the ustar header's name field.
    pub(super) const NAME_CAP: usize = 100;

    /// Append one regular-file entry (`name` header + padded `data`) to `out`.
    pub(super) fn write_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        let mut header = [0u8; BLOCK];
        let name_bytes = name.as_bytes();
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        // mode 0644, zeroed uid/gid — all octal fields are `len-1` digits + NUL.
        octal(&mut header, 100, 8, 0o644);
        octal(&mut header, 108, 8, 0);
        octal(&mut header, 116, 8, 0);
        octal(&mut header, 124, 12, data.len() as u64);
        octal(&mut header, 136, 12, 0); // mtime zeroed: reproducible archives
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        checksum(&mut header);

        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let rem = data.len() % BLOCK;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, BLOCK - rem));
        }
    }

    /// Parse every regular-file entry into `(name, bytes)`, in archive order.
    pub(super) fn read(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
        let mut out = Vec::new();
        let mut off = 0usize;
        loop {
            let header_end = off
                .checked_add(BLOCK)
                .ok_or_else(|| "archive header offset overflow".to_string())?;
            if header_end > bytes.len() {
                if off == bytes.len() {
                    break;
                }
                return Err("truncated archive header".to_string());
            }
            let header = &bytes[off..header_end];
            // A zeroed header block ends the archive.
            if header.iter().all(|&b| b == 0) {
                break;
            }
            verify_checksum(header)?;
            if !matches!(header[156], 0 | b'0') {
                return Err(format!("unsupported tar entry type {:#x}", header[156]));
            }
            let name = cstr(&header[..NAME_CAP])?;
            let size = usize::try_from(parse_octal(&header[124..136])?)
                .map_err(|_| format!("entry `{name}` size does not fit this platform"))?;
            let data_start = header_end;
            let data_end = data_start
                .checked_add(size)
                .ok_or_else(|| format!("entry `{name}` size overflows the archive"))?;
            if data_end > bytes.len() {
                return Err(format!("entry `{name}` runs past the archive"));
            }
            out.push((name, bytes[data_start..data_end].to_vec()));
            // Advance past the payload, rounded up to a block boundary.
            let padded = size
                .checked_add(BLOCK - 1)
                .ok_or_else(|| "archive entry padding overflow".to_string())?
                / BLOCK
                * BLOCK;
            off = data_start
                .checked_add(padded)
                .ok_or_else(|| "archive entry offset overflow".to_string())?;
        }
        Ok(out)
    }

    fn verify_checksum(header: &[u8]) -> Result<(), String> {
        let recorded = parse_octal(&header[148..156])?;
        let actual: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| if (148..156).contains(&i) { b' ' } else { b } as u64)
            .sum();
        if recorded != actual {
            return Err(format!(
                "tar header checksum mismatch: recorded {recorded}, computed {actual}"
            ));
        }
        Ok(())
    }

    /// Write `value` as a right-justified `len-1`-digit octal field with a
    /// trailing NUL at `header[at..at+len]`.
    fn octal(header: &mut [u8; BLOCK], at: usize, len: usize, value: u64) {
        let digits = format!("{value:0width$o}", width = len - 1);
        header[at..at + len - 1].copy_from_slice(digits.as_bytes());
        header[at + len - 1] = 0;
    }

    /// Fill the checksum field (offset 148, 8 bytes): the unsigned sum of every
    /// header byte with the field itself read as spaces, stored as 6 octal
    /// digits, a NUL, then a space (the ustar convention).
    pub(super) fn checksum(header: &mut [u8; BLOCK]) {
        for b in &mut header[148..156] {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        let digits = format!("{sum:06o}");
        header[148..154].copy_from_slice(digits.as_bytes());
        header[154] = 0;
        header[155] = b' ';
    }

    /// A NUL-terminated (or full-width) string field as an owned `String`.
    fn cstr(field: &[u8]) -> Result<String, String> {
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        std::str::from_utf8(&field[..end])
            .map(str::to_owned)
            .map_err(|_| "tar member name is not UTF-8".to_string())
    }

    /// Parse a NUL/space-padded octal field.
    fn parse_octal(field: &[u8]) -> Result<u64, String> {
        let text = cstr(field)?;
        let trimmed = text.trim_matches(|c: char| c == ' ' || c == '\0');
        if trimmed.is_empty() {
            return Ok(0);
        }
        u64::from_str_radix(trimmed, 8).map_err(|_| format!("bad octal field `{trimmed}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (name, content) in entries {
            tar::write_entry(&mut bytes, name, content);
        }
        bytes.extend_from_slice(&[0; tar::BLOCK * 2]);
        bytes
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

    #[test]
    fn tar_rejects_bad_checksum_and_non_file_entries() {
        let mut bad_checksum = archive(&[("file", b"data")]);
        bad_checksum[0] ^= 1;
        assert!(tar::read(&bad_checksum).unwrap_err().contains("checksum"));

        let mut directory = archive(&[("directory", b"")]);
        directory[156] = b'5';
        let header: &mut [u8; tar::BLOCK] = (&mut directory[..tar::BLOCK]).try_into().unwrap();
        tar::checksum(header);
        assert!(tar::read(&directory).unwrap_err().contains("entry type"));
    }

    #[test]
    fn tar_rejects_truncated_and_oversized_entries_without_panicking() {
        let truncated = vec![1; tar::BLOCK - 1];
        assert!(tar::read(&truncated).is_err());

        let mut oversized = archive(&[("file", b"")]);
        // Maximum eleven-digit octal size, far beyond this small archive.
        oversized[124..136].copy_from_slice(b"77777777777\0");
        let header: &mut [u8; tar::BLOCK] = (&mut oversized[..tar::BLOCK]).try_into().unwrap();
        tar::checksum(header);
        assert!(tar::read(&oversized).is_err());
    }
}
