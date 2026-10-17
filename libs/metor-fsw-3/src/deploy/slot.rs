//! Resolves the candidates of every `fsw.dl_slot` in a deployment.
//!
//! A slot lists candidate systems from packs. For each candidate, `metor
//! build` copies the pack's library into the store and writes its path, hash,
//! and ports into the slot's params, so the target can load it without the
//! pack at run time.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{DeploySystem, DeploymentConfig};
use crate::FrameStr;
use crate::coordinator::SystemTable;
use crate::def::{DefCx, DefError};
use crate::dl::slot::{Candidate, SlotParams, Stored};
use crate::dl::{Pack, PackRef, store};
use crate::pack::def::DefCxOwned;
use crate::system::SystemDef;

use super::error::DeployError;
use super::index::{DefIndex, SystemIndex};

/// The size of the frame string a channel's status carries a library's file
/// name in.
const ARTIFACT_LEN: usize = 64;

/// A slot's params as a deployment writes them.
#[derive(Deserialize)]
pub(crate) struct DeploySlot {
    allow: Vec<DeployCandidate>,
    #[serde(default)]
    initial: Option<String>,
}

/// A candidate as a deployment writes it.
#[derive(Deserialize)]
struct DeployCandidate {
    name: String,
    ty: String,
    #[serde(default)]
    params: Value,
    #[serde(flatten)]
    library: Library,
}

/// Where a candidate's library is: in a pack `metor build` has not opened
/// yet, or already in the store.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Library {
    Pack(PackRef),
    Stored(Stored),
}

impl DeploySlot {
    /// Decodes the params of the slot `system`, at `index`.
    pub(crate) fn read(
        envelope: &DeploymentConfig,
        index: SystemIndex,
        system: &DeploySystem,
    ) -> Result<Self, DeployError> {
        Self::deserialize(&system.params).map_err(|e| {
            let namespace = envelope.targets[index.target].namespace();
            let slot = format!("{namespace}.{}", system.id);
            let message = e.to_string();
            DeployError::Slot { slot, message }.at(system.location.as_ref())
        })
    }

    /// Whether a candidate still points at a pack.
    pub(crate) fn pending(&self) -> bool {
        let mut allow = self.allow.iter();
        allow.any(|candidate| matches!(candidate.library, Library::Pack(_)))
    }
}

/// Resolves the pending `slots` of `envelope`, copying candidate libraries
/// into `store`.
///
/// Slots are resolved one at a time, in step order. That way, when a slot
/// reads another slot's output, the other slot's ports are already known.
pub(crate) fn resolve_slots(
    envelope: &mut DeploymentConfig,
    tables: &[SystemTable],
    store: &Path,
    slots: Vec<(SystemIndex, DeploySlot)>,
) -> Result<(), DeployError> {
    for (index, params) in slots {
        let resolved = SlotCx::new(envelope, tables, index)
            .and_then(|slot| slot.params(params, store))
            .map_err(|e| e.at(index.get(envelope).location.as_ref()))?;
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
                outputs: defs.runtime(index)?.outputs,
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

    /// Returns the slot's runtime params, with every candidate resolved.
    ///
    /// Every candidate must have the same ports as the first one.
    fn params(&self, params: DeploySlot, store: &Path) -> Result<Value, DeployError> {
        let allow = params
            .allow
            .into_iter()
            .map(|candidate| self.resolve(candidate, store))
            .collect::<Result<Vec<_>, _>>()?;
        if let [first, rest @ ..] = allow.as_slice()
            && let Some(other) = rest
                .iter()
                .find(|other| !first.stored.def.same_ports(&other.stored.def))
        {
            return Err(DeployError::SlotContract {
                slot: self.slot.clone(),
                first: first.name.clone(),
                candidate: other.name.clone(),
            });
        }
        let params = SlotParams {
            allow,
            initial: params.initial,
        };
        serde_json::to_value(&params).map_err(|e| self.fail(e.to_string()))
    }

    /// Returns the runtime form of `candidate`, copying a pack candidate's
    /// library into the store.
    fn resolve(&self, candidate: DeployCandidate, store: &Path) -> Result<Candidate, DeployError> {
        let DeployCandidate {
            name,
            ty,
            params,
            library,
        } = candidate;
        let stored = match library {
            Library::Stored(stored) => stored,
            Library::Pack(pack) => self.store(&name, &ty, &params, &pack, store)?,
        };
        Ok(Candidate {
            name,
            ty,
            params,
            stored,
        })
    }

    /// Copies a candidate's library or module into the store.
    ///
    /// Returns the stored entry, which records the candidate's ports and the
    /// pack's kind.
    fn store(
        &self,
        candidate: &str,
        ty: &str,
        params: &Value,
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
        let within = |e: String| self.fail(format!("`{candidate}`: {e}"));
        let opened = pack.open().map_err(|e| within(e.to_string()))?;
        let def = self
            .def(&opened, ty, params)
            .map_err(|e| within(e.to_string()))?;
        let put = store::put(store, &artifact).map_err(|e| within(e.to_string()))?;
        Ok(Stored {
            path: put.path,
            hash: put.hash,
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
