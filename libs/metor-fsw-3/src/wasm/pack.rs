//! Loads packs built as WebAssembly modules.

use core::ptr::NonNull;

use metor_fsw_3_ring::{NoWake, PAYLOAD_ALIGNMENT, RingBuffer, View};
use serde::{Deserialize, Serialize};

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::dl::{InstanceRings, PackError};
use crate::io::{Cqe, IoState, Sqe};
use crate::mirror::{Mirror, Mirrors, mirror_config};
use crate::pack::ABI_VERSION;
use crate::pack::def::{DefCxOwned, Instance, PackDef};
use crate::port::Input;
use crate::system::{PortDef, SystemDef};

use super::abi::{Args, GuestRing, guest_usize};
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
        guest.check_abi_version()?;
        let def = guest.descriptor()?;
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
        let cx = DefCxOwned::encode(cx)?;
        let (status, bytes) = self.call_def(ty, &cx).map_err(|e| DefError::Pack {
            message: e.to_string(),
        })?;
        SystemDef::decode(status, Some(&bytes))
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
        InstanceRings { inputs, outputs }: InstanceRings<'_>,
        io: &mut IoState,
    ) -> Result<WasmStep, ParamError> {
        let (params, def) = instance.encode(params)?;
        let mut guest = self.guest().map_err(PackError::from)?;
        let capped = !instance.def.grants.is_empty();
        let ins = Copies::inputs(&mut guest, &instance.def.inputs, inputs, capped)?;
        let outs = Copies::outputs(&mut guest, &instance.def.outputs, outputs, capped)?;
        let mirrors = Mirrors::new(&instance.def, ins.mirrors, outs.mirrors);
        let ports = ins.io.zip(outs.io).map(|(cq, sq)| IoPorts {
            sq: Input::new(vec![sq]),
            cq: Input::new(vec![cq]),
        });
        let remade = ports
            .as_ref()
            .and_then(|_| io.arena(&instance.id)?.region());
        let given = match remade {
            Some((_, len)) => Some(GuestRing::alloc_zeroed(&mut guest, len)?),
            None => None,
        };
        let call = Create {
            ty: ty.as_bytes(),
            params: &params,
            def: &def,
            inputs: &ins.rings,
            outputs: &outs.rings,
            io_arena: given,
        };
        let (handle, region) = call.call(&mut guest)?;
        let copy = ports
            .filter(|_| region.len > 0)
            .map(|ports| attach(&guest, io, instance, region, ports))
            .transpose()?;
        Ok(WasmStep::new(mirrors, copy, guest, handle, self.fuel))
    }

    /// Instantiates a new guest using setup fuel.
    fn guest(&self) -> Result<Guest, WasmError> {
        Guest::new(&self.module, self.memory, DEFAULT_SETUP_FUEL)
    }

    /// Calls `metor_fsw_def` in a new guest.
    ///
    /// Returns the status and the bytes the export wrote.
    fn call_def(&self, ty: &str, cx: &[u8]) -> Result<(u32, Vec<u8>), PackError> {
        let mut guest = self.guest()?;
        let mut args = Args::new(&mut guest);
        let ty = args.byte_slice(ty.as_bytes())?;
        let cx = args.byte_slice(cx)?;
        let out = args.out()?;
        let status = args.guest().def(ty, cx, out, DEFAULT_SETUP_FUEL)?;
        Ok((status, args.written(out)?))
    }
}

impl Guest {
    /// Returns an error unless the module speaks this host's ABI version.
    fn check_abi_version(&mut self) -> Result<(), PackError> {
        let found = self.abi_version(DEFAULT_SETUP_FUEL)?;
        if found != ABI_VERSION {
            return Err(PackError::AbiMismatch {
                found,
                expected: ABI_VERSION,
            });
        }
        Ok(())
    }

    /// Reads the pack's descriptor.
    fn descriptor(&mut self) -> Result<PackDef, PackError> {
        let mut args = Args::new(self);
        let out = args.out()?;
        let status = args.guest().pack_def(out, DEFAULT_SETUP_FUEL)?;
        PackDef::decode(status, Some(&args.written(out)?))
    }
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
    io_arena: Option<GuestRing>,
}

impl GuestRing {
    /// Allocates a zeroed region of `len` bytes in the guest, for a remade
    /// guest's constructor to lay out its IO arena in.
    fn alloc_zeroed(guest: &mut Guest, len: usize) -> Result<GuestRing, ParamError> {
        let len = guest_usize(len)?;
        let offset = guest.alloc_nonnull(len, 8)?;
        let zeroes = vec![0; len as usize];
        guest.write(offset, &zeroes).map_err(PackError::from)?;
        Ok(GuestRing { offset, len })
    }

    /// Creates a ring in guest memory to mirror a real ring of `capacity`
    /// bytes, with `readers` reader slots.
    ///
    /// The copy is [`COPY_FACTOR`] times as large. The host formats it before
    /// the guest sees it.
    fn create(
        guest: &mut Guest,
        capacity: usize,
        readers: usize,
    ) -> Result<(GuestRing, RingBuffer), ParamError> {
        let too_large = || ParamError::MirrorTooLarge { capacity };
        let config = mirror_config(capacity, COPY_FACTOR, readers)?;
        let len = config.region_len().map_err(|_| too_large())?;
        let len = u32::try_from(len).map_err(|_| too_large())?;
        let offset = guest.alloc_nonnull(len, PAYLOAD_ALIGNMENT as u32)?;
        let base = guest
            .region(offset, len as usize)
            .map_err(PackError::from)?;
        // SAFETY: the region is `len` bytes in the guest's memory, and the
        // guest has not seen it yet. Every handle on the ring either ends up in
        // `WasmStep::mirrors`, which is declared before `guest`, or is dropped
        // before `guest` on an error. So the memory outlives all of them. From
        // then on, the guest and host both follow the ring protocol.
        let ring = unsafe { RingBuffer::create_raw(base.as_ptr(), len as usize, config) }
            .map_err(|e| ParamError::Wasm(e.to_string()))?;
        Ok((GuestRing { offset, len }, ring))
    }
}

/// A capped system's IO ports: second readers on the guest's copies of its
/// SQ and CQ.
struct IoPorts {
    sq: Input<Sqe>,
    cq: Input<Cqe>,
}

/// Makes the host's mirror of the IO arena the guest laid out at `region`,
/// and the copy that keeps the two in step.
fn attach(
    guest: &Guest,
    io: &mut IoState,
    instance: &Instance,
    region: GuestRing,
    IoPorts { sq, cq }: IoPorts,
) -> Result<IoCopy, ParamError> {
    let len = region.len as usize;
    let mirror = io.get_or_add_member(&instance.id);
    if mirror.region().is_none() {
        mirror.set_owned(vec![0; len].into());
    }
    if mirror.region().is_none_or(|(_, l)| l != len) {
        return Err(ParamError::Wasm("a remade guest moved its IO arena".into()));
    }
    let base = guest
        .host_ptr(region.offset, len)
        .map_err(PackError::from)?;
    // SAFETY: the guest's memory never moves, and `WasmStep` drops the copy
    // before the guest.
    Ok(unsafe { IoCopy::new(NonNull::slice_from_raw_parts(base, len), mirror, sq, cq) })
}

impl Create<'_> {
    /// Copies these arguments into guest memory and calls `metor_fsw_create`.
    ///
    /// Returns the instance and the IO arena the guest wrote back, or the
    /// error the guest wrote.
    fn call(self, guest: &mut Guest) -> Result<(u32, GuestRing), ParamError> {
        let mut args = Args::new(guest);
        let ty = args.byte_slice(self.ty)?;
        let params = args.byte_slice(self.params)?;
        let def = args.byte_slice(self.def)?;
        let edges = self
            .inputs
            .iter()
            .map(|edge| args.rings(edge))
            .collect::<Result<Vec<_>, _>>()?;
        let inputs = args.ports(&edges)?;
        let (outs, len) = args.rings(self.outputs)?;
        let outputs = args.slice(outs, len)?;
        let io_arena = args.io_arena(self.io_arena)?;
        let error = args.slice(0, 0)?;
        let create = (ty, params, def, inputs, outputs, io_arena, error);
        let handle = args
            .guest()
            .create(create, DEFAULT_SETUP_FUEL)
            .map_err(PackError::from)?;
        if handle == 0 {
            return Err(ParamError::from_create_error(&args.slice_bytes(error)?));
        }
        Ok((handle, args.ring(io_arena)?))
    }
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

impl Copies<Vec<GuestRing>> {
    /// Creates a guest copy of each input edge's ring, and mirrors that fill
    /// each copy from its real ring. With `capped`, an IO port's copy takes a
    /// second reader.
    fn inputs(
        guest: &mut Guest,
        ports: &[PortDef],
        inputs: &[Vec<&RingBuffer>],
        capped: bool,
    ) -> Result<Self, ParamError> {
        let mut copies = Copies {
            mirrors: Vec::new(),
            rings: Vec::with_capacity(ports.len()),
            io: None,
        };
        for (port, reals) in ports.iter().zip(inputs) {
            let mut edge = Vec::with_capacity(reals.len());
            let readers = 1 + usize::from(capped && port.is_io());
            for real in reals {
                let (guest_ring, copy) = GuestRing::create(guest, real.config().capacity, readers)?;
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
                edge.push(guest_ring);
            }
            copies.rings.push(edge);
        }
        Ok(copies)
    }
}

impl Copies<GuestRing> {
    /// Creates a guest copy of each output ring, and mirrors that drain each
    /// copy into its real ring. With `capped`, an IO port's copy takes a
    /// second reader.
    fn outputs(
        guest: &mut Guest,
        ports: &[PortDef],
        outputs: &[&RingBuffer],
        capped: bool,
    ) -> Result<Self, ParamError> {
        let mut copies = Copies {
            mirrors: Vec::with_capacity(ports.len()),
            rings: Vec::with_capacity(ports.len()),
            io: None,
        };
        for (port, real) in ports.iter().zip(outputs) {
            let readers = 1 + usize::from(capped && port.is_io());
            let (guest_ring, copy) = GuestRing::create(guest, real.config().capacity, readers)?;
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
            copies.rings.push(guest_ring);
        }
        Ok(copies)
    }
}
