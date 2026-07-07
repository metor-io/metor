# Render-path performance pass

## Goal

Cut per-frame allocation and GPU churn on the panel's hot paths so many live
tiles stay at 60 fps and interactive tile-resize drags stop thrashing the
allocator and the wgpu device. Every change is a mechanical hot-path
optimization: no visual or behavioral change, no new public surface beyond the
small pools/memos introduced. Correctness parity is the bar — each step ships
with the existing tests still green plus a targeted test where the invariant is
new.

The seven items below were each re-verified against the current source (post the
`gpu.rs` index-buffer removal / timestamp-unit fix). None are moot; two have
shifted enough to restate. They are ordered by estimated impact and grouped into
independently-landable steps.

## Current state

### 1. `gpu.rs` readback allocates a fresh `Vec<u8>` per frame per plot — HIGH, always on

`read_mapped_bytes` (`src/views/time_series/gpu.rs:830`) does
`Vec::with_capacity(width*4 * height)` and row-copies the mapped staging buffer
into it every frame. `ReadbackHandle::read_image` (`gpu.rs:282`) then moves that
`Vec` into `ImageBuffer::from_raw` → a fresh `Arc<RenderImage>`
(`gpu.rs:292–294`). For a 1500×600 plot that is a **3.6 MB heap allocation +
3.6 MB memcpy every frame**, on the background executor, for *each* visible plot.
At 60 fps with a handful of plots this is the dominant allocator load in the
whole app.

The `RenderImage` round-trip (GPU→CPU staging→`Vec`→`RenderImage`→gpui re-uploads
to GPU for compositing) is inherent to the `canvas` + `paint_image` integration
and is **not** what we remove here. What we remove is the per-frame `Vec`
allocation: the pipeline is single-flight (`in_flight` `AtomicBool`,
`gpu.rs:762`) and retires the prior frame through
`take_pending_release` → `window.drop_image` (`line_plot.rs:808–813`), so at most
two `RenderImage`s are alive at once. Their backing `Vec`s can be recycled.

Reference pattern: `src/views/viewer_3d/bevy_app.rs:90–125` (`FrameSlot` +
`ClearSlot: Recycle` + `ThingBuf` transit queue) and `copy_tight_rows`
(`bevy_app.rs:240`). Note the bevy path only recycles the *transit* buffer, not
the final `RenderImage` — because `consume_frame` (`viewer_3d/mod.rs:749`)
`mem::take`s the bytes into the image and never reclaims them. We can do better
than the reference here (reclaim on retire), see design.

### 2. `RenderTarget` recreated wholesale on any size change — HIGH during drag

`PlotRenderState::render` (`gpu.rs:777–784`) rebuilds the entire `RenderTarget`
whenever `t.width != width || t.height != height`. `RenderTarget::new`
(`gpu.rs:213`) allocates **three GPU resources**: a 4×-MSAA color texture
(1500×600×4 samples×4 B ≈ **14.4 MB**), a resolve texture (≈3.6 MB, `COPY_SRC`),
and a `MAP_READ` staging buffer (≈3.6 MB). During an interactive tile-resize
drag the canvas bounds change most frames, so this is **~21 MB of GPU
allocation + 3 resource creations per plot per dragged frame** — exactly when
the GPU is already busy. At rest it costs nothing.

Unlike `viewer_3d`, the time-series target size comes straight from
`bounds.size * scale` rounded to integer pixels (`gpu.rs:766–767`) with **no
quantization** — every sub-pixel drag delta is a distinct size. `viewer_3d`
already solved this: `Viewer3d::quantize` (`viewer_3d/mod.rs:605`) snaps to a
64-pixel grid (which also aligns rows to `COPY_BYTES_PER_ROW_ALIGNMENT`, letting
`copy_tight_rows` collapse to one memcpy — a free bonus for item 1 here too).

### 3. `upload_pair` issues two small `write_buffer` calls per node-pair — MEDIUM

`upload_pair` (`gpu.rs:1417–1421`) does one `queue.write_buffer` for X and one
for Y **per node-pair**, at the running `cache.cursor` byte offset.
`plan_min_max_trace` (`gpu.rs:1292–1295`) and `plan_list_trace`
(`gpu.rs:1803–1806`) each add another pair of writes. A time-series trace spans
1–6 resident nodes, so a frame with a few traces issues on the order of
10–40 `write_buffer` calls, each with its own driver-side staging + validation
overhead, all targeting two contiguous storage buffers that could be filled in
one shot. Because the uploads are already sequential into `x_buf`/`y_buf` at
`cursor*4`, they can be coalesced into **one write per axis per frame**.

### 4. `submit` allocates `plans`/`spans` `Vec`s per frame — LOW/MEDIUM

`submit` (`gpu.rs:557`) does `Vec::with_capacity(traces.len())` for `plans`, and
every `plan_*` returns a `TracePlan { spans: Vec::new() }` (`gpu.rs:945`, `1164`,
`1753`) — one `Vec` allocation per trace per frame, plus `plan_min_max_trace`'s
`runs` `Vec` (`gpu.rs:1194`). `live_traces` is *already* a `SmallVec`
(`gpu.rs:620`), and `upload_x`/`upload_y`/`select_scratch` are *already* retained
scratch on `PlotGpu` (`gpu.rs:343–345`) — so this item is just extending the
existing retained-scratch discipline to `plans`/`spans`. Small absolute cost but
trivial to fix alongside items 3.

### 5. `effective_view` recomputed many times per frame with fresh `Vec`s — MEDIUM

`LinePlot::effective_view` (`line_plot.rs:412`) allocates **three heap `Vec`s**
(`vec![f64::INFINITY; axis_count]`, `vec![…]`, `vec![false; axis_count]` at
`line_plot.rs:420–422`), iterates every trace, reads each `Trace` entity, and
does a `tracking` hashmap lookup per trace. It is called from the render/prepaint
path (`line_plot.rs:738`) **and** from `update_lod_state` (`226`), `gap_bands`
(`284`), and across `mod.rs` from mouse/drag/cursor handlers and the outer
render: `mod.rs:1287, 1306, 1334, 1476, 1534, 1564, 1596, 1644`. During a single
paint of a plot with cursors/measurements this recomputes the same fit several
times. `axis_count` is 1–2 so the `Vec`s are tiny, but they are needless heap
traffic and the trace-iteration is redundant.

### 6. Inspector `filtered_indices` re-scores every row on every call — MEDIUM (inspector only)

`Inspector::filtered_indices` (`inspector/mod.rs:237`) builds a fresh
`nucleo_matcher::Matcher`, parses the `Pattern`, scores **every** row, sorts, and
returns a `Vec<usize>` — with no memoization. It is called from `visible_count`
(`277`), `render_rows_panel` (`549`), the `uniform_list` processor closure
(`573`, invoked once per visible-range recompute), and `confirm` (`349`). A
single inspector render re-scores the full row set 2–4×. The result only depends
on the current page's rows and the query string; nothing else changes it.
Invalidation points are exactly: `push_page`/`pop_page` (`193`/`215`, which
`self.search.clear()`) and search edits (`handle_key_down` → `self.search`,
`inspector/mod.rs:455`).

### 7. `data_table` grid materializes a strip entity per instance × field — MEDIUM/HIGH

`DataTableGrid::set_group` (`grid.rs:41`) eagerly builds a `RowState` for *every*
instance (`grid.rs:71–74`), and `build_row` (`grid.rs:94`) creates a live
`ComponentValueStrip` entity for every populated field (`grid.rs:114`). Each
strip spawns a task and registers a WAL reader. A 50-instance × 8-field group
therefore holds **~400 live streaming subscriptions regardless of what's on
screen**, when the viewport shows maybe 12 rows. This is the same problem
`component_table.rs` already solved with `VisibleEntityCache`
(`component_table.rs:136`, `300`; the cache itself is `src/views/lazy_pool.rs`).
Separately, `visible_row_indices` (`grid.rs:79`) allocates a `Vec` and is called
from `rows_count` (`167`) **and once per `render_cell`** (`182`) — O(rows) work
and an allocation per painted cell.

## Proposed design

### Item 1 — pooled readback buffers (reclaim on retire)

Introduce a process-wide recycle pool of `Vec<u8>` readback buffers, shareable
across the background executor. Match the house pattern with a
`thingbuf`-backed pool (as in `bevy_app.rs`), or — simpler and sufficient given
single-flight — an `Arc<Mutex<Vec<Vec<u8>>>>` of size ≤3. Store it on the
`PlotGpu` global (or a dedicated `Global`), hand a clone to each
`ReadbackHandle`.

- `read_mapped_bytes` pops a buffer from the pool (`clear()` + `reserve`) instead
  of `Vec::with_capacity`; on empty pool it allocates.
- **Reclaim on retire (better than the bevy reference):** when a frame leaves the
  pipeline via `take_pending_release`, its `Arc<RenderImage>` refcount is 1 once
  gpui's `drop_image` has released the composited texture. `Arc::try_unwrap` the
  `RenderImage`, pull the `Frame`'s `ImageBuffer::into_raw()` `Vec`, and push it
  back to the pool. This closes the loop so steady-state readback does **zero**
  heap allocation. If `try_unwrap` fails (gpui still holds it), just drop — the
  pool refills from a fresh alloc next frame; no correctness risk.
- Fold in the 64-px quantization from item 2 so the padded row equals the tight
  row and the row-by-row copy in `read_mapped_bytes` becomes a single
  `extend_from_slice` (mirror `copy_tight_rows`).

### Item 2 — quantize the target size

Add a `quantize` step in `PlotRenderState::render` before the size comparison,
copied from `Viewer3d::quantize` (`viewer_3d/mod.rs:605`): snap physical width
and height up to a 64-px grid, min 64. The target is then reallocated only when
the *quantized* size changes, so a resize drag reallocates at most once per
64 px crossed instead of every frame. The plot renders into an over-allocated
target and the extra margin is simply never sampled — the view uniform already
maps data space to `target.width/height` (`gpu.rs:611`), so cropping falls out
for free without a scissor. (An explicit scissor is the alternative if the
over-allocation ever proves visible, but it should not: the readback is sized to
the quantized extent and `paint_image` scales it to `bounds`.)

Prefer quantization over scissor: it also delivers the aligned-row memcpy win and
reuses a proven helper. Factor `quantize` into a shared free function if it's
awkward to reference across modules; otherwise duplicate the ~6 lines (per STYLE,
don't over-abstract a tiny helper).

### Item 3 + 4 — one upload per axis per frame, retained plan scratch

Restructure `submit`'s inner loop so the `plan_*` functions **append** into the
frame-level `upload_x`/`upload_y` accumulators (already on `PlotGpu`) and record
`(start, len)` spans, but do **not** call `write_buffer`. After all traces are
planned, `submit` issues exactly one `queue.write_buffer(&x_buf, 0, …)` and one
for `y_buf` covering `cache.cursor` samples. The byte offsets in `DrawSpan`
already index the storage buffers directly, so the draw loop is unchanged.

- `upload_pair`, `plan_min_max_trace`, `plan_list_trace` lose their per-call
  `write_buffer` pair and instead extend the shared accumulators; `cache.cursor`
  bookkeeping stays as the running fill position (now also the accumulator len).
- Make `plans` and per-`TracePlan` `spans` retained scratch: add
  `plans: Vec<TracePlan>` / a reusable spans arena to `PlotGpu`, `clear()` at the
  top of `submit`. Simplest concrete form: keep `TracePlan { spans: Vec<DrawSpan> }`
  but pool the outer `Vec<TracePlan>` and reuse each `spans` Vec by index
  (`plans[i].spans.clear()`), or flatten to a single `Vec<DrawSpan>` +
  per-trace ranges. Recommend the flattened form — it removes the nested Vecs
  entirely and pairs naturally with the accumulator rewrite.

These two items share the same code region and land together.

### Item 5 — compute `effective_view` once, thread it; `SmallVec` internals

Two independent wins:

1. Replace the three `vec![…; axis_count]` with `SmallVec<[_; 2]>` (axis_count is
   1–2), per the crate's allocation convention. Trivial, local to
   `effective_view`.
2. Kill redundant recomputation. Prefer **compute-once-and-thread**: in
   `LinePlot::render` compute `effective_view` a single time and pass the
   `PlotView` into `update_lod_state`, `gap_bands`, and the canvas closure rather
   than each re-deriving it. For the `mod.rs` mouse/drag/cursor handlers, add a
   frame-scoped memo on `LinePlot`: an `Option<(Generation, PlotView)>` cached
   value invalidated on `cx.notify()` (bump a generation counter in the same
   spots that already notify, or clear the memo in `bind_traces`/
   `set_view_override`/tracking updates). The compute-once path covers the
   render-frame cost; the memo covers out-of-render handler calls. If threading
   proves invasive, ship (1) + the memo alone — the memo subsumes most of the
   benefit.

### Item 6 — memoize `filtered_indices` on (page-generation, query)

Add a page-generation counter to `Inspector`, bumped in `push_page` and
`pop_page`. Add a cached field, e.g.
`filter_cache: RefCell<Option<(u64, SharedString, Rc<[usize]>)>>` (RefCell so the
`&self` signature of `filtered_indices` is preserved; the value is small).
`filtered_indices` compares `(generation, search.text)` against the cache and
recomputes only on miss, returning a cheap clone of the cached slice. Search
edits change `search.text`, so the key naturally invalidates without an explicit
hook; `push_page`/`pop_page` change the generation. This turns the 2–4 rescans
per render into one.

Keep the return type ergonomic — callers currently take `Vec<usize>`; switch them
to a shared slice (`Rc<[usize]>` or return `&[usize]` from a `&mut` variant). The
`Rc<[usize]>` clone is the least invasive.

### Item 7 — lazy strip materialization + drop per-cell `visible_row_indices`

Adopt `VisibleEntityCache<ComponentValueStrip>` (`src/views/lazy_pool.rs`, keyed
by `ComponentId`) in `DataTableGrid`, mirroring `component_table.rs`:

- `set_group` stops calling `build_row` eagerly; it keeps the `Group` shape and
  the per-field metadata (`element_counts`, needed by `columns`) but does **not**
  spawn strips. Compute `element_counts` from `resolve_metadata` without creating
  entities.
- `render_cell` calls `cache.get_or_create(component_id, || ComponentValueStrip::new(…))`
  for the cell it's painting, so only on-screen strips hold subscriptions. Call
  `cache.prune()` once after the paint pass (component_table does this at the end
  of its render). Cap sized above the visible row count, as in
  `component_table.rs` (`ROW_CACHE_CAP`).
- Cache `visible_row_indices` on the struct (recompute in `set_group` and when
  `filter` changes) instead of recomputing + allocating in every `render_cell`
  and `rows_count`. Store as `SmallVec` or a plain field refreshed on filter
  change.

Note the keying subtlety: a strip is keyed by `ComponentId`, but `render_cell`
also needs the per-cell `click`/`full_name` and calls `set_behavior` each frame
(`grid.rs:207–208`) — that per-frame behavior refresh stays, only the entity
creation becomes lazy/cached.

## Step-by-step migration

Each step compiles and passes tests on its own; steps are independently
landable and ordered by impact.

### Step A — quantize the plot target size (item 2)
Files: `src/views/time_series/gpu.rs` (`PlotRenderState::render`,
`read_mapped_bytes`).
Add `quantize` before the size compare; reallocate `RenderTarget` only on
quantized-size change. Because rows are now 256-B aligned, simplify
`read_mapped_bytes`'s row loop to a single `extend_from_slice` when
`padded == tight`. Highest impact-per-line: kills the resize-drag GPU thrash and
prepares item 1. No new deps.

### Step B — pooled + reclaimed readback buffers (item 1)
Files: `src/views/time_series/gpu.rs` (add pool to `PlotGpu`/a `Global`,
thread a handle into `ReadbackHandle`, reclaim in `set_frame`/
`take_pending_release`), `src/views/time_series/line_plot.rs` (retire path at
`808–813` returns the reclaimed buffer to the pool). `thingbuf` is already a
workspace dep (used by `viewer_3d`); reuse it or a small `Arc<Mutex<…>>`.
Lands on top of A so the copy is already a single memcpy. Add a test asserting
steady-state readback reuses a buffer (pool non-empty after N frames).

### Step C — single upload per axis + retained plan scratch (items 3, 4)
Files: `src/views/time_series/gpu.rs` only (`submit`, `plan_trace`,
`plan_min_max_trace`, `plan_list_trace`, `upload_pair`, and the `PlotGpu` struct
for the added scratch). Rewrite `plan_*` to append into shared accumulators and
emit spans; `submit` does the two writes. Self-contained; the existing
`long_range_renders_at_every_zoom` and `minmax_selection_keeps_impulses` tests
plus a new "one write_buffer per axis" assertion (via a counting wrapper or by
checking `cursor` == accumulator len) guard it.

### Step D — inspector filtered-indices memo (item 6)
Files: `src/inspector/mod.rs`. Add generation counter + `filter_cache`; bump in
`push_page`/`pop_page`; switch callers to the cached slice. Self-contained;
add a unit test that scoring runs once across repeated `filtered_indices` calls
with an unchanged query (e.g. via a call counter behind the cache).

### Step E — data_table lazy strips + cached visible indices (item 7)
Files: `src/views/data_table/grid.rs`. Introduce
`VisibleEntityCache<ComponentValueStrip>`, make `set_group` non-materializing,
`render_cell` create-on-demand + `prune` after paint, cache `visible_row_indices`.
Reference `src/views/component_table.rs` for the exact cache/prune shape.
Self-contained.

### Step F — effective_view compute-once + memo + SmallVec (item 5)
Files: `src/views/time_series/line_plot.rs` (SmallVec internals, memo field,
thread the value through `render`/`update_lod_state`/`gap_bands`),
`src/views/time_series/mod.rs` (handlers read the memo instead of recomputing).
Lowest impact, most call sites touched — land last so the memo-invalidation
plumbing doesn't churn earlier steps. Ships in two sub-commits if desired:
(F1) SmallVec + compute-once inside `line_plot.rs`; (F2) the cross-module memo.

## Risks and how to test

- **Item 1 reclaim (`Arc::try_unwrap`):** if gpui still references the
  `RenderImage`, `try_unwrap` fails and we must fall back to a fresh alloc — never
  reuse a buffer gpui might still be compositing (would corrupt on-screen pixels).
  Test: run several plots for many frames under a debug assertion that a reclaimed
  buffer's `Arc` strong count was exactly 1; verify no visual corruption by eye in
  `cargo run -p metor-panel` with a busy dashboard.
- **Item 2 quantization:** over-allocation must not leak into the sampled image —
  the readback is sized to the quantized extent and `paint_image` rescales to
  `bounds`, so aspect can shift by up to 63 px at tile edges. Verify plots stay
  crisp at odd sizes and during a slow resize drag (no smearing, no reallocation
  storm — watch for GPU-alloc log spam). The existing
  `long_range_renders_at_every_zoom` test uses a fixed 1500×600 target and is
  unaffected.
- **Item 3/4 single upload:** the accumulator offsets must exactly match the
  `DrawSpan` instance ranges or traces render garbage/overlap. Guard with the
  existing decimation tests plus a new assertion that `cache.cursor` equals
  `upload_x.len()` after planning and that spans stay within `[0, cursor)`.
- **Item 5 memo staleness:** a memo that fails to invalidate would freeze the
  view during pan/zoom (view_override changes) or when new data extends the range.
  Invalidate on every `cx.notify()`-adjacent mutation (`bind_traces`,
  `set_view_override`, tracking updates). Test: pan/zoom interactively and confirm
  the plot follows; unit-test that changing `view_override` changes
  `effective_view`.
- **Item 6 memo:** stale results would show wrong filtered rows after typing or
  navigating pages. The key `(generation, query)` covers both; add a test that
  edits the query and asserts the returned indices change, and that a page
  push/pop with the same query string still recomputes (generation differs).
- **Item 7 lazy strips:** a cache cap below the visible-row count would thrash
  (create/drop every frame). Size the cap above the max visible rows, as
  `component_table.rs` does. Test: scroll a large group and confirm strips update
  live and the alive-entity count tracks the viewport (not the group size);
  confirm `set_behavior` still fires per painted cell.

Run after each step: `cargo build -p metor-panel`, `cargo test -p metor-panel`,
`cargo clippy -p metor-panel`, and a manual `cargo run -p metor-panel` smoke of a
multi-plot layout plus a resize drag. The `gpu.rs` tests skip cleanly when no GPU
adapter is present (`PlotGpu::try_new` → `None`), so they run in CI-with-GPU only.

## Estimated scope

- **Step A (item 2):** ~1 file, ~30 lines. Small, high leverage.
- **Step B (item 1):** ~2 files, ~80–120 lines incl. pool + reclaim + test.
  Medium; the `try_unwrap` reclaim is the only subtle part.
- **Step C (items 3+4):** ~1 file, ~120–180 lines — the largest single rewrite,
  confined to `gpu.rs`'s `submit`/`plan_*`. Medium/high effort, self-contained.
- **Step D (item 6):** ~1 file, ~40 lines. Small.
- **Step E (item 7):** ~1 file, ~80 lines, closely mirrors an existing view.
  Small/medium.
- **Step F (item 5):** ~2 files, ~60–100 lines across many call sites. Low risk
  per site but the widest touch; land last.

Total: roughly one focused day of work, landable as six independent PRs.
Biggest wins (Steps A and B) are also among the cheapest and should go first.
