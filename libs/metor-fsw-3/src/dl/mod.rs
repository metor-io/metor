//! Loads packs and registers their systems into a [`SystemTable`].
//!
//! A pack is a separately built library of system types. It lets a target use
//! systems that are not compiled into the host binary. A pack is either a
//! shared library loaded into this process ([`DlPack`]) or a WebAssembly
//! module ([`WasmPack`]). Both export a descriptor listing their systems.

pub(crate) mod codec;
mod pack;
mod pack_ref;
pub mod slot;
pub mod store;

use std::sync::Arc;

use metor_fsw_3_ring::RingBuffer;

use crate::coordinator::{BuildError, ParamError, Step, SystemTable, TableEntry};
use crate::def::{DefCx, DefError};
use crate::io::{End, SharedIo};
use crate::pack::def::{Instance, PackDef, PackSystemDef};
use crate::pack::raw::RawRing;
use crate::system::SystemDef;
use crate::wasm::{Trap, WasmError, WasmPack};

pub(crate) use pack::is_loaded;
pub use pack::{DlPack, DlStep};
pub use pack_ref::{PackKind, PackRef, WASM_TRIPLE, cdylib_name, triple, wasm_name};

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
    #[error("reading the module: {0}")]
    Read(std::io::Error),
    #[error("the module: {0}")]
    Wasm(#[from] WasmError),
    #[error("the module trapped: {0}")]
    Trap(#[from] Trap),
}

/// The system type name that [`DlSlot`](slot::DlSlot) is registered under.
pub const SLOT_TYPE: &str = "fsw.dl_slot";

/// An opened pack of either kind.
pub enum Pack {
    Dl(DlPack),
    Wasm(WasmPack),
}

impl Pack {
    /// Returns the descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        match self {
            Pack::Dl(pack) => pack.descriptor(),
            Pack::Wasm(pack) => pack.descriptor(),
        }
    }

    /// Returns the definition of type `ty` for one instance's config.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        match self {
            Pack::Dl(pack) => pack.def(ty, cx),
            Pack::Wasm(pack) => pack.def(ty, cx),
        }
    }

    /// Creates one instance of type `ty` on the given rings.
    ///
    /// A system with a `Cap` shares its IO region with the host: on its first
    /// make the pack lays one out and the host's member adopts it; a remake
    /// is given the member's region. The host never rewinds it. A
    /// `DeviceSide` is refused, since a guest cannot reach the target's
    /// tables.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        ports: Ports<'_>,
        io: &SharedIo,
    ) -> Result<Box<dyn Step>, ParamError> {
        let id = &instance.id;
        if instance.def.grants.iter().any(|g| g.end == End::Device) {
            return Err(ParamError::GuestDeviceSide { system: id.clone() });
        }
        let arena = io.borrow().arena(id);
        let mut table = arena
            .as_deref()
            .map_or(RawRing::EMPTY, crate::pack::ring_of);
        let (inputs, outputs) = ports;
        let step: Box<dyn Step> = match self {
            Pack::Dl(pack) => {
                Box::new(pack.create(ty, params, instance, inputs, outputs, &mut table)?)
            }
            Pack::Wasm(pack) => Box::new(pack.create(ty, params, instance, ports, io)?),
        };
        if arena.is_none() {
            // SAFETY: a pack writes back a live region with an owner that
            // meets `RawOwner`'s contract, or an empty one.
            unsafe { adopt(io, instance, table) };
        }
        Ok(step)
    }
}

/// An instance's input and output rings.
pub type Ports<'a> = (&'a [Vec<&'a RingBuffer>], &'a [&'a RingBuffer]);

/// Makes system `instance`'s member over the region its pack returned.
///
/// # Safety
/// A non-empty `table` is live, and its owner meets `RawOwner`'s contract.
unsafe fn adopt(io: &SharedIo, instance: &Instance, table: RawRing) {
    let caps: Vec<String> = crate::pack::host_caps(&instance.def).collect();
    if caps.is_empty() || table.owner.is_none() {
        return;
    }
    // SAFETY: the caller's contract.
    if let Some(arena) = unsafe { crate::pack::arena_over(table) } {
        io.borrow_mut().join(&instance.id, caps, arena);
    }
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
            .map(|system| (format!("{id}.{}", system.ty), entry(&pack, system)))
            .collect();
        self.insert_all(entries)
    }
}

/// Builds the table entry for one exported system.
///
/// The entry keeps `pack` open for as long as it exists.
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
            let ports = (&cx.inputs[..], &cx.outputs[..]);
            make_pack.create(&make_ty, cx.params.0, &instance, ports, cx.io)
        }),
    }
}
