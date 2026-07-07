//! The C ABI a system `cdylib` exports and the host resolves at load.
//!
//! A system `.so` exports a small, versioned `extern "C"` surface. Both halves
//! compile against this module, which defines the `repr(C)` handles ([`FswRing`],
//! [`FswStatus`]), the serialized descriptor mirrors ([`PortDescMsg`],
//! [`SystemDescriptorMsg`]), the symbol names the host resolves, and the generic
//! `run_*` helpers that [`export_system!`](crate::export_system) delegates to so
//! the generated exports stay one-liners.
//!
//! The lifecycle, in call order:
//!
//! ```text
//! host                              system .so
//! ----                              ----------
//! fsw_abi_version      ---------->  FSW_ABI_VERSION (checked for equality first)
//! fsw_describe(sink)   ---------->  run_describe    (descriptor bytes via ByteSink)
//! fsw_create(params)   ---------->  run_create      (opaque state pointer)
//! fsw_bind_init(rings) ---------->  run_bind_init   (attach rings, System::init)
//! fsw_execute(now)     --(loop)-->  run_execute     (one step, returns FswStatus)
//! fsw_shutdown         ---------->  run_shutdown
//! fsw_destroy          ---------->  run_destroy     (drop the state in the .so)
//! ```
//!
//! Three rules make this sound across an otherwise unstable Rust ABI:
//!
//! - **Only serialized bytes and `repr(C)` handles cross the boundary.** The
//!   descriptor and the `Params` blob are postcard bytes; everything else is a
//!   `(pointer, length)` pair or a plain integer. No `Vec`, `Arc`, or vtable ever
//!   crosses by value.
//! - **No unwind crosses `extern "C"`.** Every `run_*` helper wraps its body in
//!   [`catch_unwind`] and converts a caught panic into a null pointer, a non-zero
//!   `describe` code, or [`FswStatus::Panicked`]. An escaping unwind would be
//!   undefined behavior.
//! - **Each side frees only what it allocated.** The state box is created by
//!   [`run_create`] and dropped by [`run_destroy`] inside the same `.so`, and
//!   [`run_describe`] hands its bytes to a host-owned [`ByteSink`] so the host
//!   copies rather than frees.
//!
//! Ports bind positionally. The host sends [`FswRing`] handles in the order the
//! descriptor lists the ports, and [`RawBinder`] walks them in the same order on
//! the `.so` side. Loaded systems run in-process on the cyclic schedule, so every
//! wake endpoint is `NoWake`.

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

/// The ABI word a host checks for equality before any other call.
///
/// Bump this on any change to the C surface or to the `*Msg` wire structs below,
/// once per released ABI shape. A mismatch fails the load cleanly instead of
/// risking a crash on a stale binary.
pub const FSW_ABI_VERSION: u32 = 4;

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
    /// A sequence occupant's future returned `Ready`, a terminal, non-error
    /// stop. The `Completed`/`Aborted`/`Failed` detail rides the
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) frame, not this word.
    /// Only [`run_seq_execute`] returns it; a [`CyclicRunner`]-driven system
    /// never does.
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
    pub(crate) fn from_raw(raw: u32) -> Self {
        match raw {
            0 => FswStatus::Running,
            1 => FswStatus::Panicked,
            2 => FswStatus::Done,
            _ => FswStatus::Panicked,
        }
    }

    /// Map a runner's [`SlotState`] after a step to the FFI status.
    fn from_slot(state: SlotState) -> Self {
        match state {
            // A `.so`-side runner reports a panic through `catch_unwind`, not
            // through its `SlotState`, but the match stays total.
            SlotState::Stopped {
                reason: StopReason::Panicked,
            } => FswStatus::Panicked,
            // Only Running/Stopped occur on this side of the boundary; the other
            // slot states are host-side bookkeeping and never cross the ABI.
            _ => FswStatus::Running,
        }
    }
}

/// A byte sink is the host callback a describe export feeds its serialized
/// descriptor through. The system keeps ownership of its buffer and the host
/// copies out of it, so no allocation crosses the boundary.
pub type ByteSink = extern "C" fn(ctx: *mut c_void, buf: *const u8, len: usize);

// ---------------------------------------------------------------------------
// Symbol-name constants
// ---------------------------------------------------------------------------

/// `fsw_abi_version` returns the ABI word ([`FSW_ABI_VERSION`]).
pub const SYM_ABI_VERSION: &[u8] = b"fsw_abi_version\0";
/// `fsw_describe` sends the serialized [`SystemDescriptorMsg`] via a [`ByteSink`].
pub const SYM_DESCRIBE: &[u8] = b"fsw_describe\0";
/// `fsw_create` decodes `Params`, constructs the system, and boxes the state.
pub const SYM_CREATE: &[u8] = b"fsw_create\0";
/// `fsw_bind_init` reconstructs the typed bundles and runs `System::init`.
pub const SYM_BIND_INIT: &[u8] = b"fsw_bind_init\0";
/// `fsw_execute` runs one cyclic step and returns an [`FswStatus`].
pub const SYM_EXECUTE: &[u8] = b"fsw_execute\0";
/// `fsw_shutdown` runs `System::shutdown`.
pub const SYM_SHUTDOWN: &[u8] = b"fsw_shutdown\0";
/// `fsw_destroy` drops the boxed state inside the `.so`.
pub const SYM_DESTROY: &[u8] = b"fsw_destroy\0";

// ---------------------------------------------------------------------------
// Serialized descriptor mirrors (postcard)
// ---------------------------------------------------------------------------

/// A schema message describes a port's record type in a form that crosses the
/// boundary as postcard bytes, the wire twin of [`PortSchema`].
///
/// A Table port cannot cross by value because its `announce` is a closure over
/// the static frame type, which does not exist on the host side. Its arm
/// therefore carries the unprefixed `vtable` (exactly what wiring compatibility
/// checks need) plus the unprefixed `metadata`, from which the host re-derives
/// the announce closure at load. A Postcard port is self-describing, so its arm
/// is just the packet id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PortSchemaMsg {
    /// A component-frame table port.
    Table {
        /// `F::FRAME_ID`.
        frame_id: ComponentId,
        /// `F::as_vtable()`, the unprefixed frame-relative vtable used for
        /// wiring compatibility.
        vtable: VTable,
        /// The unprefixed component metadata, from which the host synthesizes a
        /// prefixed `announce` without the static frame type.
        metadata: Vec<ComponentMetadata>,
    },
    /// A self-describing postcard message port.
    Postcard {
        /// `M::ID`.
        id: PacketId,
    },
}

/// A port message carries one port's declaration, its name, size, schema, and
/// delivery axes, across the boundary as postcard bytes, the wire twin of
/// [`PortDesc`]. A `.so` declares message ports and axis overrides exactly
/// like a static system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortDescMsg {
    /// The port name; the `&'static str` is recovered by leaking at load.
    pub name: String,
    /// Worst-case record size in bytes.
    pub max_size: usize,
    /// What a record is.
    pub schema: PortSchemaMsg,
    /// Latest-wins snapshot versus every-record log.
    pub delivery: Delivery,
    /// Producer cardinality for an input.
    pub fan_in: FanIn,
    /// Whether the telemetry downlink taps this port.
    pub telemetered: bool,
}

/// A descriptor message ships a system's whole self-description, name, kind,
/// ports, and params schema, to the host as postcard bytes, the wire twin of
/// [`SystemDescriptor`]. It carries the `Params` schema rather than the
/// `Params` type, so the host can encode params from configuration without
/// linking against the system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemDescriptorMsg {
    pub name: String,
    pub kind: SystemKind,
    pub inputs: Vec<PortDescMsg>,
    pub outputs: Vec<PortDescMsg>,
    /// `<Params as postcard_schema::Schema>::SCHEMA`, in its owned form.
    pub params_schema: OwnedNamedType,
    /// The non-port [`Capability`](crate::Capability) set. Every capability is
    /// currently host-only, so the loader rejects a non-empty list
    /// ([`DlError::UnsupportedCapabilities`](crate::dl::DlError)).
    pub capabilities: Vec<crate::Capability>,
}

impl PortDescMsg {
    /// Lower one static [`PortDesc`] into the wire mirror.
    ///
    /// A Table port's unprefixed metadata is re-derived from its own `announce`
    /// factory with the empty prefix (`PathHasher` skips empty segments); a
    /// Postcard port carries only its id.
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

    /// Reconstruct a [`PortDesc`] on the host side.
    ///
    /// The port name is `Box::leak`ed to recover the `&'static str` the wiring
    /// path expects, a one-time leak per loaded port. A Table port's `announce`
    /// closure is synthesized from the carried metadata; a Postcard port needs
    /// none.
    pub fn into_port_desc(self) -> PortDesc {
        let name: &'static str = Box::leak(self.name.into_boxed_str());
        let (id, schema) = match self.schema {
            PortSchemaMsg::Table {
                frame_id,
                vtable,
                metadata,
            } => {
                // The synthesized `announce` closes over the carried unprefixed
                // vtable and metadata and re-prefixes both by the instance name.
                // The metadata names are rehashed with the prefix; the vtable's
                // baked component ids are rewritten by `prefix_announce_vtable`.
                // The result matches what a static system's announce produces
                // bit for bit, so telemetry keys a loaded output's components
                // the same way.
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
                    // Wiring compatibility validates against the carried
                    // unprefixed vtable; prefixing happens only in `announce`.
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
            // Not carried across the ABI: a loaded system's ports are always
            // edge-connected. The other connection kinds are host-runner
            // constructs applied on the host side.
            conn: crate::descriptor::PortConn::Edge,
        }
    }
}

/// Rewrite a loaded port's unprefixed vtable into its instance-prefixed form
/// for telemetry announcement.
///
/// A static system bakes prefixed component ids into its announce vtable, with
/// each leaf id hashed from `"<prefix>.<frame>.<field>"`. A loaded system has
/// no static frame type, so it carries the unprefixed vtable plus per-component
/// metadata, and the prefixed ids are reconstructed here from that metadata.
///
/// Each leaf component id is baked as a standalone 8-byte `Op::Data` blob, so
/// this builds an unprefixed-to-prefixed id map from the metadata (a leaf's
/// unprefixed id is `ComponentId::new(meta.name)`, and its prefixed id hashes
/// `"<prefix>.<meta.name>"`, which is exactly what `with_prefix` produces) and
/// rewrites every 8-byte `Op::Data` whose value is a known leaf id. The frame
/// tag id is never prefixed and the schema type/dim blobs are absent from the
/// map, so both are left untouched; dynamic member templates compose their
/// paths at runtime through `Op::PathComponent` and carry no baked id at all.
fn prefix_announce_vtable(vtable: &VTable, metadata: &[ComponentMetadata], prefix: &str) -> VTable {
    let mut vt = vtable.clone();
    if prefix.is_empty() {
        // An empty prefix is the unprefixed identity, since `PathHasher` skips
        // empty segments. Every announce caller supplies a real instance name,
        // but stay total.
        return vt;
    }
    // Unprefixed leaf id to prefixed leaf id, from the carried metadata.
    let map: HashMap<u64, u64> = metadata
        .iter()
        .map(|m| {
            let unprefixed = ComponentId::new(&m.name).0;
            let prefixed = ComponentId::new(&format!("{prefix}.{}", m.name)).0;
            (unprefixed, prefixed)
        })
        .collect();
    // Collect the rewrites first, since the `ops` borrow and the `data` read
    // overlap on `vt`, then apply them to a fresh data buffer.
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
    /// Lower a static [`SystemDescriptor`] into the wire mirror.
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

    /// Reconstruct a [`SystemDescriptor`] on the host side, rebuilding each
    /// port and leaking the system name to the `&'static str` the wiring path
    /// expects.
    pub fn into_descriptor(self) -> SystemDescriptor {
        let name: &'static str = Box::leak(self.name.into_boxed_str());
        SystemDescriptor {
            name,
            kind: self.kind,
            inputs: self
                .inputs
                .into_iter()
                .map(PortDescMsg::into_port_desc)
                .collect(),
            outputs: self
                .outputs
                .into_iter()
                .map(PortDescMsg::into_port_desc)
                .collect(),
            // Carried verbatim; the loader has already rejected a non-empty list.
            capabilities: self.capabilities,
        }
    }
}

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
}

impl<'a> RawBinder<'a> {
    /// Build a cursor over the host's input and output handle arrays.
    ///
    /// # Safety
    /// Every region named by an `FswRing` here must satisfy
    /// [`RingBuffer::attach_raw`]'s contract, a live header-valid ring region
    /// that outlives every `Writer` and `View` this binder produces.
    pub unsafe fn new(inputs: &'a [FswRing], outputs: &'a [FswRing]) -> Self {
        Self {
            inputs: inputs.iter(),
            outputs: outputs.iter(),
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

    // `output_registry()` keeps the panicking default: a loaded system is never
    // the telemetry downlink, so it has no broad-access registry.
}

// ---------------------------------------------------------------------------
// Opaque state + generic export helpers
// ---------------------------------------------------------------------------

/// The heap allocation behind the opaque state pointer, holding a cyclic
/// system between export calls. [`run_create`] boxes it and [`run_destroy`]
/// drops it.
///
/// `pending` holds the constructed system until [`run_bind_init`] binds its
/// bundles and builds the runner, type-erased to [`CyclicSlot`] so `run_execute`
/// and `run_shutdown` need not name the output bundle type. `poisoned` latches a
/// caught execute panic so later cycles short-circuit to
/// [`FswStatus::Panicked`].
struct AbiState<S> {
    pending: Option<S>,
    runner: Option<Box<dyn CyclicSlot>>,
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

/// The describe tail shared by [`run_describe`] and [`run_seq_describe`].
/// Lowers the descriptor and `Params` schema to a [`SystemDescriptorMsg`],
/// postcard-encodes it under [`catch_unwind`], and hands the bytes to the host
/// [`ByteSink`]. Returns `0` on success, `-1` if anything panics.
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
/// [`BuildSystem::new`], and box the unbound [`AbiState`]. Returns null if
/// decoding or construction panics.
///
/// # Safety
/// `params`/`params_len` name a readable byte range (or `params` is null with
/// `params_len == 0`). The returned pointer is owned by the caller and must be
/// passed only to the other `run_*` helpers for the same `S`, then
/// [`run_destroy`].
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

/// `fsw_bind_init`: reconstruct the typed bundles from the [`FswRing`] arrays
/// via a [`RawBinder`], assemble the [`CyclicRunner`], and run `System::init`.
/// Bind and init are fused; a caught panic leaves the runner unbound, so
/// [`run_execute`] reports [`FswStatus::Panicked`].
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`.
/// `inputs`/`outputs` name `n_in`/`n_out` valid [`FswRing`] handles whose
/// regions satisfy [`RingBuffer::attach_raw`]'s contract and outlive the runner
/// (until [`run_destroy`]).
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

/// `fsw_execute`: run one cyclic step and return the mapped [`FswStatus`].
///
/// The `now` word carries the coordinator's [`Timestamp`] tick as a raw `u64`.
/// A caught panic latches the poison flag and returns [`FswStatus::Panicked`],
/// as does an unbound or already-poisoned state.
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

/// `fsw_shutdown`: run `System::shutdown` once. A panic is caught and
/// swallowed; a poisoned or unbound state is a no-op.
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

/// `fsw_destroy`: drop the boxed state inside the `.so`, running `S::drop` and
/// every port's `Drop`. Idempotent on null.
///
/// # Safety
/// `state` is a live pointer from [`run_create`] for this `S`, not used
/// afterward.
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

/// `fsw_describe`: lower this system's static [`SystemDescriptor`] and `Params`
/// schema to a [`SystemDescriptorMsg`], postcard-encode it, and hand the bytes
/// to the host [`ByteSink`]. Returns `0` on success, `-1` if anything panics.
///
/// # Safety
/// `sink`/`ctx` form a valid host callback; `sink` is called once with a buffer
/// the `.so` owns for the duration of the call.
pub unsafe fn run_describe<S>(sink: ByteSink, ctx: *mut c_void) -> i32
where
    S: CyclicSystem + BuildSystem,
    S::Params: postcard_schema::Schema,
{
    let params_schema = OwnedNamedType::from(<S::Params as postcard_schema::Schema>::SCHEMA);
    describe_common(<S as CyclicSystem>::descriptor, params_schema, sink, ctx)
}

// ---------------------------------------------------------------------------
// Sequence occupants
// ---------------------------------------------------------------------------

/// The heap allocation behind a sequence occupant's opaque state pointer, the
/// future-driven twin of [`AbiState`].
///
/// `params` holds the decoded params until [`run_seq_bind_init`] consumes them.
/// `bound` holds the owned future (with the user ports moved inside it) plus
/// the wrapper-owned output tail and the cancel input, built at bind. `clock`
/// is the per-cycle ambient [`SeqClock`], and `poisoned` latches a caught
/// execute panic so later cycles short-circuit to [`FswStatus::Panicked`].
struct SeqState<S: SeqSystem> {
    params: Option<S::Params>,
    bound: Option<SeqBound>,
    clock: Rc<SeqClock>,
    poisoned: bool,
}

/// `fsw_create` for a sequence: postcard-decode `S::Params` and box an unbound
/// [`SeqState`] with a fresh [`SeqClock`] and no future yet. Returns null on
/// panic.
///
/// # Safety
/// As [`run_create`]: `params`/`params_len` name a readable byte range (or
/// null/0); the returned pointer is owned by the caller and passed only to the
/// other `run_seq_*` helpers for the same `S`, then [`run_seq_destroy`].
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

/// `fsw_bind_init` for a sequence: build a [`RawBinder`] over the host's
/// [`FswRing`] arrays and hand it to `S::build`, which binds the ports in
/// descriptor order. The user ports and the
/// [`SlotControlIn`](crate::sequence::SlotControlIn) move into the future; the
/// [`SequenceStatus`](crate::sequence::SequenceStatus) and health tail stay in
/// the state. A caught panic leaves `bound` empty, so [`run_seq_execute`]
/// reports [`FswStatus::Panicked`].
///
/// # Safety
/// As [`run_bind_init`]: `state` is a live [`SeqState`] from
/// [`run_seq_create`]; `inputs`/`outputs` name `n_in`/`n_out` valid [`FswRing`]
/// handles whose regions satisfy [`RingBuffer::attach_raw`]'s contract and
/// outlive the future (until [`run_seq_destroy`]).
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

/// `fsw_execute` for a sequence: refresh the clock, fold the latched cancel
/// from the control input, poll the future once with `Waker::noop()` under the
/// task-local clock, publish a
/// [`SequenceStatus`](crate::sequence::SequenceStatus) record, and map
/// `Ready` to [`FswStatus::Done`] and `Pending` to [`FswStatus::Running`]. A
/// caught panic latches the poison flag and returns [`FswStatus::Panicked`],
/// as does an unbound or already-poisoned state.
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
    // A cancel stays latched once seen, even if later control frames clear it.
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

/// `fsw_shutdown` for a sequence: a no-op, since a sequence has no shutdown
/// hook and its future is dropped at [`run_seq_destroy`]. Provided so the
/// symbol surface stays uniform with [`export_system!`](crate::export_system).
///
/// # Safety
/// `state` is a live pointer from [`run_seq_create`] for this `S` (or null).
pub unsafe fn run_seq_shutdown<S>(state: *mut c_void)
where
    S: SeqSystem,
{
    let _ = state;
}

/// `fsw_destroy` for a sequence: drop the boxed [`SeqState`] inside the `.so`.
/// Dropping the future drops the ports it owns, releasing their ring roles, and
/// the wrapper tail and control input drop with the state. Idempotent on null.
///
/// # Safety
/// `state` is a live pointer from [`run_seq_create`] for this `S`, transferred
/// here exactly once and not used afterward.
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

/// `fsw_describe` for a sequence: as [`run_describe`], but over the sequence's
/// own `S::descriptor()`.
///
/// # Safety
/// As [`run_describe`]: `sink`/`ctx` form a valid host callback; `sink` is
/// called once with a buffer the `.so` owns for the duration of the call.
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
