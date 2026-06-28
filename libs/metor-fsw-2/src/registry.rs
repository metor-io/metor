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
