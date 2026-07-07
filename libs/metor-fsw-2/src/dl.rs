//! Loading and driving a system compiled as a shared object.
//!
//! [`DlSystem::open`] loads a system `cdylib`, checks its ABI version word,
//! resolves the `fsw_*` exports named by the [`abi`](crate::abi) constants, and
//! asks `fsw_describe` for a [`SystemDescriptor`]. From there the builder
//! treats the system like any statically linked one; wiring, validation, and
//! ring sizing all run against the descriptor. At `build()` the coordinator
//! binds a [`DlSlot`] in place of a typed [`CyclicRunner`](crate::CyclicRunner).
//! The slot forwards `init`, `step`, and `shutdown` across the C ABI, handing
//! the shared object its per-port ring regions as raw [`FswRing`] handles.
//! Timestamps cross the ABI as the raw `Timestamp` tick, in whatever unit the
//! coordinator's clock produces.
//!
//! Everything stays in one process. The shared object attaches to the host's
//! ring regions in place, so it reads and writes the same atomics the host's
//! statically linked systems do; there is no copy and no IPC.
//!
//! ## The trust boundary
//!
//! The shared object is foreign code the host does not control, so nothing it
//! returns is trusted as a Rust value. `fsw_execute` returns a raw `u32`
//! rather than an [`FswStatus`], because materializing a `repr(u32)` enum from
//! an out-of-range discriminant is immediate undefined behavior. Every call
//! site converts through [`FswStatus::from_raw`], which folds unknown words to
//! `Panicked`. Likewise a descriptor that fails to decode, or that declares
//! capabilities the host cannot grant across the ABI, is a clean [`DlError`]
//! at load time rather than a panic later.
//!
//! ## Teardown ordering
//!
//! A [`DlSlot`] owns an `Arc<Library>` and the opaque `*mut state` returned by
//! `fsw_create`. Its [`Drop`] calls `fsw_destroy` before the `Library` can
//! unload, because the `Arc` field drops after the `Drop` body runs, and
//! before the host frees the ring regions, because the coordinator drops its
//! slots before its ring table. So no non-owning ring attach outlives its
//! region, and no shared object code runs after the object is unloaded.
//!
//! A slot whose system panics is destroyed immediately rather than at
//! teardown. A stopped system's live input views would otherwise keep holding
//! their reader slots and backpressure every upstream producer forever.

use core::ffi::c_void;
use std::ffi::OsStr;
use std::slice;
use std::sync::Arc;

use libloading::{Library, Symbol};
use metor_proto::types::Timestamp;
use postcard_schema::schema::owned::OwnedNamedType;

use crate::abi::{self, ByteSink, FSW_ABI_VERSION, FswRing, FswStatus, SystemDescriptorMsg};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::SystemDescriptor;

// ---------------------------------------------------------------------------
// Resolved C-ABI function-pointer types
// ---------------------------------------------------------------------------

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type DescribeFn = unsafe extern "C" fn(ByteSink, *mut c_void) -> i32;
type CreateFn = unsafe extern "C" fn(*const u8, usize) -> *mut c_void;
type BindInitFn = unsafe extern "C" fn(*mut c_void, *const FswRing, usize, *const FswRing, usize);
// Returns the raw status word, not `FswStatus`; see the trust boundary note in
// the module docs.
type ExecuteFn = unsafe extern "C" fn(*mut c_void, u64) -> u32;
type ShutdownFn = unsafe extern "C" fn(*mut c_void);
type DestroyFn = unsafe extern "C" fn(*mut c_void);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure loading or describing a system shared object. A bad artifact is
/// always a clean error, never a crash of the host.
#[derive(Debug, thiserror::Error)]
pub enum DlError {
    /// `dlopen` failed (missing file, bad object, unresolved symbol at load).
    #[error("failed to open shared object: {0}")]
    Open(#[source] libloading::Error),
    /// A required `fsw_*` symbol was not present in the shared object.
    #[error("shared object is missing the `{symbol}` symbol: {source}")]
    MissingSymbol {
        symbol: &'static str,
        #[source]
        source: libloading::Error,
    },
    /// `fsw_abi_version()` did not equal the host's [`FSW_ABI_VERSION`]; the
    /// shared object was built against a different ABI and must be rebuilt.
    #[error("ABI version mismatch: .so reports {found}, host expects {expected}")]
    VersionMismatch { found: u32, expected: u32 },
    /// `fsw_describe` returned a non-zero (failure) code.
    #[error("fsw_describe failed with code {0}")]
    Describe(i32),
    /// The postcard-encoded [`SystemDescriptorMsg`] could not be decoded.
    #[error("failed to decode the system descriptor: {0}")]
    Decode(#[source] postcard::Error),
    /// The descriptor declares [`Capability`](crate::Capability)s. Capabilities
    /// are granted by the host registry, which cannot cross the ABI, so a
    /// shared object declaring any is rejected at load.
    #[error(
        "shared object declares host-only capabilities {0:?}; a dlopen'd system cannot hold them"
    )]
    UnsupportedCapabilities(Vec<crate::Capability>),
}

// ---------------------------------------------------------------------------
// DlSystem, the loaded handle
// ---------------------------------------------------------------------------

/// A handle to a system shared object loaded into the host process, holding
/// the [`Library`], the resolved lifecycle function pointers, and the
/// [`SystemDescriptor`] the builder wires against.
///
/// The `Library` lives in an `Arc` shared with every [`DlSlot`] built from
/// this handle, so the shared object stays loaded as long as any slot exists.
/// Construct with [`DlSystem::open`], register with
/// [`CoordinatorBuilder::add_dl_cyclic`](crate::CoordinatorBuilder::add_dl_cyclic).
pub struct DlSystem {
    lib: Arc<Library>,
    descriptor: SystemDescriptor,
    /// The exported `Params` schema, kept so the host can schema-encode KDL
    /// params without ever linking the `Params` type.
    params_schema: OwnedNamedType,
    create: CreateFn,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
}

/// Rejects a descriptor that declares any [`Capability`](crate::Capability).
/// Capabilities are resolved against the host registry, which cannot cross the
/// ABI, so the load fails cleanly instead of panicking at bind time.
fn reject_capabilities(msg: SystemDescriptorMsg) -> Result<SystemDescriptorMsg, DlError> {
    if msg.capabilities.is_empty() {
        Ok(msg)
    } else {
        Err(DlError::UnsupportedCapabilities(msg.capabilities))
    }
}

/// The [`ByteSink`] handed to `fsw_describe`; appends the callee's bytes to
/// the `Vec<u8>` passed as `ctx`. The shared object keeps ownership of its
/// buffer and the host only copies, so no allocation crosses the boundary.
extern "C" fn collect_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: `open` passes `&mut Vec<u8>` as `ctx`; `buf`/`len` is the
    // descriptor buffer the shared object owns for the duration of this call.
    let out = unsafe { &mut *(ctx as *mut Vec<u8>) };
    let bytes = unsafe { slice::from_raw_parts(buf, len) };
    out.extend_from_slice(bytes);
}

/// Resolves a required symbol by its NUL-terminated name, mapping a miss to
/// [`DlError::MissingSymbol`].
///
/// # Safety
/// The resolved `T` must match the shared object's actual signature for
/// `name`.
unsafe fn resolve<'lib, T>(
    lib: &'lib Library,
    name: &[u8],
    sym: &'static str,
) -> Result<Symbol<'lib, T>, DlError> {
    // SAFETY: the caller asserts `T` matches the export's signature; the bytes
    // are a NUL-terminated symbol name.
    unsafe { lib.get::<T>(name) }.map_err(|source| DlError::MissingSymbol {
        symbol: sym,
        source,
    })
}

impl DlSystem {
    /// Opens the artifact at `path`, resolves every `fsw_*` symbol, checks the
    /// ABI version word, and describes it into a [`SystemDescriptor`].
    ///
    /// Any failure is a [`DlError`]; a missing or incompatible artifact never
    /// crashes the host.
    pub fn open(path: impl AsRef<OsStr>) -> Result<Self, DlError> {
        // SAFETY: `dlopen` runs the object's initializers; a system `cdylib`
        // has none beyond Rust's, and the path is caller-chosen. There is no
        // safer form.
        let lib = unsafe { Library::new(path) }.map_err(DlError::Open)?;

        // --- Check the ABI word before any other call -----------------------
        let found = {
            // SAFETY: the export is a `fn() -> u32`.
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

        // --- Describe, decode, and reconstruct the descriptor ---------------
        // The `Params` schema is cloned off the message before `into_descriptor`
        // consumes it, so the host can schema-encode KDL params later.
        let (descriptor, params_schema) = {
            // SAFETY: the export is `fsw_describe(sink, ctx) -> i32`.
            let describe: Symbol<DescribeFn> =
                unsafe { resolve(&lib, abi::SYM_DESCRIBE, "fsw_describe")? };
            let mut buf: Vec<u8> = Vec::new();
            // SAFETY: `collect_sink` and `&mut buf` form a matching callback
            // pair; the shared object calls the sink with a buffer it owns for
            // the duration of the call.
            let rc = unsafe { describe(collect_sink, &mut buf as *mut Vec<u8> as *mut c_void) };
            if rc != 0 {
                return Err(DlError::Describe(rc));
            }
            let msg: SystemDescriptorMsg = postcard::from_bytes(&buf).map_err(DlError::Decode)?;
            let msg = reject_capabilities(msg)?;
            let params_schema = msg.params_schema.clone();
            (msg.into_descriptor(), params_schema)
        };

        // --- Resolve the lifecycle surface. Each `Symbol` is dereferenced to
        // a bare fn pointer, valid as long as the `Arc<Library>` below stays
        // loaded. ------------------------------------------------------------
        // SAFETY: each export matches its generated signature.
        let create = *unsafe { resolve::<CreateFn>(&lib, abi::SYM_CREATE, "fsw_create")? };
        let bind_init =
            *unsafe { resolve::<BindInitFn>(&lib, abi::SYM_BIND_INIT, "fsw_bind_init")? };
        let execute = *unsafe { resolve::<ExecuteFn>(&lib, abi::SYM_EXECUTE, "fsw_execute")? };
        let shutdown = *unsafe { resolve::<ShutdownFn>(&lib, abi::SYM_SHUTDOWN, "fsw_shutdown")? };
        let destroy = *unsafe { resolve::<DestroyFn>(&lib, abi::SYM_DESTROY, "fsw_destroy")? };

        Ok(Self {
            lib: Arc::new(lib),
            descriptor,
            params_schema,
            create,
            bind_init,
            execute,
            shutdown,
            destroy,
        })
    }

    /// The self-description the builder validates wiring against. Its ports'
    /// `announce` closures already prefix the vtable ids carried across the
    /// ABI.
    pub fn descriptor(&self) -> &SystemDescriptor {
        &self.descriptor
    }

    /// The exported `Params` schema, which lets the wiring resolver encode a
    /// KDL config into the canonical postcard bytes without linking the
    /// `Params` type.
    pub fn params_schema(&self) -> &OwnedNamedType {
        &self.params_schema
    }

    /// Builds a fresh [`DlSlot`] without consuming the handle. Each call runs
    /// `fsw_create` for a new opaque state and clones the `Arc<Library>`, so
    /// one loaded object can produce any number of slots, one per load or
    /// reset.
    ///
    /// # Safety
    /// Every region named by an `FswRing` in `inputs`/`outputs` must satisfy
    /// [`RingBuffer::attach_raw`](metor_fsw_ring::RingBuffer::attach_raw)'s
    /// contract, and must stay live until the slot's `Drop` has called
    /// `fsw_destroy`. The coordinator guarantees this by keeping the owning
    /// ring table alive past the slot.
    pub(crate) unsafe fn make_slot(
        &self,
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
        // SAFETY: `ptr`/`len` name a readable byte range (or null/0); the
        // export decodes them immediately, so they need not outlive the call.
        let state = unsafe { (self.create)(ptr, len) };
        // A null state means creation panicked inside the shared object (the
        // ABI shim catches the unwind and returns null). Latch the failure
        // now: `step` early-returns on a null state without touching
        // `slot_state`, so a slot that started `Running` would never report a
        // stop and the coordinator would poll a zombie forever.
        let slot_state = if state.is_null() {
            SlotState::Stopped {
                reason: StopReason::Panicked,
            }
        } else {
            SlotState::Running
        };
        DlSlot {
            lib: self.lib.clone(),
            bind_init: self.bind_init,
            execute: self.execute,
            shutdown: self.shutdown,
            destroy: self.destroy,
            state,
            inputs,
            outputs,
            name,
            slot_state,
        }
    }
}

// ---------------------------------------------------------------------------
// DlSlot, a dlopen'd system behind the CyclicSlot interface
// ---------------------------------------------------------------------------

/// A running instance of a loaded shared object, driven by the coordinator
/// through the same `Box<dyn CyclicSlot>` interface as a statically linked
/// [`CyclicRunner`](crate::CyclicRunner). `init` hands the shared object its
/// per-port [`FswRing`] arrays, `step` calls `fsw_execute` and folds the
/// returned [`FswStatus`] into the tracked [`SlotState`], and `Drop` calls
/// `fsw_destroy` (see the module teardown note). Built by
/// [`DlSystem::make_slot`].
pub(crate) struct DlSlot {
    /// Keeps the shared object loaded for the slot's whole life; drops after
    /// the `Drop` body has run `fsw_destroy`.
    #[allow(dead_code)]
    lib: Arc<Library>,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
    /// The opaque state from `fsw_create`, owned by the shared object and
    /// dropped by `fsw_destroy`. Nulled after destroy so `Drop` is idempotent.
    state: *mut c_void,
    /// Input ring handles in descriptor order, viewing the upstream producers'
    /// output rings.
    inputs: Vec<FswRing>,
    /// Output ring handles in descriptor order, this system's own writer rings
    /// (including the implicit health and log).
    outputs: Vec<FswRing>,
    /// The descriptor name, used as the slot's identity in status and health.
    name: &'static str,
    slot_state: SlotState,
}

impl DlSlot {
    /// Runs one cycle and returns the raw [`FswStatus`], including the
    /// terminal [`Done`](FswStatus::Done).
    ///
    /// [`CyclicSlot::step`] folds the status straight into the slot state and
    /// treats `Done` as keep-running, because a build-time slot has no
    /// occupant outcome to refine it with. A runtime slot runner does, so it
    /// drives its occupant through this method instead and maps `Done` into a
    /// terminal lifecycle state.
    pub(crate) fn execute_raw(&mut self, now: Timestamp) -> FswStatus {
        if self.state.is_null() {
            return FswStatus::Panicked;
        }
        // SAFETY: `state` is the live, bound `fsw_create` pointer.
        let raw = unsafe { (self.execute)(self.state, now.0 as u64) };
        // Untrusted word from foreign code; see the module trust boundary note.
        FswStatus::from_raw(raw)
    }
}

impl CyclicSlot for DlSlot {
    fn init(&mut self) {
        if self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live `fsw_create` pointer; the handle arrays
        // name live regions the coordinator keeps alive past this slot.
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

    fn step(&mut self, now: Timestamp) {
        if self.slot_state.is_stopped() || self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live, bound `fsw_create` pointer.
        let raw = unsafe { (self.execute)(self.state, now.0 as u64) };
        // Untrusted word from foreign code; see the module trust boundary note.
        let status = FswStatus::from_raw(raw);
        self.slot_state = match status {
            FswStatus::Running => SlotState::Running,
            FswStatus::Panicked => SlotState::Stopped {
                reason: StopReason::Panicked,
            },
            // A well-behaved cyclic export never returns `Done` (only a
            // sequence occupant does, and those run under the runtime slot
            // runner via `execute_raw`). With no occupant outcome to refine it
            // with, a stray `Done` here is treated as keep-running.
            FswStatus::Done => SlotState::Running,
        };
        if self.slot_state.is_stopped() {
            // Destroy the foreign state now so its non-owning ports release
            // their reader slots (a panicked system gets no `shutdown`).
            // Nulling keeps `shutdown` and `Drop` as no-ops afterwards.
            // SAFETY: `state` is the live pointer, handed to destroy exactly
            // once.
            unsafe { (self.destroy)(self.state) };
            self.state = core::ptr::null_mut();
        }
    }

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
    fn drop(&mut self) {
        if !self.state.is_null() {
            // SAFETY: `state` is the live `fsw_create` pointer, handed to
            // `fsw_destroy` exactly once; nulled so a double-drop is a no-op.
            unsafe { (self.destroy)(self.state) };
            self.state = core::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor declaring a host capability is a clean load rejection,
    /// never a bind-time panic; an empty list (every real export) passes.
    #[test]
    fn load_rejects_declared_capabilities() {
        let mk = |capabilities| SystemDescriptorMsg {
            name: "rogue".to_string(),
            kind: crate::SystemKind::Cyclic,
            inputs: Vec::new(),
            outputs: Vec::new(),
            params_schema: OwnedNamedType::from(<() as postcard_schema::Schema>::SCHEMA),
            capabilities,
        };

        assert!(reject_capabilities(mk(Vec::new())).is_ok());
        let err = reject_capabilities(mk(vec![crate::Capability::ReceiveAll]))
            .expect_err("host-only capability rejected");
        assert!(
            matches!(&err, DlError::UnsupportedCapabilities(c) if c == &[crate::Capability::ReceiveAll]),
            "{err:?}"
        );
    }
}
