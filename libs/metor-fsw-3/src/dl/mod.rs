//! Loads packs and registers their systems into a [`SystemTable`].
//!
//! A pack is a separately built library of system types. It lets a target use
//! systems that are not compiled into the host binary. A pack is a shared
//! library loaded into this process ([`DlPack`]). It exports a descriptor
//! listing its systems.

pub(crate) mod codec;
mod pack;
mod pack_ref;
pub mod slot;
pub mod store;

use std::sync::Arc;

use metor_fsw_3_ring::RingBuffer;

use crate::coordinator::{BuildError, ParamError, Step, SystemTable, TableEntry};
use crate::def::{DefCx, DefError};
use crate::io::SharedIo;
use crate::pack::def::{Instance, PackDef, PackSystemDef};
use crate::pack::raw::RawRing;
use crate::system::SystemDef;

pub(crate) use pack::is_loaded;
pub use pack::{DlPack, DlStep};
pub use pack_ref::{PackKind, PackRef, cdylib_name, triple};

/// An error from opening a pack.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("loading the pack: {0}")]
    Open(libloading::Error),
    #[error("the pack exports no `{0}`")]
    MissingSymbol(&'static str),
    #[error("the pack is built against ABI {found}, this host speaks {expected}")]
    AbiMismatch { found: u32, expected: u32 },
    #[error("the pack's descriptor did not decode")]
    Decode,
    #[error("pack descriptor exceeds {capacity} bytes")]
    DescriptorTooLarge { capacity: usize },
    #[error("pack descriptor export failed with status {0}")]
    DescriptorStatus(u32),
}

/// The system type name that [`DlSlot`](slot::DlSlot) is registered under.
pub const SLOT_TYPE: &str = "fsw.dl_slot";

/// An opened pack.
pub enum Pack {
    Dl(DlPack),
}

impl Pack {
    /// Returns the descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        match self {
            Pack::Dl(pack) => pack.descriptor(),
        }
    }

    /// Returns the definition of type `ty` for one instance's config.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        match self {
            Pack::Dl(pack) => pack.def(ty, cx),
        }
    }

    /// Creates one instance of type `ty` on the given rings.
    ///
    /// A system with a `Cap` shares its IO region with the host: on its first
    /// make the pack lays one out and the host's member adopts it; a remake
    /// is given the member's region. The host never rewinds it.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        rings: InstanceRings<'_>,
        io: &SharedIo,
    ) -> Result<Box<dyn Step>, ParamError> {
        let arena = io.borrow().arena(&instance.id);
        let mut table = arena.as_deref().map_or(RawRing::EMPTY, RawRing::from_arena);
        let step: Box<dyn Step> = match self {
            Pack::Dl(pack) => Box::new(pack.create(ty, params, instance, rings, &mut table)?),
        };
        if !instance.def.grants.is_empty() {
            let arena = io.borrow_mut().get_or_add_member(&instance.id);
            // SAFETY: a pack writes back a live region with an owner that
            // meets `RawOwner`'s contract, or an empty one.
            unsafe { arena.adopt(table) };
        }
        Ok(step)
    }
}

/// An instance's input and output rings.
#[derive(Clone, Copy)]
pub struct InstanceRings<'a> {
    pub inputs: &'a [Vec<&'a RingBuffer>],
    pub outputs: &'a [&'a RingBuffer],
}

impl SystemTable {
    /// Registers every system in `pack` under the name `"{id}.{ty}"`, in order.
    ///
    /// Stops at the first conflict. Systems registered before the conflict stay
    /// in the table.
    pub fn register_pack(&mut self, id: &str, pack: Arc<Pack>) -> Result<(), BuildError> {
        let entries = pack
            .descriptor()
            .systems
            .iter()
            .map(|system| {
                let entry = TableEntry::from_pack(&pack, system);
                (format!("{id}.{}", system.ty), entry)
            })
            .collect();
        self.insert_all(entries)
    }
}

impl TableEntry {
    /// The table entry for one system that `pack` exports.
    ///
    /// The entry keeps `pack` open for as long as it exists.
    fn from_pack(pack: &Arc<Pack>, system: &PackSystemDef) -> Self {
        let (def_pack, def_ty) = (pack.clone(), system.ty.clone());
        let (make_pack, make_ty) = (pack.clone(), system.ty.clone());
        Self {
            kind: system.kind,
            def: system.def.clone(),
            def_fn: Box::new(move |cx| def_pack.def(&def_ty, cx)),
            doc: system.doc.clone(),
            schema: system.params.clone(),
            make: Arc::new(move |cx| {
                let instance = Instance {
                    id: cx.id.to_string(),
                    def: cx.def.clone(),
                    adapter: cx.adapter.cloned(),
                };
                let rings = InstanceRings {
                    inputs: &cx.inputs,
                    outputs: &cx.outputs,
                };
                make_pack.create(&make_ty, cx.params.0, &instance, rings, cx.io)
            }),
        }
    }
}
