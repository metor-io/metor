//! Generate typed Python pack modules from pack manifests (`metor-fsw stubgen`).
//!
//! A mission's `pyproject.toml` lists its loadable artifacts under
//! `[tool.metor.artifacts]`; for each, `stubgen` reads the pack's manifest
//! (the `<cdylib>.manifest` sidecar the build driver writes, so nothing is
//! `dlopen`ed into this process) and emits a `packs/<id>.py` — into the
//! mission tree by default, or into a build directory via `--out-dir` (how
//! a mission's PEP 517 backend keeps stubs venv-only):
//!
//! - an `ARTIFACT` constant carrying the artifact id, crate, lib stem, and a
//!   `sha256:<hex>` hash of the manifest bytes (the staleness anchor
//!   [`resolve`](super::resolve) enforces);
//! - one [`Frame`] marker class per distinct frame schema, so a cross-system
//!   frame mismatch is a pyright error before the host ever runs;
//! - one `System` subclass per entry with a typed keyword-only `__init__`
//!   (real kwarg defaults split out of the manifest's whole-struct defaults
//!   blob), plus class-level port annotations;
//! - a module-level occupant callable for each snake_case entry.
//!
//! The generated text is deterministic — stable ordering, no timestamps, no
//! absolute paths — so `--check` is a byte diff and a bundle stays
//! reproducible. Type parameters are annotations for the checker and are
//! erased at runtime.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};

use crate::abi::{PackEntryDesc, PackManifest};
use crate::descriptor::{Delivery, FanIn, PortDesc, PortSchema};

use super::model::Wiring;
use super::{BuildOptions, WiringBuilder, provision_artifacts};

/// Everything a `stubgen` invocation needs: the mission directory whose
/// `pyproject.toml` lists the artifacts, whether to build them first, and
/// whether to only verify the checked-in stubs.
#[derive(Clone, Debug)]
pub struct StubgenOptions {
    /// The mission directory (holds `pyproject.toml`; stubs land in `packs/`).
    pub mission_dir: PathBuf,
    /// Where to write the `packs` package (default: `<mission_dir>/packs`).
    /// A build front-end (e.g. a PEP 517 backend) points this at its own
    /// build directory instead of the source tree.
    pub out_dir: Option<PathBuf>,
    /// Verify the checked-in stubs are byte-identical instead of writing them.
    pub check: bool,
    /// Build the listed crates first (the default). `false` requires them
    /// prebuilt, locating each `.so` and its sidecar without running cargo.
    pub build: bool,
    /// Build the `--release` profile.
    pub release: bool,
    /// Extra args appended to every `cargo build`.
    pub cargo_args: Vec<String>,
}

/// The outcome of a `stubgen` run.
#[derive(Debug, Default)]
pub struct StubgenReport {
    /// Modules written (generate mode) or found current (check mode).
    pub modules: Vec<PathBuf>,
    /// Modules that were missing or out of date (check mode only).
    pub stale: Vec<PathBuf>,
}

/// Why a `stubgen` invocation could not complete.
#[derive(Debug, thiserror::Error)]
pub enum StubgenError {
    /// `pyproject.toml` could not be read.
    #[error("failed to read `{path}`: {source}")]
    PyprojectRead {
        /// The `pyproject.toml` path.
        path: PathBuf,
        #[source]
        /// The I/O error.
        source: std::io::Error,
    },
    /// `pyproject.toml` did not parse as TOML.
    #[error("failed to parse `{path}`: {source}")]
    PyprojectParse {
        /// The `pyproject.toml` path.
        path: PathBuf,
        #[source]
        /// The TOML error.
        source: toml::de::Error,
    },
    /// No `[tool.metor.artifacts]` table, or it was empty.
    #[error(
        "`{path}` has no `[tool.metor.artifacts]` entries; add one per loadable pack, \
         e.g. `adcs = {{ crate = \"adcs-systems\", lib = \"adcs_systems\" }}`"
    )]
    NoArtifacts {
        /// The `pyproject.toml` path.
        path: PathBuf,
    },
    /// The build driver failed.
    #[error(transparent)]
    Build(#[from] super::BuildError),
    /// A prebuilt artifact could not be located under `--no-build`.
    #[error(
        "could not locate a built `{cdylib}` for artifact `{id}` (crate `{crate_name}`); \
         run `metor-fsw stubgen` without `--no-build`, or build the crate first"
    )]
    NotBuilt {
        /// The artifact id.
        id: String,
        /// The cargo package name.
        crate_name: String,
        /// The expected cdylib file name.
        cdylib: String,
    },
    /// The manifest bytes could not be obtained.
    #[error("failed to read the manifest for artifact `{id}`: {detail}")]
    Manifest {
        /// The artifact id.
        id: String,
        /// What went wrong.
        detail: String,
    },
    /// A generated module could not be written or a checked file read.
    #[error("failed to {verb} `{path}`: {source}")]
    Io {
        /// `write` or `read`.
        verb: &'static str,
        /// The file path.
        path: PathBuf,
        #[source]
        /// The I/O error.
        source: std::io::Error,
    },
    /// `--check` found stale or missing modules.
    #[error(
        "stubs are out of date: {}\nregenerate with `metor-fsw stubgen`",
        .0.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    CheckFailed(Vec<PathBuf>),
}

/// One `[tool.metor.artifacts]` entry: the crate that builds the pack cdylib
/// and its library stem (the `#[lib] name`), exactly the fields the recorder's
/// `artifact` node carries.
#[derive(Debug)]
struct ArtifactEntry {
    crate_name_field: String,
    lib: String,
}

// `crate` is a keyword, so the TOML `crate` key is read by hand rather than
// via a serde field rename.
impl ArtifactEntry {
    fn from_toml(value: &toml::Value) -> Option<Self> {
        Some(Self {
            crate_name_field: value.get("crate")?.as_str()?.to_string(),
            lib: value.get("lib")?.as_str()?.to_string(),
        })
    }
}

/// Read `pyproject.toml`'s `[tool.metor.artifacts]` into `(id, crate, lib)`
/// triples, in the table's declaration order made deterministic by sorting on
/// the id.
fn read_artifacts(pyproject: &Path) -> Result<Vec<(String, ArtifactEntry)>, StubgenError> {
    let text =
        std::fs::read_to_string(pyproject).map_err(|source| StubgenError::PyprojectRead {
            path: pyproject.to_path_buf(),
            source,
        })?;
    let doc: toml::Value = text
        .parse()
        .map_err(|source| StubgenError::PyprojectParse {
            path: pyproject.to_path_buf(),
            source,
        })?;
    let table = doc
        .get("tool")
        .and_then(|t| t.get("metor"))
        .and_then(|m| m.get("artifacts"))
        .and_then(toml::Value::as_table);
    let mut out: Vec<(String, ArtifactEntry)> = match table {
        Some(table) => table
            .iter()
            .filter_map(|(id, v)| ArtifactEntry::from_toml(v).map(|e| (id.clone(), e)))
            .collect(),
        None => Vec::new(),
    };
    if out.is_empty() {
        return Err(StubgenError::NoArtifacts {
            path: pyproject.to_path_buf(),
        });
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Generate (or verify) the pack modules for the mission at
/// [`StubgenOptions::mission_dir`].
pub fn stubgen(opts: &StubgenOptions) -> Result<StubgenReport, StubgenError> {
    let pyproject = opts.mission_dir.join("pyproject.toml");
    let artifacts = read_artifacts(&pyproject)?;

    // Locate each artifact's cdylib: build via the driver (the default, which
    // also refreshes the sidecar we hash), or a cargo-free filesystem search.
    let mut located: Vec<(String, ArtifactEntry, PathBuf)> = Vec::with_capacity(artifacts.len());
    if opts.build {
        let mut wiring: Wiring = {
            let mut b = WiringBuilder::new();
            for (id, e) in &artifacts {
                b = b.artifact(id.clone(), e.crate_name_field.clone(), e.lib.clone());
            }
            b.build()
        };
        provision_artifacts(
            &mut wiring,
            &BuildOptions {
                release: opts.release,
                extra_args: opts.cargo_args.clone(),
                manifest_sidecar: true,
            },
        )?;
        for ((id, e), art) in artifacts.into_iter().zip(wiring.artifacts.into_iter()) {
            let path = art
                .path
                .expect("provision_artifacts fills every path or errors");
            located.push((id, e, path));
        }
    } else {
        for (id, e) in artifacts {
            let cdylib = super::cdylib_file_name(&e.lib);
            let path = super::build_driver::locate_built(&opts.mission_dir, &cdylib, opts.release)
                .ok_or_else(|| StubgenError::NotBuilt {
                    id: id.clone(),
                    crate_name: e.crate_name_field.clone(),
                    cdylib: cdylib.clone(),
                })?;
            located.push((id, e, path));
        }
    }

    // The package files that never vary: an empty `__init__.py` and the
    // `py.typed` marker, written once so `from packs.<id> import ...` and
    // pyright both work.
    let packs_dir = opts
        .out_dir
        .clone()
        .unwrap_or_else(|| opts.mission_dir.join("packs"));
    let mut report = StubgenReport::default();
    reconcile(&packs_dir.join("__init__.py"), "", opts.check, &mut report)?;
    reconcile(&packs_dir.join("py.typed"), "", opts.check, &mut report)?;

    for (id, entry, so) in &located {
        let bytes = manifest_bytes(id, so)?;
        let module = render_module(
            id,
            &entry.crate_name_field,
            &entry.lib,
            &bytes,
            &StubFlavor::Local,
        )?;
        reconcile(
            &packs_dir.join(format!("{id}.py")),
            &module,
            opts.check,
            &mut report,
        )?;
    }

    if opts.check && !report.stale.is_empty() {
        return Err(StubgenError::CheckFailed(report.stale.clone()));
    }
    Ok(report)
}

/// Obtain the raw manifest bytes for a built artifact: the sidecar if present
/// (no dlopen), else a describe worker, else an in-process describe.
fn manifest_bytes(id: &str, so: &Path) -> Result<Vec<u8>, StubgenError> {
    if let Some(bytes) = crate::dl::manifest_sidecar_bytes(so) {
        return Ok(bytes);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if crate::proc::worker_guard_installed() {
        return crate::proc::describe_via_worker(None, so).map_err(|e| StubgenError::Manifest {
            id: id.to_string(),
            detail: e.to_string(),
        });
    }
    crate::dl::describe_raw(so).map_err(|e| StubgenError::Manifest {
        id: id.to_string(),
        detail: e.to_string(),
    })
}

/// Write `contents` to `path` (creating parents), or in `check` mode compare
/// it against the file on disk, recording the outcome in `report`.
fn reconcile(
    path: &Path,
    contents: &str,
    check: bool,
    report: &mut StubgenReport,
) -> Result<(), StubgenError> {
    if check {
        match std::fs::read_to_string(path) {
            Ok(existing) if existing == contents => report.modules.push(path.to_path_buf()),
            _ => report.stale.push(path.to_path_buf()),
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| StubgenError::Io {
            verb: "write",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| StubgenError::Io {
        verb: "write",
        path: path.to_path_buf(),
        source,
    })?;
    report.modules.push(path.to_path_buf());
    Ok(())
}

// ---------------------------------------------------------------------------
// Codegen
// ---------------------------------------------------------------------------

/// Which shape of `ARTIFACT` constant a generated module carries.
///
/// A **local** module (mission-level `stubgen`) records identity only; the
/// build driver compiles the crate at build/run time. A **prebuilt** module
/// (`pack dev`, and the published pack wheel) also locates its own per-triple
/// library payload (`_libs/` next to the module), stamps the generating ABI
/// version, and names the distribution it belongs to — the fields the
/// recorder passes through to [`Artifact`](super::model::Artifact)'s
/// `prebuilt_dir`/`dist`.
#[derive(Clone, Debug)]
pub enum StubFlavor {
    /// Identity-only `ARTIFACT`; the artifact is crate-built.
    Local,
    /// Self-locating `ARTIFACT` over a `_libs/<triple>/` payload.
    Prebuilt {
        /// The `FSW_ABI_VERSION` the pack was built against.
        abi_version: u32,
        /// The owning distribution's `(name, version)`, from the pack's
        /// `pyproject.toml` `[project]` table.
        dist: Option<(String, String)>,
    },
}

/// The one-module code generator: decode the manifest, collect frame markers
/// and nested dataclasses, and render the deterministic module text.
pub fn render_module(
    id: &str,
    crate_name: &str,
    lib: &str,
    manifest_bytes: &[u8],
    flavor: &StubFlavor,
) -> Result<String, StubgenError> {
    let msg: PackManifest =
        postcard::from_bytes(manifest_bytes).map_err(|e| StubgenError::Manifest {
            id: id.to_string(),
            detail: format!("manifest does not decode: {e}"),
        })?;
    let hash = manifest_hash(manifest_bytes);

    let mut cg = Codegen::default();
    // Two passes over entries so all frame markers and nested dataclasses are
    // collected before any are emitted (forward references are fine under
    // `from __future__ import annotations`, but a stable declaration order is
    // easier to read and diff).
    let mut bodies: Vec<String> = Vec::new();
    for sys in &msg.systems {
        bodies.push(cg.entry(sys));
    }

    let mut out = String::new();
    out.push_str(&header(id, crate_name, flavor));
    out.push_str("from __future__ import annotations\n\n");
    if matches!(flavor, StubFlavor::Prebuilt { .. }) {
        out.push_str("from pathlib import Path\n\n");
    }
    out.push_str(&cg.imports());
    out.push('\n');
    out.push_str(&format!(
        "ARTIFACT = Artifact(\n    id=\"{id}\",\n    crate=\"{crate_name}\",\n    lib=\"{lib}\",\n    manifest_hash=\"{hash}\",\n",
    ));
    if let StubFlavor::Prebuilt { abi_version, dist } = flavor {
        out.push_str("    prebuilt=str(Path(__file__).resolve().parent / \"_libs\"),\n");
        out.push_str(&format!("    abi_version={abi_version},\n"));
        if let Some((name, version)) = dist {
            out.push_str(&format!(
                "    dist=\"{name}\",\n    dist_version=\"{version}\",\n"
            ));
        }
    }
    out.push_str(")\n");

    for (name, base) in &cg.frames {
        out.push_str(&format!(
            "\n\nclass {name}({base}):\n    \"\"\"Frame marker (checker-only).\"\"\"\n"
        ));
    }
    for body in &cg.dataclasses {
        out.push_str("\n\n");
        out.push_str(body);
    }
    for body in bodies {
        out.push_str("\n\n");
        out.push_str(&body);
    }
    Ok(out)
}

/// The `sha256:<hex>` hash of the manifest postcard bytes — pure code changes
/// leave the manifest (and this hash) unchanged, so stubs never churn on them.
/// Shared with the resolve-time staleness check.
pub(super) fn manifest_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// The generated-file header: a provenance line and the regenerate/verify
/// commands, deliberately free of timestamps and absolute paths so `--check`
/// is a byte diff.
fn header(id: &str, crate_name: &str, flavor: &StubFlavor) -> String {
    match flavor {
        StubFlavor::Local => format!(
            "# @generated by `metor-fsw stubgen` from the `{id}` pack manifest (crate `{crate_name}`).\n\
             # Do not edit by hand.\n\
             #\n\
             # Regenerate after changing the pack's params or ports:\n\
             #     metor-fsw stubgen\n\
             # Verify the checked-in stubs are current (CI):\n\
             #     metor-fsw stubgen --check\n\
             \"\"\"Typed pack module for artifact `{id}`.\"\"\"\n\n",
        ),
        StubFlavor::Prebuilt { .. } => format!(
            "# @generated by `metor-fsw pack dev` from the `{id}` pack manifest (crate `{crate_name}`).\n\
             # Do not edit by hand.\n\
             #\n\
             # Regenerate after changing the pack's params or ports:\n\
             #     metor-fsw pack dev\n\
             # (a `uv sync` of a mission depending on this pack runs it for you)\n\
             \"\"\"Typed pack module for artifact `{id}`.\"\"\"\n\n",
        ),
    }
}

/// The mutable state a module's codegen threads: collected frame markers,
/// nested dataclasses, and which core symbols the imports must name.
#[derive(Default)]
struct Codegen {
    /// `(class name, base)` frame markers, in first-appearance order.
    frames: Vec<(String, String)>,
    /// frame_id (or packet id) → chosen class name, for dedupe and lookup.
    frame_by_id: Vec<(u64, String)>,
    /// Chosen class names, for collision suffixing.
    taken_names: HashSet<String>,
    /// Rendered nested dataclass bodies, in first-appearance order.
    dataclasses: Vec<String>,
    /// Nested dataclass names already emitted.
    dataclass_names: HashSet<String>,
    used_sequence: bool,
    used_literal: bool,
    used_msg: bool,
    used_inport: bool,
    used_outport: bool,
    used_system: bool,
}

impl Codegen {
    /// The `from typing`/`from metor_config` import block, naming only the
    /// symbols the module actually uses.
    fn imports(&self) -> String {
        let mut typing: Vec<&str> = Vec::new();
        if self.used_literal {
            typing.push("Literal");
        }
        if self.used_sequence {
            typing.push("Sequence");
        }
        let mut core: Vec<&str> = vec!["Artifact"];
        if !self.frames.is_empty() {
            core.push("Frame");
        }
        if self.used_inport {
            core.push("InPort");
        }
        if self.used_msg {
            core.push("Msg");
        }
        if self.used_outport {
            core.push("OutPort");
        }
        if self.used_system {
            core.push("System");
        }
        core.sort_unstable();
        let mut out = String::new();
        if !self.dataclasses.is_empty() {
            out.push_str("from dataclasses import dataclass\n");
        }
        if !typing.is_empty() {
            out.push_str(&format!("from typing import {}\n", typing.join(", ")));
        }
        if !self.dataclasses.is_empty() || !typing.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("from metor_config import {}\n", core.join(", ")));
        out
    }

    /// Render one manifest entry: a `System` subclass for a CapWords name, a
    /// module-level occupant callable for a snake_case (sequence task) name.
    fn entry(&mut self, entry: &PackEntryDesc) -> String {
        let desc = &entry.descriptor;
        // Collect port markers first so annotations can reference them.
        for port in desc.inputs.iter().chain(desc.outputs.iter()) {
            self.marker_name(port);
        }
        let is_class = desc.name.chars().next().is_some_and(char::is_uppercase);
        let params = self.params(&entry.params_schema, entry.params_default.as_deref());
        if is_class {
            self.render_class(entry, &params)
        } else {
            self.render_occupant(entry, &params)
        }
    }

    /// A `System` subclass: a typed keyword-only `__init__` plus class-level
    /// port annotations (checker-only; runtime access returns handles).
    fn render_class(&mut self, entry: &PackEntryDesc, params: &[Param]) -> String {
        self.used_system = true;
        let desc = &entry.descriptor;
        let name = &desc.name;
        let mut out = format!("class {name}(System):\n");
        out.push_str(&docstring(
            1,
            &format!("`{name}` pack entry."),
            params,
            &entry.params_docs,
        ));
        out.push('\n');
        out.push_str(&init_signature(params));
        out.push_str(&super_init(name, params));

        let annotations = self.port_annotations(desc);
        if annotations.is_empty() {
            out.push_str("\n    # This entry declares no ports.\n");
        } else {
            out.push('\n');
            out.push_str(&annotations);
        }
        out
    }

    /// A sequence occupant: a module-level function with the same typed kwargs,
    /// returning an occupant spec bound to this artifact.
    fn render_occupant(&mut self, entry: &PackEntryDesc, params: &[Param]) -> String {
        self.used_system = true;
        let name = &entry.descriptor.name;
        let sig = occupant_signature(name, params);
        let doc = docstring(
            1,
            &format!("Occupant `{name}` for a `mode`-style slot's allow set."),
            params,
            &entry.params_docs,
        );
        let body = occupant_body(name, params);
        format!("{sig}{doc}{body}")
    }

    /// The class-level `port: OutPort[Frame]  # …` annotation block.
    fn port_annotations(&mut self, desc: &crate::descriptor::SystemDescriptor) -> String {
        let mut out = String::new();
        for port in &desc.inputs {
            out.push_str(&self.port_line(port, true));
        }
        for port in &desc.outputs {
            out.push_str(&self.port_line(port, false));
        }
        out
    }

    fn port_line(&mut self, port: &PortDesc, input: bool) -> String {
        let marker = self.marker_name(port);
        let wrapper = if input {
            self.used_inport = true;
            "InPort"
        } else {
            self.used_outport = true;
            "OutPort"
        };
        let mut notes: Vec<String> = Vec::new();
        notes.push(if input {
            "input".into()
        } else {
            "output".into()
        });
        notes.push(match port.delivery {
            Delivery::Snapshot => "latest-wins".into(),
            Delivery::Log => "log".into(),
        });
        if input && matches!(port.fan_in, FanIn::Many) {
            notes.push("fan-in".into());
        }
        if port.telemetered {
            notes.push("telemetered".into());
        }
        format!(
            "    {}: {}[{}]  # {}\n",
            port.name,
            wrapper,
            marker,
            notes.join(", ")
        )
    }

    /// The marker class name for a port, generating a `Frame` subclass for a
    /// Table port keyed by frame id (named from its metadata, falling back to
    /// the port name) and using the shared `Msg` marker for a self-describing
    /// Postcard port. Idempotent per frame id — the first caller's name wins,
    /// so the collection pass and the annotation pass agree.
    fn marker_name(&mut self, port: &PortDesc) -> String {
        match &port.schema {
            PortSchema::Table {
                frame_id, metadata, ..
            } => {
                let id = frame_id.0;
                if let Some((_, name)) = self.frame_by_id.iter().find(|(k, _)| *k == id) {
                    return name.clone();
                }
                // A frame prefixes its components with its own name, so their
                // shared leading segment is the frame name; a frame that emits
                // no component metadata (e.g. a variable-length list) falls
                // back to the referencing port's name.
                let base = frame_name(metadata)
                    .map(|n| pascal_case(&n))
                    .unwrap_or_else(|| pascal_case(&port.name));
                let name = self.unique_name(base);
                self.taken_names.insert(name.clone());
                self.frames.push((name.clone(), "Frame".to_string()));
                self.frame_by_id.push((id, name.clone()));
                name
            }
            PortSchema::Postcard { .. } => {
                self.used_msg = true;
                "Msg".to_string()
            }
        }
    }

    /// A collision-free class name (`Name`, then `Name2`, `Name3`, …).
    fn unique_name(&self, base: String) -> String {
        if !self.taken_names.contains(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}{n}"))
            .find(|c| !self.taken_names.contains(c))
            .unwrap()
    }

    /// The typed `Param` list for an entry's params schema, splitting the
    /// whole-struct defaults blob into per-field defaults.
    fn params(&mut self, schema: &OwnedNamedType, defaults: Option<&[u8]>) -> Vec<Param> {
        let fields = match &schema.ty {
            OwnedDataModelType::Struct(fields) => fields,
            // A unit `Params` (e.g. `()`) is a paramless entry.
            _ => return Vec::new(),
        };
        let default_obj = decode_defaults(schema, defaults);
        fields
            .iter()
            .map(|field| {
                let ty = &field.ty.ty;
                let annotation = self.py_type(&field.ty);
                let default = if matches!(ty, OwnedDataModelType::Option(_)) {
                    // Option fields default to None regardless of the blob.
                    Some("None".to_string())
                } else {
                    default_obj
                        .as_ref()
                        .and_then(|o| o.get(&field.name))
                        .map(|v| py_literal(v, ty))
                };
                Param {
                    name: field.name.clone(),
                    annotation,
                    default,
                }
            })
            .collect()
    }

    /// Map a postcard-schema type to its Python annotation, registering any
    /// nested dataclass it names (design §5.2).
    fn py_type(&mut self, nt: &OwnedNamedType) -> String {
        use OwnedDataModelType as T;
        match &nt.ty {
            T::Bool => "bool".into(),
            T::I8
            | T::I16
            | T::I32
            | T::I64
            | T::I128
            | T::U8
            | T::U16
            | T::U32
            | T::U64
            | T::U128
            | T::Usize
            | T::Isize => "int".into(),
            T::F32 | T::F64 => "float".into(),
            T::Char | T::String => "str".into(),
            T::ByteArray => "bytes".into(),
            T::Option(inner) => format!("{} | None", self.py_type(inner)),
            T::Seq(inner) => {
                self.used_sequence = true;
                format!("Sequence[{}]", self.py_type(inner))
            }
            T::Tuple(items) | T::TupleStruct(items) => {
                if items.is_empty() {
                    "tuple[()]".into()
                } else {
                    let parts: Vec<String> = items.iter().map(|i| self.py_type(i)).collect();
                    format!("tuple[{}]", parts.join(", "))
                }
            }
            T::Map { key, val } => {
                format!("dict[{}, {}]", self.py_type(key), self.py_type(val))
            }
            T::NewtypeStruct(inner) => self.py_type(inner),
            T::Struct(_) => self.nested_struct(nt),
            T::Enum(variants) => self.enum_type(nt, variants),
            T::Unit | T::UnitStruct => "None".into(),
            T::Schema => "object".into(),
        }
    }

    /// Register (once) a frozen kw_only dataclass for a nested struct and
    /// return its name.
    fn nested_struct(&mut self, nt: &OwnedNamedType) -> String {
        let name = pascal_case(&type_ident(&nt.name));
        if self.dataclass_names.contains(&name) {
            return name;
        }
        self.dataclass_names.insert(name.clone());
        let OwnedDataModelType::Struct(fields) = &nt.ty else {
            return name;
        };
        // Reserve the name before recursing so a self-referential struct
        // terminates.
        let mut lines = String::new();
        for field in fields {
            let annotation = self.py_type(&field.ty);
            lines.push_str(&format!("    {}: {}\n", field.name, annotation));
        }
        if lines.is_empty() {
            lines.push_str("    pass\n");
        }
        let body = format!("@dataclass(frozen=True, kw_only=True)\nclass {name}:\n{lines}");
        self.dataclasses.push(body);
        name
    }

    /// A fieldless enum maps to a `Literal[...]`; an enum with data to a union
    /// of generated variant dataclasses (design §5.2).
    fn enum_type(
        &mut self,
        nt: &OwnedNamedType,
        variants: &[postcard_schema::schema::owned::OwnedNamedVariant],
    ) -> String {
        use postcard_schema::schema::owned::OwnedDataModelVariant as V;
        if variants.iter().all(|v| matches!(v.ty, V::UnitVariant)) {
            self.used_literal = true;
            let names: Vec<String> = variants.iter().map(|v| format!("\"{}\"", v.name)).collect();
            return format!("Literal[{}]", names.join(", "));
        }
        // Enum with data: one frozen dataclass per variant, unioned.
        let prefix = pascal_case(&type_ident(&nt.name));
        let mut union: Vec<String> = Vec::new();
        for v in variants {
            let vname = format!("{prefix}{}", pascal_case(&v.name));
            union.push(vname.clone());
            if self.dataclass_names.contains(&vname) {
                continue;
            }
            self.dataclass_names.insert(vname.clone());
            let mut lines = String::new();
            match &v.ty {
                V::UnitVariant => {}
                V::NewtypeVariant(inner) => {
                    let annotation = self.py_type(inner);
                    lines.push_str(&format!("    value: {annotation}\n"));
                }
                V::TupleVariant(items) => {
                    for (i, item) in items.iter().enumerate() {
                        let annotation = self.py_type(item);
                        lines.push_str(&format!("    field_{i}: {annotation}\n"));
                    }
                }
                V::StructVariant(fields) => {
                    for field in fields {
                        let annotation = self.py_type(&field.ty);
                        lines.push_str(&format!("    {}: {annotation}\n", field.name));
                    }
                }
            }
            if lines.is_empty() {
                lines.push_str("    pass\n");
            }
            self.dataclasses.push(format!(
                "@dataclass(frozen=True, kw_only=True)\nclass {vname}:\n{lines}"
            ));
        }
        union.join(" | ")
    }
}

/// One generated constructor parameter.
struct Param {
    name: String,
    annotation: String,
    default: Option<String>,
}

/// The `def __init__(...) -> None:` signature for a `System` subclass.
fn init_signature(params: &[Param]) -> String {
    if params.is_empty() {
        return "    def __init__(self) -> None:\n".to_string();
    }
    let mut out = String::from("    def __init__(\n        self,\n        *,\n");
    write_keyword_params(&mut out, params, 8);
    out.push_str("    ) -> None:\n");
    out
}

/// The `super().__init__(...)` body recording the spec.
fn super_init(entry: &str, params: &[Param]) -> String {
    if params.is_empty() {
        return format!("        super().__init__(\"{entry}\", ARTIFACT)\n");
    }
    let mut out =
        format!("        super().__init__(\n            \"{entry}\",\n            ARTIFACT,\n");
    write_keyword_args(&mut out, params, 12);
    out.push_str("        )\n");
    out
}

/// The `def <name>(...) -> System:` signature for an occupant callable.
fn occupant_signature(name: &str, params: &[Param]) -> String {
    if params.is_empty() {
        return format!("def {name}() -> System:\n");
    }
    let mut out = format!("def {name}(\n    *,\n");
    write_keyword_params(&mut out, params, 4);
    out.push_str(") -> System:\n");
    out
}

/// The occupant callable body: build and return the spec.
fn occupant_body(name: &str, params: &[Param]) -> String {
    if params.is_empty() {
        return format!("    return System(\"{name}\", ARTIFACT)\n");
    }
    let mut out = format!("    return System(\n        \"{name}\",\n        ARTIFACT,\n");
    write_keyword_args(&mut out, params, 8);
    out.push_str("    )\n");
    out
}

fn write_keyword_params(out: &mut String, params: &[Param], indent: usize) {
    let pad = " ".repeat(indent);
    for p in params {
        write!(out, "{pad}{}: {}", p.name, p.annotation).unwrap();
        if let Some(default) = &p.default {
            write!(out, " = {default}").unwrap();
        }
        out.push_str(",\n");
    }
}

fn write_keyword_args(out: &mut String, params: &[Param], indent: usize) {
    let pad = " ".repeat(indent);
    for p in params {
        writeln!(out, "{pad}{}={},", p.name, p.name).unwrap();
    }
}

/// A `"""..."""` docstring at `indent` levels (4 spaces each): a summary line,
/// the entry's own doc (ABI v6) if present, and a `Parameters:` list of the
/// documented params, in signature order. pyright surfaces this on hover.
fn docstring(
    indent: usize,
    summary: &str,
    params: &[Param],
    params_docs: &[(String, String)],
) -> String {
    let pad = "    ".repeat(indent);
    let documented: Vec<(&str, &str)> = params
        .iter()
        .filter_map(|p| {
            params_docs
                .iter()
                .find(|(name, _)| *name == p.name)
                .map(|(_, doc)| (p.name.as_str(), doc.as_str()))
        })
        .collect();

    // A one-liner when there is nothing extra to say.
    if documented.is_empty() {
        return format!("{pad}\"\"\"{summary}\"\"\"\n");
    }

    let mut out = format!("{pad}\"\"\"{summary}\n");
    out.push_str(&format!("\n{pad}Parameters:\n"));
    for (name, doc) in documented {
        out.push_str(&format!("{pad}    {name}: {doc}\n"));
    }
    out.push_str(&format!("{pad}\"\"\"\n"));
    out
}

/// Decode the whole-struct defaults blob into a JSON object, or `None` when the
/// entry declares no defaults (every field then required).
fn decode_defaults(
    schema: &OwnedNamedType,
    defaults: Option<&[u8]>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let bytes = defaults?;
    if bytes.is_empty() {
        return None;
    }
    match postcard_dyn::from_slice_dyn(schema, bytes) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The frame name from a Table port's component metadata: the leading dotted
/// segment every component shares (a frame prefixes its components with its
/// `#[metor_fsw(name = …)]`), or `None` when no single segment covers them all.
fn frame_name(metadata: &[metor_proto_wkt::ComponentMetadata]) -> Option<String> {
    let first = metadata.first()?.name.split('.').next()?.to_string();
    metadata
        .iter()
        .all(|m| m.name.split('.').next() == Some(first.as_str()))
        .then_some(first)
}

/// Render a JSON default value as a Python literal, tuple-vs-list decided by the
/// field's schema type.
fn py_literal(value: &serde_json::Value, ty: &OwnedDataModelType) -> String {
    use OwnedDataModelType as T;
    use serde_json::Value as J;
    match (value, ty) {
        (J::Null, _) => "None".into(),
        (J::Bool(b), _) => if *b { "True" } else { "False" }.into(),
        (J::Number(n), T::F32 | T::F64) => py_float(n.as_f64().unwrap_or(0.0)),
        (J::Number(n), _) => n.to_string(),
        (J::String(s), _) => py_str(s),
        (J::Array(items), T::Tuple(elems) | T::TupleStruct(elems)) => {
            let parts = render_seq(items, elems.iter().map(|e| &e.ty));
            if parts.len() == 1 {
                format!("({},)", parts[0])
            } else {
                format!("({})", parts.join(", "))
            }
        }
        (J::Array(items), T::Seq(inner)) => {
            let parts: Vec<String> = items.iter().map(|v| py_literal(v, &inner.ty)).collect();
            format!("[{}]", parts.join(", "))
        }
        (J::Array(items), _) => {
            let parts: Vec<String> = items.iter().map(|v| py_literal(v, ty)).collect();
            format!("[{}]", parts.join(", "))
        }
        (J::Object(map), _) => {
            // A nested-struct default renders as a plain dict; the recorder
            // JSON-ifies a dict and a dataclass identically, and pyright checks
            // the field type against the generated dataclass.
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_str(k), py_literal(v, ty)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Render each element of a JSON array against the matching schema type,
/// falling back to the first type once the schema list is exhausted.
fn render_seq<'a>(
    items: &[serde_json::Value],
    types: impl Iterator<Item = &'a OwnedDataModelType>,
) -> Vec<String> {
    let types: Vec<&OwnedDataModelType> = types.collect();
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            py_literal(
                v,
                types
                    .get(i)
                    .or_else(|| types.first())
                    .copied()
                    .unwrap_or(&OwnedDataModelType::F64),
            )
        })
        .collect()
}

/// Format an `f64` as a Python float literal that round-trips, guaranteeing a
/// decimal point so the value stays a `float` and never collapses to an `int`.
fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "float(\"nan\")".into();
    }
    if f.is_infinite() {
        return if f < 0.0 {
            "float(\"-inf\")"
        } else {
            "float(\"inf\")"
        }
        .into();
    }
    let s = format!("{f}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// A double-quoted Python string literal with the essential escapes.
fn py_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The trailing identifier of a (possibly path- or generic-qualified) Rust type
/// name, e.g. `foo::Bar<T>` → `Bar`.
fn type_ident(name: &str) -> String {
    let name = name.split('<').next().unwrap_or(name);
    name.rsplit("::").next().unwrap_or(name).trim().to_string()
}

/// Convert a snake_case or lowercase name to PascalCase for a class name.
fn pascal_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        if c == '_' || c == '.' || c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() { "Frame".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::PackManifest;
    use crate::descriptor::SystemKind;
    use metor_proto::types::ComponentId;
    use metor_proto::vtable::VTable;
    use metor_proto_wkt::ComponentMetadata;
    use postcard_schema::Schema;
    use serde::Serialize;

    // A params type exercising the scalar/Option split (the WP6 defaults blob
    // is the whole struct, so every non-Option field gets a default).
    #[derive(Serialize, Schema, Default)]
    struct Demo {
        count: u64,
        gain: f64,
        label: String,
        armed: bool,
        limit: Option<u32>,
        offsets: [f64; 3],
    }

    fn demo_schema() -> OwnedNamedType {
        OwnedNamedType::from(<Demo as Schema>::SCHEMA)
    }

    #[test]
    fn defaults_split_from_whole_struct_blob() {
        let blob = postcard::to_allocvec(&Demo {
            count: 7,
            gain: 2.5,
            label: "hi".into(),
            armed: true,
            limit: Some(9),
            offsets: [0.1, 0.0, -0.2],
        })
        .unwrap();
        let params = Codegen::default().params(&demo_schema(), Some(&blob));
        let by = |n: &str| params.iter().find(|p| p.name == n).unwrap();
        assert_eq!(by("count").annotation, "int");
        assert_eq!(by("count").default.as_deref(), Some("7"));
        assert_eq!(by("gain").annotation, "float");
        assert_eq!(by("gain").default.as_deref(), Some("2.5"));
        assert_eq!(by("label").default.as_deref(), Some("\"hi\""));
        assert_eq!(by("armed").default.as_deref(), Some("True"));
        // Option fields default to None regardless of the blob.
        assert_eq!(by("limit").annotation, "int | None");
        assert_eq!(by("limit").default.as_deref(), Some("None"));
        // `[f64; 3]` is an exact tuple.
        assert_eq!(by("offsets").annotation, "tuple[float, float, float]");
        assert_eq!(by("offsets").default.as_deref(), Some("(0.1, 0.0, -0.2)"));
    }

    #[test]
    fn no_blob_makes_non_option_fields_required() {
        let params = Codegen::default().params(&demo_schema(), None);
        let by = |n: &str| params.iter().find(|p| p.name == n).unwrap();
        assert!(
            by("count").default.is_none(),
            "no defaults blob => required"
        );
        // Even without a blob an Option field is optional.
        assert_eq!(by("limit").default.as_deref(), Some("None"));
    }

    fn table_port(name: &str, frame: &str, telemetered: bool) -> PortDesc {
        PortDesc {
            name: name.to_string(),
            max_size: 16,
            schema: PortSchema::Table {
                frame_id: ComponentId::new(frame),
                vtable: VTable::default(),
                metadata: vec![ComponentMetadata::from(format!("{frame}.value").as_str())],
            },
            delivery: Delivery::Snapshot,
            fan_in: FanIn::One,
            telemetered,
            conn: Default::default(),
        }
    }

    fn msg_port(name: &str) -> PortDesc {
        PortDesc {
            name: name.to_string(),
            max_size: 8,
            schema: PortSchema::Postcard {
                id: [7, 0],
                schema: None,
            },
            delivery: Delivery::Log,
            fan_in: FanIn::One,
            telemetered: false,
            conn: Default::default(),
        }
    }

    fn entry_desc(
        name: &str,
        inputs: Vec<PortDesc>,
        outputs: Vec<PortDesc>,
        params_docs: Vec<(String, String)>,
    ) -> PackEntryDesc {
        PackEntryDesc {
            descriptor: crate::descriptor::SystemDescriptor {
                name: name.to_string(),
                kind: SystemKind::Cyclic,
                inputs,
                outputs,
                capabilities: Vec::new(),
            },
            params_schema: demo_schema(),
            params_docs,
            reloadable: true,
            params_default: None,
        }
    }

    pub(super) fn demo_manifest() -> Vec<u8> {
        let blob = postcard::to_allocvec(&Demo::default()).unwrap();
        let widget_docs = vec![
            ("count".to_string(), "How many widgets to make.".to_string()),
            (
                "gain".to_string(),
                "Loop gain (1/s), ~3e-12 at 400 km.".to_string(),
            ),
        ];
        let msg = PackManifest {
            systems: vec![
                PackEntryDesc {
                    params_default: Some(blob),
                    ..entry_desc(
                        "Widget",
                        vec![table_port("cmd", "cmd", false)],
                        vec![table_port("sensors", "sensors", true), msg_port("events")],
                        widget_docs,
                    )
                },
                PackEntryDesc {
                    params_schema: OwnedNamedType::from(<() as Schema>::SCHEMA),
                    ..entry_desc(
                        "startup",
                        Vec::new(),
                        vec![table_port("mode_cmd", "mode_cmd", true)],
                        Vec::new(),
                    )
                },
            ],
        };
        postcard::to_allocvec(&msg).unwrap()
    }

    #[test]
    fn render_is_deterministic_and_structured() {
        let bytes = demo_manifest();
        let a = render_module(
            "demo",
            "demo-systems",
            "demo_systems",
            &bytes,
            &StubFlavor::Local,
        )
        .unwrap();
        let b = render_module(
            "demo",
            "demo-systems",
            "demo_systems",
            &bytes,
            &StubFlavor::Local,
        )
        .unwrap();
        assert_eq!(a, b, "codegen is deterministic");

        assert!(a.contains("@generated by `metor-fsw stubgen`"));
        assert!(!a.contains("202"), "no timestamps in the header");
        assert!(a.contains("ARTIFACT = Artifact("));
        assert!(a.contains("manifest_hash=\"sha256:"));
        // A CapWords entry is a class; a snake_case entry an occupant callable.
        assert!(a.contains("class Widget(System):"));
        assert!(a.contains("def startup() -> System:"));
        // Frame markers, one per distinct frame, named from metadata.
        assert!(a.contains("class Cmd(Frame):"));
        assert!(a.contains("class Sensors(Frame):"));
        assert!(a.contains("class ModeCmd(Frame):"));
        // Port annotations reference the markers; a Postcard port uses `Msg`.
        assert!(a.contains("cmd: InPort[Cmd]"));
        assert!(a.contains("sensors: OutPort[Sensors]  # output, latest-wins, telemetered"));
        assert!(a.contains("events: OutPort[Msg]"));
        // Defaults from the whole-struct blob show up as kwarg defaults.
        assert!(a.contains("count: int = 0"));
        assert!(a.contains("limit: int | None = None"));
    }

    /// The prebuilt flavor self-locates its `_libs` payload and carries the
    /// generating ABI version plus dist provenance; the local flavor carries
    /// none of that.
    #[test]
    fn prebuilt_flavor_locates_and_stamps() {
        let bytes = demo_manifest();
        let flavor = StubFlavor::Prebuilt {
            abi_version: crate::abi::FSW_ABI_VERSION,
            dist: Some(("demo-pack".into(), "1.2.0".into())),
        };
        let a = render_module("demo", "demo-systems", "demo_systems", &bytes, &flavor).unwrap();

        assert!(a.contains("@generated by `metor-fsw pack dev`"));
        assert!(a.contains("from pathlib import Path"));
        assert!(a.contains("prebuilt=str(Path(__file__).resolve().parent / \"_libs\"),"));
        assert!(a.contains(&format!("abi_version={},", crate::abi::FSW_ABI_VERSION)));
        assert!(a.contains("dist=\"demo-pack\","));
        assert!(a.contains("dist_version=\"1.2.0\","));

        let local = render_module(
            "demo",
            "demo-systems",
            "demo_systems",
            &bytes,
            &StubFlavor::Local,
        )
        .unwrap();
        assert!(!local.contains("prebuilt="));
        assert!(!local.contains("abi_version="));
        assert!(!local.contains("pathlib"));
    }

    /// One fixture, two consumers: the checked-in `python/tests/data/demo.py`
    /// (imported by the Python suite) must be exactly what this codegen
    /// produces. Regenerate it with the ignored `fixture_dump::write_demo_fixture`.
    #[test]
    fn demo_fixture_matches_checked_in() {
        let generated = render_module(
            "demo",
            "demo-systems",
            "demo_systems",
            &demo_manifest(),
            &StubFlavor::Local,
        )
        .unwrap();
        let checked_in = include_str!("../../python/tests/data/demo.py");
        assert_eq!(
            generated, checked_in,
            "codegen drifted from the checked-in fixture; rerun \
             `cargo test -p metor-fsw-2 fixture_dump -- --ignored`"
        );
    }

    #[test]
    fn manifest_hash_is_stable_and_prefixed() {
        let h = manifest_hash(b"hello");
        assert!(h.starts_with("sha256:"));
        assert_eq!(h, manifest_hash(b"hello"));
        assert_ne!(h, manifest_hash(b"hallo"));
    }

    #[test]
    fn py_float_keeps_a_decimal_point() {
        assert_eq!(py_float(5.0), "5.0");
        assert_eq!(py_float(0.0005), "0.0005");
        assert_eq!(py_float(-0.2), "-0.2");
    }

    #[test]
    fn pascal_case_and_frame_name() {
        assert_eq!(pascal_case("attitude_estimate"), "AttitudeEstimate");
        assert_eq!(pascal_case("tick_in"), "TickIn");
        let meta = vec![
            ComponentMetadata::from("sensors.gyro_b"),
            ComponentMetadata::from("sensors.mag_b"),
        ];
        assert_eq!(frame_name(&meta).as_deref(), Some("sensors"));
    }
}

#[cfg(all(test, not(miri)))]
mod integration {
    //! End-to-end over the dl fixture pack, following the build_driver test
    //! convention: build via cargo, skip (with a note) where that is
    //! unavailable, and serialize on one lock since these share the fixture's
    //! target-dir sidecar.
    use super::*;

    // Shared with the build_driver and dl fixture tests: one target-dir
    // sidecar, so they must serialize.
    use crate::dl::FIXTURE_LOCK;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mission_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.metor.artifacts]\n\
             fixture = { crate = \"metor-fsw-2-dl-fixture\", lib = \"metor_fsw_2_dl_fixture\" }\n",
        )
        .unwrap();
        dir
    }

    fn opts(dir: &std::path::Path, check: bool) -> StubgenOptions {
        StubgenOptions {
            mission_dir: dir.to_path_buf(),
            out_dir: None,
            check,
            build: true,
            release: false,
            cargo_args: Vec::new(),
        }
    }

    #[test]
    fn generate_then_check_roundtrips() {
        let _guard = lock();
        let dir = mission_dir();
        if let Err(e) = stubgen(&opts(dir.path(), false)) {
            eprintln!("skipping: stubgen build failed: {e}");
            return;
        }
        let module = dir.path().join("packs").join("fixture.py");
        let text = std::fs::read_to_string(&module).unwrap();
        assert!(text.contains("class DlCounter(System):"), "{text}");
        assert!(text.contains("class DlEcho(System):"));
        assert!(text.contains("manifest_hash=\"sha256:"));
        assert!(dir.path().join("packs").join("py.typed").exists());
        assert!(dir.path().join("packs").join("__init__.py").exists());

        // Idempotent: --check is clean immediately after generating.
        stubgen(&opts(dir.path(), true)).expect("check clean after generate");

        // Tampering the checked-in module makes --check fail.
        std::fs::write(&module, format!("{text}\n# edited\n")).unwrap();
        let err = stubgen(&opts(dir.path(), true)).expect_err("edited stub is stale");
        assert!(matches!(err, StubgenError::CheckFailed(_)), "{err}");
    }

    #[test]
    fn stale_manifest_hash_is_rejected_at_resolve() {
        let _guard = lock();
        let mut wiring = WiringBuilder::new()
            .artifact(
                "fixture",
                "metor-fsw-2-dl-fixture",
                "metor_fsw_2_dl_fixture",
            )
            .build();
        if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
            eprintln!("skipping: build failed: {e}");
            return;
        }
        // A wrong recorded hash is a stale stub.
        wiring.artifacts[0].manifest_hash = Some("sha256:0000".to_string());
        match super::super::resolve(&wiring, &super::super::Registry::with_builtins()) {
            Err(e) if matches!(e.kind, super::super::LoadErrorKind::StaleStubs { .. }) => {}
            Err(other) => panic!("expected StaleStubs, got {other:?}"),
            Ok(_) => panic!("expected StaleStubs, resolve succeeded"),
        }

        // The real hash passes the freshness gate (resolve then proceeds past
        // the check; it need not fully succeed for this assertion).
        let so = wiring.artifacts[0].path.clone().unwrap();
        let bytes = crate::dl::manifest_sidecar_bytes(&so).expect("sidecar present after build");
        wiring.artifacts[0].manifest_hash = Some(manifest_hash(&bytes));
        if let Err(e) = super::super::resolve(&wiring, &super::super::Registry::with_builtins())
            && matches!(e.kind, super::super::LoadErrorKind::StaleStubs { .. })
        {
            panic!("fresh hash wrongly rejected");
        }

        // Sidecar-absent fallback: copy the object to a fresh dir with no
        // sidecar next to it (leaving the shared one intact), so `manifest_bytes`
        // falls through to an in-process describe — this libtest binary is not a
        // describe worker — and the bytes still equal the sidecar's.
        let iso = tempfile::tempdir().unwrap();
        let iso_so = iso.path().join(so.file_name().unwrap());
        std::fs::copy(&so, &iso_so).unwrap();
        let described = manifest_bytes("fixture", &iso_so).expect("describe fallback");
        assert_eq!(described, bytes, "describe ≡ sidecar bytes");
    }
}

#[cfg(test)]
mod fixture_dump {
    //! Writes the shared demo fixture module to the Python test-data tree; run
    //! explicitly (`--ignored`) when the codegen shape changes, then reviewed
    //! as an ordinary diff. The golden test in `tests` keeps it honest.
    use super::tests::demo_manifest;

    #[test]
    #[ignore = "regenerates the checked-in Python fixture; run on codegen changes"]
    fn write_demo_fixture() {
        let text = super::render_module(
            "demo",
            "demo-systems",
            "demo_systems",
            &demo_manifest(),
            &super::StubFlavor::Local,
        )
        .unwrap();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/python/tests/data/demo.py");
        std::fs::write(path, text).unwrap();
    }
}
