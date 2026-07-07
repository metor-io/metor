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

use std::path::PathBuf;
use std::process::Command;

use super::model::Wiring;

/// Options for the build driver.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Build the `--release` profile instead of the default debug profile.
    pub release: bool,
    /// Extra args appended to every `cargo build` invocation (e.g. `--target ...`).
    pub extra_args: Vec<String>,
}

/// A build-driver failure.
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

/// Builds every [`Artifact`](super::Artifact) in `wiring` and fills in its
/// [`path`](super::Artifact::path). Safe to re-run; cargo rebuilds only what it
/// considers stale.
pub fn build_artifacts(wiring: &mut Wiring, opts: &BuildOptions) -> Result<(), BuildError> {
    for artifact in &mut wiring.artifacts {
        let path = build_one(&artifact.crate_name, &artifact.cdylib, opts)?;
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
