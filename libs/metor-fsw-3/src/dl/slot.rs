//! [`DlSlot`] lets a target swap pack systems while it runs.
//!
//! A slot is a place in the schedule with a fixed set of ports and a list of
//! candidate systems. It always runs behind a channel, and a `Load` command
//! on that channel picks which candidate runs.
//!
//! `metor build` resolves each candidate ahead of time. It copies the library
//! or module into the store and writes its path, hash, ports, pack kind, and
//! limits into the slot's params. The slot takes its ports from the first
//! candidate. On a `Load`, the slot hashes the candidate's file, opens it,
//! checks that its ports match the slot's, and creates it on the slot's rings.

use std::path::PathBuf;
use std::sync::Arc;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coordinator::{
    Adapter, EndReason, LoadError, MakeCx, Occupant, ParamError, Step, SystemKind, TableEntry,
};
use crate::def::{DefCx, DefError};
use crate::dl::{PackKind, PackRef};
use crate::pack::def::Instance;
use crate::system::{PortDef, SystemDef};
use crate::wasm::{Limits, WasmPack};
use crate::{FrameBytes, FrameStr};

use super::{DlPack, Pack, PackError, is_loaded, store};

/// The name in the slot's system definition.
const SLOT_NAME: &str = "dl_slot";

/// A slot's params.
#[derive(Serialize, Deserialize)]
pub(crate) struct SlotParams {
    pub(crate) allow: Vec<Candidate>,
    #[serde(default)]
    pub(crate) initial: Option<String>,
}

/// One system the slot is allowed to load.
#[derive(Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub(crate) name: String,
    pub(crate) ty: String,
    #[serde(default)]
    pub(crate) params: Value,
    pub(crate) source: Source,
}

/// Where a candidate's library comes from.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// A pack that `metor build` has not opened and stored yet.
    Pack(PackRef),
    /// The copy that `metor build` wrote into the store.
    Stored(Stored),
}

/// A candidate that `metor build` has copied into the store.
#[derive(Serialize, Deserialize)]
pub(crate) struct Stored {
    pub(crate) path: PathBuf,
    pub(crate) hash: String,
    pub(crate) def: SystemDef,
    #[serde(default)]
    pub(crate) kind: PackKind,
    #[serde(flatten)]
    pub(crate) limits: Limits,
}

/// A candidate after the slot has decoded it, ready to load.
struct Resolved {
    name: String,
    ty: String,
    params: Value,
    path: PathBuf,
    hash: blake3::Hash,
    kind: PackKind,
    limits: Limits,
    occupant: Occupant,
}

/// Returns the table entry for the `fsw.dl_slot` system type.
pub(crate) fn entry() -> TableEntry {
    TableEntry {
        kind: SystemKind::Cyclic,
        def: SystemDef {
            name: SLOT_NAME.into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            grants: Vec::new(),
        },
        def_fn: Box::new(def),
        doc: "Loads one of the pack systems it allows on its channel's `Load`.".into(),
        schema: None,
        make: Arc::new(make),
    }
}

/// Returns the slot's definition, using the first candidate's ports.
///
/// Fails if any candidate has not been resolved by `metor build`.
fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let refused = |message: String| DefError::Slot { message };
    let params = SlotParams::deserialize(cx.params).map_err(|e| refused(e.to_string()))?;
    let mut stored = params
        .allow
        .iter()
        .map(|candidate| match &candidate.source {
            Source::Stored(stored) => Ok(stored),
            Source::Pack(_) => Err(refused(not_resolved(&candidate.name))),
        });
    let first = stored
        .next()
        .ok_or_else(|| refused("the slot allows nothing".into()))??;
    stored.try_for_each(|candidate| candidate.map(drop))?;
    Ok(SystemDef {
        name: SLOT_NAME.into(),
        ..first.def.clone()
    })
}

fn make(cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    let Some(adapter @ Adapter::Channel(_)) = cx.adapter else {
        return Err(ParamError::SlotChannel);
    };
    let params: SlotParams = cx.params.decode()?;
    let candidates = params
        .allow
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| Resolved::of(index, candidate))
        .collect::<Result<_, _>>()?;
    let mut slot = DlSlot {
        instance: Instance {
            id: cx.id.to_string(),
            def: cx.def.clone(),
            adapter: Some(adapter.clone()),
        },
        inputs: cx
            .inputs
            .into_iter()
            .map(|rings| rings.into_iter().cloned().collect())
            .collect(),
        outputs: cx.outputs.into_iter().cloned().collect(),
        candidates,
        current: None,
        end: None,
    };
    if let Some(name) = params.initial
        && let Err(error) = slot.load(&name)
    {
        slot.end = Some(EndReason::Failed(error.to_string().into()));
    }
    Ok(Box::new(slot))
}

impl Resolved {
    fn of(index: usize, candidate: Candidate) -> Result<Self, ParamError> {
        let Source::Stored(Stored {
            path,
            hash,
            kind,
            limits,
            ..
        }) = candidate.source
        else {
            return Err(ParamError::Decode(not_resolved(&candidate.name)));
        };
        let hash = blake3::Hash::from_hex(hash).map_err(|e| ParamError::Decode(e.to_string()))?;
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ParamError::Decode(format!("`{}` has no file name", path.display())))?;
        let artifact = FrameStr::new(file).map_err(|e| ParamError::Decode(e.to_string()))?;
        Ok(Self {
            occupant: Occupant {
                // PANIC Safety: a config lists far fewer than `u32::MAX` candidates.
                index: u32::try_from(index).expect("a small allow list"),
                artifact,
                hash: FrameBytes(*hash.as_bytes()),
            },
            name: candidate.name,
            ty: candidate.ty,
            params: candidate.params,
            path,
            hash,
            kind,
            limits,
        })
    }
}

fn not_resolved(name: &str) -> String {
    format!("`{name}` is not resolved; run `metor build`")
}

/// Runs one pack system at a time, chosen from a fixed list.
///
/// A `Load` command on the slot's channel swaps in another candidate. The new
/// system reads and writes the same rings as the old one, so the rest of the
/// schedule is unaffected by the swap.
pub struct DlSlot {
    /// The id, ports, and channel adapter every loaded system is created with.
    instance: Instance,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
    candidates: Vec<Resolved>,
    current: Option<Loaded>,
    /// Why the slot is empty, if a failed load emptied it.
    end: Option<EndReason>,
}

/// The loaded system and the pack it was created from.
struct Loaded {
    at: usize,
    pack: Arc<Pack>,
    step: Box<dyn Step>,
}

impl DlSlot {
    fn check_ports(&self, pack: &Pack, candidate: &Resolved) -> Result<(), LoadError> {
        let cx = DefCx {
            id: &self.instance.id,
            params: &candidate.params,
            ..DefCx::empty()
        };
        let def = pack
            .def(&candidate.ty, &cx)
            .map_err(|e| LoadError::Mismatch(format!("{}: {e}", candidate.name)))?;
        if !same_ports(&def, &self.instance.def) {
            return Err(LoadError::Mismatch(candidate.name.clone()));
        }
        Ok(())
    }

    fn create(&self, pack: &Pack, candidate: &Resolved) -> Result<Box<dyn Step>, LoadError> {
        let inputs: Vec<Vec<&RingBuffer>> = self
            .inputs
            .iter()
            .map(|rings| rings.iter().collect())
            .collect();
        let outputs: Vec<&RingBuffer> = self.outputs.iter().collect();
        pack.create(
            &candidate.ty,
            &candidate.params,
            &self.instance,
            &inputs,
            &outputs,
        )
        .map_err(LoadError::Create)
    }

    /// Creates candidate `at` on the slot's rings.
    ///
    /// No other system may be loaded when this is called.
    fn place(&mut self, at: usize, pack: Arc<Pack>) -> Result<(), LoadError> {
        let step = self.create(&pack, &self.candidates[at])?;
        self.current = Some(Loaded { at, pack, step });
        self.end = None;
        Ok(())
    }
}

impl Step for DlSlot {
    fn execute(&mut self, now: Timestamp) {
        if let Some(loaded) = self.current.as_mut() {
            loaded.step.execute(now);
        }
    }

    /// Does nothing, because the pack has already logged the fault.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        match &self.current {
            Some(loaded) => loaded.step.ended(),
            None => self.end.as_ref().or(Some(&EndReason::Unloaded)),
        }
    }

    /// If the new system's library or ports are rejected, the current system
    /// keeps running. If the new system's constructor fails, the previous
    /// system is created again.
    fn load(&mut self, name: &str) -> Result<(), LoadError> {
        let at = self
            .candidates
            .iter()
            .position(|candidate| candidate.name == name)
            .ok_or_else(|| LoadError::Unknown(name.to_string()))?;
        let pack = Arc::new(open(&self.candidates[at])?);
        self.check_ports(&pack, &self.candidates[at])?;
        // Drop the old system first so its ring readers and writers are free
        // for the new one.
        let previous = self.current.take().map(|loaded| (loaded.at, loaded.pack));
        let placed = self.place(at, pack);
        if let (Err(_), Some((at, pack))) = (&placed, previous) {
            // The old library is still loaded and the rings are free again.
            let _ = self.place(at, pack);
        }
        placed
    }

    fn occupant(&self) -> Option<&Occupant> {
        let loaded = self.current.as_ref()?;
        Some(&self.candidates[loaded.at].occupant)
    }
}

/// Opens a candidate's library or module after checking its hash.
fn open(candidate: &Resolved) -> Result<Pack, LoadError> {
    match candidate.kind {
        PackKind::Dylib => open_dylib(candidate),
        PackKind::Wasm => open_wasm(candidate),
    }
}

/// Opens a candidate's library.
///
/// A library that is already loaded was checked when it was first opened, so
/// it is not hashed again. On a first open, the file is hashed, opened, and
/// then checked to make sure the opened file is the one that was hashed.
fn open_dylib(candidate: &Resolved) -> Result<Pack, LoadError> {
    let io_error = |e: std::io::Error| LoadError::Open(e.to_string());
    if is_loaded(&candidate.path) {
        // SAFETY: the library is already mapped in this process.
        let pack = unsafe { DlPack::open(&candidate.path) }.map_err(open_error)?;
        return Ok(Pack::Dl(pack));
    }
    let hashed = store::Hashed::of(&candidate.path).map_err(io_error)?;
    if hashed.hash != candidate.hash {
        return Err(LoadError::Hash(candidate.name.clone()));
    }
    // SAFETY: the library is in the store that `metor build` wrote, and its
    // bytes match the hash build recorded. `open` checks the ABI first.
    let pack = unsafe { DlPack::open(&candidate.path) }.map_err(open_error)?;
    if !hashed.still_at(&candidate.path).map_err(io_error)? {
        return Err(LoadError::Hash(candidate.name.clone()));
    }
    Ok(Pack::Dl(pack))
}

/// Reads a candidate's module, checks its hash, and compiles those bytes.
fn open_wasm(candidate: &Resolved) -> Result<Pack, LoadError> {
    let bytes = std::fs::read(&candidate.path).map_err(|e| LoadError::Open(e.to_string()))?;
    if blake3::hash(&bytes) != candidate.hash {
        return Err(LoadError::Hash(candidate.name.clone()));
    }
    let pack = WasmPack::open(&bytes, &candidate.limits).map_err(open_error)?;
    Ok(Pack::Wasm(pack))
}

fn open_error(error: PackError) -> LoadError {
    LoadError::Open(error.to_string())
}

/// Returns whether two definitions have the same ports in the same order.
///
/// Ports match when their names and records match. The namespace each port is
/// announced under is ignored.
pub(crate) fn same_ports(a: &SystemDef, b: &SystemDef) -> bool {
    same_list(&a.inputs, &b.inputs) && same_list(&a.outputs, &b.outputs)
}

fn same_list(a: &[PortDef], b: &[PortDef]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.name == b.name && a.same_record(b))
}

#[cfg(test)]
mod tests;
