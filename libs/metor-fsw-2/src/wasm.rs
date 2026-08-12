//! Loading and driving pack modules compiled to WebAssembly.
//!
//! [`WasmPack`] is the sandboxed twin of [`DlPack`](crate::dl::DlPack): it
//! drives the very same pack ABI, over the same `fsw_pack_*` entry points, in
//! the same order — only the boundary changes. A `.so` is `dlopen`'d into the
//! host's address space and hands back raw pointers; a `.wasm` is instantiated
//! under an interpreter whose pointers are offsets into a linear memory the
//! host can read but the guest cannot escape.
//!
//! Two properties come out of that, and they are the reason this exists:
//!
//! - **A poll is bounded.** Execution is metered in *fuel*, so a guest that
//!   will not stop is cut off mid-instruction and reported as a failure rather
//!   than stalling the cycle. Nothing bounds a natively linked occupant.
//! - **A fault is contained.** An out-of-bounds access traps the guest and
//!   leaves the host untouched, where the equivalent in a `.so` is memory
//!   corruption.
//!
//! ## Where the rings live
//!
//! A guest cannot follow a pointer into the host, so the ring regions live
//! *inside guest linear memory*: the host asks the guest's own allocator for
//! each region (`fsw_pack_alloc`), formats a ring into it with
//! [`RingBuffer::create_raw`], and passes the guest offsets through
//! `fsw_pack_bind_init` exactly as the `.so` path passes host addresses.
//!
//! The regions must be guest-allocated rather than carved out of host-grown
//! pages: Rust's wasm allocator finds its heap through `memory.size`, so pages
//! the host grows behind its back can later be handed out again by the guest.
//!
//! There is therefore **no per-cycle marshalling protocol**. The ring is still
//! the shared medium it always was; it has merely moved into the guest's
//! address space, and the host reads and writes the same records through
//! [`Memory::data_mut`].
//!
//! ## Borrowing, and why regions are re-derived
//!
//! The interpreter lends guest memory out of its `Store`, so a `RingBuffer`
//! cannot be held across calls the way the native path holds one over a mapped
//! region — the borrow would conflict with the next `call`. The host therefore
//! keeps only each port's `(offset, len, role)` and re-derives the region for
//! the duration of one access. That is affordable because the ring's cursors
//! live in the region header rather than in the `Writer`, so nothing is lost
//! by rebuilding the handle; and a full port copy measured 7 ns against a
//! 2,873 ns cycle in the Phase 0 spike, far below the noise floor.
//!
//! ## The trust boundary
//!
//! Same discipline as `dl.rs`: the module is foreign code, so nothing it
//! returns is trusted as a Rust value. `fsw_pack_execute` yields a raw `u32`
//! that is validated and folded to [`FswStatus::Panicked`] when unknown, a
//! manifest that fails to decode is a clean error rather than a panic, and
//! every trap — including running out of fuel — becomes an [`WasmError`]
//! instead of propagating.

use metor_fsw_2_core::abi::{FSW_ABI_VERSION, FswStatus, PackManifest};
use metor_fsw_ring::{Config, RingBuffer, region_len};
use wasmi::{Config as WasmConfig, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// What can go wrong loading or driving a wasm pack.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// The module could not be parsed or validated.
    #[error("invalid wasm module: {0}")]
    Module(String),
    /// Instantiating the module failed, or its start section trapped.
    #[error("wasm instantiation failed: {0}")]
    Instantiate(String),
    /// The module is missing an export the pack ABI requires.
    #[error("wasm module has no `{0}` export")]
    MissingExport(&'static str),
    /// The module's ABI word disagrees with this host's.
    #[error("pack ABI version mismatch: module has {found}, host expects {expected}")]
    VersionMismatch {
        /// The version the module reported.
        found: u32,
        /// The version this host was built against.
        expected: u32,
    },
    /// `fsw_pack_open` returned null: the crate's `pack()` fn panicked.
    #[error("fsw_pack_open failed (the pack() fn panicked)")]
    PackOpen,
    /// `fsw_pack_describe` failed, or handed back no bytes.
    #[error("fsw_pack_describe failed")]
    Describe,
    /// The manifest bytes did not decode as a [`PackManifest`].
    #[error("pack manifest failed to decode: {0}")]
    Manifest(postcard::Error),
    /// `fsw_pack_create` returned null for this entry.
    #[error("fsw_pack_create failed for entry {0}")]
    Create(u32),
    /// The guest's allocator refused a ring region.
    #[error("guest allocator returned null for {0} bytes")]
    Alloc(usize),
    /// A region the guest handed back does not lie inside its memory.
    #[error("guest returned an out-of-bounds region at {offset} ({len} bytes)")]
    BadRegion {
        /// Offset the guest reported.
        offset: u32,
        /// Length requested.
        len: usize,
    },
    /// A guest-formatted region did not attach.
    #[error("guest ring region did not attach: {0}")]
    Ring(metor_fsw_ring::AttachError),
    /// The guest refused to format a ring into the region.
    #[error("guest could not format a ring at {offset} ({len} bytes)")]
    RingInit {
        /// Offset of the region.
        offset: u32,
        /// Length of the region.
        len: usize,
    },
    /// The guest trapped — including exhausting its fuel budget.
    #[error("wasm trap: {0}")]
    Trap(String),
}

/// Whether a [`WasmError::Trap`] was the fuel budget running out, as opposed
/// to a fault in the guest. Both are terminal for the occupant, but only the
/// first is the host's own policy biting.
pub fn is_out_of_fuel(err: &WasmError) -> bool {
    matches!(err, WasmError::Trap(m) if m.contains("fuel"))
}

/// One port's ring region inside guest memory.
///
/// Deliberately just the geometry: see the module docs on why a `RingBuffer`
/// is re-derived per access rather than held.
#[derive(Clone, Copy, Debug)]
pub struct GuestRing {
    /// Offset of the region within the guest's linear memory.
    pub offset: u32,
    /// Length of the region in bytes.
    pub len: usize,
    /// `ROLE_INPUT` or `ROLE_OUTPUT` from the pack ABI.
    pub role: u8,
}

/// The `fsw_pack_*` exports, resolved once at load.
struct Exports {
    open: TypedFunc<(), i32>,
    describe: TypedFunc<i32, i64>,
    manifest_ptr: TypedFunc<i32, i32>,
    create: TypedFunc<(i32, i32, i32, i32, i32), i32>,
    bind_init: TypedFunc<(i32, i32, i32, i32, i32, i32, i32), ()>,
    execute: TypedFunc<(i32, i64), i32>,
    shutdown: TypedFunc<i32, ()>,
    destroy: TypedFunc<i32, ()>,
    close: TypedFunc<i32, ()>,
    alloc: TypedFunc<i32, i32>,
    ring_init: TypedFunc<(i32, i32, i32, i32), i32>,
}

/// A loaded wasm pack: the instantiated module plus its opened pack pointer
/// and decoded manifest.
pub struct WasmPack {
    store: Store<()>,
    memory: Memory,
    exports: Exports,
    pack: i32,
    manifest: PackManifest,
    fuel_per_call: u64,
}

impl WasmPack {
    /// Instantiate `wasm`, check its ABI word, open its pack, and decode the
    /// manifest — the wasm shape of `DlPack::open`.
    ///
    /// `fuel_per_call` bounds every subsequent guest call. It is granted
    /// before instantiation too, because the module's start section is itself
    /// metered and an ungranted store traps immediately.
    pub fn open(wasm: &[u8], fuel_per_call: u64) -> Result<Self, WasmError> {
        let mut config = WasmConfig::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| WasmError::Module(e.to_string()))?;
        let mut store = Store::new(&engine, ());
        store
            .set_fuel(fuel_per_call)
            .map_err(|e| WasmError::Instantiate(e.to_string()))?;
        let linker: Linker<()> = Linker::new(&engine);
        let instance: Instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| WasmError::Instantiate(e.to_string()))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(WasmError::MissingExport("memory"))?;

        // The ABI word comes first, before anything else is called.
        let version: TypedFunc<(), i32> = typed(&instance, &store, "fsw_abi_version")?;
        store.set_fuel(fuel_per_call).ok();
        let found = version
            .call(&mut store, ())
            .map_err(|e| WasmError::Trap(e.to_string()))? as u32;
        if found != FSW_ABI_VERSION {
            return Err(WasmError::VersionMismatch {
                found,
                expected: FSW_ABI_VERSION,
            });
        }

        let exports = Exports {
            open: typed(&instance, &store, "fsw_pack_open")?,
            describe: typed(&instance, &store, "fsw_pack_describe")?,
            manifest_ptr: typed(&instance, &store, "fsw_pack_manifest_ptr")?,
            create: typed(&instance, &store, "fsw_pack_create")?,
            bind_init: typed(&instance, &store, "fsw_pack_bind_init")?,
            execute: typed(&instance, &store, "fsw_pack_execute")?,
            shutdown: typed(&instance, &store, "fsw_pack_shutdown")?,
            destroy: typed(&instance, &store, "fsw_pack_destroy")?,
            close: typed(&instance, &store, "fsw_pack_close")?,
            alloc: typed(&instance, &store, "fsw_pack_alloc")?,
            ring_init: typed(&instance, &store, "fsw_pack_ring_init")?,
        };

        let mut this = Self {
            store,
            memory,
            exports,
            pack: 0,
            manifest: PackManifest {
                systems: Vec::new(),
            },
            fuel_per_call,
        };

        this.pack = this.call(|e| e.open, ())?;
        if this.pack == 0 {
            return Err(WasmError::PackOpen);
        }
        this.manifest = this.read_manifest()?;
        Ok(this)
    }

    /// The decoded manifest: one entry per pack registration, exactly what the
    /// `.so` path decodes.
    pub fn manifest(&self) -> &PackManifest {
        &self.manifest
    }

    /// Describe, then copy the bytes out of guest memory and decode them.
    fn read_manifest(&mut self) -> Result<PackManifest, WasmError> {
        let pack = self.pack;
        let len = self.call(|e| e.describe, pack)?;
        let len = usize::try_from(len).map_err(|_| WasmError::Describe)?;
        let base = self.call(|e| e.manifest_ptr, pack)?;
        if base == 0 {
            return Err(WasmError::Describe);
        }
        let bytes = self.read(base as u32, len)?;
        postcard::from_bytes(&bytes).map_err(WasmError::Manifest)
    }

    /// Create entry `index` with `params`, returning the guest's instance
    /// pointer. `mount` is a `Mount` discriminant from the pack ABI.
    pub fn create(&mut self, index: u32, mount: u32, params: &[u8]) -> Result<i32, WasmError> {
        let params_at = if params.is_empty() {
            0
        } else {
            let at = self.alloc(params.len())?;
            self.write(at, params)?;
            at as i32
        };
        let pack = self.pack;
        let state = self.call(
            |e| e.create,
            (
                pack,
                index as i32,
                mount as i32,
                params_at,
                params.len() as i32,
            ),
        )?;
        if state == 0 {
            return Err(WasmError::Create(index));
        }
        Ok(state)
    }

    /// Allocate and format one ring region inside guest memory.
    ///
    /// The guest's own allocator provides the bytes (see the module docs), and
    /// the ring is formatted through a transient host-side handle over that
    /// region; the guest attaches to the same bytes at `bind_init`.
    pub fn add_ring(&mut self, cfg: Config, role: u8) -> Result<GuestRing, WasmError> {
        let len = region_len(&cfg);
        let offset = self.alloc(len)?;
        // The *guest* formats the region, not the host: the header records the
        // writing target's pointer width, and a wasm guest's `usize` is four
        // bytes where this host's is eight, so a host-formatted region would
        // be rejected on attach as `ArchMismatch`.
        let rc = self.call(
            |e| e.ring_init,
            (
                offset as i32,
                len as i32,
                cfg.capacity as i32,
                cfg.max_readers as i32,
            ),
        )?;
        if rc != 0 {
            return Err(WasmError::RingInit { offset, len });
        }
        // The host and guest now agree on the region format, so verify the
        // region attaches here rather than letting a bad one surface as an
        // opaque guest trap at bind.
        let slice = self.slice_mut(offset, len)?;
        let base = slice.as_mut_ptr();
        // SAFETY: the guest just formatted this region and nothing is reading
        // it; the handle is dropped before this returns.
        let probe = unsafe { RingBuffer::attach_raw(base, len) };
        match probe {
            Ok(ring) => drop(ring),
            Err(e) => return Err(WasmError::Ring(e)),
        }
        Ok(GuestRing { offset, len, role })
    }

    /// Bind `state`'s ports to `rings` and run its init phase.
    ///
    /// The `FswRing` array is itself staged in guest memory, since the guest
    /// dereferences it. Its layout is `{ base: u32, len: u32, role: u8 }` with
    /// the trailing padding a `repr(C)` struct carries on wasm32.
    pub fn bind_init(
        &mut self,
        state: i32,
        inputs: &[GuestRing],
        outputs: &[GuestRing],
        name: &str,
    ) -> Result<(), WasmError> {
        let inputs_at = self.stage_rings(inputs)?;
        let outputs_at = self.stage_rings(outputs)?;
        let name_at = if name.is_empty() {
            0
        } else {
            let at = self.alloc(name.len())?;
            self.write(at, name.as_bytes())?;
            at as i32
        };
        self.call(
            |e| e.bind_init,
            (
                state,
                inputs_at,
                inputs.len() as i32,
                outputs_at,
                outputs.len() as i32,
                name_at,
                name.len() as i32,
            ),
        )
    }

    /// One cycle. The raw status word is validated rather than transmuted, so
    /// an out-of-range discriminant folds to [`FswStatus::Panicked`].
    pub fn execute(&mut self, state: i32, now: u64) -> Result<FswStatus, WasmError> {
        let raw = self.call(|e| e.execute, (state, now as i64))?;
        Ok(FswStatus::from_raw(raw as u32))
    }

    /// Run the instance's shutdown phase.
    pub fn shutdown(&mut self, state: i32) -> Result<(), WasmError> {
        self.call(|e| e.shutdown, state)
    }

    /// Drop one instance's state inside the guest.
    pub fn destroy(&mut self, state: i32) -> Result<(), WasmError> {
        self.call(|e| e.destroy, state)
    }

    /// Drop the guest's `Pack`. Every instance must be destroyed first.
    pub fn close(&mut self) -> Result<(), WasmError> {
        let pack = self.pack;
        self.pack = 0;
        self.call(|e| e.close, pack)
    }

    /// Read `len` bytes of a guest ring region, for a host-side consumer.
    pub fn ring_bytes(&mut self, ring: &GuestRing) -> Result<Vec<u8>, WasmError> {
        self.read(ring.offset, ring.len)
    }

    // --- guest memory and calls -------------------------------------------

    /// Write the `FswRing` array a `bind_init` argument points at.
    fn stage_rings(&mut self, rings: &[GuestRing]) -> Result<i32, WasmError> {
        if rings.is_empty() {
            return Ok(0);
        }
        // `repr(C) { *mut u8, usize, u8 }` on wasm32: two 4-byte words then a
        // byte, padded to the struct's 4-byte alignment.
        const STRIDE: usize = 12;
        let mut buf = vec![0u8; rings.len() * STRIDE];
        for (slot, ring) in buf.chunks_exact_mut(STRIDE).zip(rings) {
            slot[0..4].copy_from_slice(&ring.offset.to_le_bytes());
            slot[4..8].copy_from_slice(&(ring.len as u32).to_le_bytes());
            slot[8] = ring.role;
        }
        let at = self.alloc(buf.len())?;
        self.write(at, &buf)?;
        Ok(at as i32)
    }

    /// Ask the guest's allocator for `len` bytes.
    fn alloc(&mut self, len: usize) -> Result<u32, WasmError> {
        let at = self.call(|e| e.alloc, len as i32)?;
        if at == 0 {
            return Err(WasmError::Alloc(len));
        }
        Ok(at as u32)
    }

    /// A bounds-checked mutable slice of guest memory.
    fn slice_mut(&mut self, offset: u32, len: usize) -> Result<&mut [u8], WasmError> {
        let data = self.memory.data_mut(&mut self.store);
        let start = offset as usize;
        data.get_mut(start..start + len)
            .ok_or(WasmError::BadRegion { offset, len })
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), WasmError> {
        self.slice_mut(offset, bytes.len())?.copy_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, WasmError> {
        Ok(self.slice_mut(offset, len)?.to_vec())
    }

    /// Call one export under a fresh fuel budget, mapping a trap — including
    /// exhaustion — to [`WasmError::Trap`].
    fn call<P, R>(
        &mut self,
        pick: impl Fn(&Exports) -> TypedFunc<P, R>,
        params: P,
    ) -> Result<R, WasmError>
    where
        P: wasmi::WasmParams,
        R: wasmi::WasmResults,
    {
        let f = pick(&self.exports);
        // Refill before every call, so the budget bounds one call rather than
        // the instance's whole lifetime.
        let _ = self.store.set_fuel(self.fuel_per_call);
        f.call(&mut self.store, params)
            .map_err(|e| WasmError::Trap(e.to_string()))
    }
}

/// Resolve one typed export, naming it when it is missing.
fn typed<P, R>(
    instance: &Instance,
    store: &Store<()>,
    name: &'static str,
) -> Result<TypedFunc<P, R>, WasmError>
where
    P: wasmi::WasmParams,
    R: wasmi::WasmResults,
{
    instance
        .get_typed_func(store, name)
        .map_err(|_| WasmError::MissingExport(name))
}

#[cfg(test)]
mod tests;
