//! Eviction for the panel's session DB.
//!
//! A mirror is a cache, not an archive: what a peer can serve again is kept
//! only while someone reads it. Every per-frame read path marks the history
//! it reads with [`TimeSeries::touch`](metor_db::time_series::TimeSeries::touch)
//! — plots for the series they draw (raw, or the selected LoD level), value
//! views for their selected instant, playback for its read-ahead — and a
//! task here purges, about once a second, every evictable span nobody has
//! touched within the grace period. A plot that zooms out switches to an
//! LoD level, stops touching the raw nodes, and they go a few seconds
//! later; zooming back fetches them again.
//!
//! Under [`Eviction::Budget`] the same task instead keeps the most recently
//! touched history up to a byte budget, for a peer cache reopened day after
//! day, where what was read yesterday is worth the disk it takes.
//!
//! What may go is [`metor_db::evict`]'s rule: acked spans (hydrated from a
//! peer) and mirrored spans the peer's seals cover, never the live head or
//! locally produced data. On top of that this module adds the panel's
//! policy:
//!
//! - **Only a temporary session or a peer cache evicts.** A recording, or
//!   a database the embedding application owns, exists to keep what
//!   arrived.
//! - **Paused unless every connection is up, and at least one can serve
//!   history.** A peer that dropped may hold the only other copy, or none:
//!   an ephemeral gateway deletes its DB when it stops. Spans aren't
//!   tracked per connection, so one connection down pauses eviction for
//!   all of them; and data from a peer that was disconnected *and removed*
//!   is not protected once the remaining connections are up.
//! - **The coarsest LoD level of every source stays.** It is what a
//!   zoomed-out view reads and is tiny (a month at 4096× is ~10 MB per
//!   component), so refetching it on every zoom-out would cost more than
//!   keeping it.
//! - **Views that aren't repainting still count as reading.** gpui only
//!   renders on change, so a paused plot stops touching what it shows.
//!   Before purging, the task repaints every window and has every value
//!   view touch its selected instant, then purges only what is still idle.
//!   A window the platform doesn't draw (minimized, occluded) can lose its
//!   history this way and fetches it again when shown.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AsyncApp};
use metor_db::DB;
use metor_db::evict::{self, Evicted};
use metor_db::lod::{LOD_RATIO_KEY, LOD_SOURCE_ID_KEY};
use metor_proto::types::ComponentId;

use crate::connections::ConnectionStatus;

pub use metor_db::evict::Eviction;

/// How often the evictor looks for idle history.
const TICK: Duration = Duration::from_secs(1);
/// How long the pre-purge repaint gets to land, so views that are on screen
/// but idle have touched what they show.
const SETTLE: Duration = Duration::from_millis(250);

/// Start the evictor for `db` under `eviction`. Runs until the app quits.
pub(crate) fn spawn(db: Arc<DB>, eviction: Eviction, cx: &mut App) {
    if eviction == Eviction::Off || !evict::SUPPORTED {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            tick(&db, eviction, cx).await;
        }
    })
    .detach();
}

/// One pass: see whether anything would go, let every view on screen claim
/// what it shows, then purge.
async fn tick(db: &Arc<DB>, eviction: Eviction, cx: &mut AsyncApp) {
    if !cx.update(|cx| peers_reachable(cx)) {
        return;
    }
    let due = {
        let db = db.clone();
        cx.background_executor()
            .spawn(async move { due(&db, eviction, Instant::now()) })
            .await
    };
    if !due {
        return;
    }
    cx.update(|cx| {
        crate::temporal::samples::touch_selected(Instant::now(), cx);
        cx.refresh_windows();
    });
    cx.background_executor().timer(SETTLE).await;
    if !cx.update(|cx| peers_reachable(cx)) {
        return;
    }
    let db = db.clone();
    let evicted = cx
        .background_executor()
        .spawn(async move { purge(&db, eviction, Instant::now()) })
        .await;
    if evicted.spans > 0 {
        tracing::debug!(
            spans = evicted.spans,
            bytes = evicted.bytes,
            "evicted idle history"
        );
    }
}

/// Whether a pass under `eviction` would find anything to purge.
fn due(db: &DB, eviction: Eviction, now: Instant) -> bool {
    let kept = coarsest_lod_levels(db);
    let evictable = |id, _: &_| !kept.contains(&id);
    match eviction {
        Eviction::WorkingSet { grace } => evict::any_idle(db, now, grace, evictable),
        Eviction::Budget { max_bytes } => evict::over_budget(db, max_bytes, evictable),
        Eviction::Off => false,
    }
}

/// Purge what `eviction` lets go: under a working set everything idle past
/// its grace period; under a budget the least recently read history until
/// the rest fits, sparing what was read within the default grace period.
/// Each source's coarsest LoD level never goes and never counts toward a
/// budget.
fn purge(db: &DB, eviction: Eviction, now: Instant) -> Evicted {
    let kept = coarsest_lod_levels(db);
    let evictable = |id, _: &_| !kept.contains(&id);
    match eviction {
        Eviction::WorkingSet { grace } => evict::sweep(db, now, grace, evictable),
        Eviction::Budget { max_bytes } => {
            evict::budget(db, now, max_bytes, evict::DEFAULT_GRACE, evictable)
        }
        Eviction::Off => Evicted::default(),
    }
}

fn peers_reachable(cx: &App) -> bool {
    crate::connections::try_global(cx).is_some_and(|store| {
        peers_can_refetch(
            store
                .read(cx)
                .active()
                .iter()
                .filter(|conn| !conn.is_refused())
                .map(|conn| (conn.status(), conn.fetches_history())),
        )
    })
}

/// Whether history evicted now could be fetched back: at least one
/// connection serves history, and none is anything but connected.
fn peers_can_refetch(connections: impl IntoIterator<Item = (ConnectionStatus, bool)>) -> bool {
    let mut any_fetches = false;
    for (status, fetches) in connections {
        if status != ConnectionStatus::Connected {
            return false;
        }
        any_fetches |= fetches;
    }
    any_fetches
}

/// Each LoD source's coarsest level. Levels are recognised by metadata, so
/// they are found however their source is named.
fn coarsest_lod_levels(db: &DB) -> HashSet<ComponentId> {
    db.with_state(|state| {
        let mut coarsest: HashMap<&str, (ComponentId, u64)> = HashMap::new();
        for (id, meta) in state.component_metadata_iter() {
            let (Some(source), Some(ratio)) = (
                meta.metadata.get(LOD_SOURCE_ID_KEY),
                meta.metadata
                    .get(LOD_RATIO_KEY)
                    .and_then(|r| r.parse::<u64>().ok()),
            ) else {
                continue;
            };
            let entry = coarsest.entry(source.as_str()).or_insert((*id, ratio));
            if ratio > entry.1 {
                *entry = (*id, ratio);
            }
        }
        coarsest.into_values().map(|(id, _)| id).collect()
    })
}

#[cfg(test)]
mod tests;
