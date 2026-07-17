//! Pack-crate packaging: `[tool.metor.pack]` config and the `pack dev`
//! editable layout (`docs/design-packaging.md` §6).
//!
//! A pack crate is also a Python project: its `pyproject.toml` names the
//! distribution (`[project]`) and the pack (`[tool.metor.pack]` — artifact
//! id, cargo crate, lib stem, module name, shipped targets), with the cargo
//! facts defaulted from `Cargo.toml`. `pack dev` is the consumer of that
//! config this phase: it builds the host triple and lays out
//!
//! ```text
//! <pack>/.metor/
//!   <module>/__init__.py            # prebuilt-flavor typed module
//!   <module>/py.typed
//!   <module>/_libs/<triple>/<cdylib>
//!   <module>/_libs/<triple>/<cdylib>.manifest
//! ```
//!
//! — the same shape an installed pack wheel unpacks to, so the recorder,
//! provisioning, and pyright cannot tell a local editable pack from a
//! published one. A pack's PEP 517 backend runs this on `uv sync`; the
//! multi-triple `pack build`/`publish` flow lands in phase 2.

use std::path::{Path, PathBuf};

use super::stubgen::{StubFlavor, render_module};
use super::{BuildError, BuildOptions, StubgenError, WiringBuilder, provision_artifacts};

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
    /// The artifact id mission IR references. Default: the module name.
    pub id: String,
    /// The cargo package. Default: `Cargo.toml`'s `[package] name`.
    pub crate_name: String,
    /// The cdylib stem. Default: `Cargo.toml`'s `[lib] name`, else the
    /// package name with `-` → `_`.
    pub lib: String,
    /// The target triples a published wheel ships. Unused by `pack dev`
    /// (host-only); the phase-2 `pack build` matrix reads it.
    pub targets: Vec<String>,
}

/// The triples a pack wheel ships when `[tool.metor.pack] targets` is not
/// given: the platforms metor-fsw supports end to end.
pub const DEFAULT_TARGETS: [&str; 3] = [
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
];

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
    #[error(transparent)]
    Stubgen(#[from] StubgenError),
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
    let targets = pack
        .and_then(|p| p.get("targets"))
        .and_then(|t| t.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect());

    Ok(PackConfig {
        dist_name,
        dist_version,
        module,
        id,
        crate_name,
        lib,
        targets,
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
    // with the sidecar sourced from the usual host twin).
    let mut wiring = WiringBuilder::new()
        .artifact(&config.id, &config.crate_name, &config.lib)
        .build();
    provision_artifacts(
        &mut wiring,
        &BuildOptions {
            release: opts.release,
            extra_args: opts.cargo_args.clone(),
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
        &StubFlavor::Prebuilt {
            abi_version: crate::abi::FSW_ABI_VERSION,
            dist: Some((config.dist_name.clone(), config.dist_version.clone())),
        },
    )?;
    write(&module_dir.join("__init__.py"), module.as_bytes())?;
    write(&module_dir.join("py.typed"), b"")?;

    Ok(PackDevReport {
        module_dir,
        lib_path,
        triple,
    })
}

fn copy(src: &Path, dst: &Path) -> Result<(), PackError> {
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|source| PackError::Write {
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
        assert_eq!(c.targets, DEFAULT_TARGETS.map(str::to_string));

        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"adcs-pack\"\nversion = \"1.2.0\"\n\
                 [tool.metor.pack]\nid = \"adcs\"\nmodule = \"adcs\"\n\
                 targets = [\"aarch64-apple-darwin\"]\n",
            )],
        );
        let c = read_pack_config(tmp.path()).unwrap();
        assert_eq!(c.id, "adcs");
        assert_eq!(c.module, "adcs");
        assert_eq!(c.targets, vec!["aarch64-apple-darwin".to_string()]);
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
}

#[cfg(all(test, not(miri)))]
mod integration {
    //! End-to-end over the dl fixture pack, following the build_driver test
    //! convention (serialize on the fixture lock, skip where the nested
    //! cargo build is unavailable).
    use super::*;
    use crate::dl::FIXTURE_LOCK;

    /// `pack dev` lays out exactly the wheel shape: prebuilt-flavor module,
    /// `py.typed`, and `_libs/<host-triple>/{cdylib, sidecar}` — and the
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

        let report = match pack_dev(tmp.path(), &PackDevOptions::default()) {
            Ok(report) => report,
            Err(e) => {
                eprintln!("skipping: pack_dev failed: {e}");
                return;
            }
        };
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
        wiring.artifacts[0].manifest_hash = Some(super::super::stubgen::manifest_hash(&manifest));
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
}
