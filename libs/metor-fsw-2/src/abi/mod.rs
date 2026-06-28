//! The `dlopen` C-ABI surface a system `cdylib` exports.
//!
//! A system `.so` exports a small, versioned, `extern "C"` surface the host `dlopen`s.
//! This module is the **shared contract both halves compile against**: the `repr(C)`
//! handles ([`FswRing`], [`FswStatus`]), the serialized descriptor mirrors
//! ([`PortDescMsg`]/[`SystemDescriptorMsg`]), the resolve-by symbol-name constants, and
//! the generic `run_*` helpers the [`export_system!`](crate::export_system) macro
//! delegates to so the generated code stays a one-liner per export.
//!
//! Only **serialized bytes** (the descriptor, the postcard `Params` blob) and
//! **`repr(C)` handles** ever cross the boundary — never a `Vec`/`Arc`/`VTable`
//! by value — which is what makes "dlopen across a stable Rust ABI" sound here.
//! Same-process and cyclic-only: every port uses [`NoWake`].
//!
//! ## Containment
//!
//! Every `run_*` helper wraps its body in [`std::panic::catch_unwind`] and converts
//! a caught panic to a null-safe outcome ([`FswStatus::Panicked`] / a null pointer /
//! a non-zero `describe` code) — **no unwind ever crosses the `extern "C"` boundary**
//! (that would be UB). Allocator ownership is honest too: the state [`Box`] is
//! created by [`run_create`] and dropped by [`run_destroy`] in the same `.so`, and
//! [`run_describe`] hands its bytes to a host-owned [`ByteSink`] rather than
//! returning an allocation the host would have to free.

use core::ffi::c_void;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;

use std::sync::Arc;

use metor_fsw_ring::{RawBacking, RingBuffer, WakeSink, WakeSource};
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto::vtable::{Op, VTable};
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};

use crate::binder::{BindPorts, RingSource};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::{AnnounceFn, Hz, PortDesc, SystemDescriptor, SystemKind};
use crate::system::{BuildSystem, CyclicRunner, CyclicSystem, Out, SystemOutput};

// ---------------------------------------------------------------------------
// Version + identity
// ---------------------------------------------------------------------------

/// The monotonic ABI word a host checks for equality before any other call.
/// **Bump on any change** to the C surface or the `*Msg` wire structs below; a
/// mismatch fails the load cleanly rather than risking a crash.
pub const FSW_ABI_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// repr(C) handles
// ---------------------------------------------------------------------------

/// A ring region handle the host fills from its `RingEntry`/`RingTable` and the
/// system turns back into a ring via [`RingBuffer::attach_raw`].
///
/// Everything else the system needs — capacity, data offset, reader-table offset,
/// `max_readers`, overrun — is **self-describing in the region header**, so the
/// handle is just `(base, len, role)`. Same-process v1: `base`/`len` come straight
/// from the host ring's [`RingBuffer::region`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FswRing {
    /// Region base — same-process: the host ring's `Backing::base()`.
    pub base: *mut u8,
    /// Region length — the host ring's `Backing::len()`.
    pub len: usize,
    /// `0` = input (the system registers a `View`), `1` = output (the system is
    /// the sole `Writer`). The host hands an output region but creates **no**
    /// writer itself, preserving single-writer discipline.
    pub role: u8,
}

/// `FswRing::role` for an input port (the system registers a read-only `View`).
pub const ROLE_INPUT: u8 = 0;
/// `FswRing::role` for an output port (the system is the buffer's sole `Writer`).
pub const ROLE_OUTPUT: u8 = 1;

/// The lifecycle status [`run_execute`] returns, mapped to/from [`SlotState`] so
/// the host can update its status frame without owning the input `View`.
/// `repr(u32)` keeps it FFI-stable.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FswStatus {
    /// The system ran (or is runnable); keep cycling it.
    Running = 0,
    /// An input lapped; the system permanently stopped itself (`StopReason::LappedInput`).
    StoppedLapped = 1,
    /// A panic was caught at the boundary, or the state was never bound; the host
    /// telemeters it and hard-stops the slot.
    Panicked = 2,
}

impl FswStatus {
    /// Map a runner's [`SlotState`] (after a `step`) to the FFI status.
    fn from_slot(state: SlotState) -> Self {
        match state {
            SlotState::Running => FswStatus::Running,
            SlotState::Stopped {
                reason: StopReason::LappedInput,
            } => FswStatus::StoppedLapped,
            // A `.so`-side `CyclicRunner` never sets `Panicked` through its `SlotState`
            // (a panic is caught by `catch_unwind` and returned directly as
            // `FswStatus::Panicked`), but the match must stay total.
            SlotState::Stopped {
                reason: StopReason::Panicked,
            } => FswStatus::Panicked,
        }
    }
}

/// The host-owned sink a `describe`-style export hands its serialized bytes to,
/// so the system frees its own buffer and the host copies — no cross-allocator
/// free.
pub type ByteSink = extern "C" fn(ctx: *mut c_void, buf: *const u8, len: usize);

// ---------------------------------------------------------------------------
// Symbol-name constants — one source of truth for host resolution
// ---------------------------------------------------------------------------

/// `fsw_abi_version` — the ABI word ([`FSW_ABI_VERSION`]).
pub const SYM_ABI_VERSION: &[u8] = b"fsw_abi_version\0";
/// `fsw_describe` — the serialized [`SystemDescriptorMsg`] via a [`ByteSink`].
pub const SYM_DESCRIBE: &[u8] = b"fsw_describe\0";
/// `fsw_create` — decode `Params`, construct the system, box the state.
pub const SYM_CREATE: &[u8] = b"fsw_create\0";
/// `fsw_bind_init` — reconstruct the typed bundles, run `System::init`.
pub const SYM_BIND_INIT: &[u8] = b"fsw_bind_init\0";
/// `fsw_execute` — run one cyclic `step`, returning an [`FswStatus`].
pub const SYM_EXECUTE: &[u8] = b"fsw_execute\0";
/// `fsw_shutdown` — run `System::shutdown`.
pub const SYM_SHUTDOWN: &[u8] = b"fsw_shutdown\0";
/// `fsw_destroy` — drop the boxed state inside the `.so`.
pub const SYM_DESTROY: &[u8] = b"fsw_destroy\0";

// ---------------------------------------------------------------------------
// Serialized descriptor mirrors (postcard)
// ---------------------------------------------------------------------------

/// The serializable mirror of [`PortDesc`]. [`PortDesc`] cannot
/// cross by value: its `announce` field is a closure over the frame type `F`, which
/// does not exist on the host side. So we serialize the **unprefixed** `vtable`
/// (exactly what `compatible()` needs, so wiring validation runs unchanged) plus the
/// **unprefixed** `metadata`, from which the host re-derives the missing `announce`
/// closure (the telemetry.md §6 metadata-driven prefix rewrite).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortDescMsg {
    /// `F::FRAME_ID`.
    pub frame_id: ComponentId,
    /// `F::NAME` — was `&'static str`; reconstructed by leaking at load.
    pub frame_name: String,
    /// `F::as_vtable()` — the unprefixed, frame-relative vtable (wiring compatibility).
    pub vtable: VTable,
    /// `F::MAX_SIZE` (worst-case table bytes).
    pub max_size: usize,
    /// Advisory rate, for buffer depth / async pacing.
    pub rate_hint: Option<Hz>,
    /// The unprefixed component metadata, so the host can synthesize a prefixed
    /// `announce` for telemetry without the static `F`.
    pub metadata: Vec<ComponentMetadata>,
}

/// The serializable mirror of [`SystemDescriptor`], carrying the system's `Params`
/// **schema** so the host can encode params from KDL without linking the `Params`
/// type (the one-postcard-encoding decision).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemDescriptorMsg {
    pub name: String,
    pub kind: SystemKind,
    pub inputs: Vec<PortDescMsg>,
    pub outputs: Vec<PortDescMsg>,
    /// `<Params as postcard_schema::Schema>::SCHEMA`, in its owned form.
    pub params_schema: OwnedNamedType,
}

impl PortDescMsg {
    /// Lower one static [`PortDesc`] (its `vtable` already unprefixed) plus the
    /// port's unprefixed `metadata` into the wire mirror.
    fn lower(desc: &PortDesc, metadata: Vec<ComponentMetadata>) -> Self {
        Self {
            frame_id: desc.frame_id,
            frame_name: desc.frame_name.to_string(),
            vtable: desc.vtable.clone(),
            max_size: desc.max_size,
            rate_hint: desc.rate_hint,
            metadata,
        }
    }

    /// Reconstruct a [`PortDesc`], synthesizing the `announce` closure from the
    /// carried `metadata`. The frame name is `Box::leak`ed to recover the
    /// `&'static str` the host wiring path expects — a one-time leak per dlopen'd
    /// port at load.
    pub fn into_port_desc(self) -> PortDesc {
        let frame_name: &'static str = Box::leak(self.frame_name.into_boxed_str());
        // The `announce` factory closes over the carried unprefixed vtable + metadata
        // and re-prefixes them by the instance name. Re-prefixing the **metadata** names
        // is a rehash from the prefixed name; re-prefixing the **vtable**'s baked
        // component ids is the metadata-driven id rewrite of telemetry.md §6 —
        // [`prefix_announce_vtable`]. The result matches a static system's prefixed
        // announce (`announce_of::<F>(prefix)`) bit-for-bit, so telemetry `All` keys a
        // dlopen'd output's components the same way.
        let unprefixed_vtable = self.vtable.clone();
        let metadata = self.metadata.clone();
        let announce: AnnounceFn = Arc::new(move |prefix: &str| {
            let meta = metadata
                .iter()
                .cloned()
                .map(|m| m.with_prefix(prefix))
                .collect();
            let vtable = prefix_announce_vtable(&unprefixed_vtable, &metadata, prefix);
            (vtable, meta)
        });
        PortDesc {
            // The carried (unprefixed) vtable is what `compatible()` validates against,
            // so wiring validation is unchanged; prefixing happens only in `announce`.
            frame_id: self.frame_id,
            frame_name,
            vtable: self.vtable,
            max_size: self.max_size,
            rate_hint: self.rate_hint,
            announce,
        }
    }
}

/// Rewrite a dl port's **unprefixed** vtable into its instance-**prefixed** form for
/// telemetry (telemetry.md §6 — the `into_port_desc` rewrite).
///
/// A static system bakes prefixed component ids via `announce_of::<F>(prefix)`
/// (`AsVTable::vtable_fields(prefix)` rolls each leaf id as
/// `ComponentId::new("<prefix>.<frame>.<field>")`). A dlopen'd system has no static `F`,
/// so it carries the **unprefixed** vtable + per-component `metadata`; this reconstructs
/// the prefixed ids from that metadata.
///
/// Each leaf component id is baked as a standalone 8-byte `Op::Data` blob
/// (`builder::component` → `data(&id)`), so we build an unprefixed→prefixed id map from
/// the metadata (a leaf's unprefixed id is `ComponentId::new(meta.name)`; its prefixed id
/// is `ComponentId::new("<prefix>.<meta.name>")`, exactly `meta.with_prefix(prefix)`'s id)
/// and rewrite every 8-byte `Op::Data` whose value is a known leaf id. The frame-tag id
/// (never prefixed — `with_frame` bakes the bare `FRAME_ID`) and the schema `ty`/`dim`
/// blobs are absent from the map, so they are left untouched; dynamic member templates
/// use `Op::PathComponent` (runtime path composition, no baked id) and are likewise
/// unaffected.
fn prefix_announce_vtable(vtable: &VTable, metadata: &[ComponentMetadata], prefix: &str) -> VTable {
    let mut vt = vtable.clone();
    if prefix.is_empty() {
        // An empty prefix is the unprefixed identity (`PathHasher` skips empty segments);
        // every `announce` caller supplies a real instance name, but stay total.
        return vt;
    }
    // Unprefixed leaf id (`u64`) → prefixed leaf id (`u64`), from the carried metadata.
    let map: HashMap<u64, u64> = metadata
        .iter()
        .map(|m| {
            let unprefixed = ComponentId::new(&m.name).0;
            let prefixed = ComponentId::new(&format!("{prefix}.{}", m.name)).0;
            (unprefixed, prefixed)
        })
        .collect();
    // Collect the (data-offset, prefixed-id) rewrites first (the `ops` borrow and the
    // `data` read overlap on `vt`), then apply them to a fresh data buffer.
    let data = vt.data.as_slice();
    let mut rewrites: Vec<(usize, u64)> = Vec::new();
    for op in vt.ops.iter() {
        if let Op::Data { offset, len } = op
            && *len as usize == core::mem::size_of::<u64>()
            && let Some(slot) = data.get(offset.to_index()..offset.to_index() + 8)
        {
            let val = u64::from_le_bytes(slot.try_into().expect("8-byte slice"));
            if let Some(&prefixed) = map.get(&val) {
                rewrites.push((offset.to_index(), prefixed));
            }
        }
    }
    if !rewrites.is_empty() {
        let mut new_data = data.to_vec();
        for (off, prefixed) in rewrites {
            new_data[off..off + 8].copy_from_slice(&prefixed.to_le_bytes());
        }
        vt.data = new_data;
    }
    vt
}

impl SystemDescriptorMsg {
    /// Lower a static [`SystemDescriptor`] into the wire mirror: drop each port's
    /// `announce` fn-pointer, carrying its unprefixed `vtable` (on
    /// the [`PortDesc`]) and the per-port `metadata` supplied positionally
    /// (`input_metadata`/`output_metadata` parallel to `desc.inputs`/`desc.outputs`).
    pub fn lower(
        desc: &SystemDescriptor,
        params_schema: OwnedNamedType,
        input_metadata: Vec<Vec<ComponentMetadata>>,
        output_metadata: Vec<Vec<ComponentMetadata>>,
    ) -> Self {
        let inputs = desc
            .inputs
            .iter()
            .zip(input_metadata)
            .map(|(p, m)| PortDescMsg::lower(p, m))
            .collect();
        let outputs = desc
            .outputs
            .iter()
            .zip(output_metadata)
            .map(|(p, m)| PortDescMsg::lower(p, m))
            .collect();
        Self {
            name: desc.name.to_string(),
            kind: desc.kind,
            inputs,
            outputs,
            params_schema,
        }
    }

    /// Reconstruct a [`SystemDescriptor`] (host side), rebuilding each [`PortDesc`]
    /// and synthesizing its `announce` from the carried metadata. The system name is
    /// `Box::leak`ed for the `&'static str` the wiring path expects (load-time only).
    pub fn into_descriptor(self) -> SystemDescriptor {
        let name: &'static str = Box::leak(self.name.into_boxed_str());
        SystemDescriptor {
            name,
            kind: self.kind,
            inputs: self.inputs.into_iter().map(PortDescMsg::into_port_desc).collect(),
            outputs: self.outputs.into_iter().map(PortDescMsg::into_port_desc).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// RawBinder — the `.so`-side RingSource over a `&[FswRing]` cursor
// ---------------------------------------------------------------------------

/// The system's [`RingSource`], the twin of the host's `Binder` (binder.rs) over
/// **host-provided raw regions** rather than pre-allocated `BoundPort`s.
/// `next_output`/`next_input` pop the next [`FswRing`] and
/// [`attach_raw`](RingBuffer::attach_raw) it, with the identical positional walk
/// (`descriptors()` order). Cyclic ⇒ every wake endpoint is [`NoWake`], so the
/// generic `WD`/`WS` are default-constructed.
pub struct RawBinder<'a> {
    inputs: slice::Iter<'a, FswRing>,
    outputs: slice::Iter<'a, FswRing>,
}

impl<'a> RawBinder<'a> {
    /// Build a cursor over the host's input/output handle arrays.
    ///
    /// # Safety
    /// Every region named by an `FswRing` here must satisfy
    /// [`RingBuffer::attach_raw`]'s contract — a live, header-valid ring region that
    /// outlives every `Writer`/`View` this binder produces — for the whole lifetime
    /// of the runner the bound bundles feed.
    pub unsafe fn new(inputs: &'a [FswRing], outputs: &'a [FswRing]) -> Self {
        Self {
            inputs: inputs.iter(),
            outputs: outputs.iter(),
        }
    }

    fn attach(handle: &FswRing) -> RingBuffer<RawBacking> {
        // SAFETY: `RawBinder::new`'s caller asserts each region is a live, header-valid
        // ring that outlives the produced handles; `attach_raw` validates the header.
        unsafe { RingBuffer::<RawBacking>::attach_raw(handle.base, handle.len) }
            .expect("host handed a valid ring region (header validated)")
    }
}

impl<'a> RingSource for RawBinder<'a> {
    type B = RawBacking;

    fn next_output<WD, WS>(&mut self) -> (RingBuffer<RawBacking>, WD, WS)
    where
        WD: WakeSource + Default + Clone + 'static,
        WS: WakeSink + Default + Clone + 'static,
    {
        let h = self
            .outputs
            .next()
            .expect("bind() walks output ports in descriptors() order");
        (Self::attach(h), WD::default(), WS::default())
    }

    fn next_input<RD, RS>(&mut self) -> (RingBuffer<RawBacking>, RD, RS)
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static,
    {
        let h = self
            .inputs
            .next()
            .expect("bind() walks input ports in descriptors() order");
        (Self::attach(h), RD::default(), RS::default())
    }

    // `output_registry()` uses the panicking default: a dlopen'd system is never the
    // telemetry downlink, so it has no broad-access registry (binder.rs).
}

// ---------------------------------------------------------------------------
// The opaque state + the generic export helpers
// ---------------------------------------------------------------------------

/// The opaque state the lifecycle threads, boxed by [`run_create`]
/// and dropped by [`run_destroy`]. `pending` holds the constructed system until
/// [`run_bind_init`] binds its bundles and grows the verbatim host
/// [`CyclicRunner`] — type-erased to [`CyclicSlot`] so `run_execute`/`run_shutdown`
/// need not name the output bundle type. `poisoned` latches a caught `execute` panic
/// so subsequent cycles short-circuit to [`FswStatus::Panicked`].
struct AbiState<S> {
    pending: Option<S>,
    runner: Option<Box<dyn CyclicSlot>>,
    poisoned: bool,
}

/// `fsw_create`: postcard-decode `S::Params`, construct the system via
/// [`BuildSystem::new`], and box the (unbound) [`AbiState`].
/// Returns a null pointer if decoding or construction panics — no unwind escapes.
///
/// # Safety
/// `params`/`params_len` name a readable byte range (or `params` is null with
/// `params_len == 0`). The returned pointer is owned by the caller and must be
/// passed only to the other `run_*` helpers for the same `S`, then [`run_destroy`].
pub unsafe fn run_create<S>(params: *const u8, params_len: usize) -> *mut c_void
where
    S: BuildSystem,
    S::Params: for<'de> Deserialize<'de>,
{
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let bytes: &[u8] = if params.is_null() || params_len == 0 {
            &[]
        } else {
            // SAFETY: caller asserts `params..params+params_len` is readable.
            unsafe { slice::from_raw_parts(params, params_len) }
        };
        let params: S::Params = postcard::from_bytes(bytes).expect("params decode (postcard)");
        let system = S::new(params);
        let state = Box::new(AbiState::<S> {
            pending: Some(system),
            runner: None,
            poisoned: false,
        });
        Box::into_raw(state) as *mut c_void
    }));
    outcome.unwrap_or(core::ptr::null_mut())
}

/// `fsw_bind_init`: reconstruct the typed bundles from the [`FswRing`] arrays via a
/// [`RawBinder`] (the positional `descriptors()` walk over `attach_raw`), assemble the
/// verbatim [`CyclicRunner`], and run `System::init` (bind and init are fused). A
/// caught panic leaves `runner` unbound, so [`run_execute`] reports
/// [`FswStatus::Panicked`].
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`. `inputs`/`outputs` name
/// `n_in`/`n_out` valid [`FswRing`] handles whose regions satisfy
/// [`RingBuffer::attach_raw`]'s contract and outlive the runner (until [`run_destroy`]).
pub unsafe fn run_bind_init<S, O>(
    state: *mut c_void,
    inputs: *const FswRing,
    n_in: usize,
    outputs: *const FswRing,
    n_out: usize,
) where
    S: CyclicSystem<RawBacking, Output = Out<O, RawBacking>> + BuildSystem + 'static,
    O: SystemOutput + 'static,
    S::Input: BindPorts<RawBacking>,
    S::Output: BindPorts<RawBacking>,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `AbiState<S>` from `run_create`.
    let st = unsafe { &mut *(state as *mut AbiState<S>) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let in_slice: &[FswRing] = if inputs.is_null() || n_in == 0 {
            &[]
        } else {
            // SAFETY: caller asserts `n_in` valid handles at `inputs`.
            unsafe { slice::from_raw_parts(inputs, n_in) }
        };
        let out_slice: &[FswRing] = if outputs.is_null() || n_out == 0 {
            &[]
        } else {
            // SAFETY: caller asserts `n_out` valid handles at `outputs`.
            unsafe { slice::from_raw_parts(outputs, n_out) }
        };
        // SAFETY: caller asserts each region outlives the runner (until run_destroy).
        let mut binder = unsafe { RawBinder::new(in_slice, out_slice) };
        let input = <S::Input as BindPorts<RawBacking>>::bind(&mut binder);
        let output = <S::Output as BindPorts<RawBacking>>::bind(&mut binder);
        let system = st
            .pending
            .take()
            .expect("fsw_create populated the system before fsw_bind_init");
        let mut runner = CyclicRunner::<S, O, RawBacking>::new(system, input, output);
        runner.init();
        st.runner = Some(Box::new(runner));
    }));
}

/// `fsw_execute`: run one cyclic `step` (the verbatim lapped→hard-stop / timing /
/// health logic) and return the mapped [`FswStatus`]. The `now`
/// word carries the coordinator's raw [`Timestamp`] tick (see the module note on the
/// ABI timestamp). A caught `execute` panic latches `poisoned` and returns
/// [`FswStatus::Panicked`]; an unbound/poisoned state returns it too.
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`.
pub unsafe fn run_execute<S>(state: *mut c_void, now: u64) -> FswStatus
where
    S: BuildSystem,
{
    if state.is_null() {
        return FswStatus::Panicked;
    }
    // SAFETY: caller asserts `state` is a live `AbiState<S>` from `run_create`.
    let st = unsafe { &mut *(state as *mut AbiState<S>) };
    if st.poisoned {
        return FswStatus::Panicked;
    }
    let Some(runner) = st.runner.as_mut() else {
        return FswStatus::Panicked;
    };
    let now = Timestamp(now as i64);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        runner.step(now);
        *runner.state()
    }));
    match outcome {
        Ok(slot) => FswStatus::from_slot(slot),
        Err(_) => {
            st.poisoned = true;
            FswStatus::Panicked
        }
    }
}

/// `fsw_shutdown`: run `System::shutdown` once. A panic is caught and swallowed; a
/// poisoned/unbound state is a no-op.
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`.
pub unsafe fn run_shutdown<S>(state: *mut c_void)
where
    S: BuildSystem,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `AbiState<S>` from `run_create`.
    let st = unsafe { &mut *(state as *mut AbiState<S>) };
    if st.poisoned {
        return;
    }
    if let Some(runner) = st.runner.as_mut() {
        let _ = catch_unwind(AssertUnwindSafe(|| runner.shutdown()));
    }
}

/// `fsw_destroy`: drop the boxed state inside the `.so` (running `S::drop` and every
/// `RawBacking` port's `Drop`). Idempotent on null.
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`, not used afterward.
pub unsafe fn run_destroy<S>(state: *mut c_void)
where
    S: BuildSystem,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `AbiState<S>` from `run_create`,
    // transferred here exactly once.
    let st = unsafe { Box::from_raw(state as *mut AbiState<S>) };
    let _ = catch_unwind(AssertUnwindSafe(|| drop(st)));
}

/// `fsw_describe`: lower this system's static [`SystemDescriptor`] (plus its `Params`
/// schema and each port's unprefixed metadata) to a [`SystemDescriptorMsg`],
/// postcard-encode it, and hand the bytes to the host [`ByteSink`].
/// Per-port metadata is derived by calling each `PortDesc::announce` with the **empty**
/// prefix (`PathHasher` skips an empty segment, so this yields the unprefixed
/// vtable+metadata). Returns `0` on success, `-1` if anything panics.
///
/// # Safety
/// `sink`/`ctx` form a valid host callback (`sink` is called once with a buffer the
/// `.so` owns for the duration of the call).
pub unsafe fn run_describe<S>(sink: ByteSink, ctx: *mut c_void) -> i32
where
    S: CyclicSystem<RawBacking> + BuildSystem,
    S::Params: postcard_schema::Schema,
{
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let desc = <S as CyclicSystem<RawBacking>>::descriptor();
        let input_metadata = desc.inputs.iter().map(|p| (p.announce)("").1).collect();
        let output_metadata = desc.outputs.iter().map(|p| (p.announce)("").1).collect();
        let params_schema = OwnedNamedType::from(<S::Params as postcard_schema::Schema>::SCHEMA);
        let msg = SystemDescriptorMsg::lower(&desc, params_schema, input_metadata, output_metadata);
        postcard::to_allocvec(&msg).expect("descriptor encodes (postcard)")
    }));
    match outcome {
        Ok(bytes) => {
            sink(ctx, bytes.as_ptr(), bytes.len());
            0
        }
        Err(_) => -1,
    }
}

#[cfg(all(test, feature = "kdl"))]
mod tests;
