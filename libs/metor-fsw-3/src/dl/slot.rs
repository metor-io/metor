//! [`DlSlot`] lets a target swap pack systems while it runs.
//!
//! A slot is a place in the schedule with a fixed set of ports and a list of
//! candidate systems. It always runs behind a channel, and a `Load` command
//! on that channel picks which candidate runs.
//!
//! `metor build` resolves each candidate ahead of time. It copies the library
//! into the store and writes its path, hash, ports, and pack kind into the
//! slot's params. The slot takes its ports from the first
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
use crate::dl::PackKind;
use crate::io::SharedIo;
use crate::pack::def::Instance;
use crate::system::{PortDef, SystemDef};
use crate::{FrameBytes, FrameStr};

use super::{DlPack, InstanceRings, Pack, is_loaded, store};

/// The name in the slot's system definition.
const SLOT_NAME: &str = "dl_slot";

/// A slot's params.
#[derive(Serialize, Deserialize)]
pub(crate) struct SlotParams {
    pub(crate) allow: Vec<Candidate>,
    #[serde(default)]
    pub(crate) initial: Option<String>,
}

/// One system the slot is allowed to load, from the library `metor build`
/// stored.
#[derive(Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub(crate) name: String,
    pub(crate) ty: String,
    #[serde(default)]
    pub(crate) params: Value,
    pub(crate) stored: Stored,
}

/// A candidate's library, as `metor build` copied it into the store.
#[derive(Serialize, Deserialize)]
pub(crate) struct Stored {
    pub(crate) path: PathBuf,
    #[serde(with = "hex_hash")]
    pub(crate) hash: blake3::Hash,
    pub(crate) def: SystemDef,
    #[serde(default)]
    pub(crate) kind: PackKind,
}

/// A library hash, written as hex.
mod hex_hash {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(hash: &blake3::Hash, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(hash.to_hex().as_str())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<blake3::Hash, D::Error> {
        let hex = String::deserialize(d)?;
        blake3::Hash::from_hex(hex).map_err(D::Error::custom)
    }
}

/// A candidate after the slot has decoded it, ready to load.
struct Resolved {
    name: String,
    ty: String,
    params: Value,
    path: PathBuf,
    hash: blake3::Hash,
    kind: PackKind,
    occupant: Occupant,
}

impl TableEntry {
    /// The table entry for the `fsw.dl_slot` system type.
    pub(crate) fn dl_slot() -> Self {
        Self {
            kind: SystemKind::Cyclic,
            def: SystemDef {
                name: SLOT_NAME.into(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                grants: Vec::new(),
            },
            def_fn: Box::new(DlSlot::def),
            doc: "Loads one of the pack systems it allows on its channel's `Load`.".into(),
            schema: None,
            make: Arc::new(DlSlot::make),
        }
    }
}

impl SystemDef {
    /// Whether `self` and `other` take the same ports in the same order, and
    /// the same grants, ignoring the namespace each port is announced under.
    pub(crate) fn same_ports(&self, other: &SystemDef) -> bool {
        let same = |a: &[PortDef], b: &[PortDef]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| a.name == b.name && a.same_record(b))
        };
        same(&self.inputs, &other.inputs)
            && same(&self.outputs, &other.outputs)
            && self.grants == other.grants
    }
}

impl Resolved {
    /// Opens this candidate's library after checking its hash.
    fn open(&self) -> Result<Pack, LoadError> {
        match self.kind {
            PackKind::Dylib => self.open_dylib(),
        }
    }

    /// Opens this candidate's library.
    ///
    /// A library that is already loaded was checked when it was first opened,
    /// so it is not hashed again. On a first open, the file is hashed, opened,
    /// and then checked to make sure the opened file is the one that was
    /// hashed.
    fn open_dylib(&self) -> Result<Pack, LoadError> {
        let io_error = |e: std::io::Error| LoadError::Open(e.to_string());
        if is_loaded(&self.path) {
            // SAFETY: the library is already mapped in this process.
            let pack = unsafe { DlPack::open(&self.path) };
            return Ok(Pack::Dl(pack.map_err(|e| LoadError::Open(e.to_string()))?));
        }
        let hashed = store::Hashed::of(&self.path).map_err(io_error)?;
        if hashed.hash != self.hash {
            return Err(LoadError::Hash(self.name.clone()));
        }
        // SAFETY: the library is in the store that `metor build` wrote, and its
        // bytes match the hash build recorded. `open` checks the ABI first.
        let pack = unsafe { DlPack::open(&self.path) };
        let pack = pack.map_err(|e| LoadError::Open(e.to_string()))?;
        if !hashed.still_at(&self.path).map_err(io_error)? {
            return Err(LoadError::Hash(self.name.clone()));
        }
        Ok(Pack::Dl(pack))
    }

    fn of(index: usize, candidate: Candidate) -> Result<Self, ParamError> {
        let Stored {
            path, hash, kind, ..
        } = candidate.stored;
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
        })
    }
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
    occupancy: Occupancy,
    /// Where the slot's candidates share one IO region.
    io: SharedIo,
}

/// What a slot holds.
enum Occupancy {
    Loaded(Loaded),
    /// Nothing, and why: `Unloaded`, or the failure that emptied it.
    Empty(EndReason),
}

/// The loaded system and the pack it was created from.
struct Loaded {
    index: usize,
    pack: Arc<Pack>,
    step: Box<dyn Step>,
}

impl DlSlot {
    /// Returns the slot's definition, using the first candidate's ports.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let refused = |message: String| DefError::Slot { message };
        let params = SlotParams::deserialize(cx.params).map_err(|e| refused(e.to_string()))?;
        let first = params
            .allow
            .into_iter()
            .next()
            .ok_or_else(|| refused("the slot allows nothing".into()))?;
        Ok(SystemDef {
            name: SLOT_NAME.into(),
            ..first.stored.def
        })
    }

    /// Creates a slot on the rings `cx` gives, and loads its initial candidate.
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
            occupancy: Occupancy::Empty(EndReason::Unloaded),
            io: cx.io.clone(),
        };
        if let Some(name) = params.initial
            && let Err(error) = slot.load(&name)
        {
            slot.occupancy = Occupancy::Empty(EndReason::Failed(error.to_string().into()));
        }
        Ok(Box::new(slot))
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
        if !def.same_ports(&self.instance.def) {
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
        let rings = InstanceRings {
            inputs: &inputs,
            outputs: &outputs,
        };
        pack.create(
            &candidate.ty,
            &candidate.params,
            &self.instance,
            rings,
            &self.io,
        )
        .map_err(LoadError::Create)
    }

    /// Creates candidate `index` on the slot's rings.
    ///
    /// No other system may be loaded when this is called.
    fn place(&mut self, index: usize, pack: Arc<Pack>) -> Result<(), LoadError> {
        let step = self.create(&pack, &self.candidates[index])?;
        self.occupancy = Occupancy::Loaded(Loaded { index, pack, step });
        Ok(())
    }
}

impl Step for DlSlot {
    fn execute(&mut self, now: Timestamp) {
        if let Occupancy::Loaded(loaded) = &mut self.occupancy {
            loaded.step.execute(now);
        }
    }

    /// Does nothing, because the pack has already logged the fault.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        match &self.occupancy {
            Occupancy::Loaded(loaded) => loaded.step.ended(),
            Occupancy::Empty(end) => Some(end),
        }
    }

    /// If the new system's library or ports are rejected, the current system
    /// keeps running. If the new system's constructor fails, the previous
    /// system is created again.
    fn load(&mut self, name: &str) -> Result<(), LoadError> {
        let index = self
            .candidates
            .iter()
            .position(|candidate| candidate.name == name)
            .ok_or_else(|| LoadError::Unknown(name.to_string()))?;
        let pack = Arc::new(self.candidates[index].open()?);
        self.check_ports(&pack, &self.candidates[index])?;
        // Drop the old system first so its ring readers and writers are free
        // for the new one.
        let unloaded = Occupancy::Empty(EndReason::Unloaded);
        let previous = match core::mem::replace(&mut self.occupancy, unloaded) {
            Occupancy::Loaded(loaded) => Some((loaded.index, loaded.pack)),
            Occupancy::Empty(_) => None,
        };
        let placed = self.place(index, pack);
        if placed.is_err()
            && let Some((previous, pack)) = previous
        {
            // The old library is still loaded and the rings are free again.
            let _ = self.place(previous, pack);
        }
        placed
    }

    fn occupant(&self) -> Option<&Occupant> {
        let Occupancy::Loaded(loaded) = &self.occupancy else {
            return None;
        };
        Some(&self.candidates[loaded.index].occupant)
    }
}

#[cfg(test)]
mod tests;
