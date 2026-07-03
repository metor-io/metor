//! The general output registry (telemetry.md §2) — ONE by-key index over every
//! tappable buffer, frames and message channels alike, in build order.
//!
//! A thin, queryable index over the coordinator's ring table: one [`RegistryEntry`]
//! per registered buffer (every system's user frames plus their implicit
//! `health`/`log`, every message channel, the coordinator-owned
//! `health`/`log`/`status`/`sequences`/`commands`). It is the load-bearing capability
//! behind the telemetry downlink, but it is general: any broad/dynamic reader (a
//! logger, recorder, debugger) reaches outputs the same way.
//!
//! One keyspace instead of two parallel ones means a same-instance name collision
//! between a frame and a channel is *detectable*: `build()` rejects a duplicate key
//! (`WireError::DuplicateRegistryKey`) rather than shadowing.
//!
//! The registry never exposes the raw [`RingBuffer`] — only a
//! [`view()`](RegistryEntry::view) factory, so every reader is **slot-accounted**
//! against the buffer's build-time `max_readers` budget (telemetry.md §2.5/Q8). The
//! coordinator sizes that budget to include the known registry consumers.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw_ring::{BoxBacking, FullReaderTable, NoWake, RingBuffer, View};
use metor_proto::types::ComponentId;
use metor_proto::vtable::VTable;
use metor_proto_wkt::ComponentMetadata;

use crate::binder::RingSource;
use crate::descriptor::{Capability, Delivery, PortDecl};

/// What a tap needs to know about an entry's records — the registry projection of
/// the port's [`PortSchema`](crate::PortSchema) axis.
pub enum EntrySchema {
    /// Announce-then-Table wire form.
    Table {
        /// The unprefixed frame id (`ComponentId::new("imu")`) — shared across
        /// instances.
        frame_id: ComponentId,
        /// The **prefixed** announce vtable (`<instance>.<frame>.<field>` ids),
        /// computed once at `build()` via the port's announce factory.
        vtable: VTable,
        /// The prefixed component metadata, parallel to `vtable`.
        metadata: Vec<ComponentMetadata>,
    },
    /// Self-describing `(PacketId, postcard)` records; the 2-byte id is read from
    /// each record — no vtable, no announce.
    Postcard,
}

/// One tappable buffer, indexed by its instance-qualified id (telemetry.md §2.1).
pub struct RegistryEntry {
    /// The instance-qualified id `ComponentId::new("<instance>.<name>")` — also the
    /// on-wire prefix id, so the key, the wire id, and the prefix are one identity.
    pub key: ComponentId,
    /// The owning system's instance name (`"imu_left"`), or `"coordinator"` for the
    /// coordinator-owned buffers. Kept for human-readable subset filtering.
    pub instance: Arc<str>,
    /// The port name within the instance: `F::NAME`, `M::NAME`, or an explicit
    /// coordinator channel string (`"sequences"`).
    pub name: Arc<str>,
    /// How a tap decodes this entry's records (announce+Table vs self-describing).
    pub schema: EntrySchema,
    /// How a broad reader drains this entry: coalesce to the newest record
    /// (Snapshot) or take every record in order (Log).
    pub delivery: Delivery,
    /// Whether the downlink / [`AllOutputs`] taps this entry (A6). Untelemetered
    /// entries stay registered — visible to a debugger/test by key through the full
    /// [`Registry`] — but are filtered out of every [`AllOutputs`] surface.
    pub telemetered: bool,
    /// The read source. Crate-private so external callers must go through
    /// [`view()`](Self::view), which claims a slot-accounted reader (never the raw
    /// buffer); the coordinator sets it at `build()`.
    pub(crate) ring: RingBuffer<BoxBacking>,
}

impl RegistryEntry {
    /// Claim a read [`View`] into this buffer, consuming one reader slot from the
    /// buffer's fixed `max_readers` table. Fails with [`FullReaderTable`] if the
    /// build-time slot budget is exhausted (telemetry.md §2.5).
    pub fn view(&self) -> Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable> {
        self.ring.view(NoWake, NoWake)
    }
}

/// THE registry: a by-key index over the build-order [`RegistryEntry`] list —
/// every tappable buffer, frames and message channels alike, **including**
/// untelemetered entries. Reachable only through the host-side
/// [`Coordinator::registry()`](crate::Coordinator::registry) (debugger/test
/// surface); the in-graph broadcast tap is [`AllOutputs`], which filters
/// untelemetered entries at the source.
pub struct Registry {
    entries: Vec<RegistryEntry>,
    by_key: HashMap<ComponentId, usize>,
}

impl Registry {
    /// Assemble the registry from the build-order entries the coordinator
    /// collected. Key uniqueness was validated by `build()`
    /// (`WireError::DuplicateRegistryKey`).
    pub(crate) fn new(entries: Vec<RegistryEntry>) -> Self {
        let by_key = entries.iter().enumerate().map(|(i, e)| (e.key, i)).collect();
        Self { entries, by_key }
    }

    /// Every registered buffer, in build order (untelemetered ones included).
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.entries
    }

    /// Look up one buffer by its instance-qualified id
    /// (`ComponentId::new("<instance>.<name>")`).
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry> {
        self.by_key.get(&key).map(|&i| &self.entries[i])
    }

    /// Claim a read [`View`] into the buffer identified by `key`, or `None` if no
    /// such buffer exists. The inner `Result` is the slot-budget check
    /// (telemetry.md §2.5).
    pub fn view(
        &self,
        key: ComponentId,
    ) -> Option<Result<View<BoxBacking, NoWake, NoWake>, FullReaderTable>> {
        self.get(key).map(RegistryEntry::view)
    }

    /// Number of registered buffers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the graph produced no registered buffers.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// AllOutputs — the reusable receive-all tap capability
// ---------------------------------------------------------------------------

/// A broadcast tap over **every telemetered** buffer in the graph
/// (`docs/message-wiring.md` §4) — the reusable generalization of the telemetry
/// downlink's registry pull. Not a port but a [`Capability::ReceiveAll`] grant: it
/// reserves no ring and connects no edge; it appears in a bundle like any field and
/// binds by pulling the one [`Registry`] (consuming no bind-cursor position), so any
/// system (a downlink, a logger, a recorder) taps the whole graph just by declaring
/// an `AllOutputs` field. Each such capability counts one extra reader on every
/// buffer, which `build()` derives into every ring's `max_readers` budget.
///
/// The telemetered filter lives HERE, at the source (A6): consumers can no longer
/// forget the convention — an untelemetered entry (a command channel, an opted-out
/// frame) is simply invisible through this surface. The full unfiltered [`Registry`]
/// remains reachable via the host-side
/// [`Coordinator::registry()`](crate::Coordinator::registry).
///
/// **Init-time emit gap (B9)**: a view claimed off an entry starts at the buffer's
/// live edge, so records emitted *before* the claim — e.g. during an
/// earlier-registered system's `init`, before the holder's own `init` ran — are
/// never observed. A consumer that must not miss boot-time records should have its
/// producers (re-)publish from their first `execute` (the coordinator's boot
/// `SequenceRegistry` emits at the head of `run_for` for exactly this reason).
pub struct AllOutputs {
    registry: Arc<Registry>,
}

impl AllOutputs {
    /// What this field contributes to the bundle's `decls` walk: not a port at all
    /// but the [`Capability::ReceiveAll`] grant — no ring, no edge, no registry
    /// entry, and the bind cursor skips it ([`bind`](Self::bind) pulls the registry
    /// instead of consuming a bound ring).
    pub fn decl() -> PortDecl {
        PortDecl::Capability(Capability::ReceiveAll)
    }

    /// Pull the one broad registry the host [`Binder`](crate::Binder) carries.
    /// Host-only, like the underlying `registry()` capability.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        Self {
            registry: src.registry(),
        }
    }

    /// Every **telemetered** entry, in build order — the only iteration surface.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.registry.entries().iter().filter(|e| e.telemetered)
    }

    /// Look up one **telemetered** entry by its instance-qualified id; an
    /// untelemetered entry returns `None` (invisible through this surface).
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry> {
        self.registry.get(key).filter(|e| e.telemetered)
    }

    /// A tap reads, it never publishes — no drop counter to fold (review E6). Present
    /// so `#[derive(SystemOutput)]`'s generated `take_dropped` sum compiles over a
    /// bundle that carries an `AllOutputs` field.
    pub fn take_dropped(&mut self) -> u64 {
        0
    }
}
