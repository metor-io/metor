//! One instantiated module over a host-placed memory. This is the only file
//! that names the interpreter.

use core::ptr::NonNull;
use std::alloc::Layout;

use metor_fsw_3_ring::PAYLOAD_ALIGNMENT;
use wasmi::{
    CompilationMode, Config, Engine, ExternType, Linker, Memory, MemoryType, Store, StoreLimits,
    StoreLimitsBuilder, TrapCode, TypedFunc, WasmParams, WasmResults,
};

/// Bytes in one wasm page.
pub const PAGE: usize = 64 * 1024;

/// Pages a 32-bit memory can address.
const MAX_PAGES: usize = 1 << 16;

/// Value-stack slots the interpreter reserves for a call, 512 KiB of
/// 8-byte slots. A pack step runs far below this, so the stack never grows,
/// and so never allocates, mid-cycle.
const STACK: usize = 64 * 1024;

/// Elements a guest's table may hold. rustc's table has one per function
/// whose address is taken, which is a few hundred for a pack.
pub const TABLE_ELEMENTS: usize = 16 * 1024;

/// A `WasmError` is why a module cannot run as a guest, or why the host
/// refused an access to its memory.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("the module did not load: {0}")]
    Load(#[from] wasmi::Error),
    #[error("the module imports no 32-bit `env.memory`")]
    MemoryImport,
    #[error("the module needs {needed} bytes of memory, over the {ceiling}-byte ceiling")]
    MemoryTooSmall { needed: u64, ceiling: usize },
    #[error("a memory ceiling of {0} bytes is not 1 to 65536 pages")]
    BadCeiling(usize),
    #[error("no host memory for a {0}-byte guest")]
    OutOfMemory(usize),
    #[error("the module exports no `{0}`")]
    MissingExport(&'static str),
    #[error("the module's `{0}` has the wrong signature")]
    Signature(&'static str),
    #[error("{len} bytes at {offset} are outside the guest's memory")]
    OutOfBounds { offset: u32, len: usize },
    #[error("guest offset {0} is not 16-byte aligned")]
    Misaligned(u32),
    #[error("the guest allocator refused {0} bytes")]
    Alloc(u32),
    #[error("{0} bytes do not fit the guest")]
    TooLarge(usize),
}

/// Why a call stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    OutOfFuel,
    /// `unreachable`, which is what a guest panic compiles to.
    Unreachable,
    MemoryOutOfBounds,
    StackOverflow,
    /// Any other trap: a bad indirect call, integer overflow, and so on.
    Other,
}

impl core::fmt::Display for Trap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Trap {}

impl Trap {
    /// The text a fault line carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Trap::OutOfFuel => "out of fuel",
            Trap::Unreachable => "unreachable",
            Trap::MemoryOutOfBounds => "memory out of bounds",
            Trap::StackOverflow => "stack overflow",
            Trap::Other => "trap",
        }
    }

    fn of(error: &wasmi::Error) -> Trap {
        match error.as_trap_code() {
            Some(TrapCode::OutOfFuel) => Trap::OutOfFuel,
            Some(TrapCode::UnreachableCodeReached) => Trap::Unreachable,
            Some(TrapCode::MemoryOutOfBounds) => Trap::MemoryOutOfBounds,
            Some(TrapCode::StackOverflow) => Trap::StackOverflow,
            _ => Trap::Other,
        }
    }
}

/// A `Module` is a compiled pack module and the engine it runs on.
pub struct Module(wasmi::Module);

impl Module {
    /// Compiles every function now, so no call translates code mid-cycle.
    pub fn compile(bytes: &[u8]) -> Result<Module, WasmError> {
        let mut config = Config::default();
        config
            .compilation_mode(CompilationMode::Eager)
            .consume_fuel(true)
            .set_min_stack_height(STACK);
        let engine = Engine::new(&config);
        Ok(Module(wasmi::Module::new(&engine, bytes)?))
    }

    /// The memory the module imports as `env.memory`.
    fn memory_import(&self) -> Result<MemoryType, WasmError> {
        let import = self
            .0
            .imports()
            .find(|import| import.module() == "env" && import.name() == "memory");
        match import.as_ref().map(|import| import.ty()) {
            Some(ExternType::Memory(ty)) if !ty.is_64() => Ok(*ty),
            _ => Err(WasmError::MemoryImport),
        }
    }
}

/// Where an export writes a descriptor: `capacity` bytes at `dst`, and the
/// `usize` slot at `written` it stores the length in.
#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub dst: u32,
    pub capacity: u32,
    pub written: u32,
}

/// The exports the ABI fixes, resolved to typed functions once.
struct Exports {
    abi_version: TypedFunc<(), u32>,
    pack_def: TypedFunc<(u32, u32, u32), u32>,
    def: TypedFunc<(u32, u32, u32, u32, u32), u32>,
    create: TypedFunc<(u32, u32, u32, u32, u32, u32), u32>,
    execute: TypedFunc<(u32, i64), u32>,
    alloc: TypedFunc<(u32, u32), u32>,
    free: TypedFunc<(u32, u32, u32), ()>,
}

/// A `Guest` is one instantiated module: its store, its memory, and the
/// ABI's exports.
pub struct Guest {
    /// Declared before `_arena`, so it drops while the arena backs its memory.
    store: Store<StoreLimits>,
    memory: Memory,
    exports: Exports,
    _arena: Arena,
}

impl Guest {
    /// Instantiates `module` over a fresh memory of at most `ceiling` bytes,
    /// rounded up to whole pages. Instantiation copies the module's data in
    /// under a grant of `fuel`.
    pub fn new(module: &Module, ceiling: usize, fuel: u64) -> Result<Guest, WasmError> {
        let ty = module.memory_import()?;
        let arena = Arena::new(ceiling)?;
        let ty = bounded(ty, &arena)?;
        let mut store = Store::new(module.0.engine(), limits());
        store.limiter(|limits| limits);
        store.set_fuel(fuel)?;
        // SAFETY: the store is the only user of these bytes from here. It is
        // declared after `arena` here and before it in `Guest`, so it drops
        // first either way.
        let bytes = unsafe { arena.bytes() };
        let memory = Memory::new_static(&mut store, ty, bytes)?;
        let mut linker = Linker::new(module.0.engine());
        linker
            .define("env", "memory", memory)
            .map_err(wasmi::Error::from)?;
        let instance = linker.instantiate_and_start(&mut store, &module.0)?;
        let exports = Exports::resolve(&instance, &store)?;
        Ok(Guest {
            store,
            memory,
            exports,
            _arena: arena,
        })
    }

    /// `metor_fsw_abi_version`.
    pub fn abi_version(&mut self, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.abi_version, (), fuel)
    }

    /// `metor_fsw_pack_def`, writing the descriptor into `out`.
    pub fn pack_def(&mut self, out: Out, fuel: u64) -> Result<u32, Trap> {
        let args = (out.dst, out.capacity, out.written);
        call(&mut self.store, self.exports.pack_def, args, fuel)
    }

    /// `metor_fsw_def` for the `RawSlice`s at `ty` and `cx`, into `out`.
    pub fn def(&mut self, ty: u32, cx: u32, out: Out, fuel: u64) -> Result<u32, Trap> {
        let args = (ty, cx, out.dst, out.capacity, out.written);
        call(&mut self.store, self.exports.def, args, fuel)
    }

    /// `metor_fsw_create` over `(ty, params, def, inputs, outputs, error)`,
    /// a `RawSlice` address each, returning the instance, zero on a refusal.
    pub fn create(&mut self, args: (u32, u32, u32, u32, u32, u32), fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.create, args, fuel)
    }

    /// `metor_fsw_execute`, returning the status word.
    pub fn execute(&mut self, instance: u32, now: i64, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.execute, (instance, now), fuel)
    }

    /// `metor_fsw_alloc`, returning the offset, zero when it refused.
    pub fn alloc(&mut self, len: u32, align: u32, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.alloc, (len, align), fuel)
    }

    /// `metor_fsw_free`.
    pub fn free(&mut self, ptr: u32, len: u32, align: u32, fuel: u64) -> Result<(), Trap> {
        call(&mut self.store, self.exports.free, (ptr, len, align), fuel)
    }

    /// Copies `dst.len()` bytes at `offset` out of guest memory.
    pub fn read(&self, offset: u32, dst: &mut [u8]) -> Result<(), WasmError> {
        let src = self.at(offset, dst.len())?;
        // SAFETY: `at` checked the range lies in the memory, and no guest call
        // runs while `self` is borrowed.
        unsafe { core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    /// Copies `src` into guest memory at `offset`.
    pub fn write(&mut self, offset: u32, src: &[u8]) -> Result<(), WasmError> {
        let dst = self.at(offset, src.len())?;
        // SAFETY: as in `read`; `src` is host memory, so the two do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
        Ok(())
    }

    /// The host address of `len` bytes at `offset`, 16-byte aligned, for
    /// formatting a ring there.
    pub fn region(&self, offset: u32, len: usize) -> Result<NonNull<u8>, WasmError> {
        if !(offset as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
            return Err(WasmError::Misaligned(offset));
        }
        let ptr = self.at(offset, len)?;
        NonNull::new(ptr).ok_or(WasmError::OutOfBounds { offset, len })
    }

    /// The host address of `len` bytes at `offset`, checked against the
    /// memory's current size. The arena is page aligned, so an offset's
    /// alignment is its address's.
    fn at(&self, offset: u32, len: usize) -> Result<*mut u8, WasmError> {
        let size = self.memory.data_size(&self.store);
        let start = offset as usize;
        match start.checked_add(len) {
            // SAFETY: `start` is within the memory, so the offset stays in
            // the one allocation `data_ptr` points into.
            Some(end) if end <= size => Ok(unsafe { self.memory.data_ptr(&self.store).add(start) }),
            _ => Err(WasmError::OutOfBounds { offset, len }),
        }
    }
}

impl Exports {
    fn resolve(instance: &wasmi::Instance, store: &Store<StoreLimits>) -> Result<Self, WasmError> {
        Ok(Exports {
            abi_version: typed(instance, store, "metor_fsw_abi_version")?,
            pack_def: typed(instance, store, "metor_fsw_pack_def")?,
            def: typed(instance, store, "metor_fsw_def")?,
            create: typed(instance, store, "metor_fsw_create")?,
            execute: typed(instance, store, "metor_fsw_execute")?,
            alloc: typed(instance, store, "metor_fsw_alloc")?,
            free: typed(instance, store, "metor_fsw_free")?,
        })
    }
}

/// One export as a typed function, naming it when it is missing or its
/// signature differs.
fn typed<P: WasmParams, R: WasmResults>(
    instance: &wasmi::Instance,
    store: &Store<StoreLimits>,
    name: &'static str,
) -> Result<TypedFunc<P, R>, WasmError> {
    let func = instance
        .get_func(store, name)
        .ok_or(WasmError::MissingExport(name))?;
    func.typed(store).map_err(|_| WasmError::Signature(name))
}

/// Calls `func` under a fresh grant of `fuel`.
fn call<P: WasmParams, R: WasmResults>(
    store: &mut Store<StoreLimits>,
    func: TypedFunc<P, R>,
    params: P,
    fuel: u64,
) -> Result<R, Trap> {
    // Every engine `Module::compile` builds meters fuel, so this cannot fail.
    store.set_fuel(fuel).map_err(|_| Trap::Other)?;
    func.call(store, params).map_err(|e| Trap::of(&e))
}

/// What a store may hold: one instance, one table, and the host's memory,
/// which instantiation counts again as the module's import.
fn limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memories(2)
        .tables(1)
        .table_elements(TABLE_ELEMENTS)
        .instances(1)
        .build()
}

/// The import's type, with its maximum capped at the arena's pages.
fn bounded(ty: MemoryType, arena: &Arena) -> Result<MemoryType, WasmError> {
    let pages = (arena.len() / PAGE) as u64;
    if ty.minimum() > pages {
        return Err(WasmError::MemoryTooSmall {
            needed: ty.minimum().saturating_mul(PAGE as u64),
            ceiling: arena.len(),
        });
    }
    let mut builder = MemoryType::builder();
    builder
        .min(ty.minimum())
        .max(Some(ty.maximum().map_or(pages, |max| max.min(pages))));
    builder.build().map_err(|e| wasmi::Error::from(e).into())
}

/// An `Arena` is the page-aligned, zeroed allocation a guest's memory lives in.
struct Arena {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Arena {
    /// Whole pages covering `ceiling` bytes.
    fn new(ceiling: usize) -> Result<Arena, WasmError> {
        let pages = ceiling.div_ceil(PAGE);
        if pages == 0 || pages > MAX_PAGES {
            return Err(WasmError::BadCeiling(ceiling));
        }
        let len = pages * PAGE;
        let layout =
            Layout::from_size_align(len, PAGE).map_err(|_| WasmError::BadCeiling(ceiling))?;
        // SAFETY: `len` is nonzero.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(WasmError::OutOfMemory(len))?;
        Ok(Arena { ptr, layout })
    }

    fn len(&self) -> usize {
        self.layout.size()
    }

    /// The arena as one slice, for the one user of its bytes.
    ///
    /// # Safety
    /// Nothing else accesses the bytes while the slice or anything derived
    /// from it is live, and none of those is used after the arena drops.
    unsafe fn bytes(&self) -> &'static mut [u8] {
        // SAFETY: `new` allocated `len` bytes at `ptr`; the caller's contract
        // covers aliasing and lifetime.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len()) }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: `new` allocated `ptr` with this layout, and it is freed once.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}
