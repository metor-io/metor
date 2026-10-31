//! Builds packs with cargo and reads their `pyproject.toml` settings.
//!
//! A pack builds as a native cdylib, and optionally as a wasm module too.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::pack_dev::PackDevError;
use crate::dl::WASM_TRIPLE;

/// An error building a pack's cdylib or wasm module.
#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error("running cargo: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("cargo build failed: {status}")]
    Cargo {
        status: ExitStatus,
        /// The compiler's errors as cargo renders them, or cargo's stderr
        /// if it reported none, for the caller to show.
        output: Vec<u8>,
    },
    #[error("cargo built no {0} for `{1}`")]
    NoArtifact(&'static str, String),
}

/// How a cargo run reports as it goes.
pub struct Watch<'a> {
    /// Pass cargo's stderr through instead of keeping it.
    pub verbose: bool,
    /// Render compiler errors with colour.
    pub color: bool,
    /// Called with each crate cargo finishes.
    pub artifact: Box<dyn FnMut(&str) + 'a>,
}

impl Watch<'_> {
    /// Keeps cargo's stderr and reports no progress.
    pub fn quiet() -> Self {
        Self {
            verbose: false,
            color: false,
            artifact: Box::new(|_| {}),
        }
    }
}

/// Builds `package` from `manifest_dir` and returns the path of its cdylib.
pub fn cargo_build(
    package: &str,
    manifest_dir: &Path,
    watch: &mut Watch<'_>,
) -> Result<PathBuf, CargoError> {
    let args = ["build", "-p", package, message_format(watch)];
    let stdout = cargo(manifest_dir, &args, watch)?;
    cdylib(&stdout).ok_or_else(|| CargoError::NoArtifact("cdylib", package.to_string()))
}

/// Builds `package` from `manifest_dir` as a wasm module and returns its path.
///
/// The module imports its memory from the host. This uses `cargo rustc` so
/// the `--import-memory` link flag applies only to the pack crate, and its
/// dependencies stay cached.
pub fn wasm_build(
    package: &str,
    manifest_dir: &Path,
    watch: &mut Watch<'_>,
) -> Result<PathBuf, CargoError> {
    let args = [
        "rustc",
        "-p",
        package,
        "--target",
        WASM_TRIPLE,
        "--release",
        message_format(watch),
        "--",
        "-C",
        "link-arg=--import-memory",
    ];
    let stdout = cargo(manifest_dir, &args, watch)?;
    module(&stdout).ok_or_else(|| CargoError::NoArtifact("module", package.to_string()))
}

/// JSON messages on stdout, each diagnostic carrying its rendered text.
/// Verbose, cargo renders diagnostics, warnings too, to its own stderr.
fn message_format(watch: &Watch<'_>) -> &'static str {
    if watch.verbose {
        "--message-format=json-render-diagnostics"
    } else if watch.color {
        "--message-format=json-diagnostic-rendered-ansi"
    } else {
        "--message-format=json"
    }
}

/// Runs cargo with `args` in `dir` and returns its stdout.
///
/// Each finished crate is reported to `watch` as it arrives. Compiler
/// errors and cargo's stderr are kept, and returned with the error if the
/// build fails.
fn cargo(dir: &Path, args: &[&str], watch: &mut Watch<'_>) -> Result<Vec<u8>, CargoError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = Command::new(cargo)
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(if watch.verbose {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .spawn()?;
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = pipe.read_to_end(&mut kept);
            kept
        })
    });
    let mut stdout = Vec::new();
    let mut errors = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        for line in BufReader::new(pipe).split(b'\n').map_while(Result::ok) {
            if let Some(name) = artifact_name(&line) {
                (watch.artifact)(&name);
            }
            if let Some(rendered) = rendered_error(&line) {
                errors.extend_from_slice(rendered.as_bytes());
            }
            stdout.extend_from_slice(&line);
            stdout.push(b'\n');
        }
    }
    let status = child.wait()?;
    let stderr = stderr.and_then(|kept| kept.join().ok()).unwrap_or_default();
    if !status.success() {
        let output = if errors.is_empty() { stderr } else { errors };
        return Err(CargoError::Cargo { status, output });
    }
    Ok(stdout)
}

/// The rendered text of a compiler error, or `None` for any other message.
fn rendered_error(line: &[u8]) -> Option<String> {
    let message: serde_json::Value = serde_json::from_slice(line).ok()?;
    let diagnostic = &message["message"];
    if message["reason"] != "compiler-message" || diagnostic["level"] != "error" {
        return None;
    }
    diagnostic["rendered"].as_str().map(str::to_string)
}

/// The crate name of a `compiler-artifact` message, or `None` for any
/// other message or a build script.
fn artifact_name(line: &[u8]) -> Option<String> {
    let message: serde_json::Value = serde_json::from_slice(line).ok()?;
    let target = &message["target"];
    let script = target["kind"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "custom-build"));
    if message["reason"] != "compiler-artifact" || script {
        return None;
    }
    target["name"].as_str().map(str::to_string)
}

/// Finds the native library of the last cdylib in cargo's JSON output.
fn cdylib(stdout: &[u8]) -> Option<PathBuf> {
    cdylib_file(stdout, |name| {
        !name.ends_with(".rlib") && !name.ends_with(".rmeta")
    })
}

/// Finds the wasm module of the last cdylib in cargo's JSON output.
fn module(stdout: &[u8]) -> Option<PathBuf> {
    cdylib_file(stdout, |name| name.ends_with(".wasm"))
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
    /// Whether the pack is also built as a wasm module, from
    /// `[tool.metor.pack] wasm`.
    pub wasm: bool,
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
            wasm: pack.wasm,
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
    #[serde(default)]
    wasm: bool,
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

    #[test]
    fn test_artifact_name_skips_build_scripts_and_other_messages() {
        let lib = br#"{"reason":"compiler-artifact","target":{"name":"nalgebra","kind":["lib"]}}"#;
        let script = br#"{"reason":"compiler-artifact","target":{"name":"build-script-build","kind":["custom-build"]}}"#;
        let note = br#"{"reason":"compiler-message","target":{"name":"nalgebra","kind":["lib"]}}"#;
        assert_eq!(artifact_name(lib).as_deref(), Some("nalgebra"));
        assert_eq!(artifact_name(script), None);
        assert_eq!(artifact_name(note), None);
        assert_eq!(artifact_name(b"not json"), None);
    }

    #[test]
    fn test_rendered_error_keeps_only_errors() {
        let error =
            br#"{"reason":"compiler-message","message":{"level":"error","rendered":"error: nope\n"}}"#;
        let warning =
            br#"{"reason":"compiler-message","message":{"level":"warning","rendered":"warning: hm\n"}}"#;
        assert_eq!(rendered_error(error).as_deref(), Some("error: nope\n"));
        assert_eq!(rendered_error(warning), None);
        assert_eq!(rendered_error(b"{}"), None);
    }

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
    fn test_parse_module_artifact() {
        let line = r#"{"reason":"compiler-artifact",
            "target":{"name":"echo_pack","kind":["cdylib"]},
            "filenames":["/t/wasm32-unknown-unknown/release/echo_pack.wasm"]}"#
            .replace('\n', "");
        let module = PathBuf::from("/t/wasm32-unknown-unknown/release/echo_pack.wasm");
        assert_eq!(super::module(line.as_bytes()), Some(module));
        assert_eq!(super::module(ARTIFACT.replace('\n', "").as_bytes()), None);
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
                wasm: false,
            }
        );
    }

    #[test]
    fn test_pyproject_overrides_defaults() {
        let dir = root(
            "[project]\nname = \"adcs-pack\"\n\n[tool.metor.pack]\n\
             id = \"adcs\"\ncrate = \"adcs\"\nlib = \"adcs_dylib\"\nmodule = \"adcs_mod\"\n\
             wasm = true\n",
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
                wasm: true,
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
