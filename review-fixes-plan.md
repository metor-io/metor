# Review-fix plan: cc4d46a3..HEAD findings

Status of the full finding list from the review:

**Already applied (mechanical, committed to working tree):**
- cube-sat: restored real-time pacing sleep (leftover 5µs debug sleep).
- `measurements.rs` / `msg_ingest.rs` / `GetMsgs` handler: reverse the
  newest-first `as_iter` node order so samples/messages are chronological
  (`limit` in GetMsgs now keeps the oldest-first prefix).
- `metor-proto` `Timestamp`↔`Epoch`: exact integer-nanosecond conversions
  (was f64 unix-milliseconds, ~1µs drift per layout save/load). Round-trip
  test hardened with sub-millisecond endpoints.
- `PeerStore::lock_connected`: dial + `GetDbInfo` handshake bounded by a
  5s timeout (same policy as `remote::db::handshake_request`) so a
  never-replying peer can't wedge the client mutex forever.
- `lod.rs`: both bare waits (discovery loop on `vtable_gen`, period
  estimation on `seal_waiter`) now race `IDLE_RECHECK`, closing the two
  lost-wakeup stalls.

**Below: the six design-heavy fixes, for review before implementation.**

---

## 1. `get_range` two-snapshot race → slices covering the whole archive

`TimeSeries::get_range` calls `binary_search_nearest` twice; each call
takes its own read-locked iterator over `list`. A `purge_span` /
`install_node` splice between the two calls rebuilds the Arc shells of
every newer node, so `start` and `end` resolve on *different spines*.
`TimeSeriesSlice::as_iter` terminates only on `Arc::ptr_eq(node,
start.node)` — across spines the sentinel never matches and iteration
walks to the oldest node.

**Fix (two layers):**
1. **One snapshot per query.** Capture the head once (`list.head()` under
   the read lock) and run both nearest-searches over iterators seeded from
   that same captured head. Refactor `binary_search_nearest` into a
   private `binary_search_nearest_from(head, ts, inclusive)`; the public
   method and `get_range` both call it. Both `TimestampRef`s then live on
   one immutable spine, and `as_iter` termination is correct by
   construction. (Nodes and `prev` links are immutable once published, so
   a captured spine is a consistent snapshot.)
2. **Belt-and-braces bound in `as_iter`.** Stop iteration when a node's
   newest timestamp is older than `start.node`'s oldest — even a misused
   slice can then only over-read by one node, never the archive.

Apply the same one-snapshot change to `MsgLog::get_range`
(`msg_log_2.rs` has the identical two-search shape; msg-log nodes aren't
spliced today, but the fix is symmetrical and cheap).

## 2. `install_node` newer-than-head guard keyed on `has_writer`

The lazy writer acquisition in `Component::persist` is deliberate (an
archive that only receives pushed nodes must accept spans above its
head), but it opens a window after reopen — before the first live
sample — where a peer span newer than the local head installs above it,
and the next live write breaks the newest-first node order.

**Fix: derive the guard from state, not from the writer flag.** Reject a
span above the head when **the head is unsealed** (its `start_ts` has no
manifest span). Rationale:
- Live-ingest component: the head is the writer's working node — never in
  the manifest, even across restarts (sealing skips the head). Installs
  above it are rejected in exactly the window the current code misses.
- Archive component: every node arrived via `install_node`, which
  publishes a Resident manifest span — the head is always sealed, so
  pushes above it keep landing.
The `has_writer` check stays as an additional condition (`unsealed head
|| has_writer`), since a live writer with an empty list-head edge case
should still reject.

## 3. LoD buckets permanently lost to offload (skip-forever + TOCTOU)

`run_pass` snapshots the manifest, folds buckets via `get_range`, skips
buckets overlapping non-Resident spans, and advances `cursor`
unconditionally. Two holes: a span purged *between* snapshot and scan
folds a bucket from partial data; and buckets over offloaded spans are
skipped forever even though the bytes are rehydratable.

**Fix: order the pipeline — tiering must not purge what LoD hasn't
summarized.**
- `lod.rs` exposes `summarized_frontier(db, source_id) -> Option<i64>`:
  the min cursor across that source's levels (levels already persist
  their frontier as their own latest sample). `None` when the source has
  no levels (not eligible, or engine not running).
- `tiering::collect_victims` skips a span when the source's frontier
  exists and is `< span.cover_end` — an unsummarized span is simply not
  cold yet. Sources without levels are unaffected (mirrors, archives,
  ineligible components keep today's behavior).
- With purges gated, a mid-pass purge of an in-window span can no longer
  happen on the system of record. Keep the hole-skip logic for
  pre-existing RemoteOnly history (seeded mirrors), but downgrade it from
  silent to a `debug!` log, and re-check the manifest generation after
  the fold: if it changed, return without emitting or advancing — the
  next pass re-runs over consistent state.

## 4. `NodeBoundsCache` keyed by recyclable Arc address

Splices free node shells and allocate fresh same-sized ones, so
`Arc::as_ptr` collides across frames and one node inherits another's
cached min/max (wrong Y auto-fit, no rescan).

**Fix:** key by the node's first sample timestamp (`timestamps()[0]`) —
already the node's identity everywhere else (directory name, manifest
span key, `begin_fetch`). Survives shell rebuilds (the rebuilt shell
shares the same inner node value), unique per component by construction.
Nodes with zero samples are skipped (they cache nothing today anyway).
`NodeBoundsCache` becomes `HashMap<i64, NodeBounds>`; the `live` retain
set follows. Touches `time_series/mod.rs` + the two `NodeBoundsCache`
holders (xy_plot, time_series line_plot) — type alias only.

## 5. Hydration retry storm on deterministically-failing spans

`hydrate_span` failure → `abort_fetch` → span back to RemoteOnly → panel
re-requests the same gap next frame → full re-download, forever.

**Fix: per-span backoff memo in `Hydrator`.**
`HashMap<(ComponentId, Timestamp), {attempts: u32, retry_at: Instant}>`
inside `HydratorInner`. `serve` skips a gap whose `retry_at` is in the
future; on failure it records exponential backoff (1s doubling, capped
at 60s, never giving up permanently — a rejected span can become
installable after state changes); on success it clears the entry. Memo
is bounded by pruning entries older than the cap whenever it exceeds a
small limit (e.g. 256). No panel-side changes — gap_bands stays a dumb
per-frame requester.

## 6. Writer-ownership race between persist and the LoD engine

`persist` (lazy claim on first WAL grant) races `setup_levels` (claims
the writer to emit buckets). Either loser degrades silently: a dead LoD
level for the session, or WAL telemetry dropped with a warn.

**Fix: decide ownership at construction.** `Component::create/open` gain
knowledge of whether the component is engine-owned — keyed off the LoD
metadata/name the DB already consults (`is_lod_name` today; the metadata
key once #2-adjacent naming cleanup lands). For engine-owned components,
`persist` is spawned in *drain mode*: it never claims the writer and
warn-drops any WAL grant (preserving the "bogus traffic" diagnostic)
— so `setup_levels` can never lose the claim. For normal components the
lazy claim stays (it is what archives need); a failed claim there still
warns but now indicates a real bug rather than a routine race.

---

## Also in scope (small, after the above)

- `estimate_samples`: count every resident list node lacking a manifest
  span (today: head only), so rolled-but-unsealed nodes stop skewing the
  panel's raw/LoD switchover.
- `OfferNode` handler: use the `component_name` already on the wire
  instead of the id-string fallback when auto-creating components.
  (Shipping LoD metadata over the wire needs a protocol bump — noted as
  follow-up, not done here.)

## Explicitly deferred (efficiency/cleanup, separate pass)

`plan_min_max_trace` per-frame full-bucket scan; `effective_view`
computed 3× per render; `seed_loop` sequential 60s re-list; serial
`Hydrator` fetches; `coverage()` dead enum; `read_f64` duplication;
`resolve_lod_levels` shared resolver; `RAW_SAMPLE_BUDGET` derivation;
`flush_fold` param struct + dead `component_id`; `LocalDirStore`
blocking fsync; `seed_manifests` last_updated inflation; system-of-record
role signal.
