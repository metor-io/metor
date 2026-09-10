//! The tap set a telemetry sink claims over the graph, and the per-cycle walk
//! that drains it.
//!
//! Both halves are free functions over [`AllOutputs`] so a downlink framing
//! packets and a recorder writing a db share one definition of what a tap is
//! and when it contributes.

use metor_fsw_2_core::{AllOutputs, Delivery, RegistryEntry};
use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{PacketId, table_id};
use metor_proto::vtable::VTable;
use metor_proto_wkt::{ComponentMetadata, MsgMetadata, SetMsgMetadata};

/// Which registry entries a sink taps.
pub(crate) enum TelemetryMode {
    /// Tap every entry: every system's user frames and their implicit
    /// `system_status`/`log`, plus the coordinator-owned `system_status`/`log`/`status`.
    All,
    /// Tap only the entries whose instance name or frame name appears in the
    /// configured lists; matching either is enough.
    Subset {
        instances: Vec<String>,
        frames: Vec<String>,
    },
}

impl TelemetryMode {
    /// Whether `entry` is tapped. The `frames` list matches
    /// [`RegistryEntry::name`], which covers frame names and channel names
    /// alike.
    pub(crate) fn matches(&self, entry: &RegistryEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| f.as_str() == entry.name())
            }
        }
    }
}

/// One announced tap's wire schema, replayed to each new connection: a table
/// tap's vtable + component metadata, or a message channel's payload schema.
pub(crate) enum Announce {
    Table {
        packet_id: PacketId,
        vtable: VTable,
        metadata: Vec<ComponentMetadata>,
    },
    Msg(SetMsgMetadata),
}

/// How a tap frames a drained record, projected from the entry's schema.
pub(crate) enum Wire {
    /// A `Table` packet under the announce-assigned packet id.
    Table { packet_id: PacketId },
    /// A self-describing `Msg` packet; the id is the record's first two bytes.
    Msg,
}

/// A read view into one tapped buffer plus the delivery axis and the [`Wire`]
/// framing projected from the entry.
pub(crate) struct Tap {
    pub view: View<NoWake>,
    pub delivery: Delivery,
    pub wire: Wire,
    /// Snapshot taps only: the ring's `committed` at the last contribution, so
    /// a cycle with no new record contributes nothing (the pinned newest
    /// record is not re-sent). `u64::MAX` means nothing contributed yet.
    pub last_committed: u64,
    /// Snapshot *message* taps: this tap's slot in the link's retained
    /// store. The newest framed record is held there and replayed to every
    /// late-joining connection: latest-wins boot state (a wiring manifest,
    /// a sequence registry) that would otherwise stream exactly once.
    /// Continuously-republished frames need no retention (a new connection
    /// sees them within a cycle), so frame taps stay `None`.
    pub retain_slot: Option<usize>,
}

/// The tap set one system claims: views, announces, and the entries no reader
/// slot or free packet id was left for.
pub(crate) struct Taps {
    pub taps: Vec<Tap>,
    pub announces: Vec<Announce>,
    pub retained: usize,
    /// Keys of entries whose ring had no reader slot left.
    pub exhausted: Vec<String>,
    /// `(refused, kept)` keys of two tables hashing to one packet id.
    pub collisions: Vec<(String, String)>,
}

/// Filter `all` by `mode`, claim one view per entry, and build its announce
/// under [`table_id`].
///
/// Reports are returned rather than logged: iterating the registry borrows the
/// output bundle the caller's log port lives in.
pub(crate) fn collect_taps(all: &AllOutputs, mode: &TelemetryMode) -> Taps {
    // `AllOutputs::entries()` is already filtered to telemetered entries,
    // so a command channel or an opted-out frame never reaches the matcher.
    let mut out = Taps {
        taps: Vec::new(),
        announces: Vec::new(),
        retained: 0,
        exhausted: Vec::new(),
        collisions: Vec::new(),
    };
    let mut announced_msgs = std::collections::HashSet::new();
    let mut announced_tables: std::collections::HashMap<PacketId, String> = Default::default();
    for entry in all.entries() {
        if !mode.matches(entry) {
            continue;
        }
        let Ok(view) = entry.view() else {
            out.exhausted
                .push(format!("{}.{}", entry.instance, entry.name()));
            continue;
        };
        // Delivery and wire are independent projections of the entry:
        // delivery picks how much each cycle contributes, schema picks
        // the framing.
        let wire = match entry.announce() {
            Some((vtable, metadata)) => {
                let packet_id = table_id(&vtable);
                let key = format!("{}.{}", entry.instance, entry.name());
                if let Some(kept) = announced_tables.get(&packet_id) {
                    out.collisions.push((key, kept.clone()));
                    continue;
                }
                announced_tables.insert(packet_id, key);
                out.announces.push(Announce::Table {
                    packet_id,
                    vtable,
                    metadata,
                });
                Wire::Table { packet_id }
            }
            None => {
                // Several ports may share a message ID, such as LogEvent.
                if let crate::PortSchema::Postcard {
                    id,
                    schema: Some(schema),
                } = &entry.desc.schema
                    && announced_msgs.insert(*id)
                {
                    out.announces.push(Announce::Msg(SetMsgMetadata {
                        id: *id,
                        metadata: MsgMetadata {
                            name: schema.name.clone(),
                            schema: (**schema).clone(),
                            metadata: Default::default(),
                        },
                    }));
                }
                Wire::Msg
            }
        };
        let retain_slot = (entry.delivery() == Delivery::Snapshot && matches!(wire, Wire::Msg))
            .then(|| {
                let slot = out.retained;
                out.retained += 1;
                slot
            });
        out.taps.push(Tap {
            view,
            delivery: entry.delivery(),
            wire,
            last_committed: u64::MAX,
            retain_slot,
        });
    }
    out
}

/// Walk every tap once: a snapshot tap yields its newest record when
/// `committed` moved, a log tap every record. Returns the count of corrupt
/// reads.
///
/// The callback takes the tap's framing rather than the tap itself: the drain
/// holds the view mutably while the record is alive.
pub(crate) fn drain_taps(
    taps: &mut [Tap],
    mut on_record: impl FnMut(&Wire, Option<usize>, &[u8]),
) -> usize {
    let mut corrupt = 0;
    for tap in taps {
        match tap.delivery {
            // An unchanged `committed` means no new record this cycle;
            // contribute nothing rather than re-sending the pinned record.
            Delivery::Snapshot => {
                let committed = tap.view.committed();
                if committed == tap.last_committed {
                    continue;
                }
                tap.last_committed = committed;
                let Tap {
                    view,
                    wire,
                    retain_slot,
                    ..
                } = tap;
                match view.try_latest() {
                    Ok(Some(grant)) => on_record(wire, *retain_slot, &grant),
                    Ok(None) => {}
                    Err(_) => corrupt += 1,
                }
            }
            // Every record, in order.
            Delivery::Log => {
                let Tap {
                    view,
                    wire,
                    retain_slot,
                    ..
                } = tap;
                let retain_slot = *retain_slot;
                let on_record = &mut on_record;
                if metor_fsw_2_core::drain_view(view, |rec| on_record(wire, retain_slot, rec))
                    .is_err()
                {
                    corrupt += 1;
                }
            }
        }
    }
    corrupt
}

#[cfg(test)]
mod tests {
    use metor_fsw_ring::{Config, RingBuffer, Writer};

    use super::*;

    fn ring(delivery: Delivery, records: &[&[u8]]) -> (Tap, Writer<NoWake>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 4096,
            max_readers: 2,
        });
        let view = ring.view(NoWake).expect("a reader slot");
        let mut writer = ring.writer(NoWake).expect("the writer claim");
        for rec in records {
            writer.try_write(rec).expect("room in the ring");
        }
        let tap = Tap {
            view,
            delivery,
            wire: Wire::Table { packet_id: [1, 2] },
            last_committed: u64::MAX,
            retain_slot: None,
        };
        (tap, writer)
    }

    fn drained(taps: &mut [Tap]) -> (Vec<Vec<u8>>, usize) {
        let mut out = Vec::new();
        let corrupt = drain_taps(taps, |_, _, rec| out.push(rec.to_vec()));
        (out, corrupt)
    }

    /// A snapshot tap contributes its newest record once per commit and
    /// nothing on a cycle that added none.
    #[test]
    fn a_snapshot_tap_yields_once_per_change() {
        let (mut tap, mut writer) = ring(Delivery::Snapshot, &[b"one", b"two"]);
        assert_eq!(drained(std::slice::from_mut(&mut tap)).0, [b"two".to_vec()]);
        assert!(drained(std::slice::from_mut(&mut tap)).0.is_empty());

        writer.try_write(b"three").unwrap();
        assert_eq!(
            drained(std::slice::from_mut(&mut tap)).0,
            [b"three".to_vec()]
        );
    }

    /// A log tap contributes every pending record in commit order.
    #[test]
    fn a_log_tap_yields_every_record_in_order() {
        let (mut tap, _writer) = ring(Delivery::Log, &[b"one", b"two", b"three"]);
        let (records, corrupt) = drained(std::slice::from_mut(&mut tap));
        assert_eq!(
            records,
            [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        assert_eq!(corrupt, 0);
        assert!(drained(std::slice::from_mut(&mut tap)).0.is_empty());
    }

    /// A record whose length word was corrupted after publication is counted,
    /// not framed.
    #[test]
    fn a_corrupt_record_counts() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 256,
            max_readers: 2,
        });
        let view = ring.view(NoWake).expect("a reader slot");
        let mut writer = ring.writer(NoWake).expect("the writer claim");
        writer.try_write(&[1; 8]).expect("room in the ring");

        // `RegionHeader::data_offset` is the u64 at byte offset 16; the first
        // record's length word sits there.
        let (base, _) = ring.region();
        let data_offset = unsafe { (base.add(16) as *const u64).read() } as usize;
        unsafe { (base.add(data_offset) as *mut u64).write(u64::MAX) };

        let mut tap = Tap {
            view,
            delivery: Delivery::Log,
            wire: Wire::Msg,
            last_committed: u64::MAX,
            retain_slot: None,
        };
        let (records, corrupt) = drained(std::slice::from_mut(&mut tap));
        assert!(records.is_empty());
        assert_eq!(corrupt, 1);
    }
}
