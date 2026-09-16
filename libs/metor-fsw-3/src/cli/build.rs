//! Building a pack's cdylib with cargo, and finding what it wrote.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// A `BuildError` is why a pack's cdylib did not appear.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("running cargo: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("cargo build failed: {0}")]
    Cargo(ExitStatus),
    #[error("cargo built no cdylib for `{0}`")]
    NoCdylib(String),
}

/// Builds `package` from `manifest_dir` and returns the cdylib cargo wrote.
///
/// Cargo's stderr is inherited, so its progress is the caller's progress.
pub fn cargo_build(
    package: &str,
    manifest_dir: &Path,
    release: bool,
) -> Result<PathBuf, BuildError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(manifest_dir)
        .args(["build", "-p", package, "--message-format=json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if release {
        command.arg("--release");
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(BuildError::Cargo(output.status));
    }
    cdylib(&output.stdout).ok_or_else(|| BuildError::NoCdylib(package.to_string()))
}

/// Reads the last cdylib filename out of cargo's `compiler-artifact` lines.
fn cdylib(stdout: &[u8]) -> Option<PathBuf> {
    stdout.split(|&byte| byte == b'\n').rev().find_map(|line| {
        let message: serde_json::Value = serde_json::from_slice(line).ok()?;
        if message["reason"] != "compiler-artifact" {
            return None;
        }
        let kinds = message["target"]["kind"].as_array()?;
        if !kinds.iter().any(|kind| kind == "cdylib") {
            return None;
        }
        let names = message["filenames"].as_array()?;
        names
            .iter()
            .filter_map(|name| name.as_str())
            .find(|name| !name.ends_with(".rlib") && !name.ends_with(".rmeta"))
            .map(PathBuf::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT: &str = r#"{"reason":"compiler-artifact",
        "target":{"name":"echo_pack","kind":["cdylib"]},
        "filenames":["/t/libecho_pack.dylib"]}"#;

    #[test]
    fn the_cdylib_comes_from_the_artifact_line() {
        let stdout = format!(
            "{{\"reason\":\"compiler-message\"}}\n{}\n",
            ARTIFACT.replace('\n', "")
        );
        assert_eq!(
            cdylib(stdout.as_bytes()),
            Some(PathBuf::from("/t/libecho_pack.dylib"))
        );
    }

    #[test]
    fn an_rlib_only_artifact_is_no_cdylib() {
        let line = r#"{"reason":"compiler-artifact",
            "target":{"name":"metor_fsw_3","kind":["lib"]},
            "filenames":["/t/libmetor_fsw_3.rlib"]}"#
            .replace('\n', "");
        assert_eq!(cdylib(line.as_bytes()), None);
    }

    #[test]
    fn noise_that_is_no_json_is_skipped() {
        assert_eq!(cdylib(b"warning: something\n"), None);
        assert_eq!(cdylib(b""), None);
    }
}
