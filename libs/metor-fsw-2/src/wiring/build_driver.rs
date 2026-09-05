//! Build or locate the artifacts referenced by [`Wiring`].
//!
//! Cargo artifacts are located through compiler-artifact JSON, respecting
//! custom target directories and profiles. Prebuilt artifacts are selected
//! from `<dir>/<target-triple>/<library>`.
//!
//! Unless disabled, each library receives a `.manifest` sidecar. Creating it
//! executes the pack's `pack()` function. Cross-compilation builds a host copy
//! to describe, since the target library cannot run on the build machine.

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
    /// [`PackManifest`](metor_fsw_2_core::abi::PackManifest) bytes from describing
    /// a host-runnable build of the pack.
    ///
    /// Sidecar generation executes the crate's `pack()` at build time, the
    /// same trust model as a `build.rs`. Opt out for pack crates that cannot
    /// be built for the host architecture: a cross build sources its sidecar
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
    /// much later as a codegen or staleness mystery.
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
    /// The target's captured Python program did not compile. `detail`
    /// carries the diagnostics, each mapped to its `target.py` line.
    #[error("compiling artifact `{artifact}` from the target program failed:\n{detail}")]
    ProgramCompile {
        /// The program artifact's id.
        artifact: String,
        /// The rendered diagnostics.
        detail: String,
    },
    /// A wasm artifact with no path, no prebuilt dir, and no program behind
    /// it: nothing can produce its module.
    #[error("wasm artifact `{artifact}` has no path and nothing to build it from")]
    WasmSourceless {
        /// The artifact id.
        artifact: String,
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
    // Program artifacts compile last, against the other artifacts' built
    // manifests, which the build-time resolver reads from their sidecars.
    let mut programs: Vec<usize> = Vec::new();
    for (at, artifact) in wiring.artifacts.iter_mut().enumerate() {
        if let Some(dir) = artifact.prebuilt_dir.clone() {
            artifact.path = Some(select_prebuilt(artifact, &dir, target.as_deref())?);
            continue;
        }
        // The kind dispatch also guards the dl arm: a wasm module must never
        // reach `cargo build` (or, later, a dlopen) by silent fallthrough.
        if artifact.kind == crate::ir::ArtifactKind::Wasm {
            match (artifact.path.is_some(), artifact.is_program()) {
                // Located or builder-authored: one arch-neutral file, as-is.
                (true, _) => {}
                (false, true) => programs.push(at),
                (false, false) => {
                    return Err(BuildError::WasmSourceless {
                        artifact: artifact.id.clone(),
                    });
                }
            }
            continue;
        }
        let cdylib = match &target {
            Some(triple) => super::cdylib_file_name_for(triple, &artifact.lib),
            None => super::cdylib_file_name(&artifact.lib),
        };
        let label = match target.as_deref() {
            Some(triple) => format!("{} ({triple})", artifact.crate_name),
            None => artifact.crate_name.clone(),
        };
        let path = build_cdylib(&artifact.crate_name, &cdylib, opts, &label)?;
        if opts.manifest_sidecar {
            write_manifest_sidecar(&artifact.crate_name, &artifact.lib, &path, opts, cross)?;
        }
        artifact.path = Some(path);
    }
    for at in programs {
        let out_dir = wasm_out_dir(wiring, opts.release);
        let path = super::program::provision_program(wiring, at, &out_dir)?;
        wiring.artifacts[at].path = Some(path);
    }
    Ok(())
}

/// Where a program-compiled module lands: next to the crate-built cdylibs
/// when the target has any (one directory holds every produced artifact),
/// else the workspace `target/<profile>` dir, found like [`locate_built`]
/// and created on demand.
fn wasm_out_dir(wiring: &Wiring, release: bool) -> PathBuf {
    if let Some(dir) = wiring
        .artifacts
        .iter()
        .filter(|a| a.prebuilt_dir.is_none() && a.kind == crate::ir::ArtifactKind::Cdylib)
        .filter_map(|a| a.path.as_deref().and_then(Path::parent))
        .next()
    {
        return dir.to_path_buf();
    }
    let profile = if release { "release" } else { "debug" };
    target_root().join(profile)
}

/// The workspace `target/` dir: `CARGO_TARGET_DIR`, else the nearest
/// existing `target/` walking up from the working directory, else a fresh
/// `./target`.
fn target_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let target = d.join("target");
        if target.exists() {
            return target;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    PathBuf::from("target")
}

/// Select a prebuilt artifact's library for `target` (or the host): the
/// `<dir>/<triple>/<cdylib>` the pack shipped. No compile, no sidecar work;
/// the shipped sidecar sits next to the library and resolve verifies it.
fn select_prebuilt(
    artifact: &super::model::Artifact,
    dir: &Path,
    target: Option<&str>,
) -> Result<PathBuf, BuildError> {
    let Some(triple) = target.map(str::to_string).or_else(host_triple) else {
        return Err(BuildError::HostTripleUnknown {
            artifact: artifact.id.clone(),
        });
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

/// Runs a cargo build and returns the located cdylib path.
///
/// The build reports through tracing: a `build`-target span for its whole
/// duration (the CLI renders active spans as a pinned progress line, keyed on
/// `label`), each cargo stderr line as a `cargo`-target event as it arrives,
/// and a `build`-target completion event. Without a subscriber all of it is
/// silent, so library callers see no output.
pub(super) fn build_cdylib(
    crate_name: &str,
    cdylib: &str,
    opts: &BuildOptions,
    label: &str,
) -> Result<PathBuf, BuildError> {
    // Prefer the cargo that invoked this process, falling back to PATH.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "-p", crate_name, "--message-format=json"]);
    if opts.release {
        cmd.arg("--release");
    }
    cmd.args(&opts.extra_args);
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

        // Stream cargo's stderr through tracing as it arrives, keeping a copy
        // for the failure diagnostic, while this thread drains the JSON
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
                        // `Compiling <name> <version> (<path>)`; drop the path.
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
/// under the workspace target directory. A missing library is a hard
/// [`BuildError::NotBuilt`]. No sidecar work; a previously built library has
/// its sidecar adjacent, and resolve's staleness gate covers it.
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
        // A previously compiled program module is located like a built
        // cdylib; `--no-build` never compiles, by the same contract.
        let file = match artifact.kind {
            crate::ir::ArtifactKind::Wasm => {
                if artifact.path.is_some() {
                    continue;
                }
                format!("{}.wasm", artifact.id)
            }
            crate::ir::ArtifactKind::Cdylib => super::cdylib_file_name(&artifact.lib),
        };
        let path =
            locate_built(target_dir, &file, release).ok_or_else(|| BuildError::NotBuilt {
                crate_name: artifact.crate_name.clone(),
                cdylib: file.clone(),
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

/// Source and write the `<so>.manifest` sidecar for one built artifact.
///
/// A native build describes the just-built library. A cross build cannot run
/// its own output, so the crate is additionally built for the host (no
/// `--target`) and that twin is described instead; a host build failure is a
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
        build_cdylib(crate_name, &cdylib, &host_opts, &label).map_err(|source| {
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
/// Prefers a describe worker, the dlopen quarantined in a short-lived child,
/// the same isolation `resolve` gives process systems, but re-executing this
/// binary as a worker requires a `main` that installed
/// [`worker_entry`](crate::proc::worker_entry); a libtest binary would run
/// its harness instead. Without the guard, and on targets without worker
/// machinery, the library is described in-process.
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
    requested_target(extra_args).is_some_and(|triple| host_triple().as_deref() != Some(triple))
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

/// `extra_args` with every `--target` (and its value) removed, the args for
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
/// The compile-time triple of this binary answers first: the running host is
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
/// library it sits next to. A stale sidecar describes an older build that
/// cargo has since rewritten, so it must not be compared against.
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
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
mod tests;
