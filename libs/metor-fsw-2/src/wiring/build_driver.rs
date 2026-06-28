//! The build driver — turn a [`Wiring`]'s [`Artifact`](super::Artifact)s into located
//! `.so`s (dl-open.md §6.4).
//!
//! For each artifact it runs `cargo build -p <crate_name>` and locates the produced
//! `cdylib`, writing its path back into [`Artifact::path`](super::Artifact::path) so the
//! resolver ([`resolve`](super::resolve)) can `dlopen` it. Because each system cdylib is
//! its own cargo package, **cargo already does the incremental work** — only changed
//! crates recompile; the driver's value is knowing *which* crates a mission needs and
//! *where* their `.so`s land. It is std-only (`std::process::Command`), no new deps.
//!
//! The `.so` is located the same way `tests/dl_integration.rs` does it: parse cargo's
//! `--message-format=json` `compiler-artifact` lines for the file matching the
//! artifact's `cdylib` name (robust to a custom target dir / profile).

use std::path::PathBuf;
use std::process::Command;

use super::model::Wiring;

/// Options for the build driver (dl-open.md §6.4).
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Build the `--release` profile (default: debug, matching `cargo build`).
    pub release: bool,
    /// Extra args appended to every `cargo build` invocation (e.g. `--target ...`).
    pub extra_args: Vec<String>,
}

/// A build-driver failure (dl-open.md §6.4). Each is a clean error — a build problem
/// never panics the caller.
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
}

/// Build every [`Artifact`](super::Artifact) in `wiring` and fill in its
/// [`path`](super::Artifact::path) (dl-open.md §6.4). Idempotent and incremental —
/// re-running only rebuilds crates cargo considers stale.
pub fn build_artifacts(wiring: &mut Wiring, opts: &BuildOptions) -> Result<(), BuildError> {
    for artifact in &mut wiring.artifacts {
        let path = build_one(&artifact.crate_name, &artifact.cdylib, opts)?;
        artifact.path = Some(path);
    }
    Ok(())
}

/// `cargo build -p <crate_name>` and return the located cdylib path.
fn build_one(crate_name: &str, cdylib: &str, opts: &BuildOptions) -> Result<PathBuf, BuildError> {
    // `env!("CARGO")` is the cargo that built this crate; fall back to PATH otherwise.
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

    // Each `compiler-artifact` JSON line carries a `"filenames":[ ... ]` array; the
    // cdylib is the entry ending in the requested file name. Scan without a JSON dep,
    // mirroring `tests/dl_integration.rs::locate_fixture`.
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
