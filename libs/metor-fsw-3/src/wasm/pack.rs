//! Loads packs built as WebAssembly modules.

use metor_fsw_3_ring::{NoWake, PAYLOAD_ALIGNMENT, RingBuffer, View};
use serde::{Deserialize, Serialize};

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::dl::PackError;
use crate::dl::Ports;
use crate::dl::codec::{
    decode_create_error, decode_def, decode_descriptor, encode_def_cx, encode_instance,
};
use crate::io::{Arena, Cqe, IO_CQ, IO_SQ, SharedIo, Sqe};
use crate::mirror::{Mirror, Mirrors, mirror_config};
use crate::pack::def::{Instance, PackDef};
use crate::pack::{ABI_VERSION, host_caps};
use crate::port::Input;
use crate::system::{PortDef, SystemDef};

use super::abi::{Args, GuestRing, allocated};
use super::guest::{Guest, Module, WasmError};
use super::io::IoCopy;
use super::step::WasmStep;
use super::{DEFAULT_FUEL, DEFAULT_MEMORY, DEFAULT_SETUP_FUEL};

/// How many times larger a guest's copy of a ring is than the real ring.
///
/// The copy wraps at a different point than the real ring, so the records from
/// a full ring can need more than one ring's worth of space.
const COPY_FACTOR: usize = 2;

/// Resource limits for each guest of a module.
///
/// Unset fields use the defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The memory ceiling for each guest, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<usize>,
    /// The fuel each step gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
}

/// A pack compiled from a WebAssembly module.
///
/// Each instance it creates runs in its own guest.
pub struct WasmPack {
    module: Module,
    /// The memory ceiling for each guest, in bytes.
    memory: usize,
    /// The fuel each step gets.
    fuel: u64,
    def: PackDef,
}

impl WasmPack {
    /// Compiles a module and reads its descriptor.
    ///
    /// The ABI version is checked first, in a scratch guest that then reads the
    /// descriptor. Setup runs with `DEFAULT_SETUP_FUEL`.
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

    /// Returns the descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        &self.def
    }

    /// Returns the definition of type `ty` for one instance's config.
    ///
    /// Each call runs in a scratch guest.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let cx = encode_def_cx(cx)?;
        let (status, bytes) = self.call_def(ty, &cx).map_err(|e| DefError::Pack {
            message: e.to_string(),
        })?;
        decode_def(status, bytes.as_deref())
    }

    /// Creates one instance of type `ty` in a new guest.
    ///
    /// The guest works on copies of the host's rings, placed in its own memory.
    /// A system with a `Cap` lays out its IO arena in the guest, and the host
    /// keeps a mirror of it as the system's member. A remade guest is given a
    /// copy of the mirror, rewinds it, and runs its constructor in it.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        (inputs, outputs): Ports<'_>,
        io: &SharedIo,
    ) -> Result<WasmStep, ParamError> {
        let (params, def) = encode_instance(params, instance)?;
        let mut guest = self.guest().map_err(PackError::from)?;
        let capped = host_caps(&instance.def).next().is_some();
        let io_port = |name: &str| capped && (name == IO_SQ || name == IO_CQ);
        let ins = copy_inputs(&mut guest, &instance.def.inputs, inputs, io_port)?;
        let outs = copy_outputs(&mut guest, &instance.def.outputs, outputs, io_port)?;
        let mirrors = Mirrors::new(&instance.def, ins.mirrors, outs.mirrors);
        let mirror = io.borrow().arena(&instance.id);
        let given = match mirror.filter(|m| m.region().is_some()) {
            Some(mirror) => Some(give(&mut guest, &mirror)?),
            None => None,
        };
        let call = Create {
            ty: ty.as_bytes(),
            params: &params,
            def: &def,
            inputs: &ins.rings,
            outputs: &outs.rings,
            table: given,
        };
        let (handle, region) = call_create(&mut guest, call)?;
        let copy = match (ins.io, outs.io) {
            (Some(cq), Some(sq)) if region.len > 0 => {
                let views = (Input::new(vec![sq]), Input::new(vec![cq]));
                Some(attach(&guest, io, instance, region, views)?)
            }
            _ => None,
        };
        Ok(WasmStep::new(mirrors, copy, guest, handle, self.fuel))
    }

    /// Instantiates a new guest using setup fuel.
    fn guest(&self) -> Result<Guest, WasmError> {
        Guest::new(&self.module, self.memory, DEFAULT_SETUP_FUEL)
    }

    /// Calls `metor_fsw_def` in a new guest.
    ///
    /// Returns the status and the bytes the export wrote.
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
    /// A trap means the guest panicked. Any other failure is the host's.
    fn from(error: PackError) -> Self {
        match error {
            PackError::Trap(trap) => ParamError::Panicked(trap.to_string()),
            error => ParamError::Wasm(error.to_string()),
        }
    }
}

/// The arguments to `create`, before they are copied into the guest.
struct Create<'a> {
    ty: &'a [u8],
    params: &'a [u8],
    def: &'a [u8],
    inputs: &'a [Vec<GuestRing>],
    outputs: &'a [GuestRing],
    /// Room for the system's IO arena, on a remake.
    table: Option<GuestRing>,
}

/// A zeroed region in the guest the length of its mirror, for a remade
/// guest's constructor to lay out its arena in.
fn give(guest: &mut Guest, mirror: &Arena) -> Result<GuestRing, ParamError> {
    let len = mirror.region().map_or(0, |(_, len)| len);
    let len = u32::try_from(len)
        .map_err(|_| WasmError::TooLarge(len))
        .map_err(PackError::from)?;
    let offset = allocated(guest, len, 8)?;
    let zeroes = vec![0; len as usize];
    guest.write(offset, &zeroes).map_err(PackError::from)?;
    Ok(GuestRing { offset, len })
}

/// Makes the host's mirror of the IO arena the guest laid out at `region`,
/// and the copy that keeps the two in step.
fn attach(
    guest: &Guest,
    io: &SharedIo,
    instance: &Instance,
    region: GuestRing,
    (sq, cq): (Input<Sqe>, Input<Cqe>),
) -> Result<IoCopy, ParamError> {
    let len = region.len as usize;
    let mirror = io.borrow_mut().member(&instance.id);
    if mirror.region().is_none() {
        mirror.set_owned(vec![0; len].into());
    }
    if mirror.region().is_none_or(|(_, l)| l != len) {
        return Err(ParamError::Wasm("a remade guest moved its IO arena".into()));
    }
    let base = guest.span(region.offset, len).map_err(PackError::from)?;
    // SAFETY: the guest's memory never moves, and `WasmStep` drops the copy
    // before the guest.
    Ok(unsafe { IoCopy::new((base, len), mirror, sq, cq) })
}

/// Copies `call` into guest memory and calls `metor_fsw_create`.
///
/// Returns the instance and the IO region the guest wrote back, or the error
/// the guest wrote.
fn call_create(guest: &mut Guest, call: Create<'_>) -> Result<(u32, GuestRing), ParamError> {
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
    let table = args.table(call.table)?;
    let error = args.slice(0, 0)?;
    let create = (ty, params, def, inputs, outputs, table, error);
    let handle = args
        .guest()
        .create(create, DEFAULT_SETUP_FUEL)
        .map_err(PackError::from)?;
    if handle == 0 {
        return Err(decode_create_error(&args.slice_bytes(error)?));
    }
    Ok((handle, args.ring(table)?))
}

/// The guest copies of a system's rings.
struct Copies<R> {
    /// The mirrors between each copy and its real ring.
    mirrors: Vec<Mirror<NoWake>>,
    /// Where each copy is in guest memory.
    rings: Vec<R>,
    /// A second reader on the copy of the IO port, for `IoCopy`.
    io: Option<View<NoWake>>,
}

/// Creates a guest copy of each input edge's ring, and mirrors that fill each
/// copy from its real ring. An IO port's copy takes a second reader.
fn copy_inputs(
    guest: &mut Guest,
    ports: &[PortDef],
    inputs: &[Vec<&RingBuffer>],
    io_port: impl Fn(&str) -> bool,
) -> Result<Copies<Vec<GuestRing>>, ParamError> {
    let mut copies = Copies {
        mirrors: Vec::new(),
        rings: Vec::with_capacity(ports.len()),
        io: None,
    };
    for (port, reals) in ports.iter().zip(inputs) {
        let mut edge = Vec::with_capacity(reals.len());
        let readers = 1 + usize::from(io_port(&port.name));
        for real in reals {
            let (at, copy) = guest_ring(guest, real.config().capacity, readers)?;
            // PANIC Safety: build counted this reader, and the copy is fresh.
            copies.mirrors.push(Mirror::new(
                port.name.clone(),
                real.view(NoWake).expect("a counted reader slot"),
                copy.writer(NoWake).expect("a fresh ring"),
            ));
            if readers > 1 {
                // PANIC Safety: the copy was made with a second reader slot.
                copies.io = Some(copy.view(NoWake).expect("a second reader"));
            }
            edge.push(at);
        }
        copies.rings.push(edge);
    }
    Ok(copies)
}

/// Creates a guest copy of each output ring, and mirrors that drain each copy
/// into its real ring. An IO port's copy takes a second reader.
fn copy_outputs(
    guest: &mut Guest,
    ports: &[PortDef],
    outputs: &[&RingBuffer],
    io_port: impl Fn(&str) -> bool,
) -> Result<Copies<GuestRing>, ParamError> {
    let mut copies = Copies {
        mirrors: Vec::with_capacity(ports.len()),
        rings: Vec::with_capacity(ports.len()),
        io: None,
    };
    for (port, real) in ports.iter().zip(outputs) {
        let readers = 1 + usize::from(io_port(&port.name));
        let (at, copy) = guest_ring(guest, real.config().capacity, readers)?;
        // PANIC Safety: the copy is fresh, and build counted one writer per output.
        copies.mirrors.push(Mirror::new(
            port.name.clone(),
            copy.view(NoWake).expect("a fresh ring"),
            real.writer(NoWake).expect("one writer per output ring"),
        ));
        if readers > 1 {
            // PANIC Safety: the copy was made with a second reader slot.
            copies.io = Some(copy.view(NoWake).expect("a second reader"));
        }
        copies.rings.push(at);
    }
    Ok(copies)
}

/// Creates a ring in guest memory to mirror a real ring of `capacity` bytes.
///
/// The copy is [`COPY_FACTOR`] times as large. The host formats it before the
/// guest sees it.
fn guest_ring(
    guest: &mut Guest,
    capacity: usize,
    readers: usize,
) -> Result<(GuestRing, RingBuffer), ParamError> {
    let (config, len) = mirror_config(capacity, COPY_FACTOR, readers)?;
    let len = u32::try_from(len).map_err(|_| ParamError::MirrorTooLarge { capacity })?;
    let offset = allocated(guest, len, PAYLOAD_ALIGNMENT as u32)?;
    let base = guest
        .region(offset, len as usize)
        .map_err(PackError::from)?;
    // SAFETY: the region is `len` bytes in the guest's arena, and the guest has
    // not seen it yet. Every handle on the ring either ends up in
    // `WasmStep::mirrors`, which is declared before `guest`, or is dropped
    // before `guest` on an error. So the arena outlives all of them. From then
    // on, the guest and host both follow the ring protocol.
    let ring = unsafe { RingBuffer::create_raw(base.as_ptr(), len as usize, config) }
        .map_err(|e| ParamError::Wasm(e.to_string()))?;
    Ok((GuestRing { offset, len }, ring))
}
