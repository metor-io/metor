//! Finds the prebuilt mc-rt pack library, `libmc_rt_pack.a`, that AOT
//! builds link against.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The checkout this compiler was built from, which can build the runtime.
const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");
const LIB: &str = "libmc_rt_pack.a";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("no runtime library: set METOR_CODE_RT or pass --runtime")]
    Missing,
    #[error("building the runtime: {0}")]
    Io(#[from] std::io::Error),
    #[error("cargo could not build the runtime ({0})")]
    Cargo(std::process::ExitStatus),
}

/// Returns the runtime library for `triple`.
///
/// It is `$METOR_CODE_RT`, else one built next to this executable for the
/// host, else one cargo builds from this checkout.
pub fn runtime_lib(triple: Option<&str>) -> Result<PathBuf, RuntimeError> {
    if let Some(path) = std::env::var_os("METOR_CODE_RT") {
        return Ok(path.into());
    }
    if triple.is_none()
        && let Some(sibling) = std::env::current_exe()?.parent().map(|dir| dir.join(LIB))
        && sibling.is_file()
    {
        return Ok(sibling);
    }
    if Path::new(REPO).join("libs/metor-code/rt-pack").is_dir() {
        return build(triple);
    }
    Err(RuntimeError::Missing)
}

/// Builds the runtime with cargo, in release, and returns its path.
pub fn build(triple: Option<&str>) -> Result<PathBuf, RuntimeError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(REPO)
        .args(["build", "-p", "mc-rt-pack", "--release", "--message-format=json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(triple) = triple {
        cmd.args(["--target", triple]);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(RuntimeError::Cargo(output.status));
    }
    archive(&output.stdout).ok_or(RuntimeError::Missing)
}

/// Finds the static library among cargo's artifact messages.
fn archive(stdout: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg["reason"] == "compiler-artifact")
        .flat_map(|msg| msg["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|name| name.as_str().map(PathBuf::from))
        .find(|path| path.file_name().is_some_and(|n| n == LIB))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_finds_the_archive_in_cargo_output() {
        let out = br#"{"reason":"compiler-artifact","filenames":["/t/release/libmc_rt.rlib"]}
{"reason":"compiler-artifact","filenames":["/t/release/libmc_rt_pack.a"]}
{"reason":"build-finished","success":true}"#;
        assert_eq!(archive(out), Some(PathBuf::from("/t/release/libmc_rt_pack.a")));
        assert_eq!(archive(b"not json"), None);
    }
}
