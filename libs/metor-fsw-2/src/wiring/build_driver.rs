//! Builds a [`Wiring`]'s [`Artifact`](super::Artifact)s and records where the
//! resulting `.so`s live.
//!
//! Each artifact names a cargo package that produces a `cdylib`. The driver runs
//! `cargo build -p <crate_name>` for each one, finds the produced library in
//! cargo's `--message-format=json` output, and writes its path into
//! [`Artifact::path`](super::Artifact::path) so the resolver
//! ([`resolve`](super::resolve)) can `dlopen` it. Since every system cdylib is its
//! own package, cargo decides what is stale and what can be skipped; the driver
//! only supplies the list of crates and reads back where their outputs landed.
//!
//! The library is located by scanning `compiler-artifact` JSON lines for a
//! `filenames` entry ending in the artifact's `cdylib` name, which stays correct
//! under a custom target directory or profile.
//!
//! ## The manifest sidecar
//!
//! Unless [`BuildOptions::manifest_sidecar`] is off, each built library also
//! gets a `<cdylib>.manifest` sidecar next to it: the raw postcard
//! [`PackManifestMsg`](crate::abi::PackManifestMsg) bytes `fsw_pack_describe`
//! reports, so downstream consumers (stubgen, cross-arch resolve) can read the
//! pack's self-description without running the artifact. Sourcing the sidecar
//! *does* run the crate's `pack()` once, at build time — the same trust model
//! as a `build.rs`. A cross build (`--target` differing from the host) cannot
//! run its own output, so the crate is additionally built for the host and
//! that twin is described instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::model::Wiring;

/// Knobs applied to every `cargo build` invocation run by
/// [`build_artifacts`].
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Build the `--release` profile instead of the default debug profile.
    pub release: bool,
    /// Extra args appended to every `cargo build` invocation (e.g. `--target ...`).
    pub extra_args: Vec<String>,
    /// Write a `<cdylib>.manifest` sidecar next to each built library
    /// (default `true`): the raw postcard
    /// [`PackManifestMsg`](crate::abi::PackManifestMsg) bytes from describing
    /// a host-runnable build of the pack.
    ///
    /// Sidecar generation executes the crate's `pack()` at build time, the
    /// same trust model as a `build.rs`. Opt out for pack crates that cannot
    /// be built for the host architecture — a cross build sources its sidecar
    /// from an additional host-arch build, and that build failing is a hard
    /// error while this flag is on.
    pub manifest_sidecar: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            release: false,
            extra_args: Vec::new(),
            manifest_sidecar: true,
        }
    }
}

/// Why [`build_artifacts`] could not produce a library path for an artifact.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// `cargo` could not be spawned.
    #[error("failed to run `cargo build -p {crate_name}`: {source}")]
    Spawn {
        /// The crate that was being built.
        crate_name: String,
        #[source]
        /// The spawn error.
        source: std::io::Error,
    },
    /// `cargo build` exited non-zero.
    #[error("`cargo build -p {crate_name}` failed:\n{stderr}")]
    CargoFailed {
        /// The crate that failed to build.
        crate_name: String,
        /// Cargo's captured stderr.
        stderr: String,
    },
    /// The build succeeded but the named `cdylib` was not found in cargo's output.
    #[error("built `{crate_name}` but could not locate its cdylib `{cdylib}` in cargo's output")]
    ArtifactNotFound {
        /// The crate that was built.
        crate_name: String,
        /// The cdylib file name that was expected.
        cdylib: String,
    },
    /// A cross build's additional host-arch build (the manifest sidecar's
    /// source) failed. Not a silent skip: a missing sidecar would surface
    /// much later as a stubgen or staleness mystery.
    #[error(
        "host-arch build of `{crate_name}` for its manifest sidecar failed; if this pack \
         cannot build for the host, set `BuildOptions::manifest_sidecar = false`"
    )]
    HostBuild {
        /// The crate whose host build failed.
        crate_name: String,
        #[source]
        /// Why the host build failed.
        source: Box<BuildError>,
    },
    /// Describing the host-runnable library for its manifest sidecar failed.
    #[error("failed to describe `{crate_name}` for its manifest sidecar: {detail}")]
    SidecarDescribe {
        /// The crate whose library failed to describe.
        crate_name: String,
        /// The describe failure.
        detail: String,
    },
    /// The manifest sidecar could not be written.
    #[error("failed to write manifest sidecar `{path}`: {source}")]
    SidecarIo {
        /// The sidecar path being written.
        path: PathBuf,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A cross build's host-described manifest differs from the up-to-date
    /// sidecar already next to the target library. Pack manifests are meant
    /// to be architecture-independent; divergence is a design escalation, not
    /// something to paper over.
    #[error(
        "manifest described from the host build of `{crate_name}` diverges from the existing \
         sidecar `{path}`: pack manifests must not depend on the target architecture"
    )]
    ManifestDivergence {
        /// The crate whose manifests diverge.
        crate_name: String,
        /// The existing sidecar next to the target library.
        path: PathBuf,
    },
}

/// Builds every [`Artifact`](super::Artifact) in `wiring` and fills in its
/// [`path`](super::Artifact::path). Safe to re-run; cargo rebuilds only what it
/// considers stale.
pub fn build_artifacts(wiring: &mut Wiring, opts: &BuildOptions) -> Result<(), BuildError> {
    let cross = opts.manifest_sidecar && is_cross(&opts.extra_args);
    for artifact in &mut wiring.artifacts {
        let path = build_one(&artifact.crate_name, &artifact.cdylib, opts)?;
        if opts.manifest_sidecar {
            write_manifest_sidecar(&artifact.crate_name, &artifact.cdylib, &path, opts, cross)?;
        }
        artifact.path = Some(path);
    }
    Ok(())
}

/// Runs `cargo build -p <crate_name>` and returns the located cdylib path.
fn build_one(crate_name: &str, cdylib: &str, opts: &BuildOptions) -> Result<PathBuf, BuildError> {
    // Prefer the cargo that invoked this process, falling back to PATH.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "-p", crate_name, "--message-format=json"]);
    if opts.release {
        cmd.arg("--release");
    }
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    let output = cmd.output().map_err(|source| BuildError::Spawn {
        crate_name: crate_name.to_string(),
        source,
    })?;
    if !output.status.success() {
        return Err(BuildError::CargoFailed {
            crate_name: crate_name.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    // Each `compiler-artifact` line carries a `"filenames":[...]` array whose
    // entries are quoted paths, so splitting on `"` yields each path as one token.
    // That is enough to find the cdylib without pulling in a JSON parser.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(cdylib) {
            continue;
        }
        for tok in line.split('"') {
            if tok.ends_with(cdylib) {
                let path = PathBuf::from(tok);
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }
    Err(BuildError::ArtifactNotFound {
        crate_name: crate_name.to_string(),
        cdylib: cdylib.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Manifest sidecars
// ---------------------------------------------------------------------------

/// Source and write the `<so>.manifest` sidecar for one built artifact.
///
/// A native build describes the just-built library. A cross build cannot run
/// its own output, so the crate is additionally built for the host (no
/// `--target`) and that twin is described instead — a host build failure is a
/// hard [`BuildError::HostBuild`], never a silently missing sidecar. And
/// arch-independence of manifests is verified, not assumed: when the target
/// library already carries an up-to-date sidecar (say, written by a native
/// build on a target-arch machine into a shared target dir), the two are
/// compared and divergence is an error.
fn write_manifest_sidecar(
    crate_name: &str,
    cdylib: &str,
    so: &Path,
    opts: &BuildOptions,
    cross: bool,
) -> Result<(), BuildError> {
    let described = if cross {
        let host_opts = BuildOptions {
            extra_args: strip_target_args(&opts.extra_args),
            ..opts.clone()
        };
        build_one(crate_name, cdylib, &host_opts).map_err(|source| BuildError::HostBuild {
            crate_name: crate_name.to_string(),
            source: Box::new(source),
        })?
    } else {
        so.to_path_buf()
    };
    let bytes = describe_manifest(&described).map_err(|detail| BuildError::SidecarDescribe {
        crate_name: crate_name.to_string(),
        detail,
    })?;
    let sidecar = crate::dl::manifest_sidecar_path(so);
    if cross
        && let Some(existing) = fresh_sidecar(&sidecar, so)
        && existing != bytes
    {
        return Err(BuildError::ManifestDivergence {
            crate_name: crate_name.to_string(),
            path: sidecar,
        });
    }
    write_atomic(&sidecar, &bytes).map_err(|source| BuildError::SidecarIo {
        path: sidecar,
        source,
    })
}

/// Obtain the raw postcard manifest bytes for a host-runnable pack library.
///
/// Prefers a describe worker — the dlopen quarantined in a short-lived child,
/// the same isolation `resolve` gives process systems — but re-executing this
/// binary as a worker requires a `main` that installed
/// [`worker_entry`](crate::proc::worker_entry); a libtest binary would run
/// its harness instead (the adcs test suites document that constraint).
/// Without the guard, and on targets without worker machinery, the library is
/// described in-process.
fn describe_manifest(so: &Path) -> Result<Vec<u8>, String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if crate::proc::worker_guard_installed() {
        return crate::proc::describe_via_worker(None, so).map_err(|e| e.to_string());
    }
    crate::dl::describe_raw(so).map_err(|e| e.to_string())
}

/// Whether `extra_args` requests a `--target` other than the host triple. An
/// undeterminable host reads as cross: the sidecar is then sourced from an
/// explicit host-arch build, which is correct either way.
fn is_cross(extra_args: &[String]) -> bool {
    match requested_target(extra_args) {
        Some(triple) => host_triple().as_deref() != Some(triple),
        None => false,
    }
}

/// The `--target` triple `extra_args` carries, in either `--target <t>` or
/// `--target=<t>` form; the last occurrence wins, matching cargo.
fn requested_target(extra_args: &[String]) -> Option<&str> {
    let mut target = None;
    let mut args = extra_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--target" {
            target = args.next().map(String::as_str);
        } else if let Some(triple) = arg.strip_prefix("--target=") {
            target = Some(triple);
        }
    }
    target
}

/// `extra_args` with every `--target` (and its value) removed — the args for
/// the host-arch twin of a cross build.
fn strip_target_args(extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(extra_args.len());
    let mut args = extra_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--target" {
            args.next();
        } else if !arg.starts_with("--target=") {
            out.push(arg.clone());
        }
    }
    out
}

/// The host target triple, from `cargo -vV`'s `host:` line.
fn host_triple() -> Option<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo).arg("-vV").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|triple| triple.trim().to_string())
}

/// The existing sidecar's bytes, provided it is at least as new as the
/// library it sits next to. A stale sidecar describes an older build and must
/// not be compared against — cargo rewrote the library since.
fn fresh_sidecar(sidecar: &Path, so: &Path) -> Option<Vec<u8>> {
    let sidecar_mtime = std::fs::metadata(sidecar).ok()?.modified().ok()?;
    let so_mtime = std::fs::metadata(so).ok()?.modified().ok()?;
    if sidecar_mtime < so_mtime {
        return None;
    }
    std::fs::read(sidecar).ok()
}

/// Write via a temp file + rename, so concurrent builders sharing one target
/// dir (parallel test binaries, say) never expose a torn sidecar.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
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
    // sidecar in the workspace target dir, so they serialize on one lock, and
    // they skip (with a note) where the nested cargo build is unavailable —
    // the same convention as tests/dl_integration.rs.
    // -----------------------------------------------------------------------

    #[cfg(not(miri))]
    static FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// The sidecar is the exact bytes `fsw_pack_describe` reports (the plan's
    /// sidecar-hash ≡ describe-hash property), and decodes to the fixture's
    /// entries.
    #[test]
    #[cfg(not(miri))]
    fn sidecar_matches_describe() {
        let _guard = FIXTURE_LOCK.lock().unwrap();
        let mut wiring = fixture_wiring();
        if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
            eprintln!("skipping: build_artifacts failed: {e}");
            return;
        }
        let so = wiring.artifacts[0].path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        let bytes = std::fs::read(&sidecar).expect("sidecar written next to the .so");
        let described = crate::dl::describe_raw(&so).expect("in-process describe");
        assert_eq!(bytes, described, "sidecar bytes ≡ fsw_pack_describe bytes");

        let entries = crate::dl::read_manifest_sidecar(&so)
            .expect("sidecar present")
            .expect("sidecar decodes as a pack manifest");
        assert_eq!(
            entries
                .iter()
                .map(|e| e.descriptor.name)
                .collect::<Vec<_>>(),
            ["DlCounter", "DlEcho"]
        );
    }

    /// The cross arm verifies arch-independence rather than assuming it: an
    /// up-to-date target sidecar whose bytes differ from the host-described
    /// manifest is a hard error, and matching bytes pass. Driven with
    /// `cross = true` over the host build itself — a real foreign `--target`
    /// is not buildable in this test environment, but the code path is the
    /// same: build the host twin, describe it, compare, write.
    #[test]
    #[cfg(not(miri))]
    fn cross_divergence_is_a_hard_error() {
        let _guard = FIXTURE_LOCK.lock().unwrap();
        let mut wiring = fixture_wiring();
        if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
            eprintln!("skipping: build_artifacts failed: {e}");
            return;
        }
        let artifact = &wiring.artifacts[0];
        let so = artifact.path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        let opts = BuildOptions::default();

        // Plant a fresh-but-divergent "target" sidecar; the host-described
        // bytes must refuse to replace it.
        std::fs::write(&sidecar, b"divergent").unwrap();
        let err = write_manifest_sidecar(&artifact.crate_name, &artifact.cdylib, &so, &opts, true)
            .expect_err("a fresh divergent sidecar is refused");
        assert!(
            matches!(err, BuildError::ManifestDivergence { .. }),
            "{err}"
        );

        // Identical bytes pass the comparison and are rewritten in place.
        let described = crate::dl::describe_raw(&so).expect("in-process describe");
        std::fs::write(&sidecar, &described).unwrap();
        write_manifest_sidecar(&artifact.crate_name, &artifact.cdylib, &so, &opts, true)
            .expect("a matching sidecar is accepted");
        assert_eq!(std::fs::read(&sidecar).unwrap(), described);
    }

    /// `manifest_sidecar: false` builds the library and writes nothing next
    /// to it.
    #[test]
    #[cfg(not(miri))]
    fn sidecar_opt_out_writes_nothing() {
        let _guard = FIXTURE_LOCK.lock().unwrap();
        let opts = BuildOptions {
            manifest_sidecar: false,
            ..BuildOptions::default()
        };
        let mut wiring = fixture_wiring();
        if let Err(e) = build_artifacts(&mut wiring, &opts) {
            eprintln!("skipping: build_artifacts failed: {e}");
            return;
        }
        let so = wiring.artifacts[0].path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        // Another test (or an earlier run) may have left one; the assertion
        // is that *this* build does not produce it.
        let _ = std::fs::remove_file(&sidecar);
        let mut again = fixture_wiring();
        build_artifacts(&mut again, &opts).expect("rebuild is a cargo no-op");
        assert!(!sidecar.exists(), "opted out, so no sidecar is written");
        assert!(crate::dl::read_manifest_sidecar(&so).is_none());
    }
}
