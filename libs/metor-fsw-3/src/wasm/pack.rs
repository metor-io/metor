//! A pack built as a WebAssembly module.

use metor_fsw_3_ring::{NoWake, PAYLOAD_ALIGNMENT, RingBuffer, checked_region_len};

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::dl::PackError;
use crate::dl::codec::{
    decode_create_error, decode_def, decode_descriptor, encode_def_cx, encode_instance,
};
use crate::mirror::{Mirror, Mirrors, mirror_config};
use crate::pack::ABI_VERSION;
use crate::pack::def::{Instance, PackDef};
use crate::system::{PortDef, SystemDef};

use super::abi::{Args, GuestRing};
use super::guest::{CreateArgs, Guest, Module, WasmError};
use super::step::WasmStep;

/// How much deeper a guest's copy of a ring is than the ring. The copy's
/// wrap falls elsewhere, so a full ring's records can need more than one
/// capacity of room.
const COPY_FACTOR: usize = 2;

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

    /// Creates one instance of `ty` in a fresh guest, over copies of the
    /// host's rings in its memory.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        inputs: &[Vec<&RingBuffer>],
        outputs: &[&RingBuffer],
    ) -> Result<WasmStep, ParamError> {
        let (params, def) = encode_instance(params, instance)?;
        let fuel = self.fuel.setup;
        let mut guest = Guest::new(&self.module, self.memory, fuel).map_err(PackError::from)?;
        let (edges, in_rings) = copy_inputs(&mut guest, &instance.def.inputs, inputs, fuel)?;
        let (copies, out_rings) = copy_outputs(&mut guest, &instance.def.outputs, outputs, fuel)?;
        let mirrors = Mirrors::new(&instance.def, edges, copies);
        let call = Create {
            ty: ty.as_bytes(),
            params: &params,
            def: &def,
            inputs: &in_rings,
            outputs: &out_rings,
        };
        let handle = call_create(&mut guest, call, fuel)?;
        Ok(WasmStep::new(mirrors, guest, handle, self.fuel.step))
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

impl From<PackError> for ParamError {
    /// A guest that trapped panicked; any other failure is the host's.
    fn from(error: PackError) -> Self {
        match error {
            PackError::Trap(trap) => ParamError::Panicked(trap.to_string()),
            error => ParamError::Wasm(error.to_string()),
        }
    }
}

/// `create`'s arguments, as the host holds them.
struct Create<'a> {
    ty: &'a [u8],
    params: &'a [u8],
    def: &'a [u8],
    inputs: &'a [Vec<GuestRing>],
    outputs: &'a [GuestRing],
}

/// Places `call` in guest memory and calls `metor_fsw_create`, returning the
/// instance or the error the guest wrote.
fn call_create(guest: &mut Guest, call: Create<'_>, fuel: u64) -> Result<u32, ParamError> {
    let mut args = Args::new(guest, fuel);
    let ty = args.byte_slice(call.ty)?;
    let params = args.byte_slice(call.params)?;
    let def = args.byte_slice(call.def)?;
    let edges = call
        .inputs
        .iter()
        .map(|edge| args.rings(edge))
        .collect::<Result<Vec<_>, _>>()?;
    let inputs = args.ports(&edges)?;
    let (outs, len) = args.rings(call.outputs)?;
    let outputs = args.slice(outs, len)?;
    let error = args.slice(0, 0)?;
    let create = CreateArgs {
        ty,
        params,
        def,
        inputs,
        outputs,
        error,
    };
    let handle = args.guest().create(create, fuel).map_err(PackError::from)?;
    if handle == 0 {
        return Err(decode_create_error(&args.slice_bytes(error)?));
    }
    Ok(handle)
}

type Copies = (Vec<Mirror<NoWake>>, Vec<Vec<GuestRing>>);

/// Each input edge's copy in guest memory, and the mirror that fills it
/// from the real ring.
fn copy_inputs(
    guest: &mut Guest,
    ports: &[PortDef],
    inputs: &[Vec<&RingBuffer>],
    fuel: u64,
) -> Result<Copies, ParamError> {
    let mut mirrors = Vec::new();
    let mut rings = Vec::with_capacity(ports.len());
    for (port, reals) in ports.iter().zip(inputs) {
        let mut edge = Vec::with_capacity(reals.len());
        for real in reals {
            let (at, copy) = guest_ring(guest, real, fuel)?;
            // PANIC Safety: build counted this reader, and the copy is fresh.
            mirrors.push(Mirror {
                port: port.name.clone(),
                from: real.view(NoWake).expect("a counted reader slot"),
                into: copy.writer(NoWake).expect("a fresh ring"),
                dropped: 0,
            });
            edge.push(at);
        }
        rings.push(edge);
    }
    Ok((mirrors, rings))
}

/// Each output's copy in guest memory, and the mirror that drains it into
/// the real ring.
fn copy_outputs(
    guest: &mut Guest,
    ports: &[PortDef],
    outputs: &[&RingBuffer],
    fuel: u64,
) -> Result<(Vec<Mirror<NoWake>>, Vec<GuestRing>), ParamError> {
    let mut mirrors = Vec::with_capacity(ports.len());
    let mut rings = Vec::with_capacity(ports.len());
    for (port, real) in ports.iter().zip(outputs) {
        let (at, copy) = guest_ring(guest, real, fuel)?;
        // PANIC Safety: the copy is fresh, and build counted one writer per output.
        mirrors.push(Mirror {
            port: port.name.clone(),
            from: copy.view(NoWake).expect("a fresh ring"),
            into: real.writer(NoWake).expect("one writer per output ring"),
            dropped: 0,
        });
        rings.push(at);
    }
    Ok((mirrors, rings))
}

/// A ring [`COPY_FACTOR`] times as deep as `real`, allocated in guest memory
/// and formatted by the host.
fn guest_ring(
    guest: &mut Guest,
    real: &RingBuffer,
    fuel: u64,
) -> Result<(GuestRing, RingBuffer), ParamError> {
    let capacity = real.config().capacity;
    let config = mirror_config(capacity, COPY_FACTOR)?;
    let len = checked_region_len(&config)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or(ParamError::MirrorTooLarge { capacity })?;
    let offset = guest
        .alloc(len, PAYLOAD_ALIGNMENT as u32, fuel)
        .map_err(PackError::from)?;
    if offset == 0 {
        return Err(PackError::from(WasmError::Alloc(len)).into());
    }
    let base = guest
        .region(offset, len as usize)
        .map_err(PackError::from)?;
    // SAFETY: the region is `len` bytes in the guest's arena, which the guest
    // has not seen yet. Every handle on it ends up in `WasmStep::mirrors`,
    // declared before `guest`, or drops before `guest` on an error, so the
    // arena outlives them all. The guest and host then both follow the ring
    // protocol over it.
    let ring = unsafe { RingBuffer::create_raw(base.as_ptr(), len as usize, config) }
        .map_err(|e| ParamError::Wasm(e.to_string()))?;
    Ok((GuestRing { offset, len }, ring))
}
