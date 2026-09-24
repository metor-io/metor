//! `DlSlot`: one place in the schedule a pack system is loaded into.
//!
//! The slot's ports are its first candidate's, baked into its params by
//! `metor build`. A `Load` hashes the candidate's library, opens it, checks
//! its ports against the slot's, and creates it over the slot's rings.

use std::path::PathBuf;
use std::sync::Arc;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::coordinator::{Adapter, End, LoadError, MakeCx, Occupant, ParamError, Step, TableEntry};
use crate::def::{DefCx, DefError};
use crate::pack::def::Instance;
use crate::system::{PortDef, SystemDef};
use crate::{FrameBytes, FrameStr};

use super::{DlStep, Pack, create, store};

/// The name the slot's definition carries.
const SLOT_NAME: &str = "dl_slot";

#[derive(Deserialize)]
struct SlotParams {
    allow: Vec<Candidate>,
    #[serde(default)]
    initial: Option<String>,
}

/// One system the slot may load, as the config lists it.
#[derive(Deserialize)]
struct Candidate {
    name: String,
    ty: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    def: Option<SystemDef>,
}

/// A candidate `metor build` resolved: where its library is and what it is.
struct Resolved {
    name: String,
    ty: String,
    params: Value,
    path: PathBuf,
    hash: blake3::Hash,
    occupant: Occupant,
}

/// The table entry `fsw.dl_slot` registers as.
pub(crate) fn entry() -> TableEntry {
    TableEntry {
        def: SystemDef {
            name: SLOT_NAME.into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        },
        def_fn: Box::new(def),
        doc: "Loads one of the pack systems it allows on its channel's `Load`.".into(),
        schema: None,
        make: Arc::new(make),
    }
}

/// The slot's ports: its first candidate's, as `metor build` baked them.
fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let refused = |message: String| DefError::Slot { message };
    let params = SlotParams::deserialize(cx.params).map_err(|e| refused(e.to_string()))?;
    let first = params
        .allow
        .first()
        .ok_or_else(|| refused("the slot allows nothing".into()))?;
    let def = first.def.clone().ok_or_else(|| {
        refused(format!(
            "`{}` is not resolved; run `metor build`",
            first.name
        ))
    })?;
    Ok(SystemDef {
        name: SLOT_NAME.into(),
        ..def
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
        slot.end = Some(End::Failed(error.to_string().into()));
    }
    Ok(Box::new(slot))
}

impl Resolved {
    fn of(index: usize, candidate: Candidate) -> Result<Self, ParamError> {
        let unresolved = || {
            ParamError::Decode(format!(
                "`{}` is not resolved; run `metor build`",
                candidate.name
            ))
        };
        let path = candidate.path.clone().ok_or_else(unresolved)?;
        let hash = candidate.hash.as_deref().ok_or_else(unresolved)?;
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
        })
    }
}

/// A `DlSlot` holds at most one pack system, over rings it keeps.
pub struct DlSlot {
    /// The id, ports, and channel every occupant is created with.
    instance: Instance,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
    candidates: Vec<Resolved>,
    current: Option<Loaded>,
    /// Why the slot is empty when a load left it so.
    end: Option<End>,
}

/// The occupant and the pack it was created from.
struct Loaded {
    at: usize,
    pack: Pack,
    step: DlStep,
}

impl DlSlot {
    /// Opens a candidate's library once its bytes and ports check out.
    ///
    /// A resident library is the bytes checked when it was first opened, so
    /// only a first open hashes the file, and re-checks that the file it
    /// hashed is the one it opened.
    fn open(&self, candidate: &Resolved) -> Result<Pack, LoadError> {
        let open_error = |e: super::PackError| LoadError::Open(e.to_string());
        let io_error = |e: std::io::Error| LoadError::Open(e.to_string());
        if super::is_loaded(&candidate.path) {
            // SAFETY: the library is already mapped in this process.
            return unsafe { Pack::open(&candidate.path) }.map_err(open_error);
        }
        let hashed = store::Hashed::of(&candidate.path).map_err(io_error)?;
        if hashed.hash != candidate.hash {
            return Err(LoadError::Hash(candidate.name.clone()));
        }
        // SAFETY: the library is in the store `metor build` wrote, and its
        // bytes are the ones build hashed; `open` checks the ABI first.
        let pack = unsafe { Pack::open(&candidate.path) }.map_err(open_error)?;
        if !hashed.still_at(&candidate.path).map_err(io_error)? {
            return Err(LoadError::Hash(candidate.name.clone()));
        }
        Ok(pack)
    }

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

    fn create(&self, pack: &Pack, candidate: &Resolved) -> Result<DlStep, LoadError> {
        let inputs: Vec<Vec<&RingBuffer>> = self
            .inputs
            .iter()
            .map(|rings| rings.iter().collect())
            .collect();
        let outputs: Vec<&RingBuffer> = self.outputs.iter().collect();
        create(
            &pack.lib,
            pack.fns,
            &candidate.ty,
            &candidate.params,
            &self.instance,
            &inputs,
            &outputs,
        )
        .map_err(LoadError::Create)
    }

    /// Creates candidate `at` over the slot's rings, which must be free.
    fn place(&mut self, at: usize, pack: Pack) -> Result<(), LoadError> {
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

    /// Nothing: the pack has already written the fault line.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&End> {
        match &self.current {
            Some(loaded) => loaded.step.ended(),
            None => self.end.as_ref().or(Some(&End::Unloaded)),
        }
    }

    /// Keeps the occupant when the new one's library or ports are refused,
    /// and puts it back when the new one's constructor refuses.
    fn load(&mut self, name: &str) -> Result<(), LoadError> {
        let at = self
            .candidates
            .iter()
            .position(|candidate| candidate.name == name)
            .ok_or_else(|| LoadError::Unknown(name.to_string()))?;
        let pack = self.open(&self.candidates[at])?;
        self.check_ports(&pack, &self.candidates[at])?;
        // The old occupant releases its views and writers before the new one claims them.
        let previous = self.current.take().map(|loaded| (loaded.at, loaded.pack));
        let placed = self.place(at, pack);
        if let (Err(_), Some((at, pack))) = (&placed, previous) {
            // Its library is resident and its rings are free again.
            let _ = self.place(at, pack);
        }
        placed
    }

    fn occupant(&self) -> Option<&Occupant> {
        let loaded = self.current.as_ref()?;
        Some(&self.candidates[loaded.at].occupant)
    }
}

/// Whether two definitions have the same ports in the same order, carrying
/// the same records, wherever their names are rooted.
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
