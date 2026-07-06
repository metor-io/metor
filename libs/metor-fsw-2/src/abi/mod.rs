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
use core::task::{Context, Poll, Waker};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::slice;

use std::sync::Arc;

use metor_fsw_ring::{RingBuffer, WakeSink, WakeSource};
use metor_proto::types::{ComponentId, PacketId, Timestamp};
use metor_proto::vtable::{Op, VTable};
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};

use crate::binder::{BindPorts, RingSource};
use crate::coordinator::{CyclicSlot, SlotState, StopReason};
use crate::descriptor::{
    AnnounceFn, Delivery, FanIn, PortDesc, PortId, PortSchema, SystemDescriptor, SystemKind,
};
use crate::sequence::{SeqBound, SeqClock, SeqSystem, publish_status, with_clock};
use crate::system::{BuildSystem, CyclicRunner, CyclicSystem, Out, SystemOutput};

// ---------------------------------------------------------------------------
// Version + identity
// ---------------------------------------------------------------------------

/// The monotonic ABI word a host checks for equality before any other call.
/// **Bump on any change** to the C surface or the `*Msg` wire structs below; a
/// mismatch fails the load cleanly rather than risking a crash.
///
/// `2` adds the terminal [`FswStatus::Done`] code a sequence occupant returns when
/// its future is `Ready` (the only ABI addition for slots/sequences, §8).
/// `3` (unshipped; one bump per released ABI shape) re-shapes the descriptor wire
/// for the port unification: drops the unused `rate_hint` field, makes
/// [`PortDescMsg`] schema-tagged ([`PortSchemaMsg`]: `Table` | `Postcard` — a `.so`
/// can now declare a message port) and carries the behavior axes
/// (`delivery`/`fan_in`) + the `telemetered` flag, and adds `capabilities` to
/// [`SystemDescriptorMsg`] (rejected non-empty at load in v1 — every capability is
/// host-only).
/// `4` (unshipped) retires the ring's overwrite mode: the `on_lap` axis leaves
/// [`PortDescMsg`] and the `StoppedLapped` status word is gone
/// ([`FswStatus`] renumbers to Running=0 / Panicked=1 / Done=2).
pub const FSW_ABI_VERSION: u32 = 4;

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
    /// Region base — same-process: the host ring's `RingBuffer::region().0`.
    pub base: *mut u8,
    /// Region length — the host ring's `RingBuffer::region().1`.
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
    /// A panic was caught at the boundary, or the state was never bound; the host
    /// telemeters it and hard-stops the slot.
    Panicked = 1,
    /// A sequence occupant's future returned `Ready` — a **terminal, non-error**
    /// stop (§8). The host maps it to the slot-layer terminal state and stops
    /// polling; the `Completed`/`Aborted`/`Failed` detail rides the
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) frame, not this word. A
    /// `CyclicRunner`-driven (`export_system!`) system never returns it, so
    /// [`from_slot`](FswStatus::from_slot) — which maps a [`SlotState`] with no
    /// `Done` — is unchanged; sequences return it directly from `run_seq_execute`.
    Done = 2,
}

impl FswStatus {
    /// Convert an **untrusted raw `u32`** — the bare ABI word a foreign `fsw_execute`
    /// export returned — into an `FswStatus`, defending the dlopen trust boundary.
    ///
    /// `FswStatus` is `repr(u32)`, so building one directly from an arbitrary `u32` (a
    /// bald `transmute`, or a fn-pointer typed to return `FswStatus` and trusted
    /// verbatim) is a validity-invariant violation — instant UB — the moment the `.so`
    /// returns anything outside `0..=3`. The host `run_execute`/`run_seq_execute`
    /// exports genuinely only ever return one of the four declared discriminants (this
    /// is what a *well-behaved* `.so` sends), but a `.so` is foreign code the host
    /// cannot make Rust trust: a stale/mismatched build, a hand-rolled non-Rust
    /// exporter, or memory corruption could hand back any word. Any value outside the
    /// declared range folds to [`Panicked`](FswStatus::Panicked) — the same outcome a
    /// caught panic or an unbound state already produces — rather than being trusted.
    ///
    /// Callers that read the raw `u32` off a resolved dlopen symbol
    /// ([`dl::DlSlot`](crate::dl::DlSlot)) must route it through here instead of
    /// constructing an `FswStatus` directly.
    pub(crate) fn from_raw(raw: u32) -> Self {
        match raw {
            0 => FswStatus::Running,
            1 => FswStatus::Panicked,
            2 => FswStatus::Done,
            _ => FswStatus::Panicked,
        }
    }

    /// Map a runner's [`SlotState`] (after a `step`) to the FFI status.
    fn from_slot(state: SlotState) -> Self {
        match state {
            // A `.so`-side `CyclicRunner` never sets `Panicked` through its `SlotState`
            // (a panic is caught by `catch_unwind` and returned directly as
            // `FswStatus::Panicked`), but the match must stay total.
            SlotState::Stopped {
                reason: StopReason::Panicked,
            } => FswStatus::Panicked,
            // A `.so`-side `CyclicRunner` only ever inhabits Running/Stopped (the
            // runtime-slot states Empty/Loaded/Done are host-side `SlotRunner`
            // territory and never cross the ABI); everything not Stopped keeps running.
            _ => FswStatus::Running,
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

/// The serializable mirror of the [`PortSchema`] axis. A Table port cannot cross by
/// value ([`PortSchema::Table`]'s `announce` is a closure over the frame type `F`,
/// which does not exist on the host side), so the Table arm serializes the
/// **unprefixed** `vtable` (exactly what `compatible()` needs, so wiring validation
/// runs unchanged) plus the **unprefixed** `metadata`, from which the host
/// re-derives the missing `announce` closure (the telemetry.md §6 metadata-driven
/// prefix rewrite). A Postcard port is self-describing — the 2-byte id *is* the
/// schema — so its arm is just the id (the [`NamedMsg`](crate::NamedMsg) token
/// rides [`PortDescMsg::name`], as every port's name does).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PortSchemaMsg {
    /// A component-frame table port.
    Table {
        /// `F::FRAME_ID`.
        frame_id: ComponentId,
        /// `F::as_vtable()` — the unprefixed, frame-relative vtable (wiring
        /// compatibility).
        vtable: VTable,
        /// The unprefixed component metadata, so the host can synthesize a prefixed
        /// `announce` for telemetry without the static `F`.
        metadata: Vec<ComponentMetadata>,
    },
    /// A self-describing `(PacketId, postcard)` message port.
    Postcard {
        /// `M::ID`.
        id: PacketId,
    },
}

/// The serializable mirror of [`PortDesc`]: the port name, worst-case size, the
/// schema axis ([`PortSchemaMsg`]), the behavior axes, and the telemetry flag —
/// a `.so` declares message ports and axis overrides exactly like a static
/// system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortDescMsg {
    /// `F::NAME` / `M::NAME` — was `&'static str`; reconstructed by leaking at load.
    pub name: String,
    /// `F::MAX_SIZE` / `MAX_MSG_BYTES` (worst-case record bytes).
    pub max_size: usize,
    /// Axis 1 — what a record is.
    pub schema: PortSchemaMsg,
    /// Axis 2 — latest-wins snapshot vs every-record log.
    pub delivery: Delivery,
    /// Axis 3 — producer cardinality for an input.
    pub fan_in: FanIn,
    /// Whether the downlink / `AllOutputs` taps this port (A6).
    pub telemetered: bool,
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
    /// The non-port [`Capability`](crate::Capability) set. Every v1 capability is
    /// **host-only** (`ReceiveAll` — the host registry cannot cross the ABI), so
    /// the loader rejects a non-empty list
    /// ([`DlError::UnsupportedCapabilities`](crate::dl::DlError)).
    pub capabilities: Vec<crate::Capability>,
}

impl PortDescMsg {
    /// Lower one static [`PortDesc`] into the wire mirror. A Table port's
    /// unprefixed metadata is re-derived from its own `announce` factory with the
    /// empty prefix (`PathHasher` skips empty segments); a Postcard port carries
    /// only its id.
    fn lower(desc: &PortDesc) -> Self {
        let schema = match &desc.schema {
            PortSchema::Table { vtable, announce } => {
                let (_, metadata) = announce("");
                PortSchemaMsg::Table {
                    frame_id: desc
                        .id
                        .component()
                        .expect("a Table port keys on a frame ComponentId"),
                    vtable: vtable.clone(),
                    metadata,
                }
            }
            PortSchema::Postcard => PortSchemaMsg::Postcard {
                id: desc
                    .id
                    .packet()
                    .expect("a Postcard port keys on a PacketId"),
            },
        };
        Self {
            name: desc.name.to_string(),
            max_size: desc.max_size,
            schema,
            delivery: desc.delivery,
            fan_in: desc.fan_in,
            telemetered: desc.telemetered,
        }
    }

    /// Reconstruct a [`PortDesc`]. The port name is `Box::leak`ed to recover the
    /// `&'static str` the host wiring path expects — a one-time leak per dlopen'd
    /// port at load. A Table port's `announce` closure is synthesized from the
    /// carried `metadata`; a Postcard port needs none (self-describing).
    pub fn into_port_desc(self) -> PortDesc {
        let name: &'static str = Box::leak(self.name.into_boxed_str());
        let (id, schema) = match self.schema {
            PortSchemaMsg::Table {
                frame_id,
                vtable,
                metadata,
            } => {
                // The `announce` factory closes over the carried unprefixed vtable +
                // metadata and re-prefixes them by the instance name. Re-prefixing the
                // **metadata** names is a rehash from the prefixed name; re-prefixing
                // the **vtable**'s baked component ids is the metadata-driven id
                // rewrite of telemetry.md §6 — [`prefix_announce_vtable`]. The result
                // matches a static system's prefixed announce
                // (`announce_of::<F>(prefix)`) bit-for-bit, so telemetry `All` keys a
                // dlopen'd output's components the same way.
                let unprefixed_vtable = vtable.clone();
                let announce: AnnounceFn = Arc::new(move |prefix: &str| {
                    let meta = metadata
                        .iter()
                        .cloned()
                        .map(|m| m.with_prefix(prefix))
                        .collect();
                    let vt = prefix_announce_vtable(&unprefixed_vtable, &metadata, prefix);
                    (vt, meta)
                });
                (
                    PortId::Component(frame_id),
                    // The carried (unprefixed) vtable is what `compatible()` validates
                    // against, so wiring validation is unchanged; prefixing happens
                    // only in `announce`.
                    PortSchema::Table { vtable, announce },
                )
            }
            PortSchemaMsg::Postcard { id } => (PortId::Packet(id), PortSchema::Postcard),
        };
        PortDesc {
            id,
            name,
            max_size: self.max_size,
            schema,
            delivery: self.delivery,
            fan_in: self.fan_in,
            telemetered: self.telemetered,
            // Not carried across the ABI: a dlopen'd system's ports are always
            // edge-connected (Host/SelfTap are host-runner constructs; the slot
            // derivation marks its occupant's SlotControlIn Host on the host side).
            conn: crate::descriptor::PortConn::Edge,
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
    /// Lower a static [`SystemDescriptor`] into the wire mirror: drop each Table
    /// port's `announce` closure, carrying its unprefixed `vtable` + `metadata`
    /// (re-derived per port inside [`PortDescMsg::lower`]) so the host can
    /// synthesize the announce at load; Postcard ports lower to their bare id.
    pub fn lower(desc: &SystemDescriptor, params_schema: OwnedNamedType) -> Self {
        Self {
            name: desc.name.to_string(),
            kind: desc.kind,
            inputs: desc.inputs.iter().map(PortDescMsg::lower).collect(),
            outputs: desc.outputs.iter().map(PortDescMsg::lower).collect(),
            params_schema,
            capabilities: desc.capabilities.clone(),
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
            // Carried verbatim; the loader has already rejected a non-empty list
            // (every v1 capability is host-only).
            capabilities: self.capabilities,
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

    fn attach(handle: &FswRing) -> RingBuffer {
        // SAFETY: `RawBinder::new`'s caller asserts each region is a live, header-valid
        // ring that outlives the produced handles; `attach_raw` validates the header.
        unsafe { RingBuffer::attach_raw(handle.base, handle.len) }
            .expect("host handed a valid ring region (header validated)")
    }
}

impl<'a> RingSource for RawBinder<'a> {
    fn next_output<WD, WS>(&mut self) -> (RingBuffer, WD, WS)
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

    fn next_input<RD, RS>(&mut self) -> (RingBuffer, RD, RS)
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

/// View a nullable `(ptr, len)` byte range as a slice (`&[]` for null/0) — the
/// params decode shared by [`run_create`]/[`run_seq_create`] (C4).
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

/// View a nullable `(ptr, n)` [`FswRing`] array as a slice (`&[]` for null/0) — the
/// bind-time handle walk shared by [`run_bind_init`]/[`run_seq_bind_init`] (C4).
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

/// The describe tail shared by [`run_describe`]/[`run_seq_describe`] (C4): lower the
/// descriptor + `Params` schema to a [`SystemDescriptorMsg`], postcard-encode it
/// under `catch_unwind`, and hand the bytes to the host [`ByteSink`]. Returns `0`
/// on success, `-1` if anything panics.
fn describe_common(
    desc: impl FnOnce() -> SystemDescriptor + std::panic::UnwindSafe,
    params_schema: OwnedNamedType,
    sink: ByteSink,
    ctx: *mut c_void,
) -> i32 {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let msg = SystemDescriptorMsg::lower(&desc(), params_schema);
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
    // SAFETY: caller asserts `params..params+params_len` is readable (or null/0).
    let bytes = unsafe { bytes_from_raw(params, params_len) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
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
    S: CyclicSystem<Output = Out<O>> + BuildSystem + 'static,
    // `Out<O>: BindPorts` needs `O: BindPorts` spelled out: the `Output = Out<O>`
    // equality makes the compiler prove the blanket impl rather than elaborate
    // the `System::Output: BindPorts` associated-type bound.
    O: SystemOutput + BindPorts + 'static,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `AbiState<S>` from `run_create`.
    let st = unsafe { &mut *(state as *mut AbiState<S>) };
    // SAFETY: caller asserts the handle arrays are valid (or null/0).
    let (in_slice, out_slice) =
        unsafe { (rings_from_raw(inputs, n_in), rings_from_raw(outputs, n_out)) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller asserts each region outlives the runner (until run_destroy).
        let mut binder = unsafe { RawBinder::new(in_slice, out_slice) };
        let input = <S::Input as BindPorts>::bind(&mut binder);
        let output = <S::Output as BindPorts>::bind(&mut binder);
        let system = st
            .pending
            .take()
            .expect("fsw_create populated the system before fsw_bind_init");
        let mut runner = CyclicRunner::new(system, input, output);
        runner.init();
        st.runner = Some(Box::new(runner));
    }));
}

/// `fsw_execute`: run one cyclic `step` (the verbatim timing / health logic) and
/// return the mapped [`FswStatus`]. The `now`
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
/// non-owning port's `Drop`). Idempotent on null.
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
/// schema) to a [`SystemDescriptorMsg`], postcard-encode it, and hand the bytes to
/// the host [`ByteSink`]. A Table port's unprefixed metadata is derived inside the
/// lowering by calling its `announce` with the **empty** prefix (`PathHasher` skips
/// an empty segment); Postcard ports carry only their id. Returns `0` on success,
/// `-1` if anything panics.
///
/// # Safety
/// `sink`/`ctx` form a valid host callback (`sink` is called once with a buffer the
/// `.so` owns for the duration of the call).
pub unsafe fn run_describe<S>(sink: ByteSink, ctx: *mut c_void) -> i32
where
    S: CyclicSystem + BuildSystem,
    S::Params: postcard_schema::Schema,
{
    let params_schema = OwnedNamedType::from(<S::Params as postcard_schema::Schema>::SCHEMA);
    describe_common(
        <S as CyclicSystem>::descriptor,
        params_schema,
        sink,
        ctx,
    )
}

// ---------------------------------------------------------------------------
// Sequence occupants — the `run_seq_*` twins of the `run_*` helpers
// ---------------------------------------------------------------------------

/// The opaque state a sequence occupant's lifecycle threads — the twin of
/// [`AbiState`] for a future-driven occupant (§4.2). `params` holds the decoded
/// params until [`run_seq_bind_init`] consumes them; `bound` holds the owned future
/// (with the user ports moved inside it) + the wrapper-owned `Out` tail + the cancel
/// input, built at bind; `clock` is the per-cycle ambient ([`SeqClock`]); `poisoned`
/// latches a caught `execute` panic so later cycles short-circuit to
/// [`FswStatus::Panicked`].
struct SeqState<S: SeqSystem> {
    params: Option<S::Params>,
    bound: Option<SeqBound>,
    clock: Rc<SeqClock>,
    poisoned: bool,
}

/// `fsw_create` (sequence): postcard-decode `S::Params` and box an **unbound**
/// [`SeqState`] (a fresh [`SeqClock`], no future yet). Mirrors [`run_create`],
/// including the empty/null-params path. Returns null on panic.
///
/// # Safety
/// As [`run_create`]: `params`/`params_len` name a readable byte range (or null/0);
/// the returned pointer is owned by the caller and passed only to the other
/// `run_seq_*` helpers for the same `S`, then [`run_seq_destroy`].
pub unsafe fn run_seq_create<S>(params: *const u8, params_len: usize) -> *mut c_void
where
    S: SeqSystem,
    S::Params: for<'de> Deserialize<'de>,
{
    // SAFETY: caller asserts `params..params+params_len` is readable (or null/0).
    let bytes = unsafe { bytes_from_raw(params, params_len) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let params: S::Params = postcard::from_bytes(bytes).expect("params decode (postcard)");
        let state = Box::new(SeqState::<S> {
            params: Some(params),
            bound: None,
            clock: Rc::new(SeqClock::default()),
            poisoned: false,
        });
        Box::into_raw(state) as *mut c_void
    }));
    outcome.unwrap_or(core::ptr::null_mut())
}

/// `fsw_bind_init` (sequence): build a [`RawBinder`] over the host's [`FswRing`]
/// arrays and hand it to `S::build`, which binds the ports in `descriptor()` order —
/// the user ports + [`SlotControlIn`](crate::sequence::SlotControlIn) **move into the
/// future**, the [`SequenceStatus`](crate::sequence::SequenceStatus) + health/log tail
/// stays in the state. A caught panic leaves `bound = None`, so [`run_seq_execute`]
/// reports [`FswStatus::Panicked`].
///
/// # Safety
/// As [`run_bind_init`]: `state` is a live [`SeqState`] from [`run_seq_create`];
/// `inputs`/`outputs` name `n_in`/`n_out` valid [`FswRing`] handles whose regions
/// satisfy [`RingBuffer::attach_raw`]'s contract and outlive the future (until
/// [`run_seq_destroy`]).
pub unsafe fn run_seq_bind_init<S>(
    state: *mut c_void,
    inputs: *const FswRing,
    n_in: usize,
    outputs: *const FswRing,
    n_out: usize,
) where
    S: SeqSystem,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `SeqState<S>` from `run_seq_create`.
    let st = unsafe { &mut *(state as *mut SeqState<S>) };
    // SAFETY: caller asserts the handle arrays are valid (or null/0).
    let (in_slice, out_slice) =
        unsafe { (rings_from_raw(inputs, n_in), rings_from_raw(outputs, n_out)) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller asserts each region outlives the future (until run_seq_destroy).
        let mut binder = unsafe { RawBinder::new(in_slice, out_slice) };
        let params = st
            .params
            .take()
            .expect("run_seq_create populated params before run_seq_bind_init");
        st.bound = Some(S::build(params, &mut binder, &st.clock));
    }));
}

/// `fsw_execute` (sequence): refresh `clock.now`, fold the latched cancel from the
/// control input, poll the future **once** with `Waker::noop()` under the task-local
/// clock, publish a [`SequenceStatus`](crate::sequence::SequenceStatus) record + drive
/// `Out::health().end_cycle` on the wrapper tail, and map `Ready(outcome)` →
/// [`FswStatus::Done`] / `Pending` → [`FswStatus::Running`]. A caught panic latches
/// `poisoned` and returns [`FswStatus::Panicked`] (as does an unbound/poisoned state).
///
/// # Safety
/// `state` is a live pointer from [`run_seq_create`] for this `S`.
pub unsafe fn run_seq_execute<S>(state: *mut c_void, now: u64) -> FswStatus
where
    S: SeqSystem,
{
    if state.is_null() {
        return FswStatus::Panicked;
    }
    // SAFETY: caller asserts `state` is a live `SeqState<S>` from `run_seq_create`.
    let st = unsafe { &mut *(state as *mut SeqState<S>) };
    if st.poisoned {
        return FswStatus::Panicked;
    }
    let Some(bound) = st.bound.as_mut() else {
        return FswStatus::Panicked;
    };
    let now_ts = Timestamp(now as i64);
    st.clock.now.set(now_ts);
    // Fold the cancel control frame (latched once seen, §4.4).
    if let Some(f) = bound.control.latest()
        && f.get().cancel != 0
    {
        st.clock.cancel.set(true);
    }
    let start = std::time::Instant::now();
    let clock = &st.clock;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        with_clock(clock, || {
            bound
                .future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        })
    }));
    let micros = start.elapsed().as_micros() as u64;
    match outcome {
        Ok(Poll::Ready(outcome)) => {
            let lines = st.clock.drain_progress();
            publish_status(&mut bound.status, now_ts, outcome.run_state(), &lines);
            bound.status.health().end_cycle(now_ts, micros);
            FswStatus::Done
        }
        Ok(Poll::Pending) => {
            let lines = st.clock.drain_progress();
            publish_status(&mut bound.status, now_ts, 0, &lines);
            bound.status.health().end_cycle(now_ts, micros);
            FswStatus::Running
        }
        Err(_) => {
            st.poisoned = true;
            FswStatus::Panicked
        }
    }
}

/// `fsw_shutdown` (sequence): a no-op — v1 sequences have no shutdown hook (the
/// future is dropped at [`run_seq_destroy`]). Provided so the symbol surface stays
/// uniform with `export_system!`. Safe on null/poisoned.
///
/// # Safety
/// `state` is a live pointer from [`run_seq_create`] for this `S` (or null).
pub unsafe fn run_seq_shutdown<S>(state: *mut c_void)
where
    S: SeqSystem,
{
    let _ = state;
}

/// `fsw_destroy` (sequence): drop the boxed [`SeqState`] inside the `.so` — dropping
/// the future drops its owned non-owning ports (releasing their ring roles), and
/// the wrapper tail/control drop too. Idempotent on null.
///
/// # Safety
/// `state` is a live pointer from [`run_seq_create`] for this `S`, transferred here
/// exactly once and not used afterward.
pub unsafe fn run_seq_destroy<S>(state: *mut c_void)
where
    S: SeqSystem,
{
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `SeqState<S>` from `run_seq_create`,
    // transferred here exactly once.
    let st = unsafe { Box::from_raw(state as *mut SeqState<S>) };
    let _ = catch_unwind(AssertUnwindSafe(|| drop(st)));
}

/// `fsw_describe` (sequence): lower the macro's signature-derived `S::descriptor()`
/// (plus its `Params` schema) to a [`SystemDescriptorMsg`], postcard-encode it, and
/// hand the bytes to the host [`ByteSink`]. Mirrors [`run_describe`], but over
/// `S::descriptor()` (the sequence's own descriptor) rather than
/// `CyclicSystem::descriptor`.
///
/// # Safety
/// As [`run_describe`]: `sink`/`ctx` form a valid host callback (`sink` is called once
/// with a buffer the `.so` owns for the duration of the call).
pub unsafe fn run_seq_describe<S>(sink: ByteSink, ctx: *mut c_void) -> i32
where
    S: SeqSystem,
    S::Params: postcard_schema::Schema,
{
    let params_schema = OwnedNamedType::from(<S::Params as postcard_schema::Schema>::SCHEMA);
    describe_common(S::descriptor, params_schema, sink, ctx)
}

#[cfg(all(test, feature = "kdl"))]
mod tests;
