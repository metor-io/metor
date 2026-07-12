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

/// Extension of the single-file bundle form: an uncompressed tar of the
/// directory layout.
pub const METOR_EXTENSION: &str = "metor";

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
        ir_version: IR_VERSION,
        target: opts.target.clone(),
        profile: if opts.release { "release" } else { "debug" }.to_string(),
        built_at_unix,
        // Hash the exact wiring.json bytes, excluding the timestamp above.
        ir_sha256: sha256_hex(json.as_bytes()),
        metor_config_version: opts.metor_config_version.clone(),
    };
    let meta_json = serde_json::to_string_pretty(&meta).expect("BundleMeta serializes to JSON");

    let mut members = vec![
        (META_FILE.to_string(), MemberSource::Inline(meta_json.into_bytes())),
        (WIRING_FILE.to_string(), MemberSource::Inline(json.into_bytes())),
    ];

    // Artifacts sorted by id so entry order is stable regardless of the order
    // the front-end recorded them in.
    let mut artifacts: Vec<&super::model::Artifact> = wiring.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.id.cmp(&b.id));
    for artifact in artifacts {
        let src = artifact.path.as_ref().ok_or_else(|| BundleError::NotBuilt {
            artifact: artifact.id.clone(),
        })?;
        members.push((artifact.cdylib.clone(), MemberSource::Path(src.clone())));
        // The manifest sidecar rides along when the build driver wrote one; a
        // bundle without it stays valid (the manifest-hash check is skipped).
        let sidecar = crate::dl::manifest_sidecar_path(src);
        if sidecar.exists() {
            let name = format!("{}.manifest", artifact.cdylib);
            members.push((name, MemberSource::Path(sidecar)));
        }
    }

    if let Some(source) = &opts.provenance {
        members.push((provenance_name(source), MemberSource::Path(source.clone())));
    }
    Ok(members)
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
/// extension so a consumer (and `--check-ir`) can tell Python from KDL, or
/// bare `mission` when the source has none.
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
        let mut off = 0;
        while off + BLOCK <= bytes.len() {
            let header = &bytes[off..off + BLOCK];
            // A zeroed header block ends the archive.
            if header.iter().all(|&b| b == 0) {
                break;
            }
            let name = cstr(&header[..100]);
            let size = parse_octal(&header[124..136])? as usize;
            off += BLOCK;
            if off + size > bytes.len() {
                return Err(format!("entry `{name}` runs past the archive"));
            }
            out.push((name, bytes[off..off + size].to_vec()));
            // Advance past the payload, rounded up to a block boundary.
            off += size.div_ceil(BLOCK) * BLOCK;
        }
        Ok(out)
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
    fn checksum(header: &mut [u8; BLOCK]) {
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
    fn cstr(field: &[u8]) -> String {
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        String::from_utf8_lossy(&field[..end]).into_owned()
    }

    /// Parse a NUL/space-padded octal field.
    fn parse_octal(field: &[u8]) -> Result<u64, String> {
        let text = cstr(field);
        let trimmed = text.trim_matches(|c: char| c == ' ' || c == '\0');
        if trimmed.is_empty() {
            return Ok(0);
        }
        u64::from_str_radix(trimmed, 8).map_err(|_| format!("bad octal field `{trimmed}`"))
    }
}
