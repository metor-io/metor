//! The slot pass: each `fsw.dl_slot` candidate's library copied into the
//! store, and its path, hash, and ports written into the slot's params.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::DeploymentConfig;
use crate::FrameStr;
use crate::coordinator::{SystemConfig, SystemTable};
use crate::def::{DefCx, DefError};
use crate::dl::slot::{Candidate, SlotParams, Source, Stored, same_ports};
use crate::dl::{Pack, PackRef, SLOT_TYPE, store};
use crate::pack::def::DefCxOwned;
use crate::system::SystemDef;

use super::error::DeployError;
use super::index::{At, DefIndex};

/// The longest library file name a channel's status can carry.
const ARTIFACT_CAP: usize = FrameStr::<64>::CAPACITY;

/// Whether any slot in `envelope` has a candidate `metor build` has not resolved.
pub(crate) fn unresolved(envelope: &DeploymentConfig) -> bool {
    first_unresolved(envelope).is_some()
}

/// Whether `system` is a slot with a candidate from a pack, or one whose
/// params do not decode, which `resolve_slots` then reports.
fn unresolved_slot(system: &SystemConfig) -> bool {
    if system.ty != SLOT_TYPE {
        return false;
    }
    match SlotParams::deserialize(&system.params) {
        Ok(params) => params
            .allow
            .iter()
            .any(|candidate| matches!(candidate.source, Source::Pack(_))),
        Err(_) => true,
    }
}

/// The first slot in step order with a candidate still to resolve.
fn first_unresolved(envelope: &DeploymentConfig) -> Option<At> {
    envelope.targets.iter().enumerate().find_map(|(t, target)| {
        let systems = &target.coordinator.systems;
        let s = systems.iter().position(unresolved_slot)?;
        Some(At::new(t, s))
    })
}

/// Resolves every slot in `envelope`, putting candidate libraries in `store`.
///
/// Slots resolve in step order, one at a time, so a slot reading another
/// slot's output sees that slot's baked ports.
pub fn resolve_slots(
    envelope: &mut DeploymentConfig,
    tables: &[SystemTable],
    store: &Path,
) -> Result<(), DeployError> {
    while let Some(at) = first_unresolved(envelope) {
        let params = resolve_slot(envelope, tables, at, store)?;
        at.get_mut(envelope).params = params;
    }
    Ok(())
}

fn resolve_slot(
    envelope: &DeploymentConfig,
    tables: &[SystemTable],
    at: At,
    store: &Path,
) -> Result<Value, DeployError> {
    let mut index = DefIndex::new(envelope, tables);
    let system = at.get(envelope);
    let cx = SlotCx {
        slot: index.name(at),
        cx: DefCxOwned {
            id: system.id.clone(),
            namespace: envelope.targets[at.target].coordinator.namespace.clone(),
            inputs: index.edges(at)?,
            outputs: system.outputs.clone(),
            records: index.records(at.target).clone(),
            params: Value::Null,
        },
    };
    cx.params(&system.params, store)
}

/// What a slot's candidates compute their ports under: the slot's own config.
struct SlotCx {
    slot: String,
    cx: DefCxOwned,
}

impl SlotCx {
    fn fail(&self, message: String) -> DeployError {
        DeployError::Slot {
            slot: self.slot.clone(),
            message,
        }
    }

    /// The slot's params with every candidate resolved.
    fn params(&self, params: &Value, store: &Path) -> Result<Value, DeployError> {
        let mut params = SlotParams::deserialize(params).map_err(|e| self.fail(e.to_string()))?;
        let mut first: Option<(String, SystemDef)> = None;
        for candidate in &mut params.allow {
            let def = self.resolve(candidate, store)?;
            match &first {
                Some((first, first_def)) if !same_ports(first_def, &def) => {
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

    /// Stores a pack candidate's library and makes it the candidate's source.
    /// Returns the candidate's ports.
    fn resolve(&self, candidate: &mut Candidate, store: &Path) -> Result<SystemDef, DeployError> {
        let stored = match &candidate.source {
            Source::Stored(stored) => return Ok(stored.def.clone()),
            Source::Pack(pack) => self.store(candidate, pack, store)?,
        };
        let def = stored.def.clone();
        candidate.source = Source::Stored(stored);
        Ok(def)
    }

    /// Copies a candidate's library or module into the store, with its ports
    /// and the pack's kind and limits.
    fn store(
        &self,
        candidate: &Candidate,
        pack: &PackRef,
        store: &Path,
    ) -> Result<Stored, DeployError> {
        let artifact = pack.artifact();
        self.check_name(&artifact)?;
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
            limits: pack.limits,
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

    /// Refuses a library whose file name the channel's status cannot carry.
    fn check_name(&self, artifact: &Path) -> Result<(), DeployError> {
        let name = artifact
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.len() > ARTIFACT_CAP {
            return Err(DeployError::ArtifactName {
                slot: self.slot.clone(),
                name,
                cap: ARTIFACT_CAP,
            });
        }
        Ok(())
    }
}
