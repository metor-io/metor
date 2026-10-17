//! Builds packs with cargo and reads their `pyproject.toml` settings.
//!
//! A pack builds as a native cdylib.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::pack_dev::PackDevError;

/// An error building a pack's cdylib.
#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error("running cargo: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("cargo build failed: {0}")]
    Cargo(ExitStatus),
    #[error("cargo built no {0} for `{1}`")]
    NoArtifact(&'static str, String),
}

/// Builds `package` from `manifest_dir` and returns the path of its cdylib.
///
/// Cargo's stderr is passed through, so the user sees its progress.
pub fn cargo_build(package: &str, manifest_dir: &Path) -> Result<PathBuf, CargoError> {
    let args = ["build", "-p", package, "--message-format=json"];
    let stdout = cargo(manifest_dir, &args)?;
    cdylib(&stdout).ok_or_else(|| CargoError::NoArtifact("cdylib", package.to_string()))
}

/// Runs cargo with `args` in `dir` and returns its stdout.
///
/// Cargo's stderr is passed through, so the user sees its progress.
fn cargo(dir: &Path, args: &[&str]) -> Result<Vec<u8>, CargoError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(CargoError::Cargo(output.status));
    }
    Ok(output.stdout)
}

/// Finds the native library of the last cdylib in cargo's JSON output.
fn cdylib(stdout: &[u8]) -> Option<PathBuf> {
    cdylib_file(stdout, |name| {
        !name.ends_with(".rlib") && !name.ends_with(".rmeta")
    })
}

/// Returns the first file matching `pick` from the last cdylib artifact in
/// cargo's JSON output.
fn cdylib_file(stdout: &[u8], pick: impl Fn(&str) -> bool) -> Option<PathBuf> {
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
            .find(|name| pick(name))
            .map(PathBuf::from)
    })
}

/// A pack's names and build settings, read from its `pyproject.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackConfig {
    /// The distribution name, from `[project] name`.
    pub dist: String,
    /// The pack id. Every system type in the pack starts with it.
    pub id: String,
    /// The cargo package to build.
    pub krate: String,
    /// The cdylib stem, without the platform's prefix and extension.
    pub lib: String,
    /// The Python module the generated code is written to.
    pub module: String,
}

impl PackConfig {
    /// Reads `<root>/pyproject.toml`. Fields it leaves out are filled in from
    /// `Cargo.toml` where possible.
    pub fn read(root: &Path) -> Result<PackConfig, PackDevError> {
        let PyProject { project, tool } = read_toml(&root.join("pyproject.toml"))?;
        let pack = tool.metor.pack;
        let module = pack.module.unwrap_or_else(|| identifier(&project.name));
        let (krate, lib) = match (pack.krate, pack.lib) {
            (Some(krate), Some(lib)) => (krate, lib),
            (krate, lib) => cargo_names(root, krate, lib)?,
        };
        Ok(PackConfig {
            id: pack.id.unwrap_or_else(|| module.clone()),
            dist: project.name,
            krate,
            lib,
            module,
        })
    }
}

/// The parts of a pack's `pyproject.toml` that [`PackConfig`] reads.
#[derive(Deserialize)]
struct PyProject {
    project: Project,
    #[serde(default)]
    tool: Tool,
}

#[derive(Deserialize)]
struct Project {
    name: String,
}

#[derive(Default, Deserialize)]
struct Tool {
    #[serde(default)]
    metor: MetorTool,
}

#[derive(Default, Deserialize)]
struct MetorTool {
    #[serde(default)]
    pack: PackTable,
}

/// `[tool.metor.pack]`.
#[derive(Default, Deserialize)]
struct PackTable {
    id: Option<String>,
    module: Option<String>,
    #[serde(rename = "crate")]
    krate: Option<String>,
    lib: Option<String>,
}

/// The parts of a pack's `Cargo.toml` that [`PackConfig`] reads.
#[derive(Deserialize)]
struct CargoToml {
    package: Package,
    lib: Option<Lib>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
}

#[derive(Deserialize)]
struct Lib {
    name: Option<String>,
}

/// Returns the cargo package name and cdylib stem.
///
/// Any that are not given come from `<root>/Cargo.toml`: the package from
/// `[package] name`, and the stem from `[lib] name`, falling back to the
/// package name.
fn cargo_names(
    root: &Path,
    krate: Option<String>,
    lib: Option<String>,
) -> Result<(String, String), PackDevError> {
    let cargo: CargoToml = read_toml(&root.join("Cargo.toml"))?;
    let krate = krate.unwrap_or(cargo.package.name);
    let lib = lib
        .or_else(|| cargo.lib.and_then(|lib| lib.name))
        .unwrap_or_else(|| identifier(&krate));
    Ok((krate, lib))
}

/// Reads the TOML file at `path` as a `T`.
pub(super) fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T, PackDevError> {
    let text = std::fs::read_to_string(path).map_err(|source| PackDevError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| PackDevError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

/// Turns a distribution or crate name into a Python or Rust identifier.
fn identifier(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT: &str = r#"{"reason":"compiler-artifact",
        "target":{"name":"echo_pack","kind":["cdylib"]},
        "filenames":["/t/libecho_pack.dylib"]}"#;

    #[test]
    fn test_parse_cdylib_artifact() {
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
    fn test_skip_rlib_artifact() {
        let line = r#"{"reason":"compiler-artifact",
            "target":{"name":"metor_fsw_3","kind":["lib"]},
            "filenames":["/t/libmetor_fsw_3.rlib"]}"#
            .replace('\n', "");
        assert_eq!(cdylib(line.as_bytes()), None);
    }

    #[test]
    fn test_skip_non_json_output() {
        assert_eq!(cdylib(b"warning: something\n"), None);
        assert_eq!(cdylib(b""), None);
    }

    /// Creates a pack directory with the two manifests `PackConfig::read`
    /// reads.
    fn root(pyproject: &str, cargo: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("pyproject.toml"), pyproject).expect("writes");
        std::fs::write(dir.path().join("Cargo.toml"), cargo).expect("writes");
        dir
    }

    #[test]
    fn test_pyproject_defaults_from_cargo() {
        let dir = root(
            "[project]\nname = \"adcs-pack\"\n",
            "[package]\nname = \"adcs-systems\"\n",
        );
        assert_eq!(
            PackConfig::read(dir.path()).expect("reads"),
            PackConfig {
                dist: "adcs-pack".into(),
                id: "adcs_pack".into(),
                krate: "adcs-systems".into(),
                lib: "adcs_systems".into(),
                module: "adcs_pack".into(),
            }
        );
    }

    #[test]
    fn test_pyproject_overrides_defaults() {
        let dir = root(
            "[project]\nname = \"adcs-pack\"\n\n[tool.metor.pack]\n\
             id = \"adcs\"\ncrate = \"adcs\"\nlib = \"adcs_dylib\"\nmodule = \"adcs_mod\"\n",
            "[package]\nname = \"ignored\"\n\n[lib]\nname = \"ignored_too\"\n",
        );
        assert_eq!(
            PackConfig::read(dir.path()).expect("reads"),
            PackConfig {
                dist: "adcs-pack".into(),
                id: "adcs".into(),
                krate: "adcs".into(),
                lib: "adcs_dylib".into(),
                module: "adcs_mod".into(),
            }
        );
    }

    #[test]
    fn test_cargo_library_override_and_missing_project() {
        let dir = root(
            "[project]\nname = \"adcs-pack\"\n",
            "[package]\nname = \"adcs\"\n\n[lib]\nname = \"adcs_systems\"\n",
        );
        assert_eq!(
            PackConfig::read(dir.path()).expect("reads").lib,
            "adcs_systems"
        );

        let dir = root(
            "[tool.metor.pack]\nid = \"adcs\"\n",
            "[package]\nname = \"adcs\"\n",
        );
        let Err(PackDevError::Toml { source, .. }) = PackConfig::read(dir.path()) else {
            panic!("a pyproject.toml without [project] is an error")
        };
        assert!(source.to_string().contains("project"), "{source}");
    }
}
