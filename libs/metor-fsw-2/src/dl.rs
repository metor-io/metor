//! The host side of WP8 — the `dlopen` loader (`DlSystem`) and the cyclic slot
//! (`DlSlot`) that drives a system `.so` across the C ABI (dl-open.md §4).
//!
//! A [`DlSystem`] opens a system `cdylib`, resolves its `fsw_*` symbols by the
//! [`abi::SYM_*`](crate::abi) constants, checks the ABI word, and `fsw_describe`s
//! it into a [`SystemDescriptor`] the **existing** wiring/validation/sizing passes
//! consume unchanged (dl-open.md §4.1) — a dlopen'd system is wiring-validated
//! exactly like a statically-linked one. [`CoordinatorBuilder::add_dl_cyclic`]
//! registers it; at `build()` the coordinator allocates and sizes its rings like any
//! cyclic system and, instead of a typed [`CyclicRunner`](crate::CyclicRunner), binds
//! a [`DlSlot`] that forwards `init`/`step`/`shutdown` over the ABI by passing the
//! per-port ring regions as raw [`FswRing`] handles (dl-open.md §4.2).
//!
//! Same-process, cyclic-only per the locked v1 scope (dl-open.md §5, §9): the `.so`
//! reconstructs each ring over the host's `BoxBacking` region via `attach_raw`, so
//! there is no copy and no IPC — the system sees the identical atomics the host's
//! other systems do.
//!
//! ## Teardown ordering (dl-open.md §2.5) — load-bearing
//!
//! A [`DlSlot`] owns an `Arc<Library>` and the opaque `*mut state`. Its [`Drop`]
//! calls `fsw_destroy` (dropping the `.so`'s `RawBacking` ports and the user system)
//! **before** the `Library` unloads (the `Arc` field drops after the `Drop` body) and
//! **before** the host [`RingTable`](crate::coordinator) frees the regions (the
//! coordinator drops its `cyclic` slot vec before its `rings` field). So no
//! `RawBacking` ever outlives its region, and no `.so` code runs after its `Library`
//! is gone.

use core::ffi::c_void;
use std::ffi::OsStr;
use std::slice;
use std::sync::Arc;

use libloading::{Library, Symbol};
use metor_proto::types::Timestamp;

use crate::abi::{
    self, ByteSink, FSW_ABI_VERSION, FswRing, FswStatus, SystemDescriptorMsg,
};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::SystemDescriptor;

// ---------------------------------------------------------------------------
// Resolved C-ABI function-pointer types (dl-open.md §2)
// ---------------------------------------------------------------------------

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type DescribeFn = unsafe extern "C" fn(ByteSink, *mut c_void) -> i32;
type CreateFn = unsafe extern "C" fn(*const u8, usize) -> *mut c_void;
type BindInitFn = unsafe extern "C" fn(*mut c_void, *const FswRing, usize, *const FswRing, usize);
type ExecuteFn = unsafe extern "C" fn(*mut c_void, u64) -> FswStatus;
type ShutdownFn = unsafe extern "C" fn(*mut c_void);
type DestroyFn = unsafe extern "C" fn(*mut c_void);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure loading or describing a system `.so` (dl-open.md §4.1). Every variant
/// is a clean error — a bad artifact never crashes the host.
#[derive(Debug, thiserror::Error)]
pub enum DlError {
    /// `dlopen` failed (missing file, bad object, unresolved symbol at load).
    #[error("failed to open shared object: {0}")]
    Open(#[source] libloading::Error),
    /// A required `fsw_*` symbol was not present in the `.so`.
    #[error("shared object is missing the `{symbol}` symbol: {source}")]
    MissingSymbol {
        symbol: &'static str,
        #[source]
        source: libloading::Error,
    },
    /// `fsw_abi_version()` did not equal the host's [`FSW_ABI_VERSION`]: the `.so`
    /// was built against a different ABI and must be rebuilt (dl-open.md §2.4).
    #[error("ABI version mismatch: .so reports {found}, host expects {expected}")]
    VersionMismatch { found: u32, expected: u32 },
    /// `fsw_describe` returned a non-zero (failure) code.
    #[error("fsw_describe failed with code {0}")]
    Describe(i32),
    /// The postcard-encoded [`SystemDescriptorMsg`] could not be decoded.
    #[error("failed to decode the system descriptor: {0}")]
    Decode(#[source] postcard::Error),
}

// ---------------------------------------------------------------------------
// DlSystem — the loaded handle
// ---------------------------------------------------------------------------

/// A loaded system `cdylib`: the kept-alive [`Library`], the resolved lifecycle
/// function pointers, and the reconstructed [`SystemDescriptor`] for wiring
/// (dl-open.md §4.1). The handle **owns** the `Library` (an `Arc` it shares with the
/// [`DlSlot`] it builds) so the `.so` outlives every slot and `*mut state`.
///
/// Construct with [`DlSystem::open`], hand to
/// [`CoordinatorBuilder::add_dl_cyclic`](crate::CoordinatorBuilder::add_dl_cyclic).
pub struct DlSystem {
    lib: Arc<Library>,
    descriptor: SystemDescriptor,
    create: CreateFn,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
}

/// The host [`ByteSink`] `fsw_describe` writes its postcard bytes to: append them to
/// the `Vec<u8>` passed as `ctx`. The `.so` owns the buffer for the call's duration
/// and frees it itself — the host only copies (dl-open.md §2.1/§2.5, no cross-free).
extern "C" fn collect_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: `open` passes `&mut Vec<u8>` as `ctx`; `buf`/`len` is the descriptor
    // buffer the `.so` owns for this call.
    let out = unsafe { &mut *(ctx as *mut Vec<u8>) };
    let bytes = unsafe { slice::from_raw_parts(buf, len) };
    out.extend_from_slice(bytes);
}

/// Resolve a required symbol by its NUL-terminated [`abi::SYM_*`](crate::abi) name,
/// mapping a miss to a clean [`DlError::MissingSymbol`].
///
/// # Safety
/// The resolved `T` must match the `.so`'s actual signature for `name`.
unsafe fn resolve<'lib, T>(
    lib: &'lib Library,
    name: &[u8],
    sym: &'static str,
) -> Result<Symbol<'lib, T>, DlError> {
    // SAFETY: the caller asserts `T` matches the export's signature; the bytes are a
    // NUL-terminated symbol name.
    unsafe { lib.get::<T>(name) }.map_err(|source| DlError::MissingSymbol { symbol: sym, source })
}

impl DlSystem {
    /// `dlopen` the artifact at `path`, resolve every `fsw_*` symbol, check the ABI
    /// word, and `fsw_describe` it into a [`SystemDescriptor`] (dl-open.md §4.1).
    ///
    /// On any failure returns a [`DlError`] (never a crash): a missing/incompatible
    /// artifact is a clean load error.
    pub fn open(path: impl AsRef<OsStr>) -> Result<Self, DlError> {
        // SAFETY: `dlopen` runs the `.so`'s initializers; a system `cdylib` has none
        // beyond Rust's, and the path is caller-chosen. There is no safer form.
        let lib = unsafe { Library::new(path) }.map_err(DlError::Open)?;

        // --- ABI word: equality before any other call (dl-open.md §2.4) ----------
        let found = {
            // SAFETY: the export is a `fn() -> u32` (the macro's `fsw_abi_version`).
            let f: Symbol<AbiVersionFn> =
                unsafe { resolve(&lib, abi::SYM_ABI_VERSION, "fsw_abi_version")? };
            // SAFETY: a plain version read, no arguments.
            unsafe { f() }
        };
        if found != FSW_ABI_VERSION {
            return Err(DlError::VersionMismatch {
                found,
                expected: FSW_ABI_VERSION,
            });
        }

        // --- Describe → postcard → SystemDescriptor (dl-open.md §4.1) ------------
        let descriptor = {
            // SAFETY: the export is the macro's `fsw_describe(sink, ctx) -> i32`.
            let describe: Symbol<DescribeFn> =
                unsafe { resolve(&lib, abi::SYM_DESCRIBE, "fsw_describe")? };
            let mut buf: Vec<u8> = Vec::new();
            // SAFETY: `collect_sink`/`&mut buf` form a valid host callback pair; the
            // `.so` calls the sink with a buffer it owns for the call.
            let rc = unsafe { describe(collect_sink, &mut buf as *mut Vec<u8> as *mut c_void) };
            if rc != 0 {
                return Err(DlError::Describe(rc));
            }
            let msg: SystemDescriptorMsg =
                postcard::from_bytes(&buf).map_err(DlError::Decode)?;
            msg.into_descriptor()
        };

        // --- Resolve the lifecycle surface, dereferencing each `Symbol` to a bare
        // fn pointer kept alive by the `Arc<Library>` below (the standard libloading
        // pattern: the pointer is valid as long as the `Library` stays loaded). ----
        // SAFETY: each export matches the macro's generated signature.
        let create = *unsafe { resolve::<CreateFn>(&lib, abi::SYM_CREATE, "fsw_create")? };
        let bind_init =
            *unsafe { resolve::<BindInitFn>(&lib, abi::SYM_BIND_INIT, "fsw_bind_init")? };
        let execute = *unsafe { resolve::<ExecuteFn>(&lib, abi::SYM_EXECUTE, "fsw_execute")? };
        let shutdown = *unsafe { resolve::<ShutdownFn>(&lib, abi::SYM_SHUTDOWN, "fsw_shutdown")? };
        let destroy = *unsafe { resolve::<DestroyFn>(&lib, abi::SYM_DESTROY, "fsw_destroy")? };

        Ok(Self {
            lib: Arc::new(lib),
            descriptor,
            create,
            bind_init,
            execute,
            shutdown,
            destroy,
        })
    }

    /// The reconstructed self-description the builder validates wiring against
    /// (dl-open.md §4.1). Its ports' `announce` closures already prefix the carried
    /// vtable ids (the dl telemetry rewrite, [`abi`](crate::abi)).
    pub fn descriptor(&self) -> &SystemDescriptor {
        &self.descriptor
    }

    /// Consume the handle into a bound, ready-to-init [`DlSlot`] (called by the
    /// coordinator at `build()`): `fsw_create` the opaque state from the postcard
    /// `params`, and capture the host-built per-port [`FswRing`] arrays the slot hands
    /// `fsw_bind_init` at `init` (dl-open.md §4.2). The `Arc<Library>` moves into the
    /// slot, so the `.so` outlives the state.
    ///
    /// # Safety
    /// Every region named by an `FswRing` in `inputs`/`outputs` must satisfy
    /// [`RingBuffer::attach_raw`](metor_fsw_ring::RingBuffer::attach_raw)'s contract —
    /// a live, header-valid ring region that outlives the slot (until its `Drop` calls
    /// `fsw_destroy`). The coordinator guarantees this by keeping the owning
    /// `RingTable` alive past the slot.
    pub(crate) unsafe fn into_slot(
        self,
        params: &[u8],
        inputs: Vec<FswRing>,
        outputs: Vec<FswRing>,
        name: &'static str,
    ) -> DlSlot {
        let (ptr, len) = if params.is_empty() {
            (core::ptr::null(), 0)
        } else {
            (params.as_ptr(), params.len())
        };
        // SAFETY: `params`/`len` name a readable byte range (or null/0); the export
        // decodes them immediately, so they need not outlive the call.
        let state = unsafe { (self.create)(ptr, len) };
        DlSlot {
            lib: self.lib,
            bind_init: self.bind_init,
            execute: self.execute,
            shutdown: self.shutdown,
            destroy: self.destroy,
            state,
            inputs,
            outputs,
            name,
            slot_state: SlotState::Running,
        }
    }
}

// ---------------------------------------------------------------------------
// DlSlot — the dlopen twin of CyclicRunner's CyclicSlot impl (dl-open.md §4.2)
// ---------------------------------------------------------------------------

/// A bound dlopen'd cyclic system the coordinator drives as a `Box<dyn CyclicSlot>`,
/// indistinguishable from a static [`CyclicRunner`](crate::CyclicRunner) in the
/// per-cycle loop (dl-open.md §4.2). It forwards `init`/`step`/`shutdown` across the C
/// ABI: `init` hands the `.so` the per-port [`FswRing`] arrays for `fsw_bind_init`,
/// `step` calls `fsw_execute` and maps the returned [`FswStatus`] into a tracked
/// [`SlotState`] (the lapped/panic check lives inside the `.so`, which owns the input
/// `View`), and `Drop` calls `fsw_destroy` (see the module teardown note).
pub(crate) struct DlSlot {
    /// Kept alive for the slot's whole life; dropped **after** `fsw_destroy` (Drop
    /// body runs before this field drops), so no `.so` code runs after unload.
    #[allow(dead_code)]
    lib: Arc<Library>,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
    /// The opaque runner state from `fsw_create`; owned by the `.so`, dropped by
    /// `fsw_destroy`. Nulled after destroy so `Drop` is idempotent.
    state: *mut c_void,
    /// Per-port input handles (role=INPUT), in `descriptors()` order — views into the
    /// upstream producers' output rings.
    inputs: Vec<FswRing>,
    /// Per-port output handles (role=OUTPUT), in `descriptors()` order — this system's
    /// own writer rings (incl. the implicit health/log).
    outputs: Vec<FswRing>,
    /// The system's descriptor name (the `dyn CyclicSlot` identity in status/health).
    name: &'static str,
    slot_state: SlotState,
}

impl CyclicSlot for DlSlot {
    /// `fsw_bind_init`: hand the `.so` the per-port ring regions so it reconstructs
    /// its typed `Input`/`Output` bundles and runs `System::init` (dl-open.md §4.2).
    fn init(&mut self) {
        if self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live `fsw_create` pointer; the handle arrays name
        // live regions the coordinator keeps alive past this slot.
        unsafe {
            (self.bind_init)(
                self.state,
                self.inputs.as_ptr(),
                self.inputs.len(),
                self.outputs.as_ptr(),
                self.outputs.len(),
            );
        }
    }

    /// `fsw_execute`: run one cycle and fold the returned status into the slot state.
    /// `now` carries the coordinator's **raw `Timestamp` tick** (`now.0 as u64`), the
    /// unit-agnostic reversible contract — NOT nanoseconds (dl-open.md §3.0).
    /// `StoppedLapped` and `Panicked` are permanent hard-stops surfaced through the
    /// coordinator status frame (a `Panicked` slot is also telemetered there); once
    /// stopped the slot is never ticked again, mirroring [`CyclicRunner::step`].
    fn step(&mut self, now: Timestamp) {
        if self.slot_state.is_stopped() || self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live, bound `fsw_create` pointer.
        let status = unsafe { (self.execute)(self.state, now.0 as u64) };
        self.slot_state = match status {
            FswStatus::Running => SlotState::Running,
            FswStatus::StoppedLapped => SlotState::Stopped {
                reason: StopReason::LappedInput,
            },
            FswStatus::Panicked => SlotState::Stopped {
                reason: StopReason::Panicked,
            },
        };
    }

    /// `fsw_shutdown`: run `System::shutdown` once inside the `.so`.
    fn shutdown(&mut self) {
        if self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live `fsw_create` pointer.
        unsafe { (self.shutdown)(self.state) };
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }
}

impl Drop for DlSlot {
    /// `fsw_destroy` the state **before** the `Library` unloads (the `Arc<Library>`
    /// field drops after this body) and before the host `RingTable` frees the regions
    /// (the coordinator drops `cyclic` before `rings`) — dl-open.md §2.5.
    fn drop(&mut self) {
        if !self.state.is_null() {
            // SAFETY: `state` is the live `fsw_create` pointer, transferred to
            // `fsw_destroy` exactly once; nulled so a double-drop is a no-op.
            unsafe { (self.destroy)(self.state) };
            self.state = core::ptr::null_mut();
        }
    }
}
