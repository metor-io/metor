//! Loading packs: the systems a pack exports, registered into a table.

mod codec;
mod pack;
pub mod slot;
pub mod store;

use std::sync::Arc;

use metor_fsw_3_ring::RingBuffer;

use crate::coordinator::{BuildError, ParamError, Step, SystemTable, TableEntry};
use crate::def::{DefCx, DefError};
use crate::pack::def::{Instance, PackDef, PackSystemDef};
use crate::system::SystemDef;

pub(crate) use pack::is_loaded;
pub use pack::{DlPack, DlStep};

/// A `PackError` is why an artifact is no usable pack.
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

/// The type a `DlSlot` registers as.
pub const SLOT_TYPE: &str = "fsw.dl_slot";

/// A `Pack` is an opened pack of any kind and the descriptor it exported.
pub enum Pack {
    Dl(DlPack),
}

impl Pack {
    /// The descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        match self {
            Pack::Dl(pack) => pack.descriptor(),
        }
    }

    /// One exported type's definition under an instance's config.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        match self {
            Pack::Dl(pack) => pack.def(ty, cx),
        }
    }

    /// Creates one instance of `ty` over the host's rings.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        inputs: &[Vec<&RingBuffer>],
        outputs: &[&RingBuffer],
    ) -> Result<Box<dyn Step>, ParamError> {
        match self {
            Pack::Dl(pack) => Ok(Box::new(
                pack.create(ty, params, instance, inputs, outputs)?,
            )),
        }
    }
}

impl SystemTable {
    /// Registers systems in `pack` under `"{id}.{ty}"` in order.
    /// Stops at the first conflict, leaving earlier registrations in the table.
    pub fn register_pack(&mut self, id: &str, pack: Arc<Pack>) -> Result<(), BuildError> {
        let entries = pack
            .descriptor()
            .systems
            .iter()
            .map(|system| (format!("{id}.{}", system.ty), entry(&pack, system)))
            .collect();
        self.insert_all(entries)
    }
}

/// The table entry for one exported system, whose closures share `pack`.
fn entry(pack: &Arc<Pack>, system: &PackSystemDef) -> TableEntry {
    let (def_pack, def_ty) = (pack.clone(), system.ty.clone());
    let (make_pack, make_ty) = (pack.clone(), system.ty.clone());
    TableEntry {
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
            make_pack.create(&make_ty, cx.params.0, &instance, &cx.inputs, &cx.outputs)
        }),
    }
}
