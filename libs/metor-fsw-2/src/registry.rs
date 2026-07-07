//! A by-key index over every tappable buffer in the graph.
//!
//! The coordinator builds one [`RegistryEntry`] per registered buffer, in
//! build order. That covers every system's user frames plus their implicit
//! `health` and `log` outputs, every message channel, and the
//! coordinator-owned `health`, `log`, `status`, `sequences`, and `commands`
//! buffers. Broad or dynamic readers (a downlink, a logger, a recorder, a
//! debugger) all reach outputs through this one index rather than through any
//! per-port wiring.
//!
//! Frames and message channels share a single keyspace, so a name collision
//! within an instance is detectable rather than silently shadowed. `build()`
//! rejects a duplicate key with
//! [`WireError::DuplicateRegistryKey`](crate::WireError::DuplicateRegistryKey).
//!
//! The registry never hands out a raw [`RingBuffer`]. Every reader goes
//! through [`RegistryEntry::view`], which claims a slot from the buffer's
//! fixed `max_readers` table, so read capacity stays accounted against the
//! budget the coordinator sized at build time.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw_ring::{FullReaderTable, NoWake, RingBuffer, View};
use metor_proto::types::ComponentId;
use metor_proto::vtable::VTable;
use metor_proto_wkt::ComponentMetadata;

use crate::binder::RingSource;
use crate::descriptor::{Capability, Delivery, PortDecl};

/// How a tap decodes an entry's records.
pub enum EntrySchema {
    /// Announce-then-Table wire form.
    Table {
        /// The unprefixed frame id (`ComponentId::new("imu")`), shared across
        /// instances of the same system.
        frame_id: ComponentId,
        /// The announce vtable with instance-prefixed
        /// `<instance>.<frame>.<field>` ids, computed once at build time from
        /// the port's announce factory.
        vtable: VTable,
        /// The prefixed component metadata, parallel to `vtable`.
        metadata: Vec<ComponentMetadata>,
    },
    /// Self-describing `(PacketId, postcard)` records. The 2-byte id is read
    /// from each record, so there is no vtable and no announce.
    Postcard,
}

/// One tappable buffer, indexed by its instance-qualified id.
pub struct RegistryEntry {
    /// The instance-qualified id `ComponentId::new("<instance>.<name>")`.
    /// This is also the on-wire prefix id, so the lookup key, the wire id,
    /// and the prefix are one identity.
    pub key: ComponentId,
    /// The owning system's instance name (`"imu_left"`), or `"coordinator"`
    /// for the coordinator-owned buffers. Kept for human-readable subset
    /// filtering.
    pub instance: Arc<str>,
    /// The port name within the instance, taken from `F::NAME`, `M::NAME`,
    /// or an explicit coordinator channel string such as `"sequences"`.
    pub name: Arc<str>,
    /// How a tap decodes this entry's records.
    pub schema: EntrySchema,
    /// How a broad reader drains this entry, coalescing to the newest record
    /// ([`Snapshot`](Delivery::Snapshot)) or taking every record in order
    /// ([`Log`](Delivery::Log)).
    pub delivery: Delivery,
    /// Whether [`AllOutputs`] exposes this entry. Untelemetered entries stay
    /// registered and remain visible by key through the full [`Registry`],
    /// but are filtered out of every [`AllOutputs`] surface.
    pub telemetered: bool,
    /// The read source. Crate-private so external callers must go through
    /// [`view()`](Self::view) and claim a slot-accounted reader rather than
    /// touching the raw buffer.
    pub(crate) ring: RingBuffer,
}

impl RegistryEntry {
    /// Claim a read [`View`] into this buffer, consuming one reader slot.
    ///
    /// Fails with [`FullReaderTable`] if the buffer's build-time slot budget
    /// is exhausted.
    pub fn view(&self) -> Result<View<NoWake, NoWake>, FullReaderTable> {
        self.ring.view(NoWake, NoWake)
    }
}

/// The full index over every registered buffer, untelemetered entries
/// included.
///
/// This surface is reachable only through the host-side
/// [`Coordinator::registry()`](crate::Coordinator::registry), which serves
/// debuggers and tests. The in-graph broadcast tap is [`AllOutputs`], which
/// filters untelemetered entries at the source.
pub struct Registry {
    entries: Vec<RegistryEntry>,
    by_key: HashMap<ComponentId, usize>,
}

impl Registry {
    /// Assemble the registry from the build-order entries the coordinator
    /// collected. `build()` has already rejected duplicate keys.
    pub(crate) fn new(entries: Vec<RegistryEntry>) -> Self {
        let by_key = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.key, i))
            .collect();
        Self { entries, by_key }
    }

    /// Every registered buffer, in build order, untelemetered ones included.
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.entries
    }

    /// Look up one buffer by its instance-qualified id
    /// (`ComponentId::new("<instance>.<name>")`).
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry> {
        self.by_key.get(&key).map(|&i| &self.entries[i])
    }

    /// Claim a read [`View`] into the buffer identified by `key`, or `None`
    /// if no such buffer exists. The inner `Result` is the reader slot-budget
    /// check.
    pub fn view(&self, key: ComponentId) -> Option<Result<View<NoWake, NoWake>, FullReaderTable>> {
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
// AllOutputs
// ---------------------------------------------------------------------------

/// A broadcast tap over every telemetered buffer in the graph.
///
/// `AllOutputs` is not a port but a [`Capability::ReceiveAll`] grant. It
/// reserves no ring and connects no edge. It appears in a bundle like any
/// other field and binds by pulling the one [`Registry`], consuming no
/// bind-cursor position, so any system taps the whole graph just by declaring
/// an `AllOutputs` field. Each such capability counts one extra reader on
/// every buffer, and `build()` folds that into every ring's `max_readers`
/// budget.
///
/// The telemetered filter lives here at the source, so a consumer cannot
/// forget it. An untelemetered entry, such as a command channel or an
/// opted-out frame, is simply invisible through this surface. The unfiltered
/// [`Registry`] remains reachable via the host-side
/// [`Coordinator::registry()`](crate::Coordinator::registry).
///
/// A view claimed off an entry starts at the buffer's live edge, so records
/// emitted before the claim, for example during an earlier-registered
/// system's `init` before the holder's own `init` ran, are never observed. A
/// consumer that must not miss boot-time records needs its producers to
/// (re-)publish from their first `execute`.
pub struct AllOutputs {
    registry: Arc<Registry>,
}

impl AllOutputs {
    /// The bundle declaration for this field, a [`Capability::ReceiveAll`]
    /// grant rather than a port. It creates no ring, no edge, and no registry
    /// entry, and the bind cursor skips it.
    pub fn decl() -> PortDecl {
        PortDecl::Capability(Capability::ReceiveAll)
    }

    /// Pull the one registry the host [`Binder`](crate::Binder) carries.
    /// Host-only, like the underlying registry capability.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        Self {
            registry: src.registry(),
        }
    }

    /// Every telemetered entry, in build order. This is the only iteration
    /// surface.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.registry.entries().iter().filter(|e| e.telemetered)
    }

    /// Look up one telemetered entry by its instance-qualified id. An
    /// untelemetered entry returns `None`.
    pub fn get(&self, key: ComponentId) -> Option<&RegistryEntry> {
        self.registry.get(key).filter(|e| e.telemetered)
    }

    /// Always zero, since a tap only reads. Present so the generated
    /// `take_dropped` sum of a `#[derive(SystemOutput)]` bundle compiles when
    /// the bundle carries an `AllOutputs` field.
    pub fn take_dropped(&mut self) -> u64 {
        0
    }
}
