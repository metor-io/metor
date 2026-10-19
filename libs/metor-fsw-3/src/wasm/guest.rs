//! Runs one instantiated module in memory the host allocates.
//!
//! This is the only file that uses the interpreter directly.

use core::ptr::NonNull;
use std::alloc::Layout;

use metor_fsw_3_ring::PAYLOAD_ALIGNMENT;

use super::DEFAULT_SETUP_FUEL;
use crate::dl::PackError;
use wasmi::{
    CompilationMode, Config, Engine, ExternType, Linker, Memory, MemoryType, Store, StoreLimits,
    StoreLimitsBuilder, TrapCode, TypedFunc, WasmParams, WasmResults,
};

/// The arguments `metor_fsw_create` takes: guest addresses, but the seed.
pub type CreateArgs = (u32, u32, u32, u32, u32, u32, u64, u32, u32, u32, u32);

/// The number of bytes in one wasm page.
pub const PAGE: usize = 64 * 1024;

/// The number of pages a 32-bit memory can address.
const MAX_PAGES: usize = 1 << 16;

/// The number of value-stack slots the interpreter reserves for each call.
///
/// That is 512 KiB of 8-byte slots. A pack step uses far less, so the stack
/// never grows mid-cycle, and so never allocates.
const STACK: usize = 64 * 1024;

/// The maximum number of elements in a guest's table.
///
/// rustc adds one table entry for each function whose address is taken. A
/// pack has a few hundred.
pub const TABLE_ELEMENTS: usize = 16 * 1024;

/// An error from running a module as a guest, or from accessing guest memory.
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

/// Why a guest call stopped before returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    OutOfFuel,
    /// The guest hit `unreachable`, which is what a guest panic compiles to.
    Unreachable,
    MemoryOutOfBounds,
    StackOverflow,
    /// Any other trap, such as a bad indirect call or integer overflow.
    Other,
}

impl core::fmt::Display for Trap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Trap {}

impl Trap {
    /// Returns the text used in the fault log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Trap::OutOfFuel => "out of fuel",
            Trap::Unreachable => "unreachable",
            Trap::MemoryOutOfBounds => "memory out of bounds",
            Trap::StackOverflow => "stack overflow",
            Trap::Other => "trap",
        }
    }
}

impl From<wasmi::Error> for Trap {
    fn from(error: wasmi::Error) -> Trap {
        match error.as_trap_code() {
            Some(TrapCode::OutOfFuel) => Trap::OutOfFuel,
            Some(TrapCode::UnreachableCodeReached) => Trap::Unreachable,
            Some(TrapCode::MemoryOutOfBounds) => Trap::MemoryOutOfBounds,
            Some(TrapCode::StackOverflow) => Trap::StackOverflow,
            _ => Trap::Other,
        }
    }
}

/// A compiled pack module.
pub struct Module(wasmi::Module);

impl Module {
    /// Compiles `bytes` into a module.
    ///
    /// Every function is compiled up front, so no call has to translate code
    /// mid-cycle.
    pub fn compile(bytes: &[u8]) -> Result<Module, WasmError> {
        let mut config = Config::default();
        config
            .compilation_mode(CompilationMode::Eager)
            .consume_fuel(true)
            .set_min_stack_height(STACK);
        let engine = Engine::new(&config);
        Ok(Module(wasmi::Module::new(&engine, bytes)?))
    }

    /// Returns the type of the memory the module imports as `env.memory`.
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

/// Where an export writes its output.
///
/// The export writes up to `capacity` bytes at `dst`, and stores the length it
/// wrote in the `usize` at `written`.
#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub dst: u32,
    pub capacity: u32,
    pub written: u32,
}

/// The ABI's exports, resolved to typed functions once.
struct Exports {
    abi_version: TypedFunc<(), u32>,
    pack_def: TypedFunc<(u32, u32, u32), u32>,
    def: TypedFunc<(u32, u32, u32, u32, u32), u32>,
    create: TypedFunc<CreateArgs, u32>,
    execute: TypedFunc<(u32, i64), u32>,
    alloc: TypedFunc<(u32, u32), u32>,
    free: TypedFunc<(u32, u32, u32), ()>,
}

/// One running instance of a pack module.
///
/// The host allocates the guest's memory, up to a fixed ceiling. Every call
/// into the guest runs with a fuel budget.
pub struct Guest {
    /// Must be declared before `backing`, so the store drops while its memory
    /// is still allocated.
    store: Store<StoreLimits>,
    memory: Memory,
    exports: Exports,
    backing: GuestMemory,
}

impl Guest {
    /// Instantiates `module` in a new memory of at most `ceiling` bytes.
    ///
    /// The ceiling is rounded up to whole pages. Instantiation copies the
    /// module's data into memory and may use up to `fuel`.
    pub fn new(module: &Module, ceiling: usize, fuel: u64) -> Result<Guest, WasmError> {
        let backing = GuestMemory::new(ceiling)?;
        let ty = backing.bound(module.memory_import()?)?;
        // A store holds one instance, one table, and the host's memory. That
        // memory counts twice, because instantiation counts it again as the
        // module's import.
        let limits = StoreLimitsBuilder::new()
            .memories(2)
            .tables(1)
            .table_elements(TABLE_ELEMENTS)
            .instances(1)
            .build();
        let mut store = Store::new(module.0.engine(), limits);
        store.limiter(|limits| limits);
        store.set_fuel(fuel)?;
        // SAFETY: the store is the only user of these bytes from here. It is
        // declared after `backing` here and before it in `Guest`, so it drops
        // first either way.
        let bytes = unsafe { backing.bytes() };
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
            backing,
        })
    }

    /// Calls `metor_fsw_abi_version`.
    pub fn abi_version(&mut self, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.abi_version, (), fuel)
    }

    /// Calls `metor_fsw_pack_def`, which writes the descriptor into `out`.
    pub fn pack_def(&mut self, out: Out, fuel: u64) -> Result<u32, Trap> {
        let args = (out.dst, out.capacity, out.written);
        call(&mut self.store, self.exports.pack_def, args, fuel)
    }

    /// Calls `metor_fsw_def` with the `RawSlice`s at `ty` and `cx`, writing
    /// into `out`.
    pub fn def(&mut self, ty: u32, cx: u32, out: Out, fuel: u64) -> Result<u32, Trap> {
        let args = (ty, cx, out.dst, out.capacity, out.written);
        call(&mut self.store, self.exports.def, args, fuel)
    }

    /// Calls `metor_fsw_create` and returns the status word.
    ///
    /// `args` holds the guest addresses of the `RawSlice`s for
    /// `(ty, params, def, inputs, outputs)` and of the `RawRing` table, the
    /// seed, then the addresses of the instance slot and of the error buffer,
    /// its capacity, and its written length.
    pub fn create(&mut self, args: CreateArgs, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.create, args, fuel)
    }

    /// Calls `metor_fsw_execute` and returns the status word.
    pub fn execute(&mut self, instance: u32, now: i64, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.execute, (instance, now), fuel)
    }

    /// Calls `metor_fsw_alloc` and returns the offset, or zero if allocation
    /// failed.
    pub fn alloc(&mut self, len: u32, align: u32, fuel: u64) -> Result<u32, Trap> {
        call(&mut self.store, self.exports.alloc, (len, align), fuel)
    }

    /// Calls `metor_fsw_free`.
    pub fn free(&mut self, ptr: u32, len: u32, align: u32, fuel: u64) -> Result<(), Trap> {
        call(&mut self.store, self.exports.free, (ptr, len, align), fuel)
    }

    /// Allocates `len` bytes with the guest's allocator, using setup fuel.
    ///
    /// Fails if the allocator returns the null offset.
    pub fn alloc_nonnull(&mut self, len: u32, align: u32) -> Result<u32, PackError> {
        match self.alloc(len, align, DEFAULT_SETUP_FUEL)? {
            0 => Err(WasmError::Alloc(len).into()),
            offset => Ok(offset),
        }
    }

    /// Reads the little-endian `u32` at `offset`.
    pub fn read_u32(&self, offset: u32) -> Result<u32, WasmError> {
        let mut word = [0; 4];
        self.read(offset, &mut word)?;
        Ok(u32::from_le_bytes(word))
    }

    /// Copies `dst.len()` bytes from guest memory at `offset` into `dst`.
    pub fn read(&self, offset: u32, dst: &mut [u8]) -> Result<(), WasmError> {
        let src = self.host_ptr(offset, dst.len())?;
        // SAFETY: `host_ptr` checked the range lies in the memory, and no
        // guest call runs while `self` is borrowed.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    /// Copies `src` into guest memory at `offset`.
    pub fn write(&mut self, offset: u32, src: &[u8]) -> Result<(), WasmError> {
        let dst = self.host_ptr(offset, src.len())?;
        // SAFETY: as in `read`. `src` is host memory, so the ranges do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len()) };
        Ok(())
    }

    /// Returns the host address of `len` bytes at `offset`, for setting up a
    /// ring there.
    ///
    /// Fails unless `offset` is 16-byte aligned.
    pub fn region(&self, offset: u32, len: usize) -> Result<NonNull<u8>, WasmError> {
        if !(offset as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
            return Err(WasmError::Misaligned(offset));
        }
        self.host_ptr(offset, len)
    }

    /// Returns the host address of `len` bytes at `offset`.
    ///
    /// Fails if the range is outside the memory's current size. The memory is
    /// page aligned, so an offset has the same alignment as its host address.
    pub fn host_ptr(&self, offset: u32, len: usize) -> Result<NonNull<u8>, WasmError> {
        let size = self.memory.data_size(&self.store);
        let start = offset as usize;
        match start.checked_add(len) {
            // SAFETY: `start` is within the memory, which is `backing`, so the
            // offset stays in that one allocation.
            Some(end) if end <= size => Ok(unsafe { self.backing.ptr.add(start) }),
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

/// Looks up one export as a typed function.
///
/// The error names the export if it is missing or has the wrong signature.
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

/// Calls `func` with a fresh fuel budget of `fuel`.
fn call<P: WasmParams, R: WasmResults>(
    store: &mut Store<StoreLimits>,
    func: TypedFunc<P, R>,
    params: P,
    fuel: u64,
) -> Result<R, Trap> {
    // `Module::compile` always enables fuel metering, so this cannot fail.
    store.set_fuel(fuel).map_err(|_| Trap::Other)?;
    func.call(store, params).map_err(Trap::from)
}

/// The page-aligned, zeroed allocation that backs a guest's memory.
struct GuestMemory {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl GuestMemory {
    /// Allocates enough whole pages to cover `ceiling` bytes.
    fn new(ceiling: usize) -> Result<GuestMemory, WasmError> {
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
        Ok(GuestMemory { ptr, layout })
    }

    fn len(&self) -> usize {
        self.layout.size()
    }

    /// Returns the imported memory type `ty` with its maximum capped at this
    /// memory's size.
    fn bound(&self, ty: MemoryType) -> Result<MemoryType, WasmError> {
        let pages = (self.len() / PAGE) as u64;
        if ty.minimum() > pages {
            return Err(WasmError::MemoryTooSmall {
                needed: ty.minimum().saturating_mul(PAGE as u64),
                ceiling: self.len(),
            });
        }
        let mut builder = MemoryType::builder();
        builder
            .min(ty.minimum())
            .max(Some(ty.maximum().map_or(pages, |max| max.min(pages))));
        builder.build().map_err(|e| wasmi::Error::from(e).into())
    }

    /// Returns the whole arena as one mutable slice.
    ///
    /// # Safety
    /// Nothing else may access the bytes while the slice, or anything derived
    /// from it, is alive. None of them may be used after the memory drops.
    unsafe fn bytes(&self) -> &'static mut [u8] {
        // SAFETY: `new` allocated `len` bytes at `ptr`. The caller's contract
        // covers aliasing and lifetime.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len()) }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // SAFETY: `new` allocated `ptr` with this layout, and it is freed once.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}
