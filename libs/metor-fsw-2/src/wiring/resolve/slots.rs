//! Resolve allowed occupants and construct slot registrations.

use std::collections::HashMap;

use metor_fsw_2_core::SystemDescriptor;

use super::artifacts::{
    EntrySource, PackCache, WasmCache, WasmModule, find_built_artifact, resolve_occupant,
};
use super::wasm_entry;
use crate::coordinator::init::{InitGraph, Node, SystemBind};
use crate::coordinator::slot::{SlotReg, plan_slot};
use crate::coordinator::{
    AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError, SystemHandle,
};
use crate::ir::ArtifactKind;
use crate::wiring::{
    AllowedOccupantSpec, LoadError, LoadErrorKind, ParamSource, SlotInitState, SlotSpec, Wiring,
};

/// The artifact whose pack exports `occ.occupant`: the `artifact=` the allow
/// line named, or (absent one) the unique artifact exporting an entry of
/// that name. No match or more than one is a clean error either way.
pub(super) fn occupant_artifact(
    wiring: &Wiring,
    packs: &mut PackCache,
    wasm: &mut WasmCache,
    occ: &AllowedOccupantSpec,
    slot: &str,
    max_memory: usize,
) -> Result<String, LoadError> {
    if let Some(artifact) = &occ.artifact {
        return Ok(artifact.clone());
    }
    let mut matches: Vec<String> = Vec::new();
    for artifact in &wiring.artifacts {
        // A wasm artifact is described through the interpreter, never dlopened.
        let exports = if artifact.kind == ArtifactKind::Wasm {
            wasm.open(wiring, &artifact.id, slot, max_memory)?
                .entries
                .iter()
                .any(|e| e.descriptor.name == occ.occupant)
        } else {
            packs
                .open(wiring, &artifact.id, slot)?
                .system_names()
                .any(|n| n == occ.occupant)
        };
        if exports {
            matches.push(artifact.id.clone());
        }
    }
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(LoadErrorKind::OccupantAmbiguous {
            slot: slot.to_string(),
            occupant: occ.occupant.clone(),
            matches,
        }
        .bare()),
    }
}

/// Map a [`SlotConfigError`], from the shared pure-spec validation or from
/// [`plan_slot`](crate::coordinator::slot::plan_slot), onto the resolver's
/// public error variants.
pub(in crate::wiring) fn slot_config_error(err: SlotConfigError, slot: &SlotSpec) -> LoadError {
    let name = slot.name.clone();
    match err {
        SlotConfigError::Empty => LoadErrorKind::EmptySlot { slot: name }.bare(),
        SlotConfigError::UnknownInitial { occupant, allowed } => {
            LoadErrorKind::UnknownInitialOccupant {
                slot: name,
                occupant,
                allowed,
            }
            .bare()
        }
        // A mount-reserved port and a declared capability are
        // occupant-contract defects too: the occupant cannot honor the
        // slot's contract as declared.
        SlotConfigError::OccupantMismatch { occupant, .. }
        | SlotConfigError::ReservedPort { occupant, .. }
        | SlotConfigError::CapabilityOccupant { occupant } => LoadErrorKind::SlotOccupantMismatch {
            slot: name,
            occupant,
        }
        .bare(),
        SlotConfigError::MixedBacking => {
            unreachable!("resolve_slot sources every occupant of a slot from one backing arm")
        }
    }
}

/// Describe one wasm occupant: open the module under the interpreter, find
/// the named entry in its manifest, and encode its params.
///
/// The module is opened only to be *described*; the returned backing carries
/// its path, and the slot loads a fresh instance per `Load`.
pub(super) fn resolve_wasm_occupant(
    art: &crate::ir::Artifact,
    module: &WasmModule,
    occ: &crate::ir::AllowedOccupantSpec,
    slot: &str,
) -> Result<AllowedOccupant, LoadError> {
    let bad = |detail: String| {
        LoadErrorKind::WasmOccupant(
            format!(
                "slot `{slot}`: wasm occupant `{}` from artifact `{}`: {detail}",
                occ.occupant, art.id
            )
            .into_boxed_str(),
        )
        .bare()
    };
    let path = art
        .path
        .as_ref()
        .ok_or_else(|| bad("wasm artifact has no path".into()))?;
    let (_, e) = wasm_entry(&module.entries, Some(&occ.occupant), &bad)?;
    let params = match &occ.params {
        ParamSource::None => e.params_default.clone().unwrap_or_default(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Value(value) => metor_fsw_2_core::params::encode_value_params(
            value,
            &e.params_schema,
            &occ.occupant,
            e.params_default.as_deref(),
        )
        .map_err(|err| bad(err.to_string()))?,
    };
    Ok(AllowedOccupant {
        name: occ.occupant.clone(),
        params,
        descriptor: e.descriptor.clone(),
        backing: OccupantBacking::Wasm {
            path: path.clone(),
            entry_identity: crate::wasm::entry_identity(e),
        },
    })
}

/// Resolve a [`SlotSpec`] into a registered slot: each allowed occupant goes
/// through [`resolve_occupant`] (or, for a `process=#true` slot,
/// [`describe_occupants`]), and the descriptor returned for the edges pass is
/// the one [`plan_slot`](crate::coordinator::slot::plan_slot) derives, not a
/// raw occupant descriptor.
pub(super) fn resolve_slot(
    slot: &SlotSpec,
    wiring: &Wiring,
    packs: &mut PackCache,
    wasm: &mut WasmCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    // Pure-spec checks (non-empty allow set, `initial` inside it) already ran
    // in `validate`. A process slot takes the describe-worker path instead of
    // opening artifacts directly; the backing decides the slot's mode in
    // `plan_slot`.
    let allowed: Vec<AllowedOccupant> = if slot.process {
        describe_occupants(slot, wiring)?
    } else {
        let max_memory = graph.config.wasm_memory_limit_bytes;
        let mut allowed = Vec::with_capacity(slot.allow.len());
        for occ in &slot.allow {
            let artifact_id = occupant_artifact(wiring, packs, wasm, occ, &slot.name, max_memory)?;
            // A wasm artifact is read as bytes and described through the
            // interpreter, not `dlopen`ed: its manifest comes from the module
            // itself, so nothing about it enters this process's address space.
            if let Some(art) = wiring.artifacts.iter().find(|a| a.id == artifact_id)
                && art.kind == crate::ir::ArtifactKind::Wasm
            {
                let module = wasm.open(wiring, &artifact_id, &slot.name, max_memory)?;
                allowed.push(resolve_wasm_occupant(art, module, occ, &slot.name)?);
                continue;
            }
            let pack = packs.open(wiring, &artifact_id, &slot.name)?;
            let source = EntrySource::Opened {
                pack,
                artifact: &artifact_id,
            };
            let (entry, params) =
                resolve_occupant(&source, Some(&occ.occupant), &occ.params, &slot.name, true)?;
            allowed.push(AllowedOccupant::dl(
                occ.occupant.clone(),
                entry.opened(),
                params,
            ));
        }
        allowed
    };

    let initial = slot.initial.as_ref().map(|i| InitialOccupant {
        occupant: i.occupant.clone(),
        start: i.state == SlotInitState::Running,
    });

    // `plan_slot` is the one place the registered contract is derived and the
    // descriptor-level checks run, so this front-end cannot drift from it.
    let (registered, ports, process) =
        plan_slot(&slot.name, &allowed).map_err(|e| slot_config_error(e, slot))?;
    let desc = registered.clone();
    let handle = graph.push_node(Node {
        name: slot.name.clone(),
        desc: registered,
        bind: SystemBind::Slot(SlotReg {
            allowed,
            initial,
            ports,
            process,
        }),
    });

    // Every declared `input`/`output` frame must name an edge-connected
    // registered port. The runner-held tail (slot control, slot status, the
    // sequence channels) is not part of the user contract; the edge outputs
    // do include the implicit status and log tail, which a declaration may
    // name but need not.
    use metor_fsw_2_core::PortConn;
    for (dir, frames, ports) in [
        ("input", &slot.inputs, &desc.inputs),
        ("output", &slot.outputs, &desc.outputs),
    ] {
        for frame in frames {
            if !ports
                .iter()
                .any(|p| p.conn == PortConn::Edge && &p.name == frame)
            {
                return Err(LoadErrorKind::SlotContractMismatch {
                    slot: slot.name.clone(),
                    dir,
                    frame: frame.clone(),
                }
                .bare());
            }
        }
    }

    Ok((handle, desc))
}

/// Resolve a process slot's allowed set: the [`resolve_proc`] recipe once
/// per allowed occupant, so the host never dlopens any occupant artifact.
/// The resulting [`AllowedOccupant`]s carry [`OccupantBacking::Artifact`],
/// which is what makes [`plan_slot`] register the slot process-mode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn describe_occupants(
    slot: &SlotSpec,
    wiring: &Wiring,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    // Manifests by artifact id, so a slot allowing several entries of one
    // pack runs one describe worker for it, not one per occupant.
    let mut manifests: HashMap<String, Vec<metor_fsw_2_core::abi::PackEntryDesc>> = HashMap::new();
    pub(super) fn describe(
        manifests: &mut HashMap<String, Vec<metor_fsw_2_core::abi::PackEntryDesc>>,
        wiring: &Wiring,
        slot: &SlotSpec,
        artifact_id: &str,
    ) -> Result<(), LoadError> {
        if manifests.contains_key(artifact_id) {
            return Ok(());
        }
        let artifact = find_built_artifact(wiring, artifact_id, &slot.name)?;
        let path = artifact
            .path
            .as_ref()
            .expect("checked by find_built_artifact");
        let proc_describe = |detail: String| {
            LoadErrorKind::ProcDescribe {
                system: slot.name.clone(),
                artifact: artifact_id.to_string(),
                detail,
            }
            .bare()
        };
        let bytes = crate::proc::host::describe_via_worker(None, path)
            .map_err(|e| proc_describe(e.to_string()))?;
        let entries =
            crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
        manifests.insert(artifact_id.to_string(), entries);
        Ok(())
    }

    let mut allowed = Vec::with_capacity(slot.allow.len());
    for occ in &slot.allow {
        // The occupant's artifact: named, or the unique artifact whose
        // manifest exports the entry (each described through a worker; the
        // host never dlopens a process slot's artifacts).
        let artifact_id = match &occ.artifact {
            Some(id) => id.clone(),
            None => {
                for artifact in &wiring.artifacts {
                    describe(&mut manifests, wiring, slot, &artifact.id)?;
                }
                let matches: Vec<String> = manifests
                    .iter()
                    .filter(|(_, entries)| {
                        entries.iter().any(|e| e.descriptor.name == occ.occupant)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                match matches.as_slice() {
                    [only] => only.clone(),
                    _ => {
                        return Err(LoadErrorKind::OccupantAmbiguous {
                            slot: slot.name.clone(),
                            occupant: occ.occupant.clone(),
                            matches,
                        }
                        .bare());
                    }
                }
            }
        };
        describe(&mut manifests, wiring, slot, &artifact_id)?;
        let source = EntrySource::Described {
            entries: &manifests[&artifact_id],
            artifact: &artifact_id,
        };
        let (entry, params) =
            resolve_occupant(&source, Some(&occ.occupant), &occ.params, &slot.name, true)?;
        let artifact = find_built_artifact(wiring, &artifact_id, &slot.name)?;
        let path = artifact
            .path
            .as_ref()
            .expect("checked by find_built_artifact");
        allowed.push(AllowedOccupant {
            name: occ.occupant.clone(),
            params,
            descriptor: entry.described(),
            backing: OccupantBacking::Artifact(path.clone()),
        });
    }
    Ok(allowed)
}

/// Without a cross-process futex there is no worker to describe or spawn, so
/// a process slot is rejected like a process system.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn describe_occupants(
    slot: &SlotSpec,
    _wiring: &Wiring,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    Err(LoadErrorKind::ProcessUnsupported {
        name: slot.name.clone(),
    }
    .bare())
}
