//! The slot pass: each `fsw.dl_slot` candidate's library copied into the
//! store, and its path, hash, and ports written into the slot's params.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::FrameStr;
use crate::cli::config::{DeploymentConfig, PackRef};
use crate::coordinator::{OutputConfig, SystemConfig, SystemTable};
use crate::def::{DefCx, DefError, Records};
use crate::dl::{Pack, SLOT_TYPE, slot::same_ports, store};
use crate::system::{PortDef, SystemDef};

use super::error::DeployError;
use super::index::{At, DefIndex};

/// The longest library file name a channel's status can carry.
const ARTIFACT_CAP: usize = FrameStr::<64>::CAPACITY;

/// Whether any slot in `envelope` has a candidate `metor build` has not resolved.
pub(crate) fn unresolved(envelope: &DeploymentConfig) -> bool {
    envelope
        .targets
        .iter()
        .flat_map(|target| &target.coordinator.systems)
        .any(unresolved_slot)
}

fn unresolved_slot(system: &SystemConfig) -> bool {
    system.ty == SLOT_TYPE && candidates(&system.params).any(|c| !resolved(c))
}

/// Resolves every slot in `envelope`, putting candidate libraries in `store`.
pub fn resolve_slots(
    envelope: &mut DeploymentConfig,
    tables: &[SystemTable],
    store: &Path,
) -> Result<(), DeployError> {
    let mut index = DefIndex::new(envelope, tables);
    let mut resolved = Vec::new();
    for (t, target) in envelope.targets.iter().enumerate() {
        for (s, system) in target.coordinator.systems.iter().enumerate() {
            if !unresolved_slot(system) {
                continue;
            }
            let at = At::new(t, s);
            let cx = SlotCx {
                slot: index.name(at),
                id: &system.id,
                namespace: target.coordinator.namespace.as_deref(),
                edges: index.edges(at)?,
                outputs: &system.outputs,
                records: index.records(t).clone(),
            };
            resolved.push((at, cx.params(&system.params, store)?));
        }
    }
    for (at, params) in resolved {
        envelope.targets[at.target].coordinator.systems[at.system].params = params;
    }
    Ok(())
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
struct SlotCx<'a> {
    slot: String,
    id: &'a str,
    namespace: Option<&'a str>,
    edges: Vec<(String, PortDef)>,
    outputs: &'a [OutputConfig],
    records: Records,
}

impl SlotCx<'_> {
    fn fail(&self, message: String) -> DeployError {
        DeployError::Slot {
            slot: self.slot.clone(),
            message,
        }
    }

    /// The slot's params with every candidate resolved and the store named.
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
        params["store"] = json!(store);
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
        // SAFETY: a pack reference names a library `pack dev` built against
        // this ABI; `open` checks the version before calling anything else.
        let opened = unsafe { Pack::open(&artifact) }.map_err(|e| within(e.to_string()))?;
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
        let inputs: Vec<(&str, &PortDef)> = self
            .edges
            .iter()
            .map(|(port, def)| (port.as_str(), def))
            .collect();
        pack.def(
            ty,
            &DefCx {
                id: self.id,
                namespace: self.namespace,
                inputs: &inputs,
                outputs: self.outputs,
                records: &self.records,
                params,
            },
        )
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
