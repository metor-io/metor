//! A by-key index over every tappable buffer in the graph.
//!
//! The coordinator builds one [`RegistryEntry`] per registered buffer, in
//! build order. That covers every system's user frames plus their implicit
//! `system_status` and `log` outputs, every message channel, and the
//! coordinator-owned `system_status`, `log`, `status`, `sequences`, and `commands`
//! buffers. A target with a wiring manifest also has a `wiring` output.
//! Broad or dynamic readers (a downlink, a logger, a recorder, a
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
use crate::descriptor::{Capability, Delivery, PortDesc};

/// One tappable buffer, indexed by its instance-qualified id.
pub struct RegistryEntry {
    /// The instance-qualified id `ComponentId::new("<instance>.<name>")`.
    /// This is also the on-wire prefix id, so the lookup key, the wire id,
    /// and the prefix are one identity.
    pub key: ComponentId,
    /// The owning system's instance name (`"imu_left"`), or `"coordinator"`
    /// for the coordinator-owned buffers. Kept for human-readable subset
    /// filtering and as the announce prefix.
    pub instance: Arc<str>,
    /// The registered port's descriptor: its name, schema, delivery, and
    /// telemetry flag, exactly as the owning system declared them.
    pub desc: PortDesc,
    /// The read source. Crate-private so external callers must go through
    /// [`view()`](Self::view) and claim a slot-accounted reader rather than
    /// touching the raw buffer.
    pub(crate) ring: RingBuffer,
}

impl RegistryEntry {
    /// One registered buffer. The key is the instance-qualified id, which is
    /// also the on-wire prefix id.
    pub fn new(key: ComponentId, instance: Arc<str>, desc: PortDesc, ring: RingBuffer) -> Self {
        Self {
            key,
            instance,
            desc,
            ring,
        }
    }

    /// The port name within the instance, taken from `F::NAME`, `M::NAME`,
    /// or an explicit coordinator channel string such as `"sequences"`.
    pub fn name(&self) -> &str {
        &self.desc.name
    }

    /// How a broad reader drains this entry, coalescing to the newest record
    /// ([`Snapshot`](Delivery::Snapshot)) or taking every record in order
    /// ([`Log`](Delivery::Log)).
    pub fn delivery(&self) -> Delivery {
        self.desc.delivery
    }

    /// Whether [`AllOutputs`] exposes this entry. Untelemetered entries stay
    /// registered and remain visible by key through the full [`Registry`],
    /// but are filtered out of every [`AllOutputs`] surface.
    pub fn telemetered(&self) -> bool {
        self.desc.telemetered
    }

    /// The announce form of a Table entry: the vtable and metadata with
    /// instance-prefixed `<instance>.<frame>.<field>` ids, the identities a
    /// tap wires records to. `None` for a Postcard entry, whose records are
    /// self-describing.
    pub fn announce(&self) -> Option<(VTable, Vec<ComponentMetadata>)> {
        self.desc.announce(&self.instance)
    }

    /// Claim a read [`View`] into this buffer, consuming one reader slot.
    ///
    /// Fails with [`FullReaderTable`] if the buffer's build-time slot budget
    /// is exhausted.
    pub fn view(&self) -> Result<View<NoWake>, FullReaderTable> {
        self.ring.view(NoWake)
    }
}

/// The full index over every registered buffer, untelemetered entries
/// included.
///
/// This surface is reachable only through the host's `Coordinator::registry()`,
/// which serves debuggers and tests. The in-graph broadcast tap is
/// [`AllOutputs`], which filters untelemetered entries at the source.
pub struct Registry {
    entries: Vec<RegistryEntry>,
    by_key: HashMap<ComponentId, usize>,
}

impl Registry {
    /// Assemble the registry from the build-order entries the coordinator
    /// collected. `build()` has already rejected duplicate keys.
    pub fn new(entries: Vec<RegistryEntry>) -> Self {
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
    pub fn view(&self, key: ComponentId) -> Option<Result<View<NoWake>, FullReaderTable>> {
        self.get(key).map(RegistryEntry::view)
    }
}

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
/// [`Registry`] remains reachable via the host's `Coordinator::registry()`.
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
    pub fn decl() -> Capability {
        Capability::ReceiveAll
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
        self.registry.entries().iter().filter(|e| e.telemetered())
    }

    /// Always zero, since a tap only reads. Present so the generated
    /// `take_dropped` sum of a `#[derive(SystemOutput)]` bundle compiles when
    /// the bundle carries an `AllOutputs` field.
    pub fn take_dropped(&mut self) -> u64 {
        0
    }
}
