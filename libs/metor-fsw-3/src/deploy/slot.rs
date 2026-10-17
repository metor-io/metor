//! Resolves the candidates of every `fsw.dl_slot` in a deployment.
//!
//! A slot lists candidate systems from packs. For each candidate, `metor
//! build` copies the pack's library into the store and writes its path, hash,
//! and ports into the slot's params, so the target can load it without the
//! pack at run time.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::DeploymentConfig;
use crate::FrameStr;
use crate::coordinator::SystemTable;
use crate::def::{DefCx, DefError};
use crate::dl::slot::{Candidate, SlotParams, Source, Stored};
use crate::dl::{Pack, PackRef, SLOT_TYPE, store};
use crate::pack::def::DefCxOwned;
use crate::system::SystemDef;

use super::error::DeployError;
use super::index::{DefIndex, SystemIndex};

/// The size of the frame string a channel's status carries a library's file
/// name in.
const ARTIFACT_LEN: usize = 64;

/// Returns true if any slot in `envelope` has a candidate that is not yet
/// resolved.
///
/// # Errors
/// Returns an error if a slot's params do not decode.
pub(crate) fn unresolved(envelope: &DeploymentConfig) -> Result<bool, DeployError> {
    Ok(!unresolved_slots(envelope)?.is_empty())
}

/// Every slot, in step order, that still has a candidate that points at a
/// pack, with its decoded params.
fn unresolved_slots(
    envelope: &DeploymentConfig,
) -> Result<Vec<(SystemIndex, SlotParams)>, DeployError> {
    let mut slots = Vec::new();
    for (index, system) in SystemIndex::iter(envelope) {
        if system.ty != SLOT_TYPE {
            continue;
        }
        let params = SlotParams::deserialize(&system.params).map_err(|e| DeployError::Slot {
            slot: format!(
                "{}.{}",
                envelope.targets[index.target].namespace(),
                system.id
            ),
            message: e.to_string(),
        })?;
        let pending = params
            .allow
            .iter()
            .any(|candidate| matches!(candidate.source, Source::Pack(_)));
        if pending {
            slots.push((index, params));
        }
    }
    Ok(slots)
}

/// Resolves every slot in `envelope`, copying candidate libraries into
/// `store`.
///
/// Slots are resolved one at a time, in step order. That way, when a slot
/// reads another slot's output, the other slot's ports are already known.
pub fn resolve_slots(
    envelope: &mut DeploymentConfig,
    tables: &[SystemTable],
    store: &Path,
) -> Result<(), DeployError> {
    for (index, params) in unresolved_slots(envelope)? {
        let resolved = SlotCx::new(envelope, tables, index)?.params(params, store)?;
        index.get_mut(envelope).params = resolved;
    }
    Ok(())
}

/// The context a slot's candidates compute their ports in. It comes from the
/// slot's own config, so every candidate sees the same edges.
struct SlotCx {
    slot: String,
    cx: DefCxOwned,
}

impl SlotCx {
    /// The context for the slot at `index`, computed from its own config.
    ///
    /// Every slot before it in step order is already resolved, so a fresh
    /// `DefIndex` sees their stored ports.
    fn new(
        envelope: &DeploymentConfig,
        tables: &[SystemTable],
        index: SystemIndex,
    ) -> Result<Self, DeployError> {
        let mut defs = DefIndex::new(envelope, tables);
        let system = index.get(envelope);
        Ok(Self {
            slot: defs.name(index),
            cx: DefCxOwned {
                id: system.id.clone(),
                namespace: envelope.targets[index.target].coordinator.namespace.clone(),
                inputs: defs.edges(index)?,
                outputs: system.outputs.clone(),
                records: defs.records(index.target).clone(),
                params: Value::Null,
            },
        })
    }

    fn fail(&self, message: String) -> DeployError {
        DeployError::Slot {
            slot: self.slot.clone(),
            message,
        }
    }

    /// Returns the slot's params with every candidate resolved.
    ///
    /// Every candidate must have the same ports as the first one.
    fn params(&self, mut params: SlotParams, store: &Path) -> Result<Value, DeployError> {
        let mut first: Option<(String, SystemDef)> = None;
        for candidate in &mut params.allow {
            let def = self.resolve(candidate, store)?;
            match &first {
                Some((first, first_def)) if !first_def.same_ports(&def) => {
                    return Err(DeployError::SlotContract {
                        slot: self.slot.clone(),
                        first: first.clone(),
                        candidate: candidate.name.clone(),
                    });
                }
                Some(_) => {}
                None => first = Some((candidate.name.clone(), def)),
            }
        }
        serde_json::to_value(&params).map_err(|e| self.fail(e.to_string()))
    }

    /// Copies a pack candidate's library into the store and points the
    /// candidate at the stored copy. Returns the candidate's ports.
    fn resolve(&self, candidate: &mut Candidate, store: &Path) -> Result<SystemDef, DeployError> {
        let stored = match &candidate.source {
            Source::Stored(stored) => return Ok(stored.def.clone()),
            Source::Pack(pack) => self.store(candidate, pack, store)?,
        };
        let def = stored.def.clone();
        candidate.source = Source::Stored(stored);
        Ok(def)
    }

    /// Copies a candidate's library into the store.
    ///
    /// Returns the stored entry, which records the candidate's ports and the
    /// pack's kind.
    fn store(
        &self,
        candidate: &Candidate,
        pack: &PackRef,
        store: &Path,
    ) -> Result<Stored, DeployError> {
        let artifact = pack.artifact();
        let name = artifact
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| self.fail(format!("`{}` names no file", artifact.display())))?;
        FrameStr::<ARTIFACT_LEN>::new(name).map_err(|_| DeployError::ArtifactName {
            slot: self.slot.clone(),
            name: name.to_string(),
            cap: FrameStr::<ARTIFACT_LEN>::CAPACITY,
        })?;
        let within = |e: String| self.fail(format!("`{}`: {e}", candidate.name));
        let opened = pack.open().map_err(|e| within(e.to_string()))?;
        let def = self
            .def(&opened, &candidate.ty, &candidate.params)
            .map_err(|e| within(e.to_string()))?;
        let put = store::put(store, &artifact).map_err(|e| within(e.to_string()))?;
        Ok(Stored {
            path: put.path,
            hash: put.hash.to_hex().to_string(),
            def,
            kind: pack.kind,
        })
    }

    fn def(&self, pack: &Pack, ty: &str, params: &Value) -> Result<SystemDef, DefError> {
        let edges = self.cx.edges();
        let cx = DefCx {
            params,
            ..self.cx.borrow(&edges)
        };
        pack.def(ty, &cx)
    }
}
