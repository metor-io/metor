//! Loading and driving pack shared objects.
//!
//! [`DlPack::open`] loads a pack `cdylib`, checks its ABI version word,
//! resolves the `fsw_pack_*` exports named by the [`abi`]
//! constants, opens the pack (`fsw_pack_open`, which runs the crate's
//! `pack()` once), and decodes its manifest into per-entry
//! [`SystemDescriptor`]s. [`DlPack::system`] selects one entry as a
//! [`DlSystem`], and from there the builder treats it like any statically
//! linked system; wiring, validation, and ring sizing all run against the
//! descriptor. At `build()` the coordinator binds a loaded slot in place of a
//! typed runner. The slot forwards `init`, `step`, and `shutdown` across the
//! C ABI, handing the shared object its per-port ring regions as raw
//! [`FswRing`] handles. Timestamps cross the ABI as the raw `Timestamp` tick,
//! in whatever unit the coordinator's clock produces.
//!
//! Everything stays in one process. The shared object attaches to the host's
//! ring regions in place, so it reads and writes the same atomics the host's
//! statically linked systems do; there is no copy and no IPC.
//!
//! ## The trust boundary
//!
//! The shared object is foreign code the host does not control, so nothing it
//! returns is trusted as a Rust value. `fsw_pack_execute` returns a raw `u32`
//! rather than an [`FswStatus`], because materializing a `repr(u32)` enum from
//! an out-of-range discriminant is immediate undefined behavior. Every call
//! site validates the raw word and folds unknown values to
//! `Panicked`. Likewise a manifest that fails to decode, or an entry that
//! declares capabilities the host cannot grant across the ABI, is a clean
//! [`DlError`] at load time rather than a panic later.
//!
//! ## Teardown ordering
//!
//! Each loaded slot owns a shared library handle and the opaque `*mut state`
//! returned by `fsw_pack_create`. Its `Drop` calls `fsw_pack_destroy` before
//! the library handle can drop, because the handle field drops after `Drop`
//! runs. `PackLib`'s own field order then closes the pack (`fsw_pack_close`)
//! before the `Library` unloads. And the coordinator drops its slots before
//! its ring table, so no non-owning ring attach outlives its region and no
//! shared-object code runs after the object is unloaded:
//! destroy states → pack_close → dlclose → rings free.
//!
//! A slot whose system panics is destroyed immediately rather than at
//! teardown. A stopped system's live input views would otherwise keep holding
//! their reader slots and backpressure every upstream producer forever.

use core::ffi::c_void;
use std::ffi::OsStr;
use std::path::Path;
#[cfg(feature = "wiring")]
use std::path::PathBuf;
use std::rc::Rc;
use std::slice;
use std::sync::Arc;

use libloading::{Library, Symbol};
use metor_proto::types::Timestamp;
use postcard_schema::schema::owned::OwnedNamedType;

use crate::abi::{self, ByteSink, FSW_ABI_VERSION, FswRing, FswStatus, PackEntryDesc};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::SystemDescriptor;

// ---------------------------------------------------------------------------
// Resolved C-ABI function-pointer types
// ---------------------------------------------------------------------------

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type PackOpenFn = unsafe extern "C" fn() -> *mut c_void;
type PackDescribeFn = unsafe extern "C" fn(*mut c_void, ByteSink, *mut c_void) -> i32;
type PackCreateFn = unsafe extern "C" fn(*mut c_void, u32, u32, *const u8, usize) -> *mut c_void;
type BindInitFn = unsafe extern "C" fn(
    *mut c_void,
    *const FswRing,
    usize,
    *const FswRing,
    usize,
    *const u8,
    usize,
);
// Returns the raw status word, not `FswStatus`; see the trust boundary note in
// the module docs.
type ExecuteFn = unsafe extern "C" fn(*mut c_void, u64) -> u32;
type ShutdownFn = unsafe extern "C" fn(*mut c_void);
type DestroyFn = unsafe extern "C" fn(*mut c_void);
type PackCloseFn = unsafe extern "C" fn(*mut c_void);

/// The per-instance lifecycle pointers, resolved once per load and shared by
/// every [`DlSystem`]/[`DlSlot`] over the pack. Bare fn pointers, valid as
/// long as the owning [`PackLib`] stays loaded.
#[derive(Clone, Copy)]
struct PackFns {
    create: PackCreateFn,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure loading or describing a pack shared object. A bad artifact is
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
    /// `fsw_pack_open` returned null: the crate's `pack()` fn panicked.
    #[error("fsw_pack_open failed (the pack() fn panicked)")]
    PackOpen,
    /// `fsw_pack_describe` returned a non-zero (failure) code.
    #[error("fsw_pack_describe failed with code {0}")]
    Describe(i32),
    /// The postcard-encoded [`PackManifest`](abi::PackManifest) could not be
    /// decoded.
    #[error("failed to decode the pack manifest: {0}")]
    Decode(#[source] postcard::Error),
    /// An entry declares [`Capability`](crate::Capability)s. Capabilities
    /// are granted by the host registry, which cannot cross the ABI, so a
    /// shared object declaring any is rejected at load.
    #[error(
        "shared object declares host-only capabilities {0:?}; a dlopen'd system cannot hold them"
    )]
    UnsupportedCapabilities(Vec<crate::Capability>),
    /// The pack exports no entry under the requested name.
    #[error("pack has no system `{name}` (exports: {})", available.join(", "))]
    UnknownPackSystem {
        name: String,
        available: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// PackLib, the opened pack + its library
// ---------------------------------------------------------------------------

/// Closes the pack (`fsw_pack_close`) on drop. Null-safe so a stub or a
/// failed open drops cleanly.
struct PackGuard {
    ptr: *mut c_void,
    close: Option<PackCloseFn>,
}

impl Drop for PackGuard {
    fn drop(&mut self) {
        if let Some(close) = self.close
            && !self.ptr.is_null()
        {
            tracing::info!("pack closing");
            // SAFETY: `ptr` came from this library's `fsw_pack_open`, every
            // instance state was destroyed by the slots dropped before this
            // `Rc` reached zero, and the `Library` field outlives this guard
            // (field order in `PackLib`).
            unsafe { close(self.ptr) };
            self.ptr = core::ptr::null_mut();
        }
    }
}

/// The loaded library and its opened pack, shared (`Rc`) by every
/// [`DlSystem`] and [`DlSlot`] over it. Field order is drop order: the pack
/// closes before the library unloads.
pub(crate) struct PackLib {
    pack: PackGuard,
    #[allow(dead_code)]
    lib: Library,
}

impl PackLib {
    fn pack_ptr(&self) -> *mut c_void {
        self.pack.ptr
    }
}

// ---------------------------------------------------------------------------
// Open + describe
// ---------------------------------------------------------------------------

/// The [`ByteSink`] handed to `fsw_pack_describe`; appends the callee's bytes
/// to the `Vec<u8>` passed as `ctx`. The shared object keeps ownership of its
/// buffer and the host only copies, so no allocation crosses the boundary.
extern "C" fn collect_sink(ctx: *mut c_void, buf: *const u8, len: usize) {
    // SAFETY: `open` passes `&mut Vec<u8>` as `ctx`; `buf`/`len` is the
    // manifest buffer the shared object owns for the duration of this call.
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

/// Load the object at `path`, check its ABI word, open its pack, and collect
/// the raw postcard [`PackManifest`](abi::PackManifest) bytes from
/// `fsw_pack_describe`. The
/// shared front of [`DlPack::open`] and [`describe_raw`].
fn open_and_describe(path: &OsStr) -> Result<(PackLib, Vec<u8>), DlError> {
    // SAFETY: `dlopen` runs the object's initializers; a pack `cdylib`
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
        tracing::error!(
            path = %Path::new(path).display(),
            found,
            expected = FSW_ABI_VERSION,
            "pack rejected: ABI version mismatch"
        );
        return Err(DlError::VersionMismatch {
            found,
            expected: FSW_ABI_VERSION,
        });
    }

    // --- Open the pack (runs the crate's pack() once) -------------------
    // Resolve close first so a successful open can always be undone.
    let close = *unsafe { resolve::<PackCloseFn>(&lib, abi::SYM_PACK_CLOSE, "fsw_pack_close")? };
    let open = *unsafe { resolve::<PackOpenFn>(&lib, abi::SYM_PACK_OPEN, "fsw_pack_open")? };
    // SAFETY: the export constructs the pack inside the `.so`.
    let pack_ptr = unsafe { open() };
    let pack_lib = PackLib {
        pack: PackGuard {
            ptr: pack_ptr,
            close: Some(close),
        },
        lib,
    };
    if pack_ptr.is_null() {
        return Err(DlError::PackOpen);
    }

    // --- Describe --------------------------------------------------------
    // SAFETY: the export is `fsw_pack_describe(pack, sink, ctx) -> i32`.
    let describe: Symbol<PackDescribeFn> =
        unsafe { resolve(&pack_lib.lib, abi::SYM_PACK_DESCRIBE, "fsw_pack_describe")? };
    let mut buf: Vec<u8> = Vec::new();
    // SAFETY: `collect_sink` and `&mut buf` form a matching callback pair;
    // the shared object calls the sink with a buffer it owns for the call.
    let rc = unsafe {
        describe(
            pack_lib.pack_ptr(),
            collect_sink,
            &mut buf as *mut Vec<u8> as *mut c_void,
        )
    };
    if rc != 0 {
        return Err(DlError::Describe(rc));
    }
    Ok((pack_lib, buf))
}

/// The raw manifest bytes alone, for the worker's describe mode and the build
/// driver's manifest sidecar: the object is loaded, ABI-checked, described,
/// and unloaded *in this process*. The worker's host decodes the bytes
/// without ever loading the object itself (see `docs/process-systems.md`).
#[cfg(any(target_os = "linux", target_os = "macos", feature = "wiring"))]
pub(crate) fn describe_raw(path: impl AsRef<OsStr>) -> Result<Vec<u8>, DlError> {
    // Dropping the PackLib closes the pack and unloads the library.
    open_and_describe(path.as_ref()).map(|(_lib, buf)| buf)
}

/// The manifest sidecar path for a built pack library: `<so_path>.manifest`,
/// the raw postcard [`PackManifest`](abi::PackManifest) bytes the build
/// driver wrote next to the `.so` (see `docs/wiring.md`).
#[cfg(feature = "wiring")]
pub(crate) fn manifest_sidecar_path(so_path: &Path) -> PathBuf {
    let mut name = so_path.as_os_str().to_owned();
    name.push(".manifest");
    PathBuf::from(name)
}

/// The raw postcard [`PackManifest`](abi::PackManifest) bytes of the `<so>.manifest` sidecar,
/// if the build driver wrote one (sidecar-hash ≡ describe-hash). `None` when no
/// sidecar sits next to the library, leaving the caller to describe it.
#[cfg(feature = "wiring")]
pub(crate) fn manifest_sidecar_bytes(so_path: &Path) -> Option<Vec<u8>> {
    std::fs::read(manifest_sidecar_path(so_path)).ok()
}

/// One lock every in-crate test that builds and describes the dl fixture pack
/// shares: they touch the same `<target>/…/<so>.manifest` sidecar, so they must
/// serialize rather than race one build's atomic write against another's read.
#[cfg(all(test, not(miri)))]
pub(crate) static FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Decode a manifest payload into its per-entry descriptors — the shared
/// tail of [`DlPack::open`] and the process path's describe-worker decode,
/// host-capability rejection included.
pub(crate) fn decode_pack_manifest(bytes: &[u8]) -> Result<Vec<PackEntryDesc>, DlError> {
    let manifest: abi::PackManifest = postcard::from_bytes(bytes).map_err(DlError::Decode)?;
    manifest
        .systems
        .into_iter()
        .map(reject_capabilities)
        .collect()
}

/// Rejects an entry whose descriptor declares any
/// [`Capability`](crate::Capability). Capabilities are resolved against the
/// host registry, which cannot cross the ABI, so the load fails cleanly
/// instead of panicking at bind time.
fn reject_capabilities(entry: PackEntryDesc) -> Result<PackEntryDesc, DlError> {
    if !entry.descriptor.capabilities.is_empty() {
        return Err(DlError::UnsupportedCapabilities(
            entry.descriptor.capabilities.clone(),
        ));
    }
    Ok(entry)
}

// ---------------------------------------------------------------------------
// DlPack + DlSystem, the loaded handles
// ---------------------------------------------------------------------------

/// A loaded pack: the shared library handle, the decoded per-entry descriptors,
/// and the resolved lifecycle pointers. [`system`](Self::system) selects one
/// entry as a [`DlSystem`], the unit the builder registers.
pub struct DlPack {
    lib: Rc<PackLib>,
    entries: Vec<PackEntryDesc>,
    fns: PackFns,
}

impl DlPack {
    /// Opens the artifact at `path`, checks the ABI version word, opens the
    /// pack, resolves every `fsw_pack_*` symbol, and decodes the manifest.
    ///
    /// Any failure is a [`DlError`]; a missing or incompatible artifact never
    /// crashes the host.
    pub fn open(path: impl AsRef<OsStr>) -> Result<Self, DlError> {
        let (pack_lib, buf) = open_and_describe(path.as_ref())?;
        let entries = decode_pack_manifest(&buf)?;
        tracing::info!(
            path = %Path::new(path.as_ref()).display(),
            entries = entries.len(),
            "pack opened"
        );

        // --- Resolve the per-instance lifecycle surface. Each `Symbol` is
        // dereferenced to a bare fn pointer, valid as long as the
        // `Rc<PackLib>` below stays loaded. -----------------------------
        let lib = &pack_lib.lib;
        // SAFETY: each export matches its generated signature.
        let create =
            *unsafe { resolve::<PackCreateFn>(lib, abi::SYM_PACK_CREATE, "fsw_pack_create")? };
        let bind_init =
            *unsafe { resolve::<BindInitFn>(lib, abi::SYM_PACK_BIND_INIT, "fsw_pack_bind_init")? };
        let execute =
            *unsafe { resolve::<ExecuteFn>(lib, abi::SYM_PACK_EXECUTE, "fsw_pack_execute")? };
        let shutdown =
            *unsafe { resolve::<ShutdownFn>(lib, abi::SYM_PACK_SHUTDOWN, "fsw_pack_shutdown")? };
        let destroy =
            *unsafe { resolve::<DestroyFn>(lib, abi::SYM_PACK_DESTROY, "fsw_pack_destroy")? };

        Ok(Self {
            lib: Rc::new(pack_lib),
            entries,
            fns: PackFns {
                create,
                bind_init,
                execute,
                shutdown,
                destroy,
            },
        })
    }

    /// The exported entry names, in manifest order.
    pub fn system_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.descriptor.name.as_str())
    }

    /// Whether the pack exports exactly one entry, in which case a `system`
    /// node may omit `type=`.
    pub fn sole_system(&self) -> Option<&str> {
        match self.entries.as_slice() {
            [only] => Some(only.descriptor.name.as_str()),
            _ => None,
        }
    }

    /// Select the entry named `name` as a registrable [`DlSystem`]. Cheap;
    /// the pack stays open (shared `Rc`) as long as any selection or slot
    /// lives.
    pub fn system(&self, name: &str) -> Result<DlSystem, DlError> {
        let index = self
            .entries
            .iter()
            .position(|e| e.descriptor.name == name)
            .ok_or_else(|| DlError::UnknownPackSystem {
                name: name.to_string(),
                available: self
                    .entries
                    .iter()
                    .map(|e| e.descriptor.name.to_string())
                    .collect(),
            })?;
        let entry = &self.entries[index];
        Ok(DlSystem {
            lib: self.lib.clone(),
            fns: self.fns,
            index: index as u32,
            descriptor: entry.descriptor.clone(),
            params_schema: entry.params_schema.clone(),
            params_default: entry.params_default.clone(),
            reloadable: entry.reloadable,
        })
    }
}

/// One selected pack entry, loaded and ready to register: the unit the wiring
/// resolver's dl path and a slot's allowed-occupant list consume.
///
/// The library handle lives in an `Rc` shared with every loaded slot built
/// from this handle. The shared object stays loaded and the pack stays open
/// while any slot exists.
pub struct DlSystem {
    lib: Rc<PackLib>,
    fns: PackFns,
    index: u32,
    descriptor: SystemDescriptor,
    /// The exported `Params` schema, kept so the host can schema-encode a
    /// params value tree without ever linking the `Params` type.
    params_schema: OwnedNamedType,
    /// The entry's declared default-params blob, the base a config value tree
    /// overlays.
    params_default: Option<Vec<u8>>,
    reloadable: bool,
}

impl DlSystem {
    /// The self-description the builder validates wiring against. Its ports'
    /// `announce` closures already prefix the vtable ids carried across the
    /// ABI.
    pub fn descriptor(&self) -> &SystemDescriptor {
        &self.descriptor
    }

    /// The exported `Params` schema, which lets the wiring resolver encode a
    /// config value tree into the canonical postcard bytes without linking the
    /// `Params` type.
    pub fn params_schema(&self) -> &OwnedNamedType {
        &self.params_schema
    }

    /// Whether the entry can be instantiated more than once. A `.state(...)`
    /// entry cannot, and so can never be a slot occupant.
    pub fn reloadable(&self) -> bool {
        self.reloadable
    }

    /// The entry's declared default-params blob, if any.
    pub fn params_default(&self) -> Option<&[u8]> {
        self.params_default.as_deref()
    }

    /// Builds a fresh [`DlSlot`] without consuming the handle. Each call runs
    /// `fsw_pack_create` for a new opaque state and clones the
    /// `Rc<PackLib>`, so one loaded entry can produce any number of slots,
    /// one per load or reset.
    ///
    /// # Safety
    /// Every region named by an `FswRing` in `inputs`/`outputs` must satisfy
    /// [`RingBuffer::attach_raw`](metor_fsw_ring::RingBuffer::attach_raw)'s
    /// contract, and must stay live until the slot's `Drop` has called
    /// `fsw_pack_destroy`. The coordinator guarantees this by keeping the
    /// owning ring table alive past the slot.
    pub(crate) unsafe fn make_slot(
        &self,
        params: &[u8],
        inputs: Vec<FswRing>,
        outputs: Vec<FswRing>,
        name: &str,
        instance: &str,
        mount: crate::Mount,
    ) -> DlSlot {
        let (ptr, len) = if params.is_empty() {
            (core::ptr::null(), 0)
        } else {
            (params.as_ptr(), params.len())
        };
        let mount_word = match mount {
            crate::Mount::Wired => 0,
            crate::Mount::SlotOccupant => 1,
        };
        // SAFETY: `ptr`/`len` name a readable byte range (or null/0); the
        // export decodes them immediately, so they need not outlive the call.
        let state =
            unsafe { (self.fns.create)(self.lib.pack_ptr(), self.index, mount_word, ptr, len) };
        // A null state means creation failed inside the shared object (bad
        // params, a non-reloadable entry re-created, or a panic; the ABI shim
        // catches the unwind and returns null). Latch the failure now: `step`
        // early-returns on a null state without touching `slot_state`, so a
        // slot that started `Running` would never report a stop and the
        // coordinator would poll a zombie forever.
        let slot_state = if state.is_null() {
            SlotState::Stopped {
                reason: StopReason::Panicked,
            }
        } else {
            SlotState::Running
        };
        DlSlot {
            lib: self.lib.clone(),
            bind_init: self.fns.bind_init,
            execute: self.fns.execute,
            shutdown: self.fns.shutdown,
            destroy: self.fns.destroy,
            state,
            inputs,
            outputs,
            name: Arc::from(name),
            instance: Arc::from(instance),
            slot_state,
        }
    }
}

// ---------------------------------------------------------------------------
// DlSlot, a dlopen'd system behind the CyclicSlot interface
// ---------------------------------------------------------------------------

/// A running instance of a loaded pack entry, driven by the coordinator
/// through the same `Box<dyn CyclicSlot>` interface as a statically linked
/// runner. `init` hands the shared object its per-port [`FswRing`] arrays,
/// `step` calls `fsw_pack_execute` and folds the returned [`FswStatus`] into
/// the tracked [`SlotState`], and `Drop` calls `fsw_pack_destroy` (see the
/// module teardown note). Built by [`DlSystem::make_slot`].
pub(crate) struct DlSlot {
    /// Keeps the shared object loaded (and its pack open) for the slot's
    /// whole life; drops after the `Drop` body has run `fsw_pack_destroy`.
    #[allow(dead_code)]
    lib: Rc<PackLib>,
    bind_init: BindInitFn,
    execute: ExecuteFn,
    shutdown: ShutdownFn,
    destroy: DestroyFn,
    /// The opaque state from `fsw_pack_create`, owned by the shared object
    /// and dropped by `fsw_pack_destroy`. Nulled after destroy so `Drop` is
    /// idempotent.
    state: *mut c_void,
    /// Input ring handles in descriptor order, viewing the producers' rings.
    inputs: Vec<FswRing>,
    /// Output ring handles in descriptor order, this system's own writer rings.
    outputs: Vec<FswRing>,
    /// Status identity, type-level like a static system's `System::NAME`.
    name: Arc<str>,
    /// Instance name passed to `fsw_pack_bind_init`, the `source` stamped on
    /// the entry's log events.
    instance: Arc<str>,
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
        // SAFETY: `state` is the live, bound `fsw_pack_create` pointer.
        let raw = unsafe { (self.execute)(self.state, now.0 as u64) };
        // Untrusted word from foreign code; see the module trust boundary note.
        FswStatus::from_raw(raw)
    }

    /// One sequence-mode step for the worker's run loop: like
    /// [`execute_raw`](Self::execute_raw), but terminal statuses are
    /// **latched** — a later call re-serves the terminal without touching the
    /// export. That makes poll-once a worker invariant: a `Ready` future is
    /// never polled again, however long a skewed or buggy host keeps ringing
    /// the doorbell.
    ///
    /// `Done` keeps the foreign state alive (the occupant holds its ring
    /// roles until shutdown, exactly like an in-process `Done` occupant
    /// holding its ports until `Reset`/`Load`/`Unload`); `Panicked` destroys
    /// it immediately, the same policy as [`step`](CyclicSlot::step).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn step_seq(&mut self, now: Timestamp) -> FswStatus {
        // The latch is checked before anything else: no path reaches the
        // execute export from a terminal state.
        match self.slot_state {
            SlotState::Done { .. } => return FswStatus::Done,
            SlotState::Stopped { .. } => return FswStatus::Panicked,
            _ if self.state.is_null() => return FswStatus::Panicked,
            _ => {}
        }
        let status = self.execute_raw(now);
        match status {
            FswStatus::Running => {}
            // The Completed/Aborted/Failed detail rides the occupant's own
            // SequenceStatus frame, host-side; the latch needs only the
            // terminal.
            FswStatus::Done => self.slot_state = SlotState::Done { outcome: 0 },
            FswStatus::Panicked => {
                self.slot_state = SlotState::Stopped {
                    reason: StopReason::Panicked,
                };
                // Destroy the foreign state now so its non-owning ports
                // release their ring roles; nulling keeps `shutdown` and
                // `Drop` as no-ops afterwards.
                // SAFETY: `state` is the live pointer, handed to destroy
                // exactly once.
                unsafe { (self.destroy)(self.state) };
                self.state = core::ptr::null_mut();
            }
        }
        status
    }
}

impl CyclicSlot for DlSlot {
    fn init(&mut self) {
        if self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live `fsw_pack_create` pointer; the handle
        // arrays name live regions the coordinator keeps alive past this slot.
        unsafe {
            (self.bind_init)(
                self.state,
                self.inputs.as_ptr(),
                self.inputs.len(),
                self.outputs.as_ptr(),
                self.outputs.len(),
                self.instance.as_ptr(),
                self.instance.len(),
            );
        }
    }

    fn step(&mut self, now: Timestamp) {
        if self.slot_state.is_stopped() || self.state.is_null() {
            return;
        }
        // SAFETY: `state` is the live, bound `fsw_pack_create` pointer.
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
        // SAFETY: `state` is the live `fsw_pack_create` pointer.
        unsafe { (self.shutdown)(self.state) };
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }
}

impl Drop for DlSlot {
    fn drop(&mut self) {
        if !self.state.is_null() {
            // SAFETY: `state` is the live `fsw_pack_create` pointer, handed to
            // `fsw_pack_destroy` exactly once; nulled so a double-drop is a
            // no-op.
            unsafe { (self.destroy)(self.state) };
            self.state = core::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `DlSlot` over stub exports, no artifact involved: `lib` is a handle
    /// to this very process (with no pack to close) and `state` a dangling
    /// non-null the stubs never dereference.
    #[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
    fn stub_slot(execute: ExecuteFn, destroy: DestroyFn) -> DlSlot {
        unsafe extern "C" fn bind(
            _: *mut c_void,
            _: *const FswRing,
            _: usize,
            _: *const FswRing,
            _: usize,
            _: *const u8,
            _: usize,
        ) {
        }
        unsafe extern "C" fn nop(_: *mut c_void) {}
        DlSlot {
            lib: Rc::new(PackLib {
                pack: PackGuard {
                    ptr: core::ptr::null_mut(),
                    close: None,
                },
                lib: libloading::os::unix::Library::this().into(),
            }),
            bind_init: bind,
            execute,
            shutdown: nop,
            destroy,
            state: core::ptr::NonNull::<c_void>::dangling().as_ptr(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            name: Arc::from("stub"),
            instance: Arc::from("stub"),
            slot_state: SlotState::Running,
        }
    }

    /// The poll-once guard: `step_seq` latches a terminal `Done` and
    /// re-serves it without ever reaching the execute export again.
    #[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
    #[test]
    fn step_seq_latches_done() {
        use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static POLLS: AtomicU32 = AtomicU32::new(0);
        unsafe extern "C" fn exec(_: *mut c_void, _: u64) -> u32 {
            POLLS.fetch_add(1, Relaxed);
            FswStatus::Done as u32
        }
        unsafe extern "C" fn nop(_: *mut c_void) {}
        let mut slot = stub_slot(exec, nop);
        assert_eq!(slot.step_seq(Timestamp(1)), FswStatus::Done);
        assert_eq!(slot.step_seq(Timestamp(2)), FswStatus::Done);
        assert_eq!(POLLS.load(Relaxed), 1, "a Ready future is never re-polled");
        assert!(matches!(slot.slot_state, SlotState::Done { .. }));
    }

    /// `Panicked` destroys the foreign state exactly once (releasing its
    /// ring roles, the same policy as `step`) and is re-served from the
    /// latch; the nulled state keeps `Drop` a no-op.
    #[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
    #[test]
    fn step_seq_panicked_destroys_once() {
        use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static POLLS: AtomicU32 = AtomicU32::new(0);
        static DESTROYS: AtomicU32 = AtomicU32::new(0);
        unsafe extern "C" fn exec(_: *mut c_void, _: u64) -> u32 {
            POLLS.fetch_add(1, Relaxed);
            FswStatus::Panicked as u32
        }
        unsafe extern "C" fn destroy(_: *mut c_void) {
            DESTROYS.fetch_add(1, Relaxed);
        }
        let mut slot = stub_slot(exec, destroy);
        assert_eq!(slot.step_seq(Timestamp(1)), FswStatus::Panicked);
        assert_eq!(slot.step_seq(Timestamp(2)), FswStatus::Panicked);
        assert_eq!(POLLS.load(Relaxed), 1);
        drop(slot);
        assert_eq!(
            DESTROYS.load(Relaxed),
            1,
            "destroyed at the panic, not at Drop"
        );
    }

    /// An entry declaring a host capability is a clean load rejection,
    /// never a bind-time panic; an empty list (every real export) passes.
    #[test]
    fn load_rejects_declared_capabilities() {
        let mk = |capabilities| PackEntryDesc {
            descriptor: crate::SystemDescriptor {
                name: "rogue".to_string(),
                kind: crate::SystemKind::Cyclic,
                inputs: Vec::new(),
                outputs: Vec::new(),
                capabilities,
            },
            params_schema: OwnedNamedType::from(<() as postcard_schema::Schema>::SCHEMA),
            params_docs: Vec::new(),
            reloadable: true,
            params_default: None,
        };

        assert!(reject_capabilities(mk(Vec::new())).is_ok());
        let err = reject_capabilities(mk(vec![crate::Capability::ReceiveAll]))
            .map(|_| ())
            .expect_err("host-only capability rejected");
        assert!(
            matches!(&err, DlError::UnsupportedCapabilities(c) if c == &[crate::Capability::ReceiveAll]),
            "{err:?}"
        );
    }
}
