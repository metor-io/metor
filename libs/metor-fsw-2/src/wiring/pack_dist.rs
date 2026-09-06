//! Pack-crate packaging: `[tool.metor.pack]` config and the `pack dev`
//! editable layout.
//!
//! A pack crate is also a Python project: its `pyproject.toml` names the
//! distribution (`[project]`) and the pack (`[tool.metor.pack]`: artifact
//! id, cargo crate, lib stem, module name), with the cargo facts defaulted
//! from `Cargo.toml`. `pack dev` builds the host triple and lays out
//!
//! ```text
//! <pack>/.metor/
//!   <module>/__init__.py            # typed module
//!   <module>/py.typed
//!   <module>/_libs/<triple>/<cdylib>
//!   <module>/_libs/<triple>/<cdylib>.manifest
//! ```
//!
//! the same shape an installed pack wheel unpacks to, so the recorder,
//! provisioning, and pyright cannot tell a local editable pack from a
//! published one. A pack's PEP 517 backend runs this on `uv sync`. The same
//! module also builds the pack's wheel.

use std::path::{Path, PathBuf};

use super::pack_module::render_module;
use super::{BuildError, BuildOptions, WiringBuilder, provision_artifacts};

/// A pack crate's packaging config: `pyproject.toml`'s `[project]` +
/// `[tool.metor.pack]`, with the cargo facts defaulted from `Cargo.toml`.
#[derive(Clone, Debug)]
pub struct PackConfig {
    /// The distribution name (`[project] name`).
    pub dist_name: String,
    /// The distribution version (`[project] version`).
    pub dist_version: String,
    /// The generated top-level module name. Default: the distribution name
    /// normalized to an identifier (`adcs-pack` → `adcs_pack`).
    pub module: String,
    /// The artifact id target IR references. Default: the module name.
    pub id: String,
    /// The cargo package. Default: `Cargo.toml`'s `[package] name`.
    pub crate_name: String,
    /// The cdylib stem. Default: `Cargo.toml`'s `[lib] name`, else the
    /// package name with `-` → `_`.
    pub lib: String,
    /// The pack's own `[project] dependencies`, carried into the wheel's
    /// `Requires-Dist` lines after the injected pins.
    pub dependencies: Vec<String>,
    /// The `[project] requires-python` specifier, if any.
    pub requires_python: Option<String>,
}

/// Why a pack-crate operation could not complete.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// A config file could not be read.
    #[error("failed to read `{path}`: {source}")]
    Read {
        /// The file path.
        path: PathBuf,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A config file is not valid TOML.
    #[error("failed to parse `{path}`: {source}")]
    Parse {
        /// The file path.
        path: PathBuf,
        #[source]
        /// The TOML error.
        source: toml::de::Error,
    },
    /// A required config field is missing.
    #[error("`{path}` is missing `{field}`")]
    MissingField {
        /// The file that lacks the field.
        path: PathBuf,
        /// The dotted field name.
        field: &'static str,
    },
    /// The pack dir still carries a legacy checked-in `packs/` package, which
    /// would shadow the venv modules via `sys.path[0]`.
    #[error(
        "`{dir}` still contains a legacy `packs/` package; delete it — generated modules are \
         venv-only now, and a source-tree copy shadows them"
    )]
    LegacyPacks {
        /// The offending directory.
        dir: PathBuf,
    },
    /// Building the pack failed.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// Rendering the typed module failed.
    #[error("failed to render typed pack module: {0}")]
    Module(String),
    /// The manifest sidecar the layout ships was not written by the build.
    #[error("no manifest sidecar next to `{so}`; the pack layout requires one")]
    MissingSidecar {
        /// The built library.
        so: PathBuf,
    },
    /// A layout file could not be written.
    #[error("failed to write `{path}`: {source}")]
    Write {
        /// The file path.
        path: PathBuf,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Read a pack crate's [`PackConfig`] from `<dir>/pyproject.toml`, defaulting
/// the cargo facts from `<dir>/Cargo.toml`.
pub fn read_pack_config(dir: &Path) -> Result<PackConfig, PackError> {
    let pyproject = dir.join("pyproject.toml");
    let doc = read_toml(&pyproject)?;

    let project = doc.get("project");
    let dist_name = get_str(project, "name").ok_or_else(|| PackError::MissingField {
        path: pyproject.clone(),
        field: "project.name",
    })?;
    let dist_version = get_str(project, "version").ok_or_else(|| PackError::MissingField {
        path: pyproject.clone(),
        field: "project.version",
    })?;

    let pack = doc
        .get("tool")
        .and_then(|t| t.get("metor"))
        .and_then(|m| m.get("pack"));

    // The cargo facts default from Cargo.toml; explicit keys win.
    let cargo = read_toml(&dir.join("Cargo.toml")).ok();
    let cargo_package = get_str(cargo.as_ref().and_then(|d| d.get("package")), "name");
    let cargo_lib = get_str(cargo.as_ref().and_then(|d| d.get("lib")), "name");

    let module = get_str(pack, "module").unwrap_or_else(|| module_name(&dist_name));
    let id = get_str(pack, "id").unwrap_or_else(|| module.clone());
    let crate_name =
        get_str(pack, "crate")
            .or(cargo_package)
            .ok_or_else(|| PackError::MissingField {
                path: pyproject.clone(),
                field: "tool.metor.pack.crate (and no Cargo.toml `package.name` to default from)",
            })?;
    let lib = get_str(pack, "lib")
        .or(cargo_lib)
        .unwrap_or_else(|| crate_name.replace('-', "_"));
    let dependencies = get_strings(project, "dependencies").unwrap_or_default();
    let requires_python = get_str(project, "requires-python");

    Ok(PackConfig {
        dist_name,
        dist_version,
        module,
        id,
        crate_name,
        lib,
        dependencies,
        requires_python,
    })
}

/// The generated module name for a distribution: PEP 503 normalization, then
/// `-` → `_` so it is a Python identifier (`Adcs.Pack` → `adcs_pack`).
fn module_name(dist: &str) -> String {
    dist.to_ascii_lowercase()
        .chars()
        .map(|c| if c == '-' || c == '.' { '_' } else { c })
        .collect()
}

fn read_toml(path: &Path) -> Result<toml::Value, PackError> {
    let text = std::fs::read_to_string(path).map_err(|source| PackError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse().map_err(|source| PackError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn get_str(table: Option<&toml::Value>, key: &str) -> Option<String> {
    table?.get(key)?.as_str().map(str::to_string)
}

fn get_strings(table: Option<&toml::Value>, key: &str) -> Option<Vec<String>> {
    table?.get(key)?.as_array().map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect()
    })
}

/// Knobs for [`pack_dev`].
#[derive(Clone, Debug, Default)]
pub struct PackDevOptions {
    /// Build the `--release` profile.
    pub release: bool,
    /// Extra args appended to the `cargo build`.
    pub cargo_args: Vec<String>,
}

/// What [`pack_dev`] produced.
#[derive(Debug)]
pub struct PackDevReport {
    /// The generated module directory (`.metor/<module>`).
    pub module_dir: PathBuf,
    /// The laid-out library (`.metor/<module>/_libs/<triple>/<cdylib>`).
    pub lib_path: PathBuf,
    /// The triple the layout was built for.
    pub triple: String,
}

/// Build a pack crate for the host and lay out its editable `.metor/` payload
/// (module + `_libs/<triple>/`), the shape a pack's PEP 517 backend exposes
/// to the venv via a `.pth`.
pub fn pack_dev(dir: &Path, opts: &PackDevOptions) -> Result<PackDevReport, PackError> {
    let config = read_pack_config(dir)?;
    if dir.join("packs").join("__init__.py").exists() {
        return Err(PackError::LegacyPacks {
            dir: dir.to_path_buf(),
        });
    }

    // Build (host by default; an explicit `--target` lays out that triple,
    // with the sidecar sourced from the usual host twin). The pack's own
    // manifest anchors the build when it has one, so the workspace resolves
    // from the pack rather than the process cwd: `run` invokes this from
    // the target dir, which need not be a cargo workspace at all.
    let mut cargo_args = opts.cargo_args.clone();
    let manifest = dir.join("Cargo.toml");
    if manifest.is_file() {
        cargo_args.extend(["--manifest-path".into(), manifest.display().to_string()]);
    }
    let mut wiring = WiringBuilder::new()
        .artifact(&config.id, &config.crate_name, &config.lib)
        .build();
    provision_artifacts(
        &mut wiring,
        &BuildOptions {
            release: opts.release,
            extra_args: cargo_args,
            manifest_sidecar: true,
        },
    )?;
    let so = wiring.artifacts[0]
        .path
        .clone()
        .expect("provision_artifacts fills every path or errors");
    let sidecar = crate::dl::manifest_sidecar_path(&so);
    let manifest =
        std::fs::read(&sidecar).map_err(|_| PackError::MissingSidecar { so: so.clone() })?;
    let triple = super::build_driver::build_target(&opts.cargo_args)
        .expect("host triple determinable on supported platforms");

    // Lay out `.metor/<module>/{__init__.py, py.typed, _libs/<triple>/…}`.
    let module_dir = dir.join(".metor").join(&config.module);
    let libs_dir = module_dir.join("_libs").join(&triple);
    std::fs::create_dir_all(&libs_dir).map_err(|source| PackError::Write {
        path: libs_dir.clone(),
        source,
    })?;
    let lib_path = libs_dir.join(so.file_name().expect("built library has a file name"));
    copy(&so, &lib_path)?;
    copy(&sidecar, &crate::dl::manifest_sidecar_path(&lib_path))?;

    let module = render_module(
        &config.id,
        &config.crate_name,
        &config.lib,
        &manifest,
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        &config.dist_name,
        &config.dist_version,
    )
    .map_err(PackError::Module)?;
    write(&module_dir.join("__init__.py"), module.as_bytes())?;
    write(&module_dir.join("py.typed"), b"")?;

    Ok(PackDevReport {
        module_dir,
        lib_path,
        triple,
    })
}

/// `true` if `dir` is a rebuildable dev pack: its own `Cargo.toml` plus an
/// explicit `[tool.metor.pack]` table in its `pyproject.toml`. Path sources
/// that are plain Python dists (e.g. the in-repo `metor-config`) fail both
/// gates. The table is required rather than "[`read_pack_config`] succeeds":
/// the config deliberately defaults everything from `Cargo.toml`, which
/// would claim any path source that merely contains a crate.
fn is_dev_pack(dir: &Path) -> bool {
    if !dir.join("Cargo.toml").is_file() {
        return false;
    }
    read_toml(&dir.join("pyproject.toml")).is_ok_and(|doc| {
        doc.get("tool")
            .and_then(|t| t.get("metor"))
            .and_then(|m| m.get("pack"))
            .is_some_and(|p| p.is_table())
    })
}

/// The target's dev-pack roots: its pyproject's path sources filtered to
/// the rebuildable packs.
fn dev_pack_roots(target_dir: &Path) -> Vec<PathBuf> {
    super::py::path_source_roots(target_dir)
        .into_iter()
        .filter(|dir| is_dev_pack(dir))
        .collect()
}

/// Re-run [`pack_dev`] for every dev pack `target_dir`'s pyproject
/// references, so their `.metor/` payloads (module + lib) are current before
/// the target is evaluated. Cheap when nothing changed, since cargo's build
/// is incremental and the layout rewrite is byte-identical. The first
/// failing pack aborts.
pub fn refresh_dev_packs(
    target_dir: &Path,
    opts: &PackDevOptions,
) -> Result<Vec<PackDevReport>, PackError> {
    dev_pack_roots(target_dir)
        .iter()
        .map(|dir| pack_dev(dir, opts))
        .collect()
}

/// Knobs for [`pack_build`].
#[derive(Clone, Debug, Default)]
pub struct PackBuildOptions {
    /// Write the wheel here (default: `<dir>/dist`).
    pub wheel_out: Option<PathBuf>,
}

/// What [`pack_build`] produced.
#[derive(Debug)]
pub struct PackBuildReport {
    /// The host triple the payload was built for.
    pub triple: String,
    /// The wheel.
    pub wheel: PathBuf,
}

/// Build a pack's wheel for the host triple: a release, stripped cdylib with
/// its manifest sidecar, the rendered prebuilt module, and the injected pins.
pub fn pack_build(dir: &Path, opts: &PackBuildOptions) -> Result<PackBuildReport, PackError> {
    let config = read_pack_config(dir)?;
    let mut cargo_args = vec![
        // Wheels ship stripped release objects; a debug fat wheel would
        // balloon silently.
        "--config".to_string(),
        "profile.release.strip=true".to_string(),
    ];
    let manifest_path = dir.join("Cargo.toml");
    if manifest_path.is_file() {
        cargo_args.extend([
            "--manifest-path".into(),
            manifest_path.display().to_string(),
        ]);
    }
    let mut wiring = WiringBuilder::new()
        .artifact(&config.id, &config.crate_name, &config.lib)
        .build();
    provision_artifacts(
        &mut wiring,
        &BuildOptions {
            release: true,
            extra_args: cargo_args,
            manifest_sidecar: true,
        },
    )?;
    let so = wiring.artifacts[0]
        .path
        .clone()
        .expect("provision_artifacts fills every path or errors");
    let manifest = std::fs::read(crate::dl::manifest_sidecar_path(&so))
        .map_err(|_| PackError::MissingSidecar { so: so.clone() })?;
    let triple = super::build_driver::build_target(&[])
        .expect("host triple determinable on supported platforms");
    let wheel_out = opts.wheel_out.clone().unwrap_or_else(|| dir.join("dist"));
    let wheel = write_pack_wheel(&config, &triple, &so, manifest, &wheel_out)?;
    Ok(PackBuildReport { triple, wheel })
}

fn write_pack_wheel(
    config: &PackConfig,
    triple: &str,
    so: &Path,
    manifest: Vec<u8>,
    wheel_out: &Path,
) -> Result<PathBuf, PackError> {
    let _span = tracing::info_span!(target: "build", "wheel", label = %config.dist_name).entered();
    let cdylib = super::cdylib_file_name_for(triple, &config.lib);
    let so_bytes = std::fs::read(so).map_err(|source| PackError::Read {
        path: so.to_path_buf(),
        source,
    })?;
    let module = render_module(
        &config.id,
        &config.crate_name,
        &config.lib,
        &manifest,
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        &config.dist_name,
        &config.dist_version,
    )
    .map_err(PackError::Module)?;
    let files = vec![
        super::wheel::WheelFile {
            arcname: format!("{}/_libs/{triple}/{cdylib}", config.module),
            bytes: so_bytes,
            mode: 0o755,
        },
        super::wheel::WheelFile {
            arcname: format!("{}/_libs/{triple}/{cdylib}.manifest", config.module),
            bytes: manifest,
            mode: 0o644,
        },
        super::wheel::WheelFile {
            arcname: format!("{}/__init__.py", config.module),
            bytes: module.into_bytes(),
            mode: 0o644,
        },
        super::wheel::WheelFile {
            arcname: format!("{}/py.typed", config.module),
            bytes: Vec::new(),
            mode: 0o644,
        },
    ];

    std::fs::create_dir_all(wheel_out).map_err(|source| PackError::Write {
        path: wheel_out.to_path_buf(),
        source,
    })?;
    super::wheel::write_wheel(
        wheel_out,
        &super::wheel::WheelMeta {
            dist_name: config.dist_name.clone(),
            version: config.dist_version.clone(),
            tag: "py3-none-any".into(),
            requires_python: config.requires_python.clone(),
            requires: injected_requires(config),
        },
        files,
    )
    .map_err(|source| PackError::Write {
        path: wheel_out.to_path_buf(),
        source,
    })
}

/// The wheel's `Requires-Dist` lines: the ABI marker pin and the
/// `metor-config` compatible range (the generated module imports it), then
/// the pack's own dependencies. A pin the pack already declares itself is not
/// injected twice.
fn injected_requires(config: &PackConfig) -> Vec<String> {
    let mut requires = Vec::new();
    let declares = |name: &str| {
        config
            .dependencies
            .iter()
            .any(|dep| dep.trim_start().starts_with(name))
    };
    if !declares("metor-fsw-abi") {
        requires.push(format!(
            "metor-fsw-abi=={}",
            metor_fsw_2_core::abi::FSW_ABI_VERSION
        ));
    }
    if !declares("metor-config") {
        let version = super::py::metor_config_version();
        let mut parts = version.split('.');
        let (major, minor) = (
            parts.next().unwrap_or("0"),
            parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0),
        );
        requires.push(format!(
            "metor-config>={major}.{minor},<{major}.{}",
            minor + 1
        ));
    }
    requires.extend(config.dependencies.iter().cloned());
    requires
}

fn copy(src: &Path, dst: &Path) -> Result<(), PackError> {
    super::build_driver::copy_atomic(src, dst).map_err(|source| PackError::Write {
        path: dst.to_path_buf(),
        source,
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), PackError> {
    std::fs::write(path, bytes).map_err(|source| PackError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_files(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
    }

    /// Explicit `[tool.metor.pack]` keys win; everything else defaults from
    /// the dist name and `Cargo.toml`.
    #[test]
    fn config_defaults_from_cargo_and_dist_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(
            tmp.path(),
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"Adcs-Pack\"\nversion = \"1.2.0\"\n",
                ),
                (
                    "Cargo.toml",
                    "[package]\nname = \"adcs-systems\"\n[lib]\nname = \"adcs_sys\"\n",
                ),
            ],
        );
        let c = read_pack_config(tmp.path()).unwrap();
        assert_eq!(c.dist_name, "Adcs-Pack");
        assert_eq!(c.module, "adcs_pack");
        assert_eq!(c.id, "adcs_pack");
        assert_eq!(c.crate_name, "adcs-systems");
        assert_eq!(c.lib, "adcs_sys");

        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"adcs-pack\"\nversion = \"1.2.0\"\n\
                 [tool.metor.pack]\nid = \"adcs\"\nmodule = \"adcs\"\n",
            )],
        );
        let c = read_pack_config(tmp.path()).unwrap();
        assert_eq!(c.id, "adcs");
        assert_eq!(c.module, "adcs");
    }

    /// No `[project]` name/version is a clean missing-field error.
    #[test]
    fn config_requires_project_identity() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(tmp.path(), &[("pyproject.toml", "[tool.metor.pack]\n")]);
        let err = read_pack_config(tmp.path()).unwrap_err();
        assert!(
            matches!(err, PackError::MissingField { field, .. } if field == "project.name"),
            "{err}"
        );
    }

    /// A leftover checked-in `packs/` package is refused before any build.
    #[test]
    fn legacy_packs_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"p\"\nversion = \"0.0.0\"\n[tool.metor.pack]\ncrate = \"p\"\n",
            )],
        );
        std::fs::create_dir_all(tmp.path().join("packs")).unwrap();
        std::fs::write(tmp.path().join("packs/__init__.py"), "").unwrap();
        let err = pack_dev(tmp.path(), &PackDevOptions::default()).unwrap_err();
        assert!(matches!(err, PackError::LegacyPacks { .. }), "{err}");
    }

    /// A dev pack needs both its own `Cargo.toml` and an explicit
    /// `[tool.metor.pack]` table; a plain Python dist (the `metor-config`
    /// shape) and a bare crate both fail the filter.
    #[test]
    fn dev_pack_needs_crate_and_pack_table() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject_plain = "[project]\nname = \"p\"\nversion = \"0.0.0\"\n";
        let pyproject_pack =
            "[project]\nname = \"p\"\nversion = \"0.0.0\"\n[tool.metor.pack]\nid = \"p\"\n";

        write_files(tmp.path(), &[("pyproject.toml", pyproject_pack)]);
        assert!(!is_dev_pack(tmp.path()), "no Cargo.toml");

        write_files(tmp.path(), &[("Cargo.toml", "[package]\nname = \"p\"\n")]);
        assert!(is_dev_pack(tmp.path()));

        write_files(tmp.path(), &[("pyproject.toml", pyproject_plain)]);
        assert!(!is_dev_pack(tmp.path()), "crate without a pack table");
    }

    /// `dev_pack_roots` keeps exactly the path sources that are dev packs,
    /// and a target with none refreshes to nothing (no cargo spawned).
    #[test]
    fn dev_pack_roots_filter_path_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = |name: &str, files: &[(&str, &str)]| {
            let d = tmp.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            write_files(&d, files);
        };
        dir(
            "pack",
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"pack\"\nversion = \"0.0.0\"\n[tool.metor.pack]\nid = \"p\"\n",
                ),
                ("Cargo.toml", "[package]\nname = \"pack\"\n"),
            ],
        );
        dir(
            "plain",
            &[(
                "pyproject.toml",
                "[project]\nname = \"plain\"\nversion = \"0.0.0\"\n",
            )],
        );
        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"m\"\nversion = \"0.0.0\"\n\
                 [tool.uv.sources]\npack = { path = \"pack\" }\n\
                 plain = { path = \"plain\" }\npinned = { version = \"1.0\" }\n",
            )],
        );
        assert_eq!(dev_pack_roots(tmp.path()), vec![tmp.path().join("pack")]);

        // Drop the dev pack's source line: nothing left to refresh.
        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"m\"\nversion = \"0.0.0\"\n\
                 [tool.uv.sources]\nplain = { path = \"plain\" }\n",
            )],
        );
        let reports = refresh_dev_packs(tmp.path(), &PackDevOptions::default()).unwrap();
        assert!(reports.is_empty(), "{reports:?}");
    }

    /// The injected pins: the ABI marker and the metor-config compatible
    /// range, skipped when the pack declares its own, followed by the pack's
    /// dependencies.
    #[test]
    fn requires_injection_and_dedupe() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"p\"\nversion = \"0.0.0\"\n\
                 dependencies = [\"numpy>=2\"]\n[tool.metor.pack]\ncrate = \"p\"\n",
            )],
        );
        let config = read_pack_config(tmp.path()).unwrap();
        let requires = injected_requires(&config);
        assert_eq!(
            requires[0],
            format!("metor-fsw-abi=={}", metor_fsw_2_core::abi::FSW_ABI_VERSION)
        );
        assert!(
            requires[1].starts_with("metor-config>=") && requires[1].contains(",<"),
            "{requires:?}"
        );
        assert_eq!(requires[2], "numpy>=2");

        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"p\"\nversion = \"0.0.0\"\n\
                 dependencies = [\"metor-config==0.3.0\", \"metor-fsw-abi==7\"]\n\
                 [tool.metor.pack]\ncrate = \"p\"\n",
            )],
        );
        let config = read_pack_config(tmp.path()).unwrap();
        let requires = injected_requires(&config);
        assert_eq!(
            requires,
            vec!["metor-config==0.3.0".to_string(), "metor-fsw-abi==7".into()],
            "explicit pins are not doubled"
        );
    }
}

#[cfg(all(test, not(miri)))]
mod integration {
    //! End-to-end over the dl fixture pack, following the build_driver test
    //! convention (serialize on the fixture lock and require the nested
    //! cargo build to succeed).
    use super::*;
    use crate::dl::FIXTURE_LOCK;

    /// `pack dev` lays out exactly the wheel shape, prebuilt-flavor module,
    /// `py.typed`, and `_libs/<host-triple>/{cdylib, sidecar}`, and the
    /// layout provisions and resolves as a prebuilt artifact.
    #[test]
    fn pack_dev_layout_provisions() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"fixture-pack\"\nversion = \"0.1.0\"\n\
             [tool.metor.pack]\nid = \"fixture\"\ncrate = \"metor-fsw-2-dl-fixture\"\n\
             lib = \"metor_fsw_2_dl_fixture\"\n",
        )
        .unwrap();

        let report =
            pack_dev(tmp.path(), &PackDevOptions::default()).expect("required pack fixture builds");
        assert!(report.module_dir.join("__init__.py").exists());
        assert!(report.module_dir.join("py.typed").exists());
        assert!(report.lib_path.exists());
        assert!(crate::dl::manifest_sidecar_path(&report.lib_path).exists());

        let module = std::fs::read_to_string(report.module_dir.join("__init__.py")).unwrap();
        assert!(module.contains("dist=\"fixture-pack\","));
        assert!(module.contains("prebuilt=str(Path(__file__).resolve().parent / \"_libs\"),"));

        // The layout is a provisionable prebuilt artifact: same id/lib, the
        // `_libs` dir as `prebuilt_dir`, and the recorded manifest hash.
        let manifest = crate::dl::manifest_sidecar_bytes(&report.lib_path).unwrap();
        let mut wiring = WiringBuilder::new()
            .artifact(
                "fixture",
                "metor-fsw-2-dl-fixture",
                "metor_fsw_2_dl_fixture",
            )
            .build();
        wiring.artifacts[0].prebuilt_dir = Some(report.module_dir.join("_libs"));
        wiring.artifacts[0].manifest_hash =
            Some(super::super::pack_module::manifest_hash(&manifest));
        provision_artifacts(&mut wiring, &BuildOptions::default())
            .expect("prebuilt layout provisions without cargo");
        assert_eq!(wiring.artifacts[0].path.as_deref(), Some(&*report.lib_path));

        // And it passes resolve's staleness gate (resolve may fail later for
        // unrelated reasons; StaleStubs specifically must not fire).
        if let Err(e) = super::super::resolve(&wiring, &super::super::Registry::with_builtins())
            && matches!(e.kind, super::super::LoadErrorKind::StaleStubs { .. })
        {
            panic!("fresh pack dev layout wrongly read as stale");
        }
    }

    /// `pack build` over the fixture: the wheel is byte-reproducible and
    /// carries the module, the `_libs/<host-triple>/` payload, and the
    /// injected pins.
    #[test]
    fn pack_build_wheel_is_reproducible() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let host = super::super::build_driver::build_target(&[])
            .expect("determine host triple for the required wheel fixture");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"fixture-pack\"\nversion = \"0.1.0\"\n\
             requires-python = \">=3.11\"\n\
             [tool.metor.pack]\nid = \"fixture\"\ncrate = \"metor-fsw-2-dl-fixture\"\n\
             lib = \"metor_fsw_2_dl_fixture\"\n",
        )
        .unwrap();

        let build = |out: &str| {
            pack_build(
                tmp.path(),
                &PackBuildOptions {
                    wheel_out: Some(tmp.path().join(out)),
                },
            )
        };
        let a = build("a").expect("required wheel fixture builds").wheel;
        assert_eq!(
            a.file_name().and_then(|n| n.to_str()),
            Some("fixture_pack-0.1.0-py3-none-any.whl")
        );
        let b = build("b").expect("rebuild").wheel;
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "identical inputs produce byte-identical wheels"
        );

        // The payload: module, marker, per-triple lib + sidecar, pins.
        let listing = std::process::Command::new("python3")
            .args(["-m", "zipfile", "-l"])
            .arg(&a)
            .output()
            .expect("python3 runs");
        let listing = String::from_utf8_lossy(&listing.stdout).to_string();
        for expected in [
            "fixture_pack/__init__.py".to_string(),
            "fixture_pack/py.typed".to_string(),
            format!("fixture_pack/_libs/{host}/"),
            "fixture_pack-0.1.0.dist-info/METADATA".to_string(),
        ] {
            assert!(
                listing.contains(&expected),
                "missing {expected}:\n{listing}"
            );
        }
    }
}
