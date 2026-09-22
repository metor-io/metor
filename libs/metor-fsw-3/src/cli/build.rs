//! Building a pack's cdylib with cargo, finding what it wrote, and the
//! `pyproject.toml` fields that name it.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use super::pack_dev::PackDevError;

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

/// A pack's identity as its `pyproject.toml` spells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackConfig {
    /// The distribution name, `[project] name`.
    pub dist: String,
    /// The pack id, the prefix every `ty` carries.
    pub id: String,
    /// The cargo package to build.
    pub krate: String,
    /// The cdylib stem, without the platform's prefix and extension.
    pub lib: String,
    /// The Python module the generated code lands in.
    pub module: String,
}

impl PackConfig {
    /// Reads `<root>/pyproject.toml`, filling every field cargo can supply.
    pub fn read(root: &Path) -> Result<PackConfig, PackDevError> {
        let pyproject = read_toml(&root.join("pyproject.toml"))?;
        let dist = string(&pyproject, &["project", "name"])
            .ok_or(PackDevError::MissingField("[project] name"))?;
        let field = |key| string(&pyproject, &["tool", "metor", "pack", key]);
        let module = field("module").unwrap_or_else(|| identifier(&dist));
        let krate = match field("crate") {
            Some(krate) => krate,
            None => cargo_package(root)?,
        };
        let lib = match field("lib") {
            Some(lib) => lib,
            None => cargo_lib(root)?.unwrap_or_else(|| identifier(&krate)),
        };
        Ok(PackConfig {
            id: field("id").unwrap_or_else(|| module.clone()),
            dist,
            krate,
            lib,
            module,
        })
    }
}

/// The cargo package name, `[package] name` of `<root>/Cargo.toml`.
fn cargo_package(root: &Path) -> Result<String, PackDevError> {
    string(&read_toml(&root.join("Cargo.toml"))?, &["package", "name"])
        .ok_or(PackDevError::MissingField("[package] name"))
}

/// The `[lib] name` of `<root>/Cargo.toml`, absent when cargo derives it.
fn cargo_lib(root: &Path) -> Result<Option<String>, PackDevError> {
    Ok(string(
        &read_toml(&root.join("Cargo.toml"))?,
        &["lib", "name"],
    ))
}

pub(super) fn read_toml(path: &Path) -> Result<toml::Value, PackDevError> {
    let text = std::fs::read_to_string(path).map_err(|source| PackDevError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse().map_err(|source| PackDevError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

/// The string at `path`, absent when any step of it is.
pub(super) fn string(table: &toml::Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(table, |value, key| value.get(key))?
        .as_str()
        .map(str::to_string)
}

/// A distribution or crate name as a Python or Rust identifier.
fn identifier(name: &str) -> String {
    name.replace('-', "_")
}

/// The host target triple, as cargo prints it.
pub fn triple() -> String {
    let arch = std::env::consts::ARCH;
    #[cfg(target_os = "macos")]
    let rest = "apple-darwin";
    #[cfg(all(target_os = "linux", target_env = "musl"))]
    let rest = "unknown-linux-musl";
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    let rest = "unknown-linux-gnu";
    format!("{arch}-{rest}")
}

/// The file name a cdylib of `stem` has on this host.
pub fn cdylib_name(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// Copies `src` over `dst` through a temp file beside it, then renames.
///
/// macOS kill-caches a code signature by inode, so a dylib written in place
/// kills the next process to `dlopen` it.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    let tmp = temp_beside(dst);
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst)
}

/// Writes `bytes` to `dst` the way [`copy_atomic`] copies onto it.
pub fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = temp_beside(dst);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dst)
}

/// A name for the temp file a replacement is renamed from, unique per process.
fn temp_beside(dst: &Path) -> PathBuf {
    let mut name = dst.as_os_str().to_owned();
    name.push(format!(".tmp{}", std::process::id()));
    PathBuf::from(name)
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

    /// A pack directory holding the two manifests `PackConfig::read` looks at.
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
        assert!(matches!(
            PackConfig::read(dir.path()),
            Err(PackDevError::MissingField("[project] name"))
        ));
    }

    #[test]
    fn test_copy_atomic_replaces_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, b"new").expect("writes");
        std::fs::write(&dst, b"old").expect("writes");
        copy_atomic(&src, &dst).expect("copies");
        assert_eq!(std::fs::read(&dst).expect("reads"), b"new");

        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temp file is left: {names:?}");
    }

    #[test]
    fn test_host_artifact_names() {
        let triple = triple();
        assert!(triple.starts_with(std::env::consts::ARCH), "{triple}");
        assert_eq!(triple.split('-').count(), 3, "{triple}");
        let name = cdylib_name("echo_pack");
        assert!(name.contains("echo_pack"));
        assert!(name.ends_with(std::env::consts::DLL_SUFFIX));
    }
}
