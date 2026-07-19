//! Shared "backfill then live-tail" ingestion for the message-log broadcasts behind
//! the alarm and sequence stores. Both stores fold several message types into one
//! app-global gpui entity, and the folds are order-sensitive *across* types — a
//! clear for a raise the store never saw is dropped, as is an event for an
//! undeclared channel. Replaying each log to completion in turn would misorder any
//! history that interleaves across logs (a clear folding before its raise), so the
//! persisted history replays as one timestamp-sorted merge across every source;
//! only the live tail runs per log.

use std::cmp::Ordering;
use std::sync::Arc;

use gpui::{AsyncApp, Task, WeakEntity};
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
/// then live-tail each source's WAL into the store entity `this`. The store is
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
    let mut tails: Vec<(Reader, Timestamp, Vec<Vec<u8>>)> = Vec::new();
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
        tails.push((reader, backfill_max, boundary));
    }

    let kept_ref = &mut kept;
    let applied = this.update(cx, move |store, cx| {
        apply_backfill(store, kept_ref, entries);
        cx.notify();
    });
    if applied.is_err() {
        return;
    }

    let tasks: Vec<Task<()>> = kept
        .into_iter()
        .zip(tails)
        .map(|(source, (reader, backfill_max, boundary))| {
            let this = this.clone();
            cx.spawn(async move |cx| {
                tail_source(source, reader, backfill_max, boundary, this, cx).await
            })
        })
        .collect();
    for task in tasks {
        task.await;
    }
}

/// Fold a multi-log backfill into the store in timestamp order. Entries are
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

async fn tail_source<S: 'static>(
    mut source: IngestSource<S>,
    mut reader: Reader,
    backfill_max: Timestamp,
    mut boundary: Vec<Vec<u8>>,
    this: WeakEntity<S>,
    cx: &mut AsyncApp,
) {
    loop {
        let grant = reader.next().await;
        let mut msgs: Vec<(Timestamp, &[u8])> = Vec::new();
        let mut buf: &[u8] = &grant;
        while let Some((rest, ts, msg)) = read_msg(buf) {
            buf = rest;
            if !replays_snapshot(ts, msg, backfill_max, &mut boundary) {
                msgs.push((ts, msg));
            }
        }
        if msgs.is_empty() {
            continue;
        }

        let apply = &mut source.apply;
        let applied = this.update(cx, move |store, cx| {
            for (ts, msg) in msgs {
                apply(store, ts, msg);
            }
            cx.notify();
        });
        if applied.is_err() {
            break;
        }
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
