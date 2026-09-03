//! The C ABI a pack `cdylib` exports and the host resolves at load.
//!
//! A pack `.so` exports a small, versioned `extern "C"` surface. Both halves
//! compile against this module, which defines the `repr(C)` handles ([`FswRing`],
//! [`FswStatus`]), the serialized manifest ([`PackManifest`], carrying each
//! entry's [`SystemDescriptor`] directly), the symbol names the host resolves,
//! and the `run_pack_*` helpers that [`export_pack!`](crate::export_pack)
//! delegates to so the generated exports stay one-liners.
//!
//! The lifecycle, in call order:
//!
//! ```text
//! host                                     pack .so
//! ----                                     --------
//! fsw_abi_version             ---------->  FSW_ABI_VERSION (checked first)
//! fsw_pack_open               ---------->  run_pack_open      (pack() once, opaque pack)
//! fsw_pack_describe           ---------->  run_pack_describe  (encode manifest, byte length)
//! fsw_pack_manifest_ptr       ---------->  run_pack_manifest_ptr (bytes the pack still owns)
//! fsw_pack_create(idx, mount) ---------->  run_pack_create    (per-instance state pointer)
//! fsw_pack_bind_init(rings)   ---------->  run_pack_bind_init (attach rings, driver init)
//! fsw_pack_execute(now)       --(loop)-->  run_pack_execute   (one step, FswStatus word)
//! fsw_pack_shutdown           ---------->  run_pack_shutdown
//! fsw_pack_destroy            ---------->  run_pack_destroy   (drop the instance state)
//! fsw_pack_close              ---------->  run_pack_close     (drop the Pack, last)
//! fsw_pack_alloc/free         ---------->  run_pack_alloc/free (ring regions, host-driven)
//! ```
//!
//! `create`..`destroy` repeat per instance (two `system` nodes over one entry,
//! or a slot occupant reloaded); `open`/`close` bracket the whole load. The
//! host guarantees every instance is destroyed before `close`, and `close`
//! runs before the library unloads.
//!
//! Three rules make this sound across an otherwise unstable Rust ABI:
//!
//! - **Only serialized bytes and `repr(C)` handles cross the boundary.** The
//!   manifest and the `Params` blob are postcard bytes; everything else is a
//!   `(pointer, length)` pair or a plain integer. No `Vec`, `Arc`, or vtable ever
//!   crosses by value.
//! - **No unwind crosses `extern "C"`.** Every `run_pack_*` helper wraps its body
//!   in [`catch_unwind`] and converts a caught panic into a null pointer, a
//!   non-zero `describe` code, or [`FswStatus::Panicked`]. An escaping unwind
//!   would be undefined behavior.
//! - **Each side frees only what it allocated.** The pack and instance boxes are
//!   created and dropped inside the same `.so`, and the manifest
//!   [`run_pack_describe`] encodes stays on the pack until [`run_pack_close`],
//!   so the host copies out of it rather than freeing it.
//!
//! Ports bind positionally. The host sends [`FswRing`] handles in the order the
//! entry's descriptor lists the ports, and [`RawBinder`] walks them in the same
//! order on the `.so` side. Loaded systems run in-process on the cyclic
//! schedule, so every wake endpoint is `NoWake`.

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;

use metor_fsw_ring::{RingBuffer, WakeSink, WakeSource};
use metor_proto::types::Timestamp;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};

use crate::binder::RingSource;
use crate::descriptor::SystemDescriptor;

// ---------------------------------------------------------------------------
// Version + identity
// ---------------------------------------------------------------------------

/// The ABI word a host checks for equality before any other call.
///
/// Bump this on any change to the C surface or to the manifest's serialized
/// shape, once per released ABI shape. A mismatch fails the load cleanly
/// instead of risking a crash on a stale binary.
pub const FSW_ABI_VERSION: u32 = 12;

// ---------------------------------------------------------------------------
// repr(C) handles
// ---------------------------------------------------------------------------

/// A ring handle points a system at one host-mapped memory region, which the
/// system attaches as a ring via [`RingBuffer::attach_raw`] at bind time.
///
/// Capacity, data offset, reader-table offset, and reader limits are all
/// self-describing in the region header, so the handle is just base, length,
/// and role.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FswRing {
    /// Base address of the ring region.
    pub base: *mut u8,
    /// Length of the ring region in bytes.
    pub len: usize,
    /// [`ROLE_INPUT`] or [`ROLE_OUTPUT`]. The host hands an output region without
    /// creating a writer of its own, so the system stays the sole `Writer`.
    pub role: u8,
}

/// [`FswRing::role`] for an input port; the system registers a read-only `View`.
pub const ROLE_INPUT: u8 = 0;
/// [`FswRing::role`] for an output port; the system is the buffer's sole `Writer`.
pub const ROLE_OUTPUT: u8 = 1;

/// A status word reports the outcome of one execute call back to the host.
/// `repr(u32)` keeps it FFI-stable.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FswStatus {
    /// The system ran (or is runnable); keep cycling it.
    Running = 0,
    /// A panic was caught at the boundary, or the state was never bound. The
    /// host telemeters it and hard-stops the slot.
    Panicked = 1,
    /// The entry's future returned `Ready`, a terminal, non-error stop. The
    /// `Completed`/`Aborted`/`Failed` detail rides the
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) frame, not this
    /// word; a cyclic entry never returns it.
    Done = 2,
}

impl FswStatus {
    /// Convert the raw `u32` a foreign execute export returned into an
    /// `FswStatus`, folding anything out of range to
    /// [`Panicked`](FswStatus::Panicked).
    ///
    /// `FswStatus` is `repr(u32)`, so constructing one directly from an
    /// arbitrary word (a transmute, or a fn pointer typed to return `FswStatus`
    /// and trusted verbatim) is undefined behavior the moment the value falls
    /// outside the declared discriminants. A `.so` is foreign code the host
    /// cannot make Rust trust; a stale build or a hand-rolled exporter could
    /// hand back anything. Callers that read the word off a resolved symbol must
    /// route it through here.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0 => FswStatus::Running,
            2 => FswStatus::Done,
            _ => FswStatus::Panicked,
        }
    }
}

// ---------------------------------------------------------------------------
// Symbol-name constants
// ---------------------------------------------------------------------------

/// `fsw_abi_version` returns the ABI word ([`FSW_ABI_VERSION`]).
pub const SYM_ABI_VERSION: &[u8] = b"fsw_abi_version\0";
/// `fsw_pack_open` constructs the crate's [`Pack`](crate::Pack) once and
/// returns it as an opaque pointer (null if `pack()` panicked).
pub const SYM_PACK_OPEN: &[u8] = b"fsw_pack_open\0";
/// `fsw_pack_describe` encodes the [`PackManifest`], stashes the bytes on the
/// pack, and returns their length (`-1` on failure).
pub const SYM_PACK_DESCRIBE: &[u8] = b"fsw_pack_describe\0";
/// `fsw_pack_manifest_ptr` returns the base of the bytes the preceding
/// `fsw_pack_describe` stashed, which stay valid until `fsw_pack_close`.
pub const SYM_PACK_MANIFEST_PTR: &[u8] = b"fsw_pack_manifest_ptr\0";
/// `fsw_pack_create` runs entry `index`'s create phase (decode params, build
/// state) and boxes the opaque per-instance state.
pub const SYM_PACK_CREATE: &[u8] = b"fsw_pack_create\0";
/// `fsw_pack_bind_init` binds the created entry's ports over the host's ring
/// handles and runs its init.
pub const SYM_PACK_BIND_INIT: &[u8] = b"fsw_pack_bind_init\0";
/// `fsw_pack_execute` runs one step and returns an [`FswStatus`] word.
pub const SYM_PACK_EXECUTE: &[u8] = b"fsw_pack_execute\0";
/// `fsw_pack_shutdown` runs the entry's shutdown.
pub const SYM_PACK_SHUTDOWN: &[u8] = b"fsw_pack_shutdown\0";
/// `fsw_pack_destroy` drops one instance's boxed state inside the `.so`.
pub const SYM_PACK_DESTROY: &[u8] = b"fsw_pack_destroy\0";
/// `fsw_pack_close` drops the [`Pack`](crate::Pack) itself, after every
/// instance state has been destroyed.
pub const SYM_PACK_CLOSE: &[u8] = b"fsw_pack_close\0";

// ---------------------------------------------------------------------------
// RawBinder
// ---------------------------------------------------------------------------

/// A raw binder walks the host-provided [`FswRing`] arrays, attaching each
/// region as a ring while the port bundles bind; it is the `.so`-side
/// [`RingSource`].
///
/// `next_output` and `next_input` pop the next handle in the same positional
/// order the descriptor lists the ports. Every wake endpoint is `NoWake`, so
/// the generic wake parameters are default-constructed.
pub struct RawBinder<'a> {
    inputs: slice::Iter<'a, FswRing>,
    outputs: slice::Iter<'a, FswRing>,
    instance: &'a str,
}

impl<'a> RawBinder<'a> {
    /// Build a cursor over the host's input and output handle arrays.
    /// `instance` is the host-assigned instance name, stamped into the bound
    /// entry's log events.
    ///
    /// # Safety
    /// Every region named by an `FswRing` here must satisfy
    /// [`RingBuffer::attach_raw`]'s contract, a live header-valid ring region
    /// that outlives every `Writer` and `View` this binder produces.
    pub unsafe fn new(inputs: &'a [FswRing], outputs: &'a [FswRing], instance: &'a str) -> Self {
        Self {
            inputs: inputs.iter(),
            outputs: outputs.iter(),
            instance,
        }
    }

    fn attach(handle: &FswRing) -> RingBuffer {
        // SAFETY: `RawBinder::new`'s caller asserts each region is a live,
        // header-valid ring that outlives the produced handles; `attach_raw`
        // validates the header.
        unsafe { RingBuffer::attach_raw(handle.base, handle.len) }
            .expect("host handed a valid ring region (header validated)")
    }
}

impl<'a> RingSource for RawBinder<'a> {
    fn next_output<WD>(&mut self) -> (RingBuffer, WD)
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        let h = self
            .outputs
            .next()
            .expect("bind() walks output ports in descriptors() order");
        (Self::attach(h), WD::default())
    }

    fn next_input<RD>(&mut self) -> (RingBuffer, RD)
    where
        RD: WakeSink + Default + Clone + 'static,
    {
        let h = self
            .inputs
            .next()
            .expect("bind() walks input ports in descriptors() order");
        (Self::attach(h), RD::default())
    }

    // `output_registry()` keeps the panicking default: a loaded system is never
    // the telemetry downlink, so it has no broad-access registry.

    fn instance_name(&self) -> &str {
        self.instance
    }
}

// ---------------------------------------------------------------------------
// The pack manifest (postcard)
// ---------------------------------------------------------------------------

/// One pack entry's manifest form: its [`SystemDescriptor`] verbatim, plus
/// the entry facts that live beside the descriptor rather than in it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackEntryDesc {
    pub descriptor: SystemDescriptor,
    /// `<Params as postcard_schema::Schema>::SCHEMA` in its owned form, so
    /// the host can encode params from configuration without linking the
    /// `Params` type.
    pub params_schema: OwnedNamedType,
    /// Per-field params docs, `(field path, doc)`, from the `Params` type's
    /// `#[derive(ParamsDocs)]`. Empty when nothing is documented.
    pub params_docs: Vec<(String, String)>,
    /// `false` for a `.state(...)` entry: one instance, never a slot occupant.
    pub reloadable: bool,
    /// Canonical postcard bytes of the entry's default params, when declared;
    /// the params encoder overlays config onto them.
    pub params_default: Option<Vec<u8>>,
}

/// The whole pack's manifest, what `fsw_pack_describe` sends: one
/// [`PackEntryDesc`] per entry, in the registration order that
/// `fsw_pack_create` indexes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackManifest {
    pub systems: Vec<PackEntryDesc>,
}

// ---------------------------------------------------------------------------
// Pack export helpers
// ---------------------------------------------------------------------------

/// The heap allocation behind the opaque pack pointer: the crate's [`Pack`],
/// constructed once by [`run_pack_open`] and dropped by [`run_pack_close`].
struct PackHost {
    pack: crate::Pack,
    /// The encoded manifest [`run_pack_describe`] stashed, read out through
    /// [`run_pack_manifest_ptr`] and freed with the pack. Keeping it here is
    /// what lets describe return plain scalars: a wasm host has no way to
    /// receive a callback, and no way to allocate guest memory before the
    /// manifest has named the allocator.
    manifest: Option<Vec<u8>>,
}

/// The heap allocation behind one instance's opaque state pointer.
///
/// `pending` holds the created (params decoded, state built) but unbound
/// entry until [`run_pack_bind_init`] binds its ports and yields the
/// [`Driver`](crate::Driver). `poisoned` latches a caught execute panic so
/// later cycles short-circuit to [`FswStatus::Panicked`].
struct PackAbiState {
    pending: Option<crate::Pending>,
    mount: crate::Mount,
    driver: Option<Box<dyn crate::Driver>>,
    poisoned: bool,
}

/// View a nullable `(ptr, len)` byte range as a slice, empty for null or zero.
///
/// # Safety
/// `ptr..ptr+len` is a readable byte range, or `ptr` is null / `len == 0`.
unsafe fn bytes_from_raw<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: caller asserts `ptr..ptr+len` is readable.
        unsafe { slice::from_raw_parts(ptr, len) }
    }
}

/// View a nullable `(ptr, n)` [`FswRing`] array as a slice, empty for null or
/// zero.
///
/// # Safety
/// `ptr` names `n` valid [`FswRing`] handles, or is null / `n == 0`.
unsafe fn rings_from_raw<'a>(ptr: *const FswRing, n: usize) -> &'a [FswRing] {
    if ptr.is_null() || n == 0 {
        &[]
    } else {
        // SAFETY: caller asserts `n` valid handles at `ptr`.
        unsafe { slice::from_raw_parts(ptr, n) }
    }
}

/// The `mount` word of `fsw_pack_create`, decoded leniently: an unknown word
/// from a foreign host folds to the ordinary wired mount.
fn mount_from_raw(raw: u32) -> crate::Mount {
    match raw {
        1 => crate::Mount::SlotOccupant,
        _ => crate::Mount::Wired,
    }
}

/// `fsw_pack_open`: construct the crate's [`Pack`](crate::Pack) by calling
/// its `pack()` fn once and box it. Returns null if construction panics.
pub fn run_pack_open(pack_fn: fn() -> crate::Pack) -> *mut c_void {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(PackHost {
            pack: pack_fn(),
            manifest: None,
        })) as *mut c_void
    }));
    outcome.unwrap_or(core::ptr::null_mut())
}

/// `fsw_pack_describe`: assemble the [`PackManifest`] off the entries,
/// postcard-encode it, and stash the bytes on the pack. Returns their length,
/// or `-1` if the pack pointer is null or anything panics; the bytes
/// themselves come back through [`run_pack_manifest_ptr`].
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`].
pub unsafe fn run_pack_describe(pack: *mut c_void) -> i64 {
    if pack.is_null() {
        return -1;
    }
    // SAFETY: caller asserts `pack` is a live `PackHost` from `run_pack_open`.
    let host = unsafe { &mut *(pack as *mut PackHost) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // Shared state and capabilities stay on the static registry: a
        // loaded artifact gets neither a cross-FFI state-construction call
        // nor a read grant over every ring.
        assert!(
            host.pack.state_entries().next().is_none(),
            "packs with shared state cannot export over the pack ABI"
        );
        let msg = PackManifest {
            systems: host
                .pack
                .entries()
                .map(|e| {
                    assert!(
                        e.descriptor().capabilities.is_empty(),
                        "entry `{}` declares a capability; capability entries \
                         cannot export over the pack ABI",
                        e.name(),
                    );
                    let schema = OwnedNamedType::from(e.params_schema());
                    // Params docs are collected from this `.so`'s own
                    // `#[derive(ParamsDocs)]` submissions, keyed by schema name.
                    let params_docs = crate::params_docs::params_docs_for(&schema.name);
                    PackEntryDesc {
                        descriptor: e.descriptor().clone(),
                        params_schema: schema,
                        params_docs,
                        reloadable: e.reloadable(),
                        params_default: e.params_default().map(<[u8]>::to_vec),
                    }
                })
                .collect(),
        };
        postcard::to_allocvec(&msg).expect("pack manifest encodes (postcard)")
    }));
    match outcome {
        Ok(bytes) => {
            let len = bytes.len() as i64;
            host.manifest = Some(bytes);
            len
        }
        Err(_) => -1,
    }
}

/// `fsw_pack_manifest_ptr`: the base of the bytes the last
/// [`run_pack_describe`] stashed, null if describe never succeeded. The pack
/// owns them until [`run_pack_close`], so the host copies rather than frees.
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`].
pub unsafe fn run_pack_manifest_ptr(pack: *mut c_void) -> *const u8 {
    if pack.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller asserts `pack` is a live `PackHost` from `run_pack_open`.
    let host = unsafe { &*(pack as *mut PackHost) };
    host.manifest
        .as_ref()
        .map_or(core::ptr::null(), |bytes| bytes.as_ptr())
}

/// `fsw_pack_create`: run entry `index`'s create phase (decode the postcard
/// params, build the user state) and box the unbound instance state. Returns
/// null for a null pack, an out-of-range index, or a create failure (bad
/// params, a moved-in state already taken, a panic).
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`]; `params`/`params_len`
/// name a readable byte range (or null/0). The returned pointer is owned by
/// the caller and passed only to the other `run_pack_*` helpers, then to
/// [`run_pack_destroy`], all before [`run_pack_close`].
pub unsafe fn run_pack_create(
    pack: *mut c_void,
    index: u32,
    mount: u32,
    params: *const u8,
    params_len: usize,
) -> *mut c_void {
    if pack.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller asserts `pack` is a live `PackHost` from `run_pack_open`.
    // Entries are `FnMut`, so creating an instance needs the mutable borrow;
    // the host serializes pack calls (the cyclic loop is single-threaded).
    let host = unsafe { &mut *(pack as *mut PackHost) };
    // SAFETY: caller asserts `params..params+params_len` is readable (or null/0).
    let bytes = unsafe { bytes_from_raw(params, params_len) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let entry = host
            .pack
            .entry_at_mut(index as usize)
            .expect("entry index in range (host resolved it from this pack's manifest)");
        let created = entry
            .create(crate::EntryParams::Postcard(bytes))
            .expect("entry create (params decode + state construction)");
        // The host binds positionally against the exported static manifest;
        // an instance that minted ports would misbind silently, so it is
        // rejected here instead.
        assert!(
            created.instance_desc.is_none(),
            "entry `{}` minted ports at create; config-minted ports cannot cross the pack ABI",
            entry.name(),
        );
        Box::into_raw(Box::new(PackAbiState {
            pending: Some(created.pending),
            mount: mount_from_raw(mount),
            driver: None,
            poisoned: false,
        })) as *mut c_void
    }));
    outcome.unwrap_or(core::ptr::null_mut())
}

/// `fsw_pack_bind_init`: bind the created entry's ports over the host's
/// [`FswRing`] arrays (positionally, in descriptor order, via [`RawBinder`])
/// and run the driver's init. Bind and init are fused; a caught panic leaves
/// the driver unbound, so [`run_pack_execute`] reports
/// [`FswStatus::Panicked`]. `name`/`name_len` carry the host-assigned
/// instance name (UTF-8; a non-UTF-8 or null name binds as empty), stamped
/// into the entry's log events.
///
/// # Safety
/// `state` is a live pointer from [`run_pack_create`]. `inputs`/`outputs`
/// name `n_in`/`n_out` valid [`FswRing`] handles whose regions satisfy
/// [`RingBuffer::attach_raw`]'s contract and outlive the driver (until
/// [`run_pack_destroy`]). `name`/`name_len` name a readable byte range (or
/// null/0), valid for the duration of the call.
pub unsafe fn run_pack_bind_init(
    state: *mut c_void,
    inputs: *const FswRing,
    n_in: usize,
    outputs: *const FswRing,
    n_out: usize,
    name: *const u8,
    name_len: usize,
) {
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `PackAbiState`.
    let st = unsafe { &mut *(state as *mut PackAbiState) };
    // SAFETY: caller asserts the handle arrays are valid (or null/0).
    let (in_slice, out_slice) =
        unsafe { (rings_from_raw(inputs, n_in), rings_from_raw(outputs, n_out)) };
    // SAFETY: caller asserts the name range is readable (or null/0).
    let instance = core::str::from_utf8(unsafe { bytes_from_raw(name, name_len) }).unwrap_or("");
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller asserts each region outlives the driver.
        let mut binder = unsafe { RawBinder::new(in_slice, out_slice, instance) };
        let pending = st
            .pending
            .take()
            .expect("fsw_pack_create populated the pending entry before bind");
        let mut src = crate::AnySource::Raw(&mut binder);
        let mut driver = pending(&mut src, st.mount);
        driver.init();
        st.driver = Some(driver);
    }));
}

/// `fsw_pack_execute`: run one step and return the [`FswStatus`] word. A
/// caught panic latches the poison flag and returns
/// [`FswStatus::Panicked`], as does an unbound or already-poisoned state; a
/// driver whose future finished reports [`FswStatus::Done`].
///
/// # Safety
/// `state` is a live pointer from [`run_pack_create`].
pub unsafe fn run_pack_execute(state: *mut c_void, now: u64) -> FswStatus {
    if state.is_null() {
        return FswStatus::Panicked;
    }
    // SAFETY: caller asserts `state` is a live `PackAbiState`.
    let st = unsafe { &mut *(state as *mut PackAbiState) };
    if st.poisoned {
        return FswStatus::Panicked;
    }
    let Some(driver) = st.driver.as_mut() else {
        return FswStatus::Panicked;
    };
    let now = Timestamp(now as i64);
    // Republish the FSW clock inside this linkage unit (a dylib links its
    // own copy of the static), so tracing events fired during the step are
    // born on the cycle timeline.
    crate::clock::set_now(now);
    let outcome = catch_unwind(AssertUnwindSafe(|| driver.step(now)));
    match outcome {
        Ok(crate::StepStatus::Running) => FswStatus::Running,
        Ok(crate::StepStatus::Done(_)) => FswStatus::Done,
        Err(_) => {
            st.poisoned = true;
            FswStatus::Panicked
        }
    }
}

/// `fsw_pack_shutdown`: run the driver's shutdown once. A panic is caught
/// and swallowed; a poisoned or unbound state is a no-op.
///
/// # Safety
/// `state` is a live pointer from [`run_pack_create`].
pub unsafe fn run_pack_shutdown(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `PackAbiState`.
    let st = unsafe { &mut *(state as *mut PackAbiState) };
    if st.poisoned {
        return;
    }
    if let Some(driver) = st.driver.as_mut() {
        let _ = catch_unwind(AssertUnwindSafe(|| driver.shutdown()));
    }
}

/// `fsw_pack_destroy`: drop one instance's boxed state inside the `.so`,
/// running the driver's drop and every port's `Drop` (releasing their ring
/// roles). Idempotent on null.
///
/// # Safety
/// `state` is a live pointer from [`run_pack_create`], not used afterward.
pub unsafe fn run_pack_destroy(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `PackAbiState`, transferred
    // here exactly once.
    let st = unsafe { Box::from_raw(state as *mut PackAbiState) };
    let _ = catch_unwind(AssertUnwindSafe(|| drop(st)));
}

/// `fsw_pack_close`: drop the [`Pack`](crate::Pack). Every instance state
/// must have been destroyed first (the host's loader guarantees the order by
/// holding the pack open as long as any slot exists). Idempotent on null.
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`], not used afterward, with
/// no live instance states.
pub unsafe fn run_pack_close(pack: *mut c_void) {
    if pack.is_null() {
        return;
    }
    // SAFETY: caller asserts `pack` is a live `PackHost`, transferred here
    // exactly once.
    let host = unsafe { Box::from_raw(pack as *mut PackHost) };
    let _ = catch_unwind(AssertUnwindSafe(|| drop(host)));
}

/// The alignment every [`RING_ALIGN`]-allocated region carries.
///
/// [`RingBuffer::attach_raw`] rejects a base that is not 8-aligned, so the
/// allocator entry points promise at least that.
///
/// [`RingBuffer::attach_raw`]: metor_fsw_ring::RingBuffer::attach_raw
pub const RING_ALIGN: usize = 8;

/// `fsw_pack_alloc`: hand the host `len` bytes out of the pack's own
/// allocator, aligned for a ring region. Null on a zero length, a bad layout,
/// or allocation failure.
///
/// A wasm host needs this because it cannot safely carve regions out of guest
/// memory itself: Rust's wasm allocator discovers its heap through
/// `memory.size`, so pages the host grows behind its back can later be handed
/// out again by the guest's own allocator. Asking the guest to allocate keeps
/// ownership on the side that manages the heap, and keeps "each side frees
/// only what it allocated" true across the boundary.
pub fn run_pack_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len, RING_ALIGN) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `layout` is non-zero-sized and well-formed.
    unsafe { std::alloc::alloc_zeroed(layout) }
}

/// `fsw_pack_set_now`: publish a cycle timestamp on the callee's own copy of
/// the ambient clock, before a phase that would otherwise fall back to wall
/// time.
///
/// A linkage unit's clock is unset until the first `fsw_pack_execute`
/// republishes it, so anything stamped during `bind`/`init`, a `LogEvent`
/// from the forward layer say, reaches for [`Timestamp::now`]. That is
/// merely inaccurate in a `.so` (wall time instead of the cycle's) and fatal
/// in wasm, where `wasm32-unknown-unknown` has no wall clock at all and
/// `SystemTime::now` panics. Calling this first gives init the same time axis
/// every later cycle uses.
pub fn run_pack_set_now(now: u64) {
    crate::clock::set_now(Timestamp(now as i64));
}

/// `fsw_pack_ring_init`: format a ring into a region from [`run_pack_alloc`].
/// Returns `0`, or `-1` on a bad region or config.
///
/// The guest does this itself because the region came from its allocator and
/// remains guest-owned. The ring header uses explicit-width fields, so the
/// wasm guest and a 64-bit host can both attach to the same bytes.
///
/// # Safety
/// `ptr`/`len` name a live region from [`run_pack_alloc`] that nothing else
/// is reading.
pub unsafe fn run_pack_ring_init(ptr: *mut u8, len: usize, capacity: u32, max_readers: u32) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let cfg = metor_fsw_ring::Config {
        capacity: capacity as usize,
        max_readers: max_readers as usize,
    };
    // `create_raw` asserts on an invalid config, and an unwind must not cross
    // the boundary; on wasm it cannot even be caught, since that target
    // aborts rather than unwinds.
    let formatted = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller asserts a live, exclusively-owned region.
        unsafe { metor_fsw_ring::RingBuffer::create_raw(ptr, len, cfg) }
    }));
    match formatted {
        Ok(Ok(ring)) => {
            drop(ring);
            0
        }
        _ => -1,
    }
}

/// `fsw_pack_free`: release a region from [`run_pack_alloc`]. Idempotent on
/// null.
///
/// # Safety
/// `ptr` came from [`run_pack_alloc`] with this same `len` and is not used
/// afterward.
pub unsafe fn run_pack_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len, RING_ALIGN) else {
        return;
    };
    // SAFETY: caller asserts `ptr`/`len` came from `run_pack_alloc`.
    unsafe { std::alloc::dealloc(ptr, layout) }
}

/// Emit the `extern "C"` exports of the pack ABI, delegating to the
/// `run_pack_*` helpers over the crate's `pack()` fn:
///
/// ```ignore
/// pub fn pack() -> Pack { Pack::new().system("nav", system(nav_execute)) }
/// metor_fsw_2::export_pack!(pack);                          // cdylib-only crate
/// metor_fsw_2::export_pack!(pack, feature = "export");     // cdylib + rlib recipe
/// ```
///
/// One invocation per crate; the symbols are un-namespaced C names, which is
/// exactly why a crate exports one *pack* rather than one system. The bare
/// form gates on `not(test)` so a test build of the same crate carries no
/// exports; the `feature =` form additionally gates on that cargo feature so
/// the rlib a host links for tests stays symbol-free.
#[macro_export]
macro_rules! export_pack {
    ($pack_fn:path) => {
        $crate::export_pack!(@items (not(test)), $pack_fn);
    };
    ($pack_fn:path, feature = $feat:literal) => {
        $crate::export_pack!(@items (all(feature = $feat, not(test))), $pack_fn);
    };
    (@items ($($cfg:tt)*), $pack_fn:path) => {
        #[cfg($($cfg)*)]
        const _: () = {
            use $crate::abi;
            use core::ffi::c_void;

            #[unsafe(no_mangle)]
            pub extern "C" fn fsw_abi_version() -> u32 {
                abi::FSW_ABI_VERSION
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn fsw_pack_open() -> *mut c_void {
                // The dylib links its own copy of tracing's dispatcher, so
                // install the pack-side forwarding subscriber here; without
                // it, tracing macros inside the pack silently no-op.
                $crate::logfwd::init_pack_tracing();
                abi::run_pack_open($pack_fn)
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_describe(pack: *mut c_void) -> i64 {
                // SAFETY: the host upholds run_pack_describe's contract.
                unsafe { abi::run_pack_describe(pack) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_manifest_ptr(pack: *mut c_void) -> *const u8 {
                // SAFETY: the host upholds run_pack_manifest_ptr's contract.
                unsafe { abi::run_pack_manifest_ptr(pack) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_create(
                pack: *mut c_void,
                index: u32,
                mount: u32,
                params: *const u8,
                params_len: usize,
            ) -> *mut c_void {
                // SAFETY: the host upholds run_pack_create's contract.
                unsafe { abi::run_pack_create(pack, index, mount, params, params_len) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_bind_init(
                state: *mut c_void,
                inputs: *const abi::FswRing,
                n_in: usize,
                outputs: *const abi::FswRing,
                n_out: usize,
                name: *const u8,
                name_len: usize,
            ) {
                // SAFETY: the host upholds run_pack_bind_init's contract.
                unsafe {
                    abi::run_pack_bind_init(state, inputs, n_in, outputs, n_out, name, name_len)
                }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_execute(state: *mut c_void, now: u64) -> u32 {
                // SAFETY: the host upholds run_pack_execute's contract.
                (unsafe { abi::run_pack_execute(state, now) }) as u32
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_shutdown(state: *mut c_void) {
                // SAFETY: the host upholds run_pack_shutdown's contract.
                unsafe { abi::run_pack_shutdown(state) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_destroy(state: *mut c_void) {
                // SAFETY: the host upholds run_pack_destroy's contract.
                unsafe { abi::run_pack_destroy(state) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_close(pack: *mut c_void) {
                // SAFETY: the host upholds run_pack_close's contract.
                unsafe { abi::run_pack_close(pack) }
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn fsw_pack_alloc(len: usize) -> *mut u8 {
                abi::run_pack_alloc(len)
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn fsw_pack_set_now(now: u64) {
                abi::run_pack_set_now(now)
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_ring_init(
                ptr: *mut u8,
                len: usize,
                capacity: u32,
                max_readers: u32,
            ) -> i32 {
                // SAFETY: the host upholds run_pack_ring_init's contract.
                unsafe { abi::run_pack_ring_init(ptr, len, capacity, max_readers) }
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_free(ptr: *mut u8, len: usize) {
                // SAFETY: the host upholds run_pack_free's contract.
                unsafe { abi::run_pack_free(ptr, len) }
            }
        };
    };
}

#[cfg(test)]
mod tests;
