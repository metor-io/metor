//! The C ABI a pack `cdylib` exports and the host resolves at load.
//!
//! A pack `.so` exports a small, versioned `extern "C"` surface. Both halves
//! compile against this module, which defines the `repr(C)` handles ([`FswRing`],
//! [`FswStatus`]), the serialized manifest mirrors ([`PackManifestMsg`],
//! [`SystemDescriptorMsg`]), the symbol names the host resolves, and the
//! `run_pack_*` helpers that [`export_pack!`](crate::export_pack) delegates to
//! so the generated exports stay one-liners.
//!
//! The lifecycle, in call order:
//!
//! ```text
//! host                                     pack .so
//! ----                                     --------
//! fsw_abi_version             ---------->  FSW_ABI_VERSION (checked first)
//! fsw_pack_open               ---------->  run_pack_open      (pack() once, opaque pack)
//! fsw_pack_describe(sink)     ---------->  run_pack_describe  (manifest bytes via ByteSink)
//! fsw_pack_create(idx, mount) ---------->  run_pack_create    (per-instance state pointer)
//! fsw_pack_bind_init(rings)   ---------->  run_pack_bind_init (attach rings, driver init)
//! fsw_pack_execute(now)       --(loop)-->  run_pack_execute   (one step, FswStatus word)
//! fsw_pack_shutdown           ---------->  run_pack_shutdown
//! fsw_pack_destroy            ---------->  run_pack_destroy   (drop the instance state)
//! fsw_pack_close              ---------->  run_pack_close     (drop the Pack, last)
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
//!   created and dropped inside the same `.so`, and [`run_pack_describe`] hands
//!   its bytes to a host-owned [`ByteSink`] so the host copies rather than frees.
//!
//! Ports bind positionally. The host sends [`FswRing`] handles in the order the
//! entry's descriptor lists the ports, and [`RawBinder`] walks them in the same
//! order on the `.so` side. Loaded systems run in-process on the cyclic
//! schedule, so every wake endpoint is `NoWake`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;

use std::sync::Arc;

use metor_fsw_ring::{RingBuffer, WakeSink, WakeSource};
use metor_proto::types::{ComponentId, PacketId, Timestamp};
use metor_proto::vtable::{Op, VTable};
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};

use crate::binder::RingSource;
use crate::descriptor::{
    AnnounceFn, Delivery, FanIn, PortDesc, PortId, PortSchema, SystemDescriptor, SystemKind,
};

// ---------------------------------------------------------------------------
// Version + identity
// ---------------------------------------------------------------------------

/// The ABI word a host checks for equality before any other call.
///
/// Bump this on any change to the C surface or to the `*Msg` wire structs below,
/// once per released ABI shape. A mismatch fails the load cleanly instead of
/// risking a crash on a stale binary.
///
/// Version 5 is the pack ABI: one cdylib exports many systems through the
/// `fsw_pack_*` surface, replacing the one-system `fsw_describe`/`fsw_create`
/// family entirely.
pub const FSW_ABI_VERSION: u32 = 5;

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
    pub(crate) fn from_raw(raw: u32) -> Self {
        match raw {
            0 => FswStatus::Running,
            1 => FswStatus::Panicked,
            2 => FswStatus::Done,
            _ => FswStatus::Panicked,
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
/// `fsw_pack_open` constructs the crate's [`Pack`](crate::Pack) once and
/// returns it as an opaque pointer (null if `pack()` panicked).
pub const SYM_PACK_OPEN: &[u8] = b"fsw_pack_open\0";
/// `fsw_pack_describe` sends the serialized [`PackManifestMsg`] via a
/// [`ByteSink`].
pub const SYM_PACK_DESCRIBE: &[u8] = b"fsw_pack_describe\0";
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
// The pack manifest (postcard)
// ---------------------------------------------------------------------------

/// One pack entry's wire form: its descriptor (the same
/// [`SystemDescriptorMsg`] a single system always shipped), whether it can be
/// instantiated more than once, and its optional default-params blob.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackSystemMsg {
    pub descriptor: SystemDescriptorMsg,
    /// `false` for a `.state(...)` entry: one instance, never a slot occupant.
    pub reloadable: bool,
    /// Canonical postcard bytes of the entry's default params, when declared;
    /// the KDL encoder overlays config onto them. Carried from day one so
    /// adding defaults later is not another ABI version.
    pub params_default: Option<Vec<u8>>,
}

/// The whole pack's wire form, what `fsw_pack_describe` sends: one
/// [`PackSystemMsg`] per entry, in registration order — the order
/// `fsw_pack_create` indexes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackManifestMsg {
    pub systems: Vec<PackSystemMsg>,
}

// ---------------------------------------------------------------------------
// Pack export helpers
// ---------------------------------------------------------------------------

/// The heap allocation behind the opaque pack pointer: the crate's [`Pack`],
/// constructed once by [`run_pack_open`] and dropped by [`run_pack_close`].
struct PackHost {
    pack: crate::Pack,
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
        Box::into_raw(Box::new(PackHost { pack: pack_fn() })) as *mut c_void
    }));
    outcome.unwrap_or(core::ptr::null_mut())
}

/// `fsw_pack_describe`: lower every entry into the [`PackManifestMsg`],
/// postcard-encode it, and hand the bytes to the host [`ByteSink`]. Returns
/// `0` on success, `-1` if the pack pointer is null or anything panics.
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`]; `sink`/`ctx` form a valid
/// host callback, called once with a buffer the `.so` owns for the call.
pub unsafe fn run_pack_describe(pack: *mut c_void, sink: ByteSink, ctx: *mut c_void) -> i32 {
    if pack.is_null() {
        return -1;
    }
    // SAFETY: caller asserts `pack` is a live `PackHost` from `run_pack_open`.
    let host = unsafe { &*(pack as *mut PackHost) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let msg = PackManifestMsg {
            systems: host
                .pack
                .entries()
                .map(|e| PackSystemMsg {
                    descriptor: SystemDescriptorMsg::lower(
                        e.descriptor(),
                        OwnedNamedType::from(e.params_schema()),
                    ),
                    reloadable: e.reloadable(),
                    params_default: e.params_default().map(<[u8]>::to_vec),
                })
                .collect(),
        };
        postcard::to_allocvec(&msg).expect("pack manifest encodes (postcard)")
    }));
    match outcome {
        Ok(bytes) => {
            sink(ctx, bytes.as_ptr(), bytes.len());
            0
        }
        Err(_) => -1,
    }
}

/// `fsw_pack_create`: run entry `index`'s create phase — decode the postcard
/// params, build the user state — and box the unbound instance state. Returns
/// null for a null pack, an out-of-range index, or a create failure (bad
/// params, a moved-in state already taken, a panic).
///
/// # Safety
/// `pack` is a live pointer from [`run_pack_open`]; `params`/`params_len`
/// name a readable byte range (or null/0). The returned pointer is owned by
/// the caller and passed only to the other `run_pack_*` helpers, then
/// [`run_pack_destroy`] — all before [`run_pack_close`].
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
        let pending = entry
            .create(crate::EntryParams::Postcard(bytes))
            .expect("entry create (params decode + state construction)");
        Box::into_raw(Box::new(PackAbiState {
            pending: Some(pending),
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
/// [`FswStatus::Panicked`].
///
/// # Safety
/// `state` is a live pointer from [`run_pack_create`]. `inputs`/`outputs`
/// name `n_in`/`n_out` valid [`FswRing`] handles whose regions satisfy
/// [`RingBuffer::attach_raw`]'s contract and outlive the driver (until
/// [`run_pack_destroy`]).
pub unsafe fn run_pack_bind_init(
    state: *mut c_void,
    inputs: *const FswRing,
    n_in: usize,
    outputs: *const FswRing,
    n_out: usize,
) {
    if state.is_null() {
        return;
    }
    // SAFETY: caller asserts `state` is a live `PackAbiState`.
    let st = unsafe { &mut *(state as *mut PackAbiState) };
    // SAFETY: caller asserts the handle arrays are valid (or null/0).
    let (in_slice, out_slice) =
        unsafe { (rings_from_raw(inputs, n_in), rings_from_raw(outputs, n_out)) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller asserts each region outlives the driver.
        let mut binder = unsafe { RawBinder::new(in_slice, out_slice) };
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

/// Emit the nine `extern "C"` exports of the pack ABI, delegating to the
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
                abi::run_pack_open($pack_fn)
            }

            #[unsafe(no_mangle)]
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            pub extern "C" fn fsw_pack_describe(
                pack: *mut c_void,
                sink: abi::ByteSink,
                ctx: *mut c_void,
            ) -> i32 {
                // SAFETY: the host upholds run_pack_describe's contract.
                unsafe { abi::run_pack_describe(pack, sink, ctx) }
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
            ) {
                // SAFETY: the host upholds run_pack_bind_init's contract.
                unsafe { abi::run_pack_bind_init(state, inputs, n_in, outputs, n_out) }
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
        };
    };
}

#[cfg(all(test, feature = "kdl"))]
mod tests;
