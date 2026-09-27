//! A pack built as a WebAssembly module.

use crate::def::{DefCx, DefError};
use crate::dl::PackError;
use crate::dl::codec::{decode_def, decode_descriptor, encode_def_cx};
use crate::pack::ABI_VERSION;
use crate::pack::def::PackDef;
use crate::system::SystemDef;

use super::abi::Args;
use super::guest::{Guest, Module};

/// The fuel grants a module's calls run under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fuel {
    /// One `execute`.
    pub step: u64,
    /// Each instantiation and each call made while loading and creating:
    /// the descriptor, `def`, `create`, and the allocator calls around them.
    pub setup: u64,
}

/// A `WasmPack` is a compiled pack module and the descriptor it exported.
pub struct WasmPack {
    module: Module,
    /// Each guest's memory ceiling, in bytes.
    memory: usize,
    fuel: Fuel,
    def: PackDef,
}

impl WasmPack {
    /// Compiles a module and reads its descriptor from a scratch guest,
    /// checking its ABI version first.
    pub fn open(bytes: &[u8], memory: usize, fuel: Fuel) -> Result<WasmPack, PackError> {
        let module = Module::compile(bytes)?;
        let mut guest = Guest::new(&module, memory, fuel.setup)?;
        check_version(&mut guest, fuel.setup)?;
        let def = descriptor(&mut guest, fuel.setup)?;
        Ok(WasmPack {
            module,
            memory,
            fuel,
            def,
        })
    }

    /// The descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        &self.def
    }

    /// One exported type's definition under an instance's config, from a
    /// scratch guest.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let cx = encode_def_cx(cx)?;
        let (status, bytes) = self.call_def(ty, &cx).map_err(|e| DefError::Pack {
            message: e.to_string(),
        })?;
        decode_def(status, bytes.as_deref())
    }

    /// Calls `metor_fsw_def` in a fresh guest, returning its status and
    /// what it wrote.
    fn call_def(&self, ty: &str, cx: &[u8]) -> Result<(u32, Option<Vec<u8>>), PackError> {
        let mut guest = Guest::new(&self.module, self.memory, self.fuel.setup)?;
        let mut args = Args::new(&mut guest, self.fuel.setup);
        let ty = args.byte_slice(ty.as_bytes())?;
        let cx = args.byte_slice(cx)?;
        let out = args.out()?;
        let status = args.guest().def(ty, cx, out, self.fuel.setup)?;
        Ok((status, args.written(out)?))
    }
}

fn check_version(guest: &mut Guest, fuel: u64) -> Result<(), PackError> {
    let found = guest.abi_version(fuel)?;
    if found != ABI_VERSION {
        return Err(PackError::AbiMismatch {
            found,
            expected: ABI_VERSION,
        });
    }
    Ok(())
}

fn descriptor(guest: &mut Guest, fuel: u64) -> Result<PackDef, PackError> {
    let mut args = Args::new(guest, fuel);
    let out = args.out()?;
    let status = args.guest().pack_def(out, fuel)?;
    decode_descriptor(status, args.written(out)?.as_deref())
}
