//! Work-Package 7 — the general output registry (telemetry.md §2).
//!
//! A thin, queryable index over the coordinator's [`RingTable`](crate::coordinator):
//! one [`RegistryEntry`] per tappable output buffer (every system's user frames plus
//! their implicit `health`/`log`, plus the coordinator-owned `health`/`log`/`status`).
//! It is the load-bearing capability behind the telemetry downlink, but it is general:
//! any broad/dynamic reader (a logger, recorder, debugger) reaches outputs the same way.
//!
//! The registry never exposes the raw [`RingBuffer`] — only a [`view()`](RegistryEntry::view)
//! factory, so every reader is **slot-accounted** against the buffer's build-time
//! `max_readers` budget (telemetry.md §2.5/Q8). The coordinator sizes that budget to
//! include the known registry consumers.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw_ring::{BoxBacking, FullReaderTable, NoWake, RingBuffer, View};
use metor_proto::types::ComponentId;
use metor_proto::vtable::VTable;
use metor_proto_wkt::ComponentMetadata;

use crate::binder::RingSource;
use crate::descriptor::PortDesc;

/// One tappable output buffer, indexed by its instance-qualified id (telemetry.md §2.1).
pub struct RegistryEntry {
    /// The instance-qualified id `ComponentId::new("<instance>.<frame>")` — also the
    /// on-wire prefix id, so the key, the wire id, and the prefix are one identity.
    pub key: ComponentId,
    /// The owning system's instance name (`"imu_left"`), or `"coordinator"` for the
    /// coordinator-owned buffers. Kept for human-readable subset filtering.
    pub instance: Arc<str>,
    /// The unprefixed frame id (`ComponentId::new("imu")`) — shared across instances.
    pub frame_id: ComponentId,
    /// The **prefixed** announce vtable (`<instance>.<frame>.<field>` ids), computed
    /// once at `build()` via [`PortDesc::announce`](crate::PortDesc::announce).
    pub vtable: VTable,
    /// The prefixed component metadata, parallel to `vtable`.
    pub metadata: Vec<ComponentMetadata>,
    /// The read source. Crate-private so external callers must go through
    /// [`view()`](Self::view), which claims a slot-accounted reader (never the raw
    /// buffer); the coordinator sets it at `build()`.
    pub(crate) ring: RingBuffer<BoxBacking>,
}

impl RegistryEntry {
    /// Claim a read [`View`] into this output, consuming one reader slot from the
    /// buffer's fixed `max_readers` table. Fails with [`FullReaderTable`] if the
    /// build-time slot budget is exhausted (telemetry.md §2.5).
    pub fn view(&self) -> Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable> {
        self.ring.view(NoWake, NoWake)
    }
}

/// The registry: a by-key index over the build-order [`RegistryEntry`] list.
pub struct OutputRegistry {
    entries: Vec<RegistryEntry>,
    by_key: HashMap<ComponentId, usize>,
}

impl OutputRegistry {
    /// Assemble the registry from the build-order entries the coordinator collected.
    pub(crate) fn new(entries: Vec<RegistryEntry>) -> Self {
        let by_key = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.key, i))
            .collect();
        Self { entries, by_key }
    }

    /// Every tappable buffer, in build order.
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.entries
    }

    /// Look up one output by its instance-qualified id (`ComponentId::new("<instance>.<frame>")`).
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry> {
        self.by_key.get(&key).map(|&i| &self.entries[i])
    }

    /// Claim a read [`View`] into the output identified by `key`, or `None` if no such
    /// output exists. The inner `Result` is the slot-budget check (telemetry.md §2.5).
    pub fn view(
        &self,
        key: ComponentId,
    ) -> Option<Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable>> {
        self.get(key).map(RegistryEntry::view)
    }

    /// Number of tappable buffers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the graph produced no tappable buffers.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// MessageRegistry — the parallel index over message channels
// ---------------------------------------------------------------------------

/// One tappable **message** channel, indexed by its instance-qualified id
/// (`docs/messages.md` §2). The message twin of [`RegistryEntry`] — but messages are
/// **self-describing** (`(PacketId, postcard)` records), so this carries **no**
/// `vtable`/`metadata`/`frame_id`/announce: a tap decodes a record from its 2-byte id
/// alone (`docs/messages.md` Q3).
pub struct MessageEntry {
    /// The instance-qualified id `ComponentId::new("<instance>.<channel>")` — the
    /// channel's stable identity, parallel to [`RegistryEntry::key`].
    pub key: ComponentId,
    /// The owning system's instance name (`"mode"`), or `"coordinator"` for the
    /// coordinator-owned channels. Kept for human-readable subset filtering.
    pub instance: Arc<str>,
    /// The channel name within the instance (`"events"`) — the message analogue of the
    /// output's frame id, but a plain name (messages have no frame type).
    pub channel: Arc<str>,
    /// Whether the downlink / [`AllOutputs`] taps this channel (`docs/message-wiring.md`
    /// §6.4). `false` for command channels (inbound control, never echoed to the panel).
    pub telemetered: bool,
    /// The read source. Crate-private so external callers go through
    /// [`view()`](Self::view), which claims a slot-accounted reader (never the raw
    /// buffer); the coordinator sets it at `build()`.
    pub(crate) ring: RingBuffer<BoxBacking>,
}

impl MessageEntry {
    /// Claim a read [`View`] into this channel, consuming one reader slot from the
    /// buffer's fixed `max_readers` table. Fails with [`FullReaderTable`] if the
    /// build-time slot budget is exhausted — exactly [`RegistryEntry::view`].
    pub fn view(&self) -> Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable> {
        self.ring.view(NoWake, NoWake)
    }
}

/// The message registry: a by-key index over the build-order [`MessageEntry`] list, the
/// [`OutputRegistry`] shape verbatim for message channels (`docs/messages.md` §2).
pub struct MessageRegistry {
    entries: Vec<MessageEntry>,
    by_key: HashMap<ComponentId, usize>,
}

impl MessageRegistry {
    /// Assemble the registry from the build-order entries the coordinator collected.
    pub(crate) fn new(entries: Vec<MessageEntry>) -> Self {
        let by_key = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.key, i))
            .collect();
        Self { entries, by_key }
    }

    /// Every tappable message channel, in build order.
    pub fn entries(&self) -> &[MessageEntry] {
        &self.entries
    }

    /// Look up one channel by its instance-qualified id (`ComponentId::new("<instance>.<channel>")`).
    pub fn get(&self, key: ComponentId) -> Option<&MessageEntry> {
        self.by_key.get(&key).map(|&i| &self.entries[i])
    }

    /// Claim a read [`View`] into the channel identified by `key`, or `None` if no such
    /// channel exists. The inner `Result` is the slot-budget check.
    pub fn view(
        &self,
        key: ComponentId,
    ) -> Option<Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable>> {
        self.get(key).map(MessageEntry::view)
    }

    /// Number of tappable message channels.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the graph produced no message channels.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// AllOutputs — the reusable receive-all tap port
// ---------------------------------------------------------------------------

/// A broadcast tap over **every** telemetered output frame and message channel in the graph
/// (`docs/message-wiring.md` §4) — the reusable generalization of the telemetry downlink's
/// twin-registry pull. It reserves no ring and connects no edge; it is a *capability* port
/// (`PortKind::ReceiveAll`) that appears in a bundle like any port and binds by pulling both
/// registries, so any system (a downlink, a logger, a recorder) taps the whole graph just by
/// declaring an `AllOutputs` field. Each such port counts one extra reader on every buffer,
/// which `build()` derives into every ring's `max_readers` budget.
pub struct AllOutputs {
    /// Every tappable component-frame output.
    pub outputs: Arc<OutputRegistry>,
    /// Every tappable message channel (skip `!telemetered` entries — command channels).
    pub messages: Arc<MessageRegistry>,
}

impl AllOutputs {
    /// The receive-all tap descriptor (`PortKind::ReceiveAll`) — ring-less, edge-less.
    pub fn descriptor() -> PortDesc {
        PortDesc::receive_all()
    }

    /// Pull both broad registries the host [`Binder`](crate::Binder) carries. Host-only, like
    /// the underlying `output_registry()`/`message_registry()` capabilities.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        Self {
            outputs: src.output_registry(),
            messages: src.message_registry(),
        }
    }

    /// A tap reads, it never publishes — no drop counter to fold (review E6). Present
    /// so `#[derive(SystemOutput)]`'s generated `take_dropped` sum compiles over a
    /// bundle that carries an `AllOutputs` field.
    pub fn take_dropped(&mut self) -> u64 {
        0
    }
}
