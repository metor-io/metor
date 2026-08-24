//! Compiling a target's captured Python program at provision time.
//!
//! The build driver's wasm arm: the program the recorder captured into
//! [`Wiring::program`] is compiled by `metor-expr` into the pack artifact its
//! `@system` specs address — at the same seam that builds path-source
//! cdylibs, so the vehicle only ever loads a finished module and a bad
//! program fails the *build* with a `target.py`-line diagnostic, mapped
//! through the program's per-declaration offsets.
//!
//! ## The build-time resolver
//!
//! The compiler's questions are answered from the *other* artifacts' decoded
//! pack manifests: every Table output port of every artifact-backed system
//! (and every slot's occupant contract) is realized field by field into an
//! addressable component, its id and record offset **carried** from the
//! descriptor — `ComponentId::new` masks the FNV top bit, so re-hashing a
//! name agrees with the real id for only about half of names, and the
//! failure mode would look like a component that never publishes. A cdylib's
//! manifest comes from its `.manifest` sidecar when the build wrote one
//! (no dlopen), else an in-process describe; a wasm artifact's through the
//! interpreter. Statically registered systems have no manifest to read, so
//! their outputs are not bindable from Python — a diagnostic, not a silent
//! misbind.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use metor_expr::{CompSchema, ComponentSource, Dtype, FrameSchema, PackResolver, Resolver, Ty};
use metor_proto::types::{ComponentId, PrimType};

use metor_fsw_2_core::abi::PackEntryDesc;
use metor_fsw_2_core::{PortSchema, SystemDescriptor};

use super::build_driver::BuildError;
use super::model::{ProgramSpec, SourceRef, Wiring};

/// Compile the wiring's program into the artifact at `wiring.artifacts[index]`,
/// writing the module and its `.manifest` sidecar into `out_dir` and returning
/// the module's path.
pub(super) fn provision_program(
    wiring: &Wiring,
    index: usize,
    out_dir: &Path,
) -> Result<PathBuf, BuildError> {
    let artifact = &wiring.artifacts[index];
    let compile_err = |detail: String| BuildError::ProgramCompile {
        artifact: artifact.id.clone(),
        detail,
    };
    let program = wiring
        .program
        .as_ref()
        .ok_or_else(|| compile_err("the wiring carries no program".into()))?;
    let resolver = BuildResolver::over(wiring, index)?;
    let compiled =
        metor_expr::compile_pack(&program.source, &resolver, wiring.coordinator.cycle_rate)
            .map_err(|diags| compile_err(render_diagnostics(&diags, program)))?;

    let out = out_dir.join(format!("{}.wasm", artifact.id));
    std::fs::create_dir_all(out_dir).map_err(|source| BuildError::SidecarIo {
        path: out_dir.to_path_buf(),
        source,
    })?;
    super::build_driver::write_atomic(&out, &compiled.wasm).map_err(|source| {
        BuildError::SidecarIo {
            path: out.clone(),
            source,
        }
    })?;
    let sidecar = crate::dl::manifest_sidecar_path(&out);
    super::build_driver::write_atomic(&sidecar, &compiled.pack_manifest).map_err(|source| {
        BuildError::SidecarIo {
            path: sidecar,
            source,
        }
    })?;
    Ok(out)
}

/// Render compile diagnostics with `target.py` locations, mapped through the
/// program's per-declaration offsets.
fn render_diagnostics(diags: &metor_expr::Diagnostics, program: &ProgramSpec) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", locate(program, d.span.start), d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `file:line:col` of a byte offset in the assembled program, in the
/// declaring file's own coordinates.
fn locate(program: &ProgramSpec, offset: u32) -> String {
    let Some(decl) = program.decl_at(offset) else {
        return format!("program byte {offset}");
    };
    let at = (offset as usize).min(program.source.len());
    let body = &program.source[decl.offset as usize..at];
    let line_in_decl = body.matches('\n').count() as u32;
    let col = body.rsplit('\n').next().map_or(0, str::len) as u32 + 1;
    match &decl.src {
        Some(SourceRef {
            file: Some(file),
            line,
            ..
        }) => format!("{file}:{}:{col}", line + line_in_decl),
        _ => format!("`{}` line {}:{col}", decl.name, line_in_decl + 1),
    }
}

/// One addressable component of the (future) resolved graph, carried from a
/// decoded pack manifest.
struct CompInfo {
    source: ComponentSource,
    ty: Ty,
}

/// The compiler's view of the target under construction (see module docs).
struct BuildResolver {
    /// Every addressable component by full instance-prefixed path, in a
    /// `BTreeMap` so nothing about iteration depends on hash order.
    components: BTreeMap<String, CompInfo>,
    /// Frame shapes by frame (port) name, fields in realized order — the
    /// `frame()` hook a `bind=`-to-host-frame class is checked against.
    frames: HashMap<String, Vec<(String, Ty)>>,
}

impl BuildResolver {
    /// Snapshot every Table output of every artifact-backed system and slot
    /// contract in `wiring`, skipping the program artifact itself.
    fn over(wiring: &Wiring, program_index: usize) -> Result<Self, BuildError> {
        let mut manifests = Manifests::default();
        let mut this = BuildResolver {
            components: BTreeMap::new(),
            frames: HashMap::new(),
        };
        for spec in &wiring.systems {
            let Some(artifact_id) = spec.artifact.as_deref() else {
                continue;
            };
            let Some(entries) = manifests.of(wiring, artifact_id, program_index)? else {
                continue;
            };
            let entry = match spec.ty.as_deref() {
                Some(ty) => entries.iter().find(|e| e.descriptor.name == ty),
                None => match entries.as_slice() {
                    [only] => Some(only),
                    _ => None,
                },
            };
            if let Some(entry) = entry {
                this.add_instance(&spec.name, &entry.descriptor);
            }
        }
        for slot in &wiring.slots {
            // The occupant contract is shared across the allowed set, so the
            // first resolvable occupant describes the slot's outputs.
            for occ in &slot.allow {
                let Some(artifact_id) = occ.artifact.as_deref() else {
                    continue;
                };
                let Some(entries) = manifests.of(wiring, artifact_id, program_index)? else {
                    continue;
                };
                if let Some(entry) = entries.iter().find(|e| e.descriptor.name == occ.occupant) {
                    this.add_instance(&slot.name, &entry.descriptor);
                    break;
                }
            }
        }
        Ok(this)
    }

    /// Realize one instance's Table outputs into addressable components,
    /// carrying ids and offsets from the descriptor's vtables.
    fn add_instance(&mut self, name: &str, desc: &SystemDescriptor) {
        for port in &desc.outputs {
            let PortSchema::Table {
                vtable, metadata, ..
            } = &port.schema
            else {
                continue;
            };
            let names: HashMap<ComponentId, &str> = metadata
                .iter()
                .map(|m| (m.component_id, m.name.as_str()))
                .collect();
            let mut fields = Vec::new();
            for field in vtable.realize_fields(None).flatten() {
                // Dynamic-container members have no static offset to read at;
                // they stay host-only.
                if field.element.is_some() || field.container.is_some() {
                    continue;
                }
                let Some(&comp_name) = names.get(&field.component_id) else {
                    continue;
                };
                let Some(ty) = ty_of(field.ty, field.shape) else {
                    continue;
                };
                let leaf = comp_name
                    .strip_prefix(&format!("{}.", port.name))
                    .unwrap_or(comp_name);
                fields.push((leaf.to_string(), ty.clone()));
                self.components.insert(
                    format!("{name}.{comp_name}"),
                    CompInfo {
                        source: ComponentSource {
                            instance: name.to_string(),
                            port_name: port.name.clone(),
                            frame_id: port
                                .id()
                                .component()
                                .expect("a Table port keys on a ComponentId"),
                            max_size: port.max_size,
                            component_id: field.component_id,
                            component_name: comp_name.to_string(),
                            prim: field.ty,
                            shape: field.shape.to_vec(),
                            offset: field.offset,
                        },
                        ty,
                    },
                );
            }
            self.frames.entry(port.name.clone()).or_insert(fields);
        }
    }
}

impl Resolver for BuildResolver {
    fn component(&self, path: &str) -> Option<CompSchema> {
        self.components.get(path).map(|info| CompSchema {
            ty: info.ty.clone(),
        })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        let tail = format!(".{name}");
        self.components
            .keys()
            .filter(|path| path.ends_with(&tail) || *path == name)
            .cloned()
            .collect()
    }

    fn frame(&self, name: &str) -> Option<FrameSchema> {
        self.frames.get(name).map(|fields| FrameSchema {
            name: name.to_string(),
            fields: fields.clone(),
        })
    }
}

impl PackResolver for BuildResolver {
    fn component_source(&self, path: &str) -> Option<ComponentSource> {
        self.components.get(path).map(|info| info.source.clone())
    }
}

/// A component's type in the language: the panel host's mapping, duplicated
/// so a promoted expression sees the identical world. Everything numeric is
/// `f64` (which is also how the runner fills a frame slot), `bool` stays
/// itself, and a shaped bool has no tensor type to be.
fn ty_of(prim: PrimType, shape: &[usize]) -> Option<Ty> {
    match (prim, shape.is_empty()) {
        (PrimType::Bool, true) => Some(Ty::Bool),
        (PrimType::Bool, false) => None,
        (_, true) => Some(Ty::F64),
        (_, false) => Some(Ty::Tensor {
            dtype: Dtype::F64,
            shape: shape.to_vec(),
        }),
    }
}

/// The decoded pack manifests the resolver reads, one per artifact: a
/// cdylib's from its sidecar (else an in-process describe), a wasm module's
/// through the interpreter. `None` for the program artifact itself and for
/// anything unreadable — an unbindable producer, not a build failure.
#[derive(Default)]
struct Manifests {
    decoded: HashMap<String, Option<Vec<PackEntryDesc>>>,
}

impl Manifests {
    fn of(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        program_index: usize,
    ) -> Result<&Option<Vec<PackEntryDesc>>, BuildError> {
        if !self.decoded.contains_key(artifact_id) {
            let entries = wiring
                .artifacts
                .iter()
                .enumerate()
                .find(|(i, a)| a.id == artifact_id && *i != program_index)
                .and_then(|(_, a)| a.path.as_deref().map(|p| (a, p)))
                .and_then(|(a, path)| decode_manifest(a.kind, path));
            self.decoded.insert(artifact_id.to_string(), entries);
        }
        Ok(&self.decoded[artifact_id])
    }
}

fn decode_manifest(kind: crate::ir::ArtifactKind, path: &Path) -> Option<Vec<PackEntryDesc>> {
    let bytes = match kind {
        crate::ir::ArtifactKind::Cdylib => crate::dl::manifest_sidecar_bytes(path)
            .or_else(|| crate::dl::describe_raw(path).ok())?,
        crate::ir::ArtifactKind::Wasm => crate::dl::manifest_sidecar_bytes(path).or_else(|| {
            let module = std::fs::read(path).ok()?;
            crate::wasm::WasmPack::open(&module, crate::coordinator::slot::WASM_SETUP_FUEL)
                .ok()
                .map(|pack| {
                    postcard::to_allocvec(&metor_fsw_2_core::abi::PackManifest {
                        systems: pack.manifest().systems.clone(),
                    })
                    .expect("a decoded manifest re-encodes")
                })
        })?,
    };
    crate::dl::decode_pack_manifest(&bytes).ok()
}
