//! Pack-crate packaging: `[tool.metor.pack]` config and the `pack dev`
//! editable layout (`docs/packaging.md`).
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
//! published one. A pack's PEP 517 backend runs this on `uv sync`. The same
//! module also builds and publishes multi-target wheels.

use std::io::BufRead;
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
    /// The artifact id target IR references. Default: the module name.
    pub id: String,
    /// The cargo package. Default: `Cargo.toml`'s `[package] name`.
    pub crate_name: String,
    /// The cdylib stem. Default: `Cargo.toml`'s `[lib] name`, else the
    /// package name with `-` → `_`.
    pub lib: String,
    /// The target triples a published wheel ships. Unused by `pack dev`
    /// (host-only); [`pack_build`] builds one payload per entry.
    pub targets: Vec<String>,
    /// The pack's own `[project] dependencies`, carried into the wheel's
    /// `Requires-Dist` lines after the injected pins.
    pub dependencies: Vec<String>,
    /// The `[project] requires-python` specifier, if any.
    pub requires_python: Option<String>,
    /// How [`pack_build`] compiles a target (`[tool.metor.pack.builder]`).
    pub builder: Builder,
}

/// How a pack target is compiled (`[tool.metor.pack.builder]`,
/// `docs/packaging.md`). Wheel builds always force `--release`
/// and, for the cargo-family builders, `--config profile.release.strip=true`.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Builder {
    /// Native `cargo build --target <triple>`; assumes installed toolchains.
    #[default]
    Cargo,
    /// `cargo zigbuild --target <triple>`, covering the linux-gnu triples
    /// from any host with a zig on `PATH`.
    Zigbuild,
    /// An argv template run in the pack dir, with `{triple}` and `{crate}`
    /// substituted — the Nix-cross-devshell (and any bespoke toolchain)
    /// hook. The command must leave the cdylib at cargo's conventional
    /// `<target-dir>/<triple>/release/` location.
    Command(Vec<String>),
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
    /// `[tool.metor.pack.builder] kind` names no known builder.
    #[error("`{path}` names unknown builder kind `{kind}` (cargo, zigbuild, command)")]
    UnknownBuilder {
        /// The pyproject path.
        path: PathBuf,
        /// The unrecognized kind.
        kind: String,
    },
    /// A command-template build ran but its output could not be located.
    #[error(
        "builder command succeeded but no `{cdylib}` was found under a \
         `<target-dir>/{triple}/release/`; the command must leave the cdylib at \
         cargo's conventional location"
    )]
    CommandOutputMissing {
        /// The expected cdylib file name.
        cdylib: String,
        /// The triple that was built.
        triple: String,
    },
    /// A builder command could not be run or exited non-zero.
    #[error("builder command for `{triple}` failed: {detail}")]
    BuilderCommand {
        /// The triple being built.
        triple: String,
        /// What went wrong.
        detail: String,
    },
    /// Staged sidecars disagree across triples. Pack manifests must be
    /// architecture-independent; skew means the stagings came from different
    /// pack revisions (or a manifest genuinely diverged by arch).
    #[error(
        "manifest sidecar for `{triple}` differs from `{reference}`'s: staged payloads \
         must come from one pack revision, and manifests must not depend on the target"
    )]
    ManifestSkew {
        /// The divergent triple.
        triple: String,
        /// The triple whose sidecar was taken as reference.
        reference: String,
    },
    /// No staged triple payloads were found to assemble.
    #[error("no `<triple>/` payloads staged under {}", .dirs.iter().map(|d| format!("`{}`", d.display())).collect::<Vec<_>>().join(", "))]
    NothingStaged {
        /// The staging directories that were searched.
        dirs: Vec<PathBuf>,
    },
    /// `uv publish` could not be run or exited non-zero.
    #[error("`uv publish` failed: {detail}")]
    Publish {
        /// What went wrong.
        detail: String,
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
    let dependencies = project
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let requires_python = get_str(project, "requires-python");
    let builder = read_builder(&pyproject, pack)?;

    Ok(PackConfig {
        dist_name,
        dist_version,
        module,
        id,
        crate_name,
        lib,
        targets,
        dependencies,
        requires_python,
        builder,
    })
}

/// Parse `[tool.metor.pack.builder]`: `kind = "cargo" | "zigbuild" |
/// "command"`, the latter with a `command = [...]` argv template.
fn read_builder(pyproject: &Path, pack: Option<&toml::Value>) -> Result<Builder, PackError> {
    let Some(builder) = pack.and_then(|p| p.get("builder")) else {
        return Ok(Builder::Cargo);
    };
    match builder.get("kind").and_then(|k| k.as_str()) {
        Some("cargo") | None => Ok(Builder::Cargo),
        Some("zigbuild") => Ok(Builder::Zigbuild),
        Some("command") => {
            let argv: Vec<String> = builder
                .get("command")
                .and_then(|c| c.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if argv.is_empty() {
                return Err(PackError::MissingField {
                    path: pyproject.to_path_buf(),
                    field: "tool.metor.pack.builder.command",
                });
            }
            Ok(Builder::Command(argv))
        }
        Some(other) => Err(PackError::UnknownBuilder {
            path: pyproject.to_path_buf(),
            kind: other.to_string(),
        }),
    }
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
    // with the sidecar sourced from the usual host twin). The pack's own
    // manifest anchors the build when it has one, so the workspace resolves
    // from the pack rather than the process cwd — `run` invokes this from
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
pub fn dev_pack_roots(target_dir: &Path) -> Vec<PathBuf> {
    super::py::path_source_roots(target_dir)
        .into_iter()
        .filter(|dir| is_dev_pack(dir))
        .collect()
}

/// Re-run [`pack_dev`] for every dev pack `target_dir`'s pyproject
/// references, so their `.metor/` payloads (module + lib) are current before
/// the target is evaluated. Cheap when nothing changed — cargo's build is
/// incremental and the layout rewrite is byte-identical. The first failing
/// pack aborts.
pub fn refresh_dev_packs(
    target_dir: &Path,
    opts: &PackDevOptions,
) -> Result<Vec<PackDevReport>, PackError> {
    dev_pack_roots(target_dir)
        .iter()
        .map(|dir| pack_dev(dir, opts))
        .collect()
}

// ---------------------------------------------------------------------------
// pack build / assemble / publish (docs/packaging.md)
// ---------------------------------------------------------------------------

/// Knobs for [`pack_build`].
#[derive(Clone, Debug, Default)]
pub struct PackBuildOptions {
    /// Build these triples instead of the config's `targets`.
    pub targets: Vec<String>,
    /// Stage `<triple>/{cdylib, sidecar}` payloads here and stop — the
    /// CI-matrix half; [`pack_assemble`] joins the stagings.
    pub libs_out: Option<PathBuf>,
    /// Assemble the wheel here (the single-machine flow). Ignored when
    /// `libs_out` is set.
    pub wheel_out: Option<PathBuf>,
    /// Override the config's builder.
    pub builder: Option<Builder>,
}

/// What [`pack_build`] / [`pack_assemble`] produced.
#[derive(Debug)]
pub struct PackBuildReport {
    /// The triples staged or assembled.
    pub triples: Vec<String>,
    /// The staging directory (`libs_out`, or the wheel flow's temp staging).
    pub staged: PathBuf,
    /// The wheel, when one was assembled.
    pub wheel: Option<PathBuf>,
}

/// Build a pack's wheel payloads for every configured target and, unless
/// staging for a CI matrix (`libs_out`), assemble the fat wheel.
///
/// Every triple's cdylib is release-built and stripped; the manifest sidecar
/// comes from one host describe (`pack dev`'s machinery), staged next to
/// each payload so [`pack_assemble`]'s N-way identity gate — and, after
/// install, resolve's staleness gate — read it in place.
pub fn pack_build(dir: &Path, opts: &PackBuildOptions) -> Result<PackBuildReport, PackError> {
    let mut config = read_pack_config(dir)?;
    if let Some(builder) = &opts.builder {
        config.builder = builder.clone();
    }
    let targets = if opts.targets.is_empty() {
        config.targets.clone()
    } else {
        opts.targets.clone()
    };

    // One host build supplies the manifest for every triple (verified
    // arch-independent by the host-twin machinery on native runners, and by
    // the assemble gate across runners).
    let mut wiring = WiringBuilder::new()
        .artifact(&config.id, &config.crate_name, &config.lib)
        .build();
    provision_artifacts(
        &mut wiring,
        &BuildOptions {
            release: true,
            extra_args: Vec::new(),
            manifest_sidecar: true,
        },
    )?;
    let host_so = wiring.artifacts[0]
        .path
        .clone()
        .expect("provision_artifacts fills every path or errors");
    let manifest = std::fs::read(crate::dl::manifest_sidecar_path(&host_so))
        .map_err(|_| PackError::MissingSidecar { so: host_so })?;

    let held;
    let staging = match &opts.libs_out {
        Some(dir) => dir.clone(),
        None => {
            held = tempfile::tempdir().map_err(|source| PackError::Write {
                path: PathBuf::from("<staging tempdir>"),
                source,
            })?;
            held.path().to_path_buf()
        }
    };

    for triple in &targets {
        let so = build_target_cdylib(dir, &config, triple)?;
        let triple_dir = staging.join(triple);
        std::fs::create_dir_all(&triple_dir).map_err(|source| PackError::Write {
            path: triple_dir.clone(),
            source,
        })?;
        let staged = triple_dir.join(super::cdylib_file_name_for(triple, &config.lib));
        copy(&so, &staged)?;
        write(&crate::dl::manifest_sidecar_path(&staged), &manifest)?;
    }

    let wheel = match (&opts.libs_out, &opts.wheel_out) {
        (Some(_), _) => None,
        (None, Some(out)) => Some(assemble_wheel(&config, &[staging.clone()], out)?),
        (None, None) => Some(assemble_wheel(
            &config,
            &[staging.clone()],
            &dir.join("dist"),
        )?),
    };
    Ok(PackBuildReport {
        triples: targets,
        staged: staging,
        wheel,
    })
}

/// Compile one target triple via the configured [`Builder`] and return the
/// built cdylib's path.
fn build_target_cdylib(
    dir: &Path,
    config: &PackConfig,
    triple: &str,
) -> Result<PathBuf, PackError> {
    let cdylib = super::cdylib_file_name_for(triple, &config.lib);
    let cargo_family = |subcommand: &str| {
        let opts = BuildOptions {
            release: true,
            extra_args: vec![
                "--target".into(),
                triple.to_string(),
                // Wheels ship stripped release objects; a debug fat wheel
                // would balloon silently (design §7.1).
                "--config".into(),
                "profile.release.strip=true".into(),
            ],
            manifest_sidecar: false,
        };
        let label = format!("{} ({triple})", config.crate_name);
        super::build_driver::build_cdylib(subcommand, &config.crate_name, &cdylib, &opts, &label)
    };
    match &config.builder {
        Builder::Cargo => cargo_family("build").map_err(PackError::Build),
        Builder::Zigbuild => cargo_family("zigbuild").map_err(PackError::Build),
        Builder::Command(template) => {
            let argv: Vec<String> = template
                .iter()
                .map(|arg| {
                    arg.replace("{triple}", triple)
                        .replace("{crate}", &config.crate_name)
                })
                .collect();
            let label = format!("{} ({triple})", config.crate_name);
            run_streamed(&argv, dir, triple, &label)?;
            locate_conventional(dir, triple, &cdylib).ok_or_else(|| {
                PackError::CommandOutputMissing {
                    cdylib,
                    triple: triple.to_string(),
                }
            })
        }
    }
}

/// Run a builder command template with its output streamed through tracing —
/// the same `build` span / `cargo` event shape as the cargo drivers, so a
/// template builder shares the CLI's pinned progress line.
fn run_streamed(argv: &[String], dir: &Path, triple: &str, label: &str) -> Result<(), PackError> {
    use tracing_indicatif::span_ext::IndicatifSpanExt;

    let command_err = |detail: String| PackError::BuilderCommand {
        triple: triple.to_string(),
        detail,
    };
    let span = tracing::info_span!(target: "build", "build", %label);
    span.pb_set_message(label);
    let started = std::time::Instant::now();
    let (tail, status) = {
        let _entered = span.enter();
        let mut child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| command_err(e.to_string()))?;
        let child_stdout = child.stdout.take().expect("stdout piped");
        let child_stderr = child.stderr.take().expect("stderr piped");

        // Both pipes stream as they arrive; stderr additionally keeps a short
        // tail for the failure diagnostic (a subscriber-less library caller
        // would otherwise see only the exit code).
        let tail = std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut tail: Vec<String> = Vec::new();
                let lines = std::io::BufReader::new(child_stderr).lines();
                for line in lines.map_while(Result::ok) {
                    tracing::info!(target: "cargo", "{line}");
                    if tail.len() == 8 {
                        tail.remove(0);
                    }
                    tail.push(line);
                }
                tail
            });
            let lines = std::io::BufReader::new(child_stdout).lines();
            for line in lines.map_while(Result::ok) {
                tracing::info!(target: "cargo", "{line}");
            }
            reader.join().expect("stderr reader does not panic")
        });
        let status = child.wait().map_err(|e| command_err(e.to_string()))?;
        (tail, status)
    };
    drop(span);
    if !status.success() {
        return Err(command_err(format!("exit {status}:\n{}", tail.join("\n"))));
    }
    tracing::info!(
        target: "build",
        "✓ {label} ({:.1}s)",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

/// Find a command-builder's output at cargo's conventional
/// `<target-dir>/<triple>/release/<cdylib>`, honoring `CARGO_TARGET_DIR` and
/// walking up from the pack dir for a workspace `target/`.
fn locate_conventional(dir: &Path, triple: &str, cdylib: &str) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(root));
    }
    let mut walk = dir.canonicalize().ok();
    while let Some(d) = walk {
        roots.push(d.join("target"));
        walk = d.parent().map(Path::to_path_buf);
    }
    roots
        .into_iter()
        .map(|root| root.join(triple).join("release").join(cdylib))
        .find(|p| p.exists())
}

/// Join staged `<triple>/` payloads into the fat wheel: verify every
/// sidecar is byte-identical (the N-way `ManifestDivergence` gate — a skewed
/// CI matrix is caught here), render the prebuilt module from the one
/// manifest, and write the wheel with the injected pins.
pub fn pack_assemble(
    dir: &Path,
    libs: &[PathBuf],
    wheel_out: &Path,
) -> Result<PackBuildReport, PackError> {
    let config = read_pack_config(dir)?;
    let wheel = assemble_wheel(&config, libs, wheel_out)?;
    Ok(PackBuildReport {
        triples: staged_triples(libs),
        staged: libs.first().cloned().unwrap_or_default(),
        wheel: Some(wheel),
    })
}

/// The `<triple>/` subdirectories staged across `libs` dirs, sorted.
fn staged_triples(libs: &[PathBuf]) -> Vec<String> {
    let mut triples: Vec<String> = libs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    triples.sort();
    triples.dedup();
    triples
}

fn assemble_wheel(
    config: &PackConfig,
    libs: &[PathBuf],
    wheel_out: &Path,
) -> Result<PathBuf, PackError> {
    let _span = tracing::info_span!(target: "build", "wheel", label = %config.dist_name).entered();
    let triples = staged_triples(libs);
    if triples.is_empty() {
        return Err(PackError::NothingStaged {
            dirs: libs.to_vec(),
        });
    }

    // Collect per-triple payloads and run the N-way identity gate.
    let mut manifest: Option<(String, Vec<u8>)> = None;
    let mut files: Vec<super::wheel::WheelFile> = Vec::new();
    for triple in &triples {
        let cdylib = super::cdylib_file_name_for(triple, &config.lib);
        let triple_dir = libs
            .iter()
            .map(|d| d.join(triple))
            .find(|d| d.join(&cdylib).exists())
            .ok_or_else(|| PackError::CommandOutputMissing {
                cdylib: cdylib.clone(),
                triple: triple.clone(),
            })?;
        let so = triple_dir.join(&cdylib);
        let sidecar_path = crate::dl::manifest_sidecar_path(&so);
        let sidecar = std::fs::read(&sidecar_path)
            .map_err(|_| PackError::MissingSidecar { so: so.clone() })?;
        match &manifest {
            None => manifest = Some((triple.clone(), sidecar.clone())),
            Some((reference, bytes)) if *bytes != sidecar => {
                return Err(PackError::ManifestSkew {
                    triple: triple.clone(),
                    reference: reference.clone(),
                });
            }
            Some(_) => {}
        }
        let so_bytes = std::fs::read(&so).map_err(|source| PackError::Read {
            path: so.clone(),
            source,
        })?;
        files.push(super::wheel::WheelFile {
            arcname: format!("{}/_libs/{triple}/{cdylib}", config.module),
            bytes: so_bytes,
            mode: 0o755,
        });
        files.push(super::wheel::WheelFile {
            arcname: format!("{}/_libs/{triple}/{cdylib}.manifest", config.module),
            bytes: sidecar,
            mode: 0o644,
        });
    }
    let (_, manifest) = manifest.expect("at least one staged triple");

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
    files.push(super::wheel::WheelFile {
        arcname: format!("{}/__init__.py", config.module),
        bytes: module.into_bytes(),
        mode: 0o644,
    });
    files.push(super::wheel::WheelFile {
        arcname: format!("{}/py.typed", config.module),
        bytes: Vec::new(),
        mode: 0o644,
    });

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
/// `metor-config` compatible range (phase 0 found the latter load-bearing —
/// the generated module imports it), then the pack's own dependencies.
/// A pin the pack already declares itself is not injected twice.
fn injected_requires(config: &PackConfig) -> Vec<String> {
    let mut requires = Vec::new();
    let declares = |name: &str| {
        config
            .dependencies
            .iter()
            .any(|dep| dep.trim_start().starts_with(name))
    };
    if !declares("metor-fsw-abi") {
        requires.push(format!("metor-fsw-abi=={}", crate::abi::FSW_ABI_VERSION));
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

/// Build (or take) the pack's wheel and hand it to `uv publish`.
pub fn pack_publish(
    dir: &Path,
    index: Option<&str>,
    wheel: Option<&Path>,
    dry_run: bool,
) -> Result<PathBuf, PackError> {
    let built;
    let wheel = match wheel {
        Some(path) => path.to_path_buf(),
        None => {
            let out = dir.join("dist");
            let report = pack_build(
                dir,
                &PackBuildOptions {
                    wheel_out: Some(out),
                    ..PackBuildOptions::default()
                },
            )?;
            built = report.wheel.expect("wheel_out set, so a wheel was built");
            built
        }
    };

    let mut argv: Vec<String> = vec!["publish".into()];
    if let Some(index) = index {
        // A URL publishes directly; a name selects a configured index.
        if index.contains("://") {
            argv.extend(["--publish-url".into(), index.to_string()]);
        } else {
            argv.extend(["--index".into(), index.to_string()]);
        }
    }
    argv.push(wheel.display().to_string());

    if dry_run {
        println!("would run: uv {}", argv.join(" "));
        return Ok(wheel);
    }
    let status = std::process::Command::new("uv")
        .args(&argv)
        .status()
        .map_err(|e| PackError::Publish {
            detail: e.to_string(),
        })?;
    if !status.success() {
        return Err(PackError::Publish {
            detail: format!("exit {status}"),
        });
    }
    Ok(wheel)
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

    /// Builder parsing: the three kinds, a command template requirement, and
    /// a clean error for an unknown kind.
    #[test]
    fn builder_kinds_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let with_builder = |body: &str| {
            write_files(
                tmp.path(),
                &[(
                    "pyproject.toml",
                    &format!(
                        "[project]\nname = \"p\"\nversion = \"0.0.0\"\n\
                         [tool.metor.pack]\ncrate = \"p\"\n\
                         [tool.metor.pack.builder]\n{body}"
                    ),
                )],
            );
            read_pack_config(tmp.path())
        };
        assert_eq!(
            with_builder("kind = \"cargo\"\n").unwrap().builder,
            Builder::Cargo
        );
        assert_eq!(
            with_builder("kind = \"zigbuild\"\n").unwrap().builder,
            Builder::Zigbuild
        );
        assert_eq!(
            with_builder("kind = \"command\"\ncommand = [\"nix\", \"{triple}\"]\n")
                .unwrap()
                .builder,
            Builder::Command(vec!["nix".into(), "{triple}".into()])
        );
        assert!(matches!(
            with_builder("kind = \"command\"\n").unwrap_err(),
            PackError::MissingField { field, .. } if field.contains("builder.command")
        ));
        assert!(matches!(
            with_builder("kind = \"bazel\"\n").unwrap_err(),
            PackError::UnknownBuilder { kind, .. } if kind == "bazel"
        ));
    }

    /// A dev pack needs both its own `Cargo.toml` and an explicit
    /// `[tool.metor.pack]` table — a plain Python dist (the `metor-config`
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
            format!("metor-fsw-abi=={}", crate::abi::FSW_ABI_VERSION)
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

    /// Assembling with a skewed sidecar is a hard error naming both triples;
    /// nothing staged is its own clean error.
    #[test]
    fn assemble_gates_on_manifest_identity() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(
            tmp.path(),
            &[(
                "pyproject.toml",
                "[project]\nname = \"toy-pack\"\nversion = \"0.1.0\"\n\
                 [tool.metor.pack]\ncrate = \"toy\"\nlib = \"toy\"\n",
            )],
        );
        let libs = tmp.path().join("libs");
        let out = tmp.path().join("dist");

        let err = pack_assemble(tmp.path(), &[libs.clone()], &out).unwrap_err();
        assert!(matches!(err, PackError::NothingStaged { .. }), "{err}");

        for (triple, manifest) in [
            ("aarch64-unknown-linux-gnu", b"manifest".as_slice()),
            ("x86_64-unknown-linux-gnu", b"DIVERGENT".as_slice()),
        ] {
            let dir = libs.join(triple);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("libtoy.so"), b"elf").unwrap();
            std::fs::write(dir.join("libtoy.so.manifest"), manifest).unwrap();
        }
        let err = pack_assemble(tmp.path(), &[libs], &out).unwrap_err();
        assert!(
            matches!(
                &err,
                PackError::ManifestSkew { triple, reference }
                    if triple == "x86_64-unknown-linux-gnu"
                        && reference == "aarch64-unknown-linux-gnu"
            ),
            "{err}"
        );
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

    /// `pack build` over the fixture (host triple): the wheel is
    /// byte-reproducible, carries the module + `_libs/<triple>/` payload and
    /// the injected pins, and `pack assemble` over the same staging produces
    /// the identical wheel.
    #[test]
    fn pack_build_wheel_is_reproducible() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let host = match super::super::build_driver::build_target(&[]) {
            Some(host) => host,
            None => {
                eprintln!("skipping: host triple undeterminable");
                return;
            }
        };
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            format!(
                "[project]\nname = \"fixture-pack\"\nversion = \"0.1.0\"\n\
                 requires-python = \">=3.11\"\n\
                 [tool.metor.pack]\nid = \"fixture\"\ncrate = \"metor-fsw-2-dl-fixture\"\n\
                 lib = \"metor_fsw_2_dl_fixture\"\ntargets = [\"{host}\"]\n"
            ),
        )
        .unwrap();

        let build = |out: &str| {
            pack_build(
                tmp.path(),
                &PackBuildOptions {
                    wheel_out: Some(tmp.path().join(out)),
                    ..PackBuildOptions::default()
                },
            )
        };
        let a = match build("a") {
            Ok(report) => report.wheel.expect("wheel assembled"),
            Err(e) => {
                eprintln!("skipping: pack_build failed: {e}");
                return;
            }
        };
        assert_eq!(
            a.file_name().and_then(|n| n.to_str()),
            Some("fixture_pack-0.1.0-py3-none-any.whl")
        );
        let b = build("b").expect("rebuild").wheel.unwrap();
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "identical inputs produce byte-identical wheels"
        );

        // Staging + assemble produces the same wheel as the one-shot flow.
        let staged = build_staged(tmp.path(), &host);
        let joined = pack_assemble(tmp.path(), &[staged], &tmp.path().join("c"))
            .expect("assemble from staging")
            .wheel
            .unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&joined).unwrap());

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

    fn build_staged(dir: &Path, host: &str) -> PathBuf {
        let staged = dir.join("staged");
        pack_build(
            dir,
            &PackBuildOptions {
                libs_out: Some(staged.clone()),
                ..PackBuildOptions::default()
            },
        )
        .expect("staging build");
        assert!(
            staged
                .join(host)
                .join(super::super::cdylib_file_name_for(
                    host,
                    "metor_fsw_2_dl_fixture"
                ))
                .exists()
        );
        staged
    }
}
