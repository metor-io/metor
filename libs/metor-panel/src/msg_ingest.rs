//! Shared "backfill then live-tail" reader for a single message-log type. Both the alarm
//! and sequence stores fold a control-system broadcast into an app-global gpui entity the
//! same way: replay the persisted history once, then tail the WAL for live updates. The
//! only thing that differs is the store type and how each message mutates it, so that is
//! all the caller supplies.

use std::sync::Arc;

use gpui::{AsyncApp, WeakEntity};
use metor_db::DB;
use metor_db::msg_log::read_msg;
use metor_proto::types::{PacketId, Timestamp};
use serde::Deserialize;

/// Backfill the persisted history for one message type, then live-tail its WAL into the
/// store entity `this`. `apply` folds one decoded message into the store; it runs inside an
/// entity update so it may call `cx.notify()` indirectly — the loop notifies after each
/// applied batch.
///
/// The WAL reader is created before backfill so it captures every write afterwards; live
/// messages at or before the backfilled timestamp are dropped to avoid double-counting the
/// overlap. The loop ends when the entity is dropped.
pub(crate) async fn ingest_loop<S, T, F>(
    db: Arc<DB>,
    id: PacketId,
    this: WeakEntity<S>,
    cx: &mut AsyncApp,
    mut apply: F,
) where
    S: 'static,
    T: for<'de> Deserialize<'de> + 'static,
    F: FnMut(&mut S, Timestamp, T) + 'static,
{
    let Ok(msg_log) = db.with_state_mut(|s| s.get_or_insert_msg_log(id, &db.path).cloned()) else {
        return;
    };

    let mut reader = msg_log.wal_reader();

    let backfill: Vec<(Timestamp, T)> =
        match msg_log.get_range(Timestamp(i64::MIN)..Timestamp(i64::MAX)) {
            Some(slice) => slice
                .as_iter()
                .flat_map(|node| {
                    node.msgs()
                        .filter_map(|(ts, bytes)| {
                            postcard::from_bytes::<T>(bytes).ok().map(|v| (ts, v))
                        })
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

    let apply_ref = &mut apply;
    let applied = this.update(cx, move |store, cx| {
        for (ts, value) in backfill {
            apply_ref(store, ts, value);
        }
        cx.notify();
    });
    if applied.is_err() {
        return;
    }

    loop {
        let grant = reader.next().await;
        let mut items: Vec<(Timestamp, T)> = Vec::new();
        let mut buf: &[u8] = &grant;
        while let Some((rest, ts, msg)) = read_msg(buf) {
            buf = rest;
            if ts > backfill_max
                && let Ok(value) = postcard::from_bytes::<T>(msg)
            {
                items.push((ts, value));
            }
        }

        let apply_ref = &mut apply;
        let applied = this.update(cx, move |store, cx| {
            if items.is_empty() {
                return;
            }
            for (ts, value) in items {
                apply_ref(store, ts, value);
            }
            cx.notify();
        });
        if applied.is_err() {
            break;
        }
    }
}
