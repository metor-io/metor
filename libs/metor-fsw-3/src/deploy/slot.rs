//! The slot pass: each `fsw.dl_slot` candidate's library copied into the
//! store, and its path, hash, and ports written into the slot's params.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::FrameStr;
use crate::cli::config::{DeploymentConfig, PackRef};
use crate::coordinator::{SystemConfig, SystemTable};
use crate::def::{DefCx, DefError};
use crate::dl::{Pack, SLOT_TYPE, slot::same_ports, store};
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

fn unresolved_slot(system: &SystemConfig) -> bool {
    system.ty == SLOT_TYPE && candidates(&system.params).any(|c| !resolved(c))
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

/// Whether build has written a candidate's path, hash, and ports.
fn resolved(candidate: &Value) -> bool {
    ["path", "hash", "def"]
        .iter()
        .all(|key| candidate.get(key).is_some_and(|v| !v.is_null()))
        && candidate.get("pack").is_none()
}

fn candidates(params: &Value) -> impl Iterator<Item = &Value> {
    params
        .get("allow")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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
        let mut params = params.clone();
        let allow = params
            .get_mut("allow")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| self.fail("the slot has no `allow` list".into()))?;
        let mut first: Option<(String, SystemDef)> = None;
        for candidate in allow {
            let candidate = candidate
                .as_object_mut()
                .ok_or_else(|| self.fail("a candidate is not an object".into()))?;
            let (name, def) = self.candidate(candidate, store)?;
            match &first {
                Some((first, first_def)) if !same_ports(first_def, &def) => {
                    return Err(DeployError::SlotContract {
                        slot: self.slot.clone(),
                        first: first.clone(),
                        candidate: name,
                    });
                }
                Some(_) => {}
                None => first = Some((name, def)),
            }
        }
        Ok(params)
    }

    /// Stores one candidate's library and writes its path, hash, and ports.
    fn candidate(
        &self,
        candidate: &mut Map<String, Value>,
        store: &Path,
    ) -> Result<(String, SystemDef), DeployError> {
        let (name, ty) = (self.text(candidate, "name")?, self.text(candidate, "ty")?);
        let pack = candidate
            .get("pack")
            .ok_or_else(|| self.fail(format!("`{name}` names no pack")))?;
        let pack = PackRef::deserialize(pack).map_err(|e| self.fail(e.to_string()))?;
        let artifact = pack.artifact();
        self.check_name(&artifact)?;
        let within = |e: String| self.fail(format!("`{name}`: {e}"));
        let opened = pack.open().map_err(|e| within(e.to_string()))?;
        let params = candidate.get("params").unwrap_or(&Value::Null);
        let def = self
            .def(&opened, &ty, params)
            .map_err(|e| within(e.to_string()))?;
        let stored = store::put(store, &artifact).map_err(|e| within(e.to_string()))?;
        candidate.remove("pack");
        candidate.insert("path".into(), json!(stored.path));
        candidate.insert("hash".into(), json!(stored.hash.to_hex().as_str()));
        candidate.insert("def".into(), json!(def));
        Ok((name, def))
    }

    fn def(&self, pack: &Pack, ty: &str, params: &Value) -> Result<SystemDef, DefError> {
        let edges = self.cx.edges();
        let cx = DefCx {
            params,
            ..self.cx.borrow(&edges)
        };
        pack.def(ty, &cx)
    }

    fn text(&self, candidate: &Map<String, Value>, key: &str) -> Result<String, DeployError> {
        candidate
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| self.fail(format!("a candidate has no `{key}`")))
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
