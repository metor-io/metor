//! Loading and driving pack modules compiled to WebAssembly.
//!
//! [`WasmPack`] is the sandboxed twin of [`DlPack`](crate::dl::DlPack): it
//! drives the very same pack ABI, over the same `fsw_pack_*` entry points, in
//! the same order, and only the boundary changes. A `.so` is `dlopen`'d into the
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
//! The host keeps matching rings on both sides and pumps records across the
//! boundary once per cycle. The ring format stays unchanged; only the copy
//! between host and guest address spaces is new.
//!
//! ## No imports
//!
//! A guest module imports nothing. `wasm32-unknown-unknown` has no clock, and
//! nothing in a guest needs one: the host times each execute and publishes
//! the `system_status` record itself.
//!
//! ## Stable memory and persistent ring roles
//!
//! A ring reader owns a persistent slot in the region header, so rebuilding it
//! each cycle would rejoin at the live edge and lose queued records. The host
//! therefore retains bridge handles over guest memory. The store enforces the
//! target's memory ceiling during load and bind, then freezes linear memory
//! before those handles are constructed. A later `memory.grow` traps, and the
//! bridge is explicitly dropped before the store releases the backing bytes.
//!
//! ## The trust boundary
//!
//! Same discipline as `dl.rs`: the module is foreign code, so nothing it
//! returns is trusted as a Rust value. `fsw_pack_execute` yields a raw `u32`
//! that is validated and folded to [`FswStatus::Panicked`] when unknown, a
//! manifest that fails to decode is a clean error rather than a panic, and
//! every trap, including running out of fuel, becomes a [`WasmError`]
//! instead of propagating.

use metor_fsw_2_core::abi::{FSW_ABI_VERSION, FswStatus, PackEntryDesc, PackManifest};
use metor_fsw_ring::{Config, RingBuffer, region_len};
use wasmi::{
    Config as WasmConfig, Engine, Instance, Linker, Memory, Module, ResourceLimiter, Store,
    TypedFunc,
};
use wasmi_core::LimiterError;

/// Default maximum linear-memory footprint of one wasm occupant.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Store-owned resource policy. Memory may grow during load and bind up to
/// `max_memory`, then is frozen before any host ring handle is retained.
struct StoreState {
    max_memory: usize,
    frozen_memory: Option<usize>,
}

impl ResourceLimiter for StoreState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        if maximum.is_some_and(|maximum| desired > maximum)
            || desired > self.max_memory
            || self.frozen_memory.is_some_and(|frozen| desired > frozen)
        {
            return Err(LimiterError::ResourceLimiterDeniedAllocation);
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        const MAX_TABLE_ELEMENTS: usize = 65_536;
        if desired > MAX_TABLE_ELEMENTS || maximum.is_some_and(|maximum| desired > maximum) {
            return Err(LimiterError::ResourceLimiterDeniedAllocation);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        1
    }

    fn memories(&self) -> usize {
        1
    }
}

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
    /// A runtime-reloaded module no longer exports the allowed entry.
    #[error("wasm module no longer exports entry `{0}`")]
    EntryMissing(String),
    /// A runtime-reloaded entry changed the contract validated at build.
    #[error("wasm entry `{0}` changed its descriptor, params schema, or reloadability")]
    EntryChanged(String),
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
    /// The guest trapped, including by exhausting its fuel budget.
    #[error("wasm trap: {0}")]
    Trap(String),
    /// A ring's reader table was full, so the bridge could not register.
    #[error("ring has no free reader slot for the bridge")]
    NoSlot,
    /// A ring already had a writer, so the bridge could not claim it.
    #[error("ring writer already claimed")]
    WriterClaimed,
    /// A ring owned by the guest was structurally corrupted.
    #[error("guest ring is corrupt: {0}")]
    RingRead(metor_fsw_ring::ReadError),
    /// The guest's linear memory grew, so any handle the host holds over a
    /// region inside it may now dangle.
    #[error("guest memory moved ({was} -> {now} bytes); ring handles are stale")]
    MemoryMoved {
        /// Size recorded when the handles were taken.
        was: usize,
        /// Size observed now.
        now: usize,
    },
}

/// One port's ring region inside guest memory.
///
/// Deliberately just the geometry used while constructing the persistent
/// bridge handles described in the module documentation.
#[derive(Clone, Copy, Debug)]
pub struct GuestRing {
    /// Offset of the region within the guest's linear memory.
    pub offset: u32,
    /// Length of the region in bytes.
    pub len: usize,
    /// `ROLE_INPUT` or `ROLE_OUTPUT` from the pack ABI.
    pub role: u8,
}

/// Stable identity of the ABI-relevant parts of one manifest entry.
/// Documentation and default values may change across a compatible hot reload;
/// positional ports, parameter encoding, and reloadability may not.
pub(crate) fn entry_identity(entry: &PackEntryDesc) -> Vec<u8> {
    postcard::to_allocvec(&(&entry.descriptor, &entry.params_schema, entry.reloadable))
        .expect("pack entry identity encodes")
}

/// `fsw_pack_create`'s signature: `(pack, index, mount, params, params_len)`.
type CreateFn = TypedFunc<(i32, i32, i32, i32, i32), i32>;
/// `fsw_pack_bind_init`'s: `(state, inputs, n_in, outputs, n_out, name, len)`.
type BindInitFn = TypedFunc<(i32, i32, i32, i32, i32, i32, i32), ()>;
/// `fsw_pack_ring_init`'s: `(ptr, len, capacity, max_readers)`.
type RingInitFn = TypedFunc<(i32, i32, i32, i32), i32>;

/// The `fsw_pack_*` exports, resolved once at load.
struct Exports {
    open: TypedFunc<(), i32>,
    describe: TypedFunc<i32, i64>,
    manifest_ptr: TypedFunc<i32, i32>,
    create: CreateFn,
    bind_init: BindInitFn,
    execute: TypedFunc<(i32, i64), i32>,
    shutdown: TypedFunc<i32, ()>,
    destroy: TypedFunc<i32, ()>,
    close: TypedFunc<i32, ()>,
    alloc: TypedFunc<i32, i32>,
    ring_init: RingInitFn,
    set_now: TypedFunc<i64, ()>,
    /// The expr-manifest pair a compiled Python pack also exports; absent on
    /// an ordinary Rust-authored pack.
    expr_describe: Option<TypedFunc<(), i32>>,
    expr_manifest_ptr: Option<TypedFunc<(), i32>>,
}

/// A loaded wasm pack: the instantiated module plus its opened pack pointer
/// and decoded manifest.
pub struct WasmPack {
    store: Store<StoreState>,
    memory: Memory,
    exports: Exports,
    pack: i32,
    manifest: PackManifest,
    fuel_per_call: u64,
    /// Size of the guest's linear memory when its ring regions were taken.
    ///
    /// A host handle over a guest region is a raw pointer into the
    /// interpreter's backing buffer, and `memory.grow` reallocates that
    /// buffer. A guest's allocator grows when it needs heap (a sequence
    /// calling `progress` allocates a `String`), so growth is not
    /// hypothetical, and a stale handle would be a use-after-free rather than
    /// a wrong answer. [`check_memory_stable`](Self::check_memory_stable)
    /// turns that into a clean occupant failure.
    pinned_len: usize,
}

impl WasmPack {
    /// Instantiate `wasm`, check its ABI word, open its pack, and decode the
    /// manifest, the wasm shape of `DlPack::open`.
    ///
    /// `fuel_per_call` bounds every subsequent guest call. It is granted
    /// before instantiation too, because the module's start section is itself
    /// metered and an ungranted store traps immediately.
    pub fn open(wasm: &[u8], fuel_per_call: u64) -> Result<Self, WasmError> {
        Self::open_with_memory_limit(wasm, fuel_per_call, DEFAULT_MAX_MEMORY_BYTES)
    }

    /// [`open`](Self::open) with an explicit linear-memory ceiling.
    pub fn open_with_memory_limit(
        wasm: &[u8],
        fuel_per_call: u64,
        max_memory: usize,
    ) -> Result<Self, WasmError> {
        let mut config = WasmConfig::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| WasmError::Module(e.to_string()))?;
        let mut store = Store::new(
            &engine,
            StoreState {
                max_memory,
                frozen_memory: None,
            },
        );
        store.limiter(|state| state);
        store
            .set_fuel(fuel_per_call)
            .map_err(|e| WasmError::Instantiate(e.to_string()))?;
        let linker: Linker<StoreState> = Linker::new(&engine);
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
            set_now: typed(&instance, &store, "fsw_pack_set_now")?,
            expr_describe: typed(&instance, &store, "expr_describe").ok(),
            expr_manifest_ptr: typed(&instance, &store, "expr_manifest_ptr").ok(),
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
            pinned_len: 0,
        };

        this.pack = this.call(|e| e.open, ())?;
        if this.pack == 0 {
            return Err(WasmError::PackOpen);
        }
        this.manifest = this.read_manifest()?;
        Ok(this)
    }

    /// Record the guest's current memory size as the baseline every later
    /// [`check_memory_stable`](Self::check_memory_stable) compares against.
    ///
    /// Called once the occupant's regions exist, i.e. after bind.
    pub fn pin_memory(&mut self) {
        self.pinned_len = self.memory.data(&self.store).len();
        self.store.data_mut().frozen_memory = Some(self.pinned_len);
    }

    /// Whether the guest's memory is still where it was when
    /// [`pin_memory`](Self::pin_memory) ran.
    ///
    /// The host must call this before touching a region through a held handle.
    /// Failing loudly is the point: the alternative to noticing growth is
    /// reading freed memory.
    pub fn check_memory_stable(&self) -> Result<(), WasmError> {
        let now = self.memory.data(&self.store).len();
        if now != self.pinned_len {
            return Err(WasmError::MemoryMoved {
                was: self.pinned_len,
                now,
            });
        }
        Ok(())
    }

    /// Change the budget granted to each subsequent guest call.
    ///
    /// Binding an occupant costs far more fuel than running one cycle of it,
    /// so a budget tight enough to bound a *poll* would refuse the bind. The
    /// two are set separately for that reason.
    pub fn set_fuel_per_call(&mut self, fuel: u64) {
        self.fuel_per_call = fuel;
    }

    /// The base of the guest's linear memory.
    ///
    /// Only valid while the memory does not move; every consumer must pair it
    /// with [`check_memory_stable`](Self::check_memory_stable).
    pub fn memory_base(&mut self) -> *mut u8 {
        self.memory.data_mut(&mut self.store).as_mut_ptr()
    }

    /// The guest's current linear-memory size.
    pub fn memory_len(&self) -> usize {
        self.memory.data(&self.store).len()
    }

    /// Ask the guest's allocator for `len` bytes, for tests that need to move
    /// its memory on purpose.
    #[cfg(test)]
    pub(crate) fn alloc_for_test(&mut self, len: usize) -> Result<u32, WasmError> {
        self.alloc(len)
    }

    /// The decoded manifest: one entry per pack registration, exactly what the
    /// `.so` path decodes.
    pub fn manifest(&self) -> &PackManifest {
        &self.manifest
    }

    /// The expr manifest a compiled Python pack bakes beside its pack one,
    /// for the resolver's edge synthesis. `None` for a module without the
    /// expr export family (an ordinary Rust-authored pack).
    pub(crate) fn expr_manifest_bytes(&mut self) -> Result<Option<Vec<u8>>, WasmError> {
        let (Some(describe), Some(manifest_ptr)) =
            (self.exports.expr_describe, self.exports.expr_manifest_ptr)
        else {
            return Ok(None);
        };
        let len = self.call(|_| describe, ())? as usize;
        let at = self.call(|_| manifest_ptr, ())?;
        Ok(Some(self.read(at as u32, len)?))
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
        postcard::from_bytes(&self.read(base as u32, len)?).map_err(WasmError::Manifest)
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
        // The guest formats the region allocated by its own allocator. The
        // ring format itself uses explicit-width fields and is compatible
        // across the guest and host pointer widths.
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
        drop(unsafe { RingBuffer::attach_raw(base, len) }.map_err(WasmError::Ring)?);
        Ok(GuestRing { offset, len, role })
    }

    /// Publish a cycle timestamp on the guest's ambient clock.
    ///
    /// Required before [`bind_init`](Self::bind_init): the guest's clock is
    /// unset until the first `execute` republishes it, and anything init
    /// stamps would otherwise reach for wall time, which
    /// `wasm32-unknown-unknown` does not have, so `SystemTime::now` panics
    /// and the guest traps.
    pub fn set_now(&mut self, now: u64) -> Result<(), WasmError> {
        self.call(|e| e.set_now, now as i64)
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
        // Give init the cycle's time axis before it can reach for wall time.
        self.set_now(0)?;
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

    /// Call one export under a fresh fuel budget, mapping a trap, including
    /// exhaustion, to [`WasmError::Trap`].
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
    store: &Store<StoreState>,
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

/// Give the guest one ring region matching each host region's geometry, so
/// the two sides of a bridge leg always agree on what a record can be.
pub(crate) fn mirror_rings(
    pack: &mut WasmPack,
    host: &[metor_fsw_2_core::abi::FswRing],
    role: u8,
) -> Result<Vec<GuestRing>, WasmError> {
    host.iter()
        .map(|r| {
            // SAFETY: a coordinator-owned region, live for the runner's life.
            let cfg =
                unsafe { metor_fsw_ring::config_of(r.base, r.len) }.map_err(WasmError::Ring)?;
            pack.add_ring(cfg, role)
        })
        .collect()
}

mod bridge;
mod slot;
mod wired;
pub use bridge::RingBridge;
pub use slot::WasmSlot;
pub(crate) use wired::WasmCyclic;

#[cfg(test)]
mod tests;
