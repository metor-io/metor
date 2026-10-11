//! A pack built as a WebAssembly module.

use metor_fsw_3_ring::{NoWake, PAYLOAD_ALIGNMENT, RingBuffer};
use serde::{Deserialize, Serialize};

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

use super::abi::{Args, GuestRing, allocated};
use super::guest::{Guest, Module, WasmError};
use super::step::WasmStep;
use super::{DEFAULT_FUEL, DEFAULT_MEMORY, DEFAULT_SETUP_FUEL};

/// How much deeper a guest's copy of a ring is than the ring. The copy's
/// wrap falls elsewhere, so a full ring's records can need more than one
/// capacity of room.
const COPY_FACTOR: usize = 2;

/// What each guest of a module may use, the defaults where unset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Each guest's memory ceiling in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<usize>,
    /// Each step's fuel grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
}

/// A `WasmPack` is a compiled pack module and the descriptor it exported.
pub struct WasmPack {
    module: Module,
    /// Each guest's memory ceiling, in bytes.
    memory: usize,
    /// Each step's fuel grant.
    fuel: u64,
    def: PackDef,
}

impl WasmPack {
    /// Compiles a module and reads its descriptor from a scratch guest,
    /// checking its ABI version first. Setup runs under [`DEFAULT_SETUP_FUEL`].
    pub fn open(bytes: &[u8], limits: &Limits) -> Result<WasmPack, PackError> {
        let module = Module::compile(bytes)?;
        let memory = limits.memory.unwrap_or(DEFAULT_MEMORY);
        let mut guest = Guest::new(&module, memory, DEFAULT_SETUP_FUEL)?;
        check_version(&mut guest)?;
        let def = descriptor(&mut guest)?;
        Ok(WasmPack {
            module,
            memory,
            fuel: limits.fuel.unwrap_or(DEFAULT_FUEL),
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
        let mut guest = self.guest().map_err(PackError::from)?;
        let (edges, in_rings) = copy_inputs(&mut guest, &instance.def.inputs, inputs)?;
        let (copies, out_rings) = copy_outputs(&mut guest, &instance.def.outputs, outputs)?;
        let mirrors = Mirrors::new(&instance.def, edges, copies);
        let call = Create {
            ty: ty.as_bytes(),
            params: &params,
            def: &def,
            inputs: &in_rings,
            outputs: &out_rings,
        };
        let handle = call_create(&mut guest, call)?;
        Ok(WasmStep::new(mirrors, guest, handle, self.fuel))
    }

    /// A fresh guest, instantiated under the setup grant.
    fn guest(&self) -> Result<Guest, WasmError> {
        Guest::new(&self.module, self.memory, DEFAULT_SETUP_FUEL)
    }

    /// Calls `metor_fsw_def` in a fresh guest, returning its status and
    /// what it wrote.
    fn call_def(&self, ty: &str, cx: &[u8]) -> Result<(u32, Option<Vec<u8>>), PackError> {
        let mut guest = self.guest()?;
        let mut args = Args::new(&mut guest);
        let ty = args.byte_slice(ty.as_bytes())?;
        let cx = args.byte_slice(cx)?;
        let out = args.out()?;
        let status = args.guest().def(ty, cx, out, DEFAULT_SETUP_FUEL)?;
        Ok((status, args.written(out)?))
    }
}

fn check_version(guest: &mut Guest) -> Result<(), PackError> {
    let found = guest.abi_version(DEFAULT_SETUP_FUEL)?;
    if found != ABI_VERSION {
        return Err(PackError::AbiMismatch {
            found,
            expected: ABI_VERSION,
        });
    }
    Ok(())
}

fn descriptor(guest: &mut Guest) -> Result<PackDef, PackError> {
    let mut args = Args::new(guest);
    let out = args.out()?;
    let status = args.guest().pack_def(out, DEFAULT_SETUP_FUEL)?;
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
fn call_create(guest: &mut Guest, call: Create<'_>) -> Result<u32, ParamError> {
    let mut args = Args::new(guest);
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
    let create = (ty, params, def, inputs, outputs, error);
    let handle = args
        .guest()
        .create(create, DEFAULT_SETUP_FUEL)
        .map_err(PackError::from)?;
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
) -> Result<Copies, ParamError> {
    let mut mirrors = Vec::new();
    let mut rings = Vec::with_capacity(ports.len());
    for (port, reals) in ports.iter().zip(inputs) {
        let mut edge = Vec::with_capacity(reals.len());
        for real in reals {
            let (at, copy) = guest_ring(guest, real.config().capacity)?;
            // PANIC Safety: build counted this reader, and the copy is fresh.
            mirrors.push(Mirror::new(
                port.name.clone(),
                real.view(NoWake).expect("a counted reader slot"),
                copy.writer(NoWake).expect("a fresh ring"),
            ));
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
) -> Result<(Vec<Mirror<NoWake>>, Vec<GuestRing>), ParamError> {
    let mut mirrors = Vec::with_capacity(ports.len());
    let mut rings = Vec::with_capacity(ports.len());
    for (port, real) in ports.iter().zip(outputs) {
        let (at, copy) = guest_ring(guest, real.config().capacity)?;
        // PANIC Safety: the copy is fresh, and build counted one writer per output.
        mirrors.push(Mirror::new(
            port.name.clone(),
            copy.view(NoWake).expect("a fresh ring"),
            real.writer(NoWake).expect("one writer per output ring"),
        ));
        rings.push(at);
    }
    Ok((mirrors, rings))
}

/// A ring [`COPY_FACTOR`] times as deep as `real`, allocated in guest memory
/// and formatted by the host.
fn guest_ring(guest: &mut Guest, capacity: usize) -> Result<(GuestRing, RingBuffer), ParamError> {
    let (config, len) = mirror_config(capacity, COPY_FACTOR)?;
    let len = u32::try_from(len).map_err(|_| ParamError::MirrorTooLarge { capacity })?;
    let offset = allocated(guest, len, PAYLOAD_ALIGNMENT as u32)?;
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
