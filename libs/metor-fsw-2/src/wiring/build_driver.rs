//! Provisions a [`Wiring`]'s [`Artifact`](super::Artifact)s: records where
//! each `.so` lives, building it when the artifact is crate-shaped.
//!
//! A **crate** artifact names a cargo package that produces a `cdylib`. The
//! driver runs `cargo build -p <crate_name>` for each one, finds the produced
//! library in cargo's `--message-format=json` output, and writes its path into
//! [`Artifact::path`](super::Artifact::path) so the resolver
//! ([`resolve`](super::resolve)) can `dlopen` it. Since every system cdylib is its
//! own package, cargo decides what is stale and what can be skipped; the driver
//! only supplies the list of crates and reads back where their outputs landed.
//!
//! A **prebuilt** artifact instead names a directory of per-triple libraries
//! ([`Artifact::prebuilt_dir`](super::Artifact::prebuilt_dir) — an installed
//! pack wheel's `_libs`, or a local pack's `.metor/libs`). Provisioning is a
//! selection, never a compile: the requested target triple (or the host's)
//! picks `<dir>/<triple>/<cdylib>`, and a triple the directory does not ship
//! is a clean error listing the ones it does. The manifest sidecar sits next
//! to the selected library, so the resolve-time staleness gate
//! (`check_manifest_hashes`) covers prebuilt artifacts unchanged.
//!
//! The library is located by scanning `compiler-artifact` JSON lines for a
//! `filenames` entry ending in the cdylib file name derived from the
//! artifact's `lib` stem and the build's target triple, which stays correct
//! under a custom target directory or profile.
//!
//! ## The manifest sidecar
//!
//! Unless [`BuildOptions::manifest_sidecar`] is off, each built library also
//! gets a `<cdylib>.manifest` sidecar next to it: the raw postcard
//! [`PackManifest`](crate::abi::PackManifest) bytes `fsw_pack_describe`
//! reports, so downstream consumers (stubgen, cross-arch resolve) can read the
//! pack's self-description without running the artifact. Sourcing the sidecar
//! *does* run the crate's `pack()` once, at build time — the same trust model
//! as a `build.rs`. A cross build (`--target` differing from the host) cannot
//! run its own output, so the crate is additionally built for the host and
//! that twin is described instead.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing_indicatif::span_ext::IndicatifSpanExt;

use super::model::Wiring;

/// Knobs applied to every `cargo build` invocation run by
/// [`provision_artifacts`].
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Build the `--release` profile instead of the default debug profile.
    pub release: bool,
    /// Extra args appended to every `cargo build` invocation (e.g. `--target ...`).
    pub extra_args: Vec<String>,
    /// Write a `<cdylib>.manifest` sidecar next to each built library
    /// (default `true`): the raw postcard
    /// [`PackManifest`](crate::abi::PackManifest) bytes from describing
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

/// Why [`provision_artifacts`] could not produce a library path for an artifact.
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
    /// A prebuilt artifact does not ship a library for the requested triple.
    #[error(
        "prebuilt artifact `{artifact}` ships no library for `{triple}` under `{dir}`\
        {}", if .available.is_empty() { String::new() } else { format!(" (available: {})", .available.join(", ")) }
    )]
    PrebuiltMissing {
        /// The artifact id.
        artifact: String,
        /// The triple that was requested.
        triple: String,
        /// The prebuilt directory that was searched.
        dir: PathBuf,
        /// The triples the directory does ship.
        available: Vec<String>,
    },
    /// `--no-build` was requested but a crate artifact's library has not
    /// been built yet.
    #[error(
        "no built `{cdylib}` found for `{crate_name}`; build it first, or drop `--no-build` to \
         let cargo provision it"
    )]
    NotBuilt {
        /// The crate whose library is missing.
        crate_name: String,
        /// The cdylib file name that was searched for.
        cdylib: String,
    },
    /// A prebuilt artifact needs a target triple to select by, and neither a
    /// `--target` nor the host triple could be determined.
    #[error(
        "cannot select a prebuilt library for artifact `{artifact}`: no `--target` given and \
         the host triple is unknown"
    )]
    HostTripleUnknown {
        /// The artifact id.
        artifact: String,
    },
}

/// Provisions every [`Artifact`](super::Artifact) in `wiring`, filling in its
/// [`path`](super::Artifact::path): a prebuilt artifact selects the requested
/// triple's library from its `prebuilt_dir`, a crate artifact is cargo-built.
/// Safe to re-run; selection is idempotent and cargo rebuilds only what it
/// considers stale.
pub fn provision_artifacts(wiring: &mut Wiring, opts: &BuildOptions) -> Result<(), BuildError> {
    let cross = opts.manifest_sidecar && is_cross(&opts.extra_args);
    let target = requested_target(&opts.extra_args).map(str::to_string);
    for artifact in &mut wiring.artifacts {
        if let Some(dir) = artifact.prebuilt_dir.clone() {
            artifact.path = Some(select_prebuilt(artifact, &dir, target.as_deref())?);
            continue;
        }
        let cdylib = match &target {
            Some(triple) => super::cdylib_file_name_for(triple, &artifact.lib),
            None => super::cdylib_file_name(&artifact.lib),
        };
        let path = build_one(&artifact.crate_name, &cdylib, opts)?;
        if opts.manifest_sidecar {
            write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &path, opts, cross)?;
        }
        artifact.path = Some(path);
    }
    Ok(())
}

/// Select a prebuilt artifact's library for `target` (or the host): the
/// `<dir>/<triple>/<cdylib>` the pack shipped. No compile, no sidecar work —
/// the shipped sidecar sits next to the library and resolve verifies it.
fn select_prebuilt(
    artifact: &super::model::Artifact,
    dir: &Path,
    target: Option<&str>,
) -> Result<PathBuf, BuildError> {
    let triple = match target.map(str::to_string).or_else(host_triple) {
        Some(triple) => triple,
        None => {
            return Err(BuildError::HostTripleUnknown {
                artifact: artifact.id.clone(),
            });
        }
    };
    let so = dir
        .join(&triple)
        .join(super::cdylib_file_name_for(&triple, &artifact.lib));
    if !so.exists() {
        return Err(BuildError::PrebuiltMissing {
            artifact: artifact.id.clone(),
            triple,
            dir: dir.to_path_buf(),
            available: shipped_triples(dir),
        });
    }
    Ok(so)
}

/// The triple subdirectories a prebuilt dir ships, for the
/// [`BuildError::PrebuiltMissing`] message.
fn shipped_triples(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut triples: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    triples.sort();
    triples
}

/// Runs `cargo build -p <crate_name>` and returns the located cdylib path.
fn build_one(crate_name: &str, cdylib: &str, opts: &BuildOptions) -> Result<PathBuf, BuildError> {
    let label = match requested_target(&opts.extra_args) {
        Some(triple) => format!("{crate_name} ({triple})"),
        None => crate_name.to_string(),
    };
    build_cdylib("build", crate_name, cdylib, opts, &label)
}

/// [`build_one`] with the cargo subcommand parameterized — `"zigbuild"` runs
/// the `cargo-zigbuild` cross builder with the same output-location scan.
///
/// The build reports through tracing: a `build`-target span for its whole
/// duration (the CLI renders active spans as a pinned progress line, keyed on
/// `label`), each cargo stderr line as a `cargo`-target event as it arrives,
/// and a `build`-target completion event. Without a subscriber all of it is
/// silent, so library callers see no output.
pub(super) fn build_cdylib(
    subcommand: &str,
    crate_name: &str,
    cdylib: &str,
    opts: &BuildOptions,
    label: &str,
) -> Result<PathBuf, BuildError> {
    // Prefer the cargo that invoked this process, falling back to PATH.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(cargo);
    cmd.args([subcommand, "-p", crate_name, "--message-format=json"]);
    if opts.release {
        cmd.arg("--release");
    }
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let span = tracing::info_span!(target: "build", "build", %label);
    span.pb_set_message(label);
    let started = std::time::Instant::now();
    let mut json = String::new();
    let (stderr, status) = {
        let _entered = span.enter();
        let mut child = cmd.spawn().map_err(|source| BuildError::Spawn {
            crate_name: crate_name.to_string(),
            source,
        })?;
        let child_stderr = child.stderr.take().expect("stderr piped");
        let mut child_stdout = child.stdout.take().expect("stdout piped");

        // Stream cargo's stderr through tracing as it arrives — keeping a
        // copy for the failure diagnostic — while this thread drains the JSON
        // stdout. Both pipes are consumed concurrently, so neither can fill
        // and stall cargo. A side benefit of the pipe: cargo emits plain
        // `Compiling` lines instead of its own progress bar.
        let stderr = std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut buf = String::new();
                let lines = std::io::BufReader::new(child_stderr).lines();
                for line in lines.map_while(Result::ok) {
                    tracing::info!(target: "cargo", "{line}");
                    if let Some(unit) = line.trim_start().strip_prefix("Compiling ") {
                        // `Compiling <name> <version> (<path>)` — drop the path.
                        let unit = unit.split(" (").next().unwrap_or(unit);
                        span.pb_set_message(&format!("{label} · {unit}"));
                    }
                    buf.push_str(&line);
                    buf.push('\n');
                }
                buf
            });
            std::io::Read::read_to_string(&mut child_stdout, &mut json).ok();
            reader.join().expect("stderr reader does not panic")
        });
        let status = child.wait().map_err(|source| BuildError::Spawn {
            crate_name: crate_name.to_string(),
            source,
        })?;
        (stderr, status)
    };
    drop(span);
    if !status.success() {
        return Err(BuildError::CargoFailed {
            crate_name: crate_name.to_string(),
            stderr,
        });
    }
    tracing::info!(
        target: "build",
        "✓ {label} ({:.1}s)",
        started.elapsed().as_secs_f32()
    );

    // Each `compiler-artifact` line carries a `"filenames":[...]` array whose
    // entries are quoted paths, so splitting on `"` yields each path as one token.
    // That is enough to find the cdylib without pulling in a JSON parser.
    for line in json.lines() {
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

/// Fill every artifact path without running cargo (`--no-build`): a prebuilt
/// artifact selects by triple as always, a crate artifact is searched for
/// under the workspace target directory. A missing
/// library is a hard [`BuildError::NotBuilt`]. No sidecar work — a previously
/// built library has its sidecar adjacent, and resolve's staleness gate
/// covers it.
pub fn locate_artifacts(
    wiring: &mut Wiring,
    target_dir: &Path,
    release: bool,
) -> Result<(), BuildError> {
    for artifact in &mut wiring.artifacts {
        if let Some(dir) = artifact.prebuilt_dir.clone() {
            artifact.path = Some(select_prebuilt(artifact, &dir, None)?);
            continue;
        }
        let cdylib = super::cdylib_file_name(&artifact.lib);
        let path =
            locate_built(target_dir, &cdylib, release).ok_or_else(|| BuildError::NotBuilt {
                crate_name: artifact.crate_name.clone(),
                cdylib: cdylib.clone(),
            })?;
        artifact.path = Some(path);
    }
    Ok(())
}

/// Search the workspace target directory for an already-built cdylib,
/// respecting `CARGO_TARGET_DIR` and the profile, without running cargo.
pub(super) fn locate_built(search_root: &Path, cdylib: &str, release: bool) -> Option<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(dir));
    }
    // Walk up from the search root looking for a `target/` sibling of a
    // workspace `Cargo.toml`.
    let mut dir = search_root.canonicalize().ok();
    while let Some(d) = dir {
        roots.push(d.join("target"));
        dir = d.parent().map(Path::to_path_buf);
    }
    for root in roots {
        let candidate = root.join(profile).join(cdylib);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
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
    lib: &str,
    so: &Path,
    opts: &BuildOptions,
    cross: bool,
) -> Result<(), BuildError> {
    let described = if cross {
        let host_opts = BuildOptions {
            extra_args: strip_target_args(&opts.extra_args),
            ..opts.clone()
        };
        // The host twin's output carries the *host* platform's file name.
        let cdylib = super::cdylib_file_name(lib);
        let label = format!("{crate_name} (host sidecar twin)");
        build_cdylib("build", crate_name, &cdylib, &host_opts, &label).map_err(|source| {
            BuildError::HostBuild {
                crate_name: crate_name.to_string(),
                source: Box::new(source),
            }
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

/// The triple a build with `extra_args` targets, for the bundle's `meta.json`:
/// the explicit `--target` when one is given (cross or not), else the host
/// triple. `None` when neither can be determined (no `--target` and `cargo -vV`
/// unavailable), in which case the bundle records no target and load skips the
/// triple check.
pub fn build_target(extra_args: &[String]) -> Option<String> {
    requested_target(extra_args)
        .map(str::to_string)
        .or_else(host_triple)
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
/// `--target=<t>` form; the last occurrence wins, matching cargo. Shared with
/// [`bundle`](super::bundle), which records the built triple in `meta.json`.
pub(super) fn requested_target(extra_args: &[String]) -> Option<&str> {
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

/// The host target triple. Shared with [`bundle`](super::bundle): packaging
/// records it as the built triple (absent a cross `--target`) and loading
/// compares it against the bundle's; prebuilt selection uses it absent a
/// `--target`.
///
/// The compile-time triple of this binary answers first — the running host is
/// its own witness, and a prebuilt-only consumer has no cargo to ask. An
/// unlisted platform falls back to `cargo -vV`'s `host:` line.
pub(super) fn host_triple() -> Option<String> {
    compiled_triple()
        .map(str::to_string)
        .or_else(cargo_host_triple)
}

/// The triple this binary was compiled for, for the platforms metor-fsw
/// ships on. `None` on an unlisted platform.
fn compiled_triple() -> Option<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "musl"
    )) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "windows",
        target_env = "msvc"
    )) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// `cargo -vV`'s `host:` line, the fallback witness on platforms
/// [`compiled_triple`] does not list.
fn cargo_host_triple() -> Option<String> {
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

/// Copy via a temp file + rename. Replacing the destination at a fresh inode
/// is load-bearing for dylib payloads on macOS: the kernel kill-caches code
/// signatures by inode, so a process that `dlopen`s an in-place-overwritten
/// dylib is SIGKILLed even though the new signature is valid.
pub(super) fn copy_atomic(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst)
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

    // Shared with the dl and stubgen fixture tests: all three build and
    // describe the same fixture pack into one target-dir sidecar.
    #[cfg(not(miri))]
    use crate::dl::FIXTURE_LOCK;

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
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut wiring = fixture_wiring();
        if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
            eprintln!("skipping: provision_artifacts failed: {e}");
            return;
        }
        let so = wiring.artifacts[0].path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        let bytes = std::fs::read(&sidecar).expect("sidecar written next to the .so");
        let described = crate::dl::describe_raw(&so).expect("in-process describe");
        assert_eq!(bytes, described, "sidecar bytes ≡ fsw_pack_describe bytes");
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
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut wiring = fixture_wiring();
        if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
            eprintln!("skipping: provision_artifacts failed: {e}");
            return;
        }
        let artifact = &wiring.artifacts[0];
        let so = artifact.path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        let opts = BuildOptions::default();

        // Plant a fresh-but-divergent "target" sidecar; the host-described
        // bytes must refuse to replace it.
        std::fs::write(&sidecar, b"divergent").unwrap();
        let err = write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &so, &opts, true)
            .expect_err("a fresh divergent sidecar is refused");
        assert!(
            matches!(err, BuildError::ManifestDivergence { .. }),
            "{err}"
        );

        // Identical bytes pass the comparison and are rewritten in place.
        let described = crate::dl::describe_raw(&so).expect("in-process describe");
        std::fs::write(&sidecar, &described).unwrap();
        write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &so, &opts, true)
            .expect("a matching sidecar is accepted");
        assert_eq!(std::fs::read(&sidecar).unwrap(), described);
    }

    /// `manifest_sidecar: false` builds the library and writes nothing next
    /// to it.
    #[test]
    #[cfg(not(miri))]
    fn sidecar_opt_out_writes_nothing() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let opts = BuildOptions {
            manifest_sidecar: false,
            ..BuildOptions::default()
        };
        let mut wiring = fixture_wiring();
        if let Err(e) = provision_artifacts(&mut wiring, &opts) {
            eprintln!("skipping: provision_artifacts failed: {e}");
            return;
        }
        let so = wiring.artifacts[0].path.clone().expect("path filled");
        let sidecar = crate::dl::manifest_sidecar_path(&so);
        // Another test (or an earlier run) may have left one; the assertion
        // is that *this* build does not produce it.
        let _ = std::fs::remove_file(&sidecar);
        let mut again = fixture_wiring();
        provision_artifacts(&mut again, &opts).expect("rebuild is a cargo no-op");
        assert!(!sidecar.exists(), "opted out, so no sidecar is written");
    }

    /// A prebuilt artifact is a selection, not a build: the host triple picks
    /// `<dir>/<triple>/<cdylib>`, the sidecar rides adjacent for the resolve
    /// gate, and a triple the directory does not ship errors naming the ones
    /// it does. Laid out from the fixture's host build — exactly the shape
    /// `pack dev` and an installed pack wheel produce.
    #[test]
    #[cfg(not(miri))]
    fn prebuilt_selects_by_triple() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut built = fixture_wiring();
        if let Err(e) = provision_artifacts(&mut built, &BuildOptions::default()) {
            eprintln!("skipping: provision_artifacts failed: {e}");
            return;
        }
        let so = built.artifacts[0].path.clone().expect("path filled");
        let host = host_triple().expect("host triple determinable");

        // Lay out `<libs>/<host-triple>/{cdylib, sidecar}`.
        let tmp = tempfile::tempdir().unwrap();
        let libs = tmp.path().join("libs");
        let triple_dir = libs.join(&host);
        std::fs::create_dir_all(&triple_dir).unwrap();
        let dst = triple_dir.join(so.file_name().unwrap());
        std::fs::copy(&so, &dst).unwrap();
        std::fs::copy(
            crate::dl::manifest_sidecar_path(&so),
            crate::dl::manifest_sidecar_path(&dst),
        )
        .unwrap();

        let mut wiring = fixture_wiring();
        wiring.artifacts[0].prebuilt_dir = Some(libs.clone());
        provision_artifacts(&mut wiring, &BuildOptions::default())
            .expect("prebuilt selection needs no cargo");
        assert_eq!(wiring.artifacts[0].path.as_deref(), Some(dst.as_path()));

        // A triple the directory does not ship is a clean error listing the
        // triples it does.
        let mut cross = fixture_wiring();
        cross.artifacts[0].prebuilt_dir = Some(libs);
        let opts = BuildOptions {
            extra_args: vec!["--target".into(), "riscv64gc-unknown-linux-gnu".into()],
            ..BuildOptions::default()
        };
        let err = provision_artifacts(&mut cross, &opts).expect_err("missing triple is refused");
        match err {
            BuildError::PrebuiltMissing {
                artifact,
                triple,
                available,
                ..
            } => {
                assert_eq!(artifact, "fixture");
                assert_eq!(triple, "riscv64gc-unknown-linux-gnu");
                assert_eq!(available, vec![host]);
            }
            other => panic!("expected PrebuiltMissing, got {other}"),
        }
    }

    /// `locate_artifacts` finds an already-built library without cargo, and a
    /// never-built one is a hard `NotBuilt` — the `run --no-build` contract.
    #[test]
    #[cfg(not(miri))]
    fn locate_artifacts_finds_built_and_refuses_missing() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut built = fixture_wiring();
        if let Err(e) = provision_artifacts(&mut built, &BuildOptions::default()) {
            eprintln!("skipping: provision_artifacts failed: {e}");
            return;
        }
        let so = built.artifacts[0].path.clone().expect("path filled");

        // Locating from the crate dir walks up to the same workspace target
        // dir the build wrote into.
        let mut wiring = fixture_wiring();
        locate_artifacts(&mut wiring, Path::new("."), false).expect("built library is found");
        assert_eq!(
            wiring.artifacts[0].path.as_ref().unwrap().file_name(),
            so.file_name()
        );

        // A library that was never built is a hard error naming the crate.
        let mut missing = super::super::WiringBuilder::new()
            .artifact("missing", "no-such-crate", "metor_fsw_2_never_built")
            .build();
        let err = locate_artifacts(&mut missing, Path::new("."), false)
            .expect_err("a never-built library is refused");
        assert!(matches!(err, BuildError::NotBuilt { .. }), "{err}");
    }

    /// The compile-time triple and cargo's own view of the host agree, so the
    /// cargo-free fallback never changes what a bundle records or a prebuilt
    /// selection picks.
    #[test]
    fn compiled_triple_matches_cargo() {
        let (Some(compiled), Some(cargo)) = (compiled_triple(), cargo_host_triple()) else {
            eprintln!("skipping: platform unlisted or cargo unavailable");
            return;
        };
        assert_eq!(compiled, cargo);
    }
}
