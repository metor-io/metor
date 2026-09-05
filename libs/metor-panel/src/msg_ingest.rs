//! Shared "backfill then live-tail" ingestion for the message-log broadcasts behind
//! the alarm and sequence stores. Both stores fold several message types into one
//! app-global gpui entity, and the folds are order-sensitive *across* types — a
//! clear for a raise the store never saw is dropped, as is an event for an
//! undeclared channel. Replaying each log to completion in turn would misorder any
//! history that interleaves across logs (a clear folding before its raise), so the
//! history and each live batch merge in timestamp order across every source.

use std::cmp::Ordering;
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::Poll;

use gpui::{AsyncApp, WeakEntity};
use metor_db::DB;
use metor_db::disruptor::Reader;
use metor_db::msg_log::read_msg;
use metor_proto::types::{PacketId, Timestamp};
use serde::Deserialize;

/// One message type feeding a store: which log to read and how a decoded message
/// mutates the store. The message type is erased behind a bytes-in closure so a
/// store's sources can be merged into one timestamp-ordered backfill.
pub(crate) struct IngestSource<S> {
    id: PacketId,
    apply: Box<dyn FnMut(&mut S, Timestamp, &[u8]) + 'static>,
}

impl<S> IngestSource<S> {
    pub(crate) fn new<T, F>(id: PacketId, mut apply: F) -> Self
    where
        T: for<'de> Deserialize<'de> + 'static,
        F: FnMut(&mut S, Timestamp, T) + 'static,
    {
        Self {
            id,
            apply: Box::new(move |store, ts, bytes| {
                if let Ok(value) = postcard::from_bytes::<T>(bytes) {
                    apply(store, ts, value);
                }
            }),
        }
    }

    /// A source that folds the raw record bytes without a static decode step,
    /// for stores that decode dynamically (an unknown message id resolved
    /// against a schema announced at runtime).
    pub(crate) fn new_raw<F>(id: PacketId, apply: F) -> Self
    where
        F: FnMut(&mut S, Timestamp, &[u8]) + 'static,
    {
        Self {
            id,
            apply: Box::new(apply),
        }
    }
}

/// Backfill the persisted history of every source as one timestamp-sorted merge,
/// then merge available WAL messages into the store entity `this`. The store is
/// notified once after the backfill and after each applied live batch. The loop
/// ends when the entity is dropped.
///
/// Each WAL reader is created before its log's snapshot so no write can fall
/// between them; live messages that merely replay the snapshot overlap are
/// dropped (see [`replays_snapshot`]).
pub(crate) async fn ingest_all<S: 'static>(
    db: Arc<DB>,
    sources: Vec<IngestSource<S>>,
    this: WeakEntity<S>,
    cx: &mut AsyncApp,
) {
    let mut kept: Vec<IngestSource<S>> = Vec::new();
    let mut tails = Vec::new();
    let mut entries: Vec<(Timestamp, usize, Vec<u8>)> = Vec::new();

    for source in sources {
        let Ok(msg_log) =
            db.with_state_mut(|s| s.get_or_insert_msg_log(source.id, &db.path).cloned())
        else {
            continue;
        };
        let reader = msg_log.wal_reader();

        // Node order is newest-first; reverse so entries at equal timestamps keep
        // their within-log chronological order through the stable merge sort.
        let backfill: Vec<(Timestamp, Vec<u8>)> =
            match msg_log.get_range(Timestamp(i64::MIN)..Timestamp(i64::MAX)) {
                Some(slice) => slice
                    .as_iter()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .flat_map(|node| {
                        node.msgs()
                            .map(|(ts, bytes)| (ts, bytes.to_vec()))
                            .collect::<Vec<_>>()
                    })
                    .collect(),
                None => Vec::new(),
            };

        let mut backfill_max = Timestamp(i64::MIN);
        for (ts, _) in &backfill {
            if *ts > backfill_max {
                backfill_max = *ts;
            }
        }
        let boundary: Vec<Vec<u8>> = backfill
            .iter()
            .filter(|(ts, _)| *ts == backfill_max)
            .map(|(_, bytes)| bytes.clone())
            .collect();

        let idx = kept.len();
        entries.extend(backfill.into_iter().map(|(ts, bytes)| (ts, idx, bytes)));
        kept.push(source);
        tails.push(LiveSource {
            reader,
            backfill_max,
            boundary,
        });
    }

    // Include writes made while snapshots were being read before folding either
    // history or live data. The DB lock makes draining a coherent ingress cut.
    drain_sources(&db, &mut tails, &mut entries);
    loop {
        if !entries.is_empty() {
            let batch = std::mem::take(&mut entries);
            if this
                .update(cx, |store, cx| {
                    apply_backfill(store, &mut kept, batch);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        }
        if tails.is_empty() {
            return;
        }
        entries = wait_for_source(&mut tails).await;
        drain_sources(&db, &mut tails, &mut entries);
    }
}

struct LiveSource {
    reader: Reader,
    backfill_max: Timestamp,
    boundary: Vec<Vec<u8>>,
}

type Entry = (Timestamp, usize, Vec<u8>);

fn collect_messages(
    index: usize,
    mut buf: &[u8],
    backfill_max: Timestamp,
    boundary: &mut Vec<Vec<u8>>,
    entries: &mut Vec<Entry>,
) {
    while let Some((rest, ts, msg)) = read_msg(buf) {
        buf = rest;
        if !replays_snapshot(ts, msg, backfill_max, boundary) {
            entries.push((ts, index, msg.to_vec()));
        }
    }
}

fn drain_sources(db: &DB, tails: &mut [LiveSource], entries: &mut Vec<Entry>) {
    // DB::push_msg holds the state lock. Excluding writers across the complete
    // scan prevents a later log's effect being read before an earlier log's cause.
    db.with_state_mut(|_| {
        for (index, source) in tails.iter_mut().enumerate() {
            while let Some(grant) = source.reader.try_next() {
                collect_messages(
                    index,
                    &grant,
                    source.backfill_max,
                    &mut source.boundary,
                    entries,
                );
            }
        }
    });
}

async fn wait_for_source(tails: &mut [LiveSource]) -> Vec<Entry> {
    let mut waits: Vec<_> = tails
        .iter_mut()
        .enumerate()
        .map(|(index, source)| {
            Box::pin(async move {
                let grant = source.reader.next().await;
                let mut entries = Vec::new();
                collect_messages(
                    index,
                    &grant,
                    source.backfill_max,
                    &mut source.boundary,
                    &mut entries,
                );
                entries
            })
        })
        .collect();
    poll_fn(|cx| {
        for wait in &mut waits {
            if let Poll::Ready(entries) = wait.as_mut().poll(cx) {
                return Poll::Ready(entries);
            }
        }
        Poll::Pending
    })
    .await
}

/// Fold a history or live batch into the store in timestamp order. Entries are
/// `(timestamp, source index, payload)`; equal timestamps fold in source
/// declaration order, so callers list sources cause before effect (defs before
/// raises, the registry before its events).
pub(crate) fn apply_backfill<S>(
    store: &mut S,
    sources: &mut [IngestSource<S>],
    mut entries: Vec<(Timestamp, usize, Vec<u8>)>,
) {
    entries.sort_by_key(|&(ts, idx, _)| (ts, idx));
    for (ts, idx, bytes) in entries {
        (sources[idx].apply)(store, ts, &bytes);
    }
}

/// Whether a live WAL message is a replay of the backfilled snapshot. Messages
/// strictly newer than the snapshot always apply; strictly older ones were
/// already persisted (the WAL persists in order) and are dropped. On the
/// boundary timestamp the snapshot may be incomplete — a burst can share one
/// timestamp with only its prefix persisted when the snapshot was taken — so
/// payload identity decides: each persisted boundary payload absorbs one
/// matching replay, and anything unmatched applies.
fn replays_snapshot(
    ts: Timestamp,
    msg: &[u8],
    backfill_max: Timestamp,
    boundary: &mut Vec<Vec<u8>>,
) -> bool {
    match ts.cmp(&backfill_max) {
        Ordering::Greater => false,
        Ordering::Less => true,
        Ordering::Equal => match boundary.iter().position(|persisted| persisted == msg) {
            Some(i) => {
                boundary.swap_remove(i);
                true
            }
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_overlap_absorbs_only_persisted_boundary_occurrences() {
        let mut boundary = vec![b"a".to_vec(), b"a".to_vec()];
        assert!(replays_snapshot(
            Timestamp(9),
            b"old",
            Timestamp(10),
            &mut boundary
        ));
        assert!(replays_snapshot(
            Timestamp(10),
            b"a",
            Timestamp(10),
            &mut boundary
        ));
        assert!(!replays_snapshot(
            Timestamp(10),
            b"b",
            Timestamp(10),
            &mut boundary
        ));
        assert!(replays_snapshot(
            Timestamp(10),
            b"a",
            Timestamp(10),
            &mut boundary
        ));
        assert!(!replays_snapshot(
            Timestamp(10),
            b"a",
            Timestamp(10),
            &mut boundary
        ));
        assert!(!replays_snapshot(
            Timestamp(11),
            b"new",
            Timestamp(10),
            &mut boundary
        ));
    }
}
