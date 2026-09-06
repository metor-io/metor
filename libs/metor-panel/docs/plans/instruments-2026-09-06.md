# Optimization plan from the September 6 Instruments capture

Start with plot update scheduling and database wakeups. This capture spends far
more sampled CPU time maintaining live subscriptions and rebuilding plot state
than submitting plot rendering work. Keep the GPU changes in
`render-path-perf.md` as follow-up candidates, with a separate rendering capture.

## Implementation status

Section 1 is implemented in the working tree. `LinePlot` now reconciles only
after explicit configuration invalidation; ordinary sample/image notifications
check a dirty flag. Trace and axis observations, reflected field/list edits,
configuration loading, and reset/sync actions invalidate configuration. Direct
edits to a plot's public configuration fields must call `configuration_changed`.
Title inputs are memoized separately, so appearance-only edits preserve the title.

The DB publishes a separate metadata generation after successful metadata writes;
plots watch it and the registration generation with condition-based waits. This
keeps idle titles current after renames without per-sample metadata lookups.

Validation: `cargo test -p metor-panel -p metor-db` passed 426 panel tests and
84 database tests (one existing ignored DB doctest). Six new configuration tests
cover live bounds updates without reconciliation/title rebuilding, inspector
field/list actions, coalesced notifications, rebind/reorder/removal, replacement
axes, idle metadata renames, and late registration. Existing rebind and temporal
zoom tests also pass. `cargo clippy -p metor-panel -p metor-db --all-targets`
completed with warnings in existing code; formatting checks for the formatted
files and `git diff --check` passed. A new Instruments capture and interactive smoke test are
still needed to measure real-world CPU/frame-time improvement. Sections 2–4 have
not been implemented.

## Evidence and limits

- Input: `~/Desktop/metor-panel.trace`, run 1, Time Profiler, September 6, 2026,
  09:41:20–09:41:45 PDT; duration **24.9579 seconds**.
- Attached process: `target/release/metor-panel`, PID 9034. Its embedded DB and
  FSW connection are included; an external DB/FSW process is not profiled.
- Exported `time-profile` has **26,375 samples**, each weighted 1 ms, all marked
  Running. Samples span 0.4485–24.9565 seconds. Total sample weight is **26.375 s
  across threads**, not elapsed application latency.
- Main thread: **10.256 s (38.9%)**. FSW connection/ingestion executor thread
  `0x69980`: **11.959 s (45.3%)**. Other threads: **4.160 s (15.8%)**.
- The `potential-hangs` table contains no rows; the recording threshold was
  250 ms. This does not establish smooth frame delivery or low input latency.
- Source inspection used the current working tree, whose HEAD was
  `4ccc86ee08edee0aacaca607f89153c392bd791a`. There were existing uncommitted
  changes, including plot and binding changes. The trace does not establish the
  exact source revision used to build its executable.
- The stacks show live FSW ingestion and time-series plots. The specific layout,
  interaction, stream rate, and window visibility/occlusion are unconfirmed.
  This is not an allocation, GPU execution, disk latency, or frame-time capture.

Rust names were demangled with the installed `llvm-cxxfilt`. Inclusive totals
count each matching function/family only once per sample. Family matches include
monomorphized callers and closures; exact-function matches are labeled below.
Inclusive rows overlap and **must not be added**. Merged symbols, inlining, and
system boundary samples limit attribution; `start_wqthread` alone is not evidence
of thread creation, and a `kevent` sample is not a measurement of blocked time.

| Sampled path | Inclusive weight | Share of all samples | Interpretation |
| --- | ---: | ---: | --- |
| `LinePlot::reconcile`, exact function | 4.777 s | 18.1% | **46.6% of main-thread weight** |
| Reconcile family, including callers/closures | 4.927 s | 18.7% | 48.0% of main-thread weight |
| `derive_title` family | 1.258 s | 4.8% | Nested in reconcile; 12.3% of main-thread weight |
| `InputChanges::changed` family | 0.532 s | 2.0% | Nested in reconcile |
| `reconcile_trackers` family | 0.813 s | 3.1% | Nested in reconcile |
| `watch_history` task family | 3.325 s | 12.6% | Includes 2.535 s in reconcile family |
| `spawn_tracker` task family | 4.759 s | 18.0% | 4.283 s on main, including 2.392 s in reconcile family |
| `DB::ingest_table` | 5.260 s | 19.9% | Includes decoding, publication, and task wakeups |
| `Component::persist` task family | 3.755 s | 14.2% | Includes 2.325 s of **self** weight in `Waiter::poll_wait` |
| `WaitQueue::wake_all` family | 2.815 s | 10.7% | Overlaps ingestion and persistence |
| `Poller::notify` | 1.647 s | 6.2% | Includes 1.482 s of **self** weight beneath ingest |
| `TimerSeriesWriter::push_buf` | 0.847 s | 3.2% | Includes downstream data notifications |
| `expand_value_bounds` family | 0.435 s | 1.6% | The actual bounds work is smaller than task/notification overhead |
| Symbolized `time_series::gpu` family | 0.097 s | 0.4% | CPU attribution only; says nothing about GPU execution time |

The expensive paths persist through the capture. In successive five-second
buckets, reconcile-family weight is 0.966, 1.004, 0.992, 1.032, and 0.933 seconds;
ingest weight is 0.957, 1.077, 1.095, 1.058, and 1.073 seconds. The first bucket
has a shorter sampled interval. These are sustained costs, not just initialization.

## 1. Separate plot configuration changes from sample updates

**Priority: first. Expected impact: high on UI CPU; moderate implementation scope.**

Relevant source: `src/views/time_series/line_plot.rs:398` (`reconcile`),
`:708` (`spawn_tracker`), `:993` (`derive_title`),
`src/data_binding.rs:214` (`InputChanges`), and
`src/views/plot_common.rs:139` (`reconcile_trackers`).

`observe_self(Self::reconcile)` runs full reconciliation on every self-notify.
It updates every trace twice, resolves bindings, compares inputs, reconciles task
membership, captures override settings, and rebuilds the title. Both sample
tracking and history notifications enter this path. Rebuilding a title from
unchanged component metadata alone accounts for a substantial part of it.

Introduce explicit configuration invalidation, independent of data/frame
invalidation. Changes to trace membership/order, source, element index, axes,
overrides, custom title, or relevant registry/metadata state trigger the necessary
configuration work. Sample arrival and image completion only invalidate data or
presentation. Inspector edits must participate even when they directly mutate
reflected fields; wire their existing entity observations into configuration
invalidation rather than assuming all edits go through setter methods.

Recompute the title only when its inputs change. Update parent links and clamp
axes only on the relevant structural changes. Avoid a full snapshot/scan on every
sample just to discover that nothing changed. A bounded once-per-frame comparison
is a reasonable migration step where direct mutation cannot yet be observed.

Acceptance: steady live samples cause no title rebuilds or tracker-membership
reconciliation after initialization. Existing rebind and local-zoom tests remain
green. Add behavior tests for source/element edits, trace add/remove/reorder,
late registration, metadata rename, axis edits, and title changes. Data and
completed-image notifications must still repaint correctly.

The 4.777 s inclusive cost is an opportunity boundary, not a promised saving.
Necessary configuration work remains; quantify the reduction with a new trace.

## 2. Consolidate and pace plot data subscriptions

**Priority: second. Expected impact: high on task churn and main-thread traffic.**

Relevant source: `src/data_binding.rs:366` (`watch_history`),
`src/views/time_series/line_plot.rs:708`, and
`src/views/time_series/mod.rs:387` (`expand_value_bounds`).

`InputChanges` creates a history watcher for each input, while each trace also
has a bounds tracker waiting on that component's time series. History watchers
therefore react to ordinary live appends too. A bounds cycle crosses from the
foreground to a freshly spawned background task and back, then notifies the plot.
History watchers independently notify it. Each cycle also constructs boxed wait
futures. Multiple elements/traces can subscribe to the same component.

Use a plot-owned coordinator with subscriptions deduplicated by component ID.
Include selected LoD components and replay input dependencies. Data wakeups should
mark generations/dirty state away from the foreground; schedule at most one
pending foreground update per plot, paced to the display cadence. Merely calling
`cx.notify()` less often inside a task that still crosses to the main thread on
every sample would leave much of this overhead intact.

Batch bounds updates for dirty traces in one background job, with one job in
flight and another pass only if a generation changed. Continue scanning every new
sample since the cached frontier, preserving short spikes and extrema. Only
presentation is coalesced; persistence and expression evaluation keep their
required sample semantics. Stop unnecessary presentation work for hidden plots
and catch up correctly when shown.

History installation, purge/replacement, selected-LoD changes, replay input
hydration, and a final update followed by silence must all cause a refresh. Use
generation checks around arming waits so an update racing completion cannot be
lost. Keep the generic history watcher available for consumers that still need it.

Acceptance: foreground update and background job counts scale with visible plots
and frame rate, rather than trace count times source rate. Validate burst input,
shared-component traces, historical hydration without a live append, LoD changes,
replay, hide/show, and source edits during an in-flight bounds job. Compare both
rendered data freshness and bounds against the existing behavior.

## 3. Reduce database waiter and scheduler overhead

**Priority: third, in small independent changes. Expected impact: high on ingest CPU.**

Relevant source: `../db/src/disruptor.rs:262` (`Reader::next`),
`../db/src/lib.rs:823` (`Component::persist`),
`../db/src/time_series_2.rs:1090` (`TimerSeriesWriter::push_buf`),
`../stellarator/maitake/sync/src/wait_queue.rs:927`,
`../stellarator/maitake/src/scheduler.rs:1648`, and
`../stellarator/src/poll/mod.rs:106`.

The trace shows expensive active waiter bookkeeping, not proof that a task is
busy-spinning. `Reader::next` always enters `wait_for_value`, which subscribes a
waiter before checking for readable data. Persistence loops through that path
after every grant. Each append wakes series readers; each WAL commit wakes WAL
readers. `LocalSpawner::schedule` unconditionally calls the external waker, which
calls `Poller::notify`, including when the ingestion executor is already running.

1. Add a readable-data fast path to `Reader::next`: check `poll_grant` first;
   when empty, retain the existing subscribe-then-recheck wait protocol. Do this
   locally to the reader rather than changing generic queue-close semantics.
   Measure ready versus pending reads: the benefit depends on backlog/batching.
2. Coalesce reactor notifications at the executor/scheduler boundary, with a
   correct running/parking handshake. Enqueued work must remain visible, and a
   producer racing the transition into sleep must wake it. Count schedules,
   actual poller notifications, failures, and retry attempts to distinguish
   redundant notifications from the existing 1,024-attempt retry behavior.
3. If persistence still dominates, append a bounded batch from each WAL grant
   before publishing one data notification per component. Preserve sample order,
   per-sample validation, node rollover/lifecycle notifications, reader lifetime,
   partial-error publication, and a bounded latency/fairness budget. Evaluate a
   shared dirty-component persistence queue only if these smaller changes leave
   significant per-component task overhead.

Do not substitute `wake_one` for broadcast notifications: independent consumers
need to observe the same committed data. Do not throttle or drop persisted
samples to make CPU numbers smaller. Current overflow handling can return success
after skipping a write, so benchmark sample counts and overflow events explicitly.

Acceptance: lower waiter registrations and poller notifications per accepted
sample, with identical persisted values/timestamps and no starvation or lost
wakeups. Exercise ready/empty reads, a push between empty-check and subscription,
wraparound, multiple readers, cancellation, batch rollover, and remote-thread
wakes racing executor parking. Run existing DB/disruptor and stellarator scheduler
tests; use the repository's Loom setup for changed synchronization and the Miri
workflow in `../db/MIRI.md` if ring internals/unsafe access change.

## 4. Compile a static ingestion plan for registered vtables

**Priority: after wakeup changes. Expected impact: medium; broader correctness surface.**

Relevant source: `../db/src/lib.rs:253` (`insert_vtable`) and `:302`
(`ingest_table`), plus `../metor-proto/src/vtable.rs:595` (`for_each_field`).

Ingest holds the DB state write lock while cloning a vtable, walking its fields,
looking up components, and publishing samples. `for_each_field` scans all ops
for template membership for each top-level field. Static schemas repeatedly
interpret op chains and reconstruct/validate views. The exact `VTable::realize`
function contributes 0.439 s self weight; `for_each_field` contributes 0.514 s
self weight. Its 4.477 s inclusive total includes publication and wakeups and
must not be presented as pure decoding cost.

Build a registration-time plan for provably static layouts: component handles,
field offsets/lengths, schema information, and timestamp/frame extraction rules.
Retain per-packet bounds and payload checks and the dynamic interpreter fallback.
For dynamic layouts, precompute only structure that is actually invariant, such
as root-field membership. Replace repeated deep copies with shared immutable
registration data where ownership allows it.

Version/invalidate plans on vtable replacement and component/schema changes.
Narrow DB lock scope only after establishing that cached handles remain valid
through replacement and concurrent metadata access. The trace does not establish
lock contention; reducing lock hold time is a design benefit to measure, not an
observed stall diagnosis.

Acceptance: compare planned and interpreted ingestion for static and dynamic
tables, inherited/overridden timestamps and frames, malformed/truncated buffers,
late members, replacement, and error behavior. Benchmark fixed packet rates and
field counts, reporting accepted samples/s and time per field after phases 1–3.

## Follow-up rendering work

Re-profile a visible dashboard during steady rendering, resize, and pan/zoom.
Record frame timing and GPU work before prioritizing readback pooling, target
reuse, and batched uploads from `render-path-perf.md`. This trace does not justify
calling readback allocation the dominant application cost. Zero symbol matches
for `effective_view` or inspector filtering also do not prove those paths are free.

If target capacity is quantized, keep logical viewport/image dimensions explicit.
Scaling a larger image to the widget bounds is not equivalent to cropping it and
can change stroke widths and text/trace alignment. If pooling readback storage,
first verify GPUI's actual image ownership/reclamation API; do not assume dropping
the window image guarantees unique ownership of the CPU buffer.

## Measurement and delivery

First preserve the exact build/source revision, dashboard, input recording/rate,
visibility state, and trace settings. The workspace already provides
`release-with-debug`; use it for better source attribution, and keep the profile
identical across comparisons. Establish its own baseline if changing from release.

Add aggregate counters or sampled spans for configuration reconciles, title
rebuilds, subscription wakeups, foreground updates, bounds jobs, frames submitted,
accepted/persisted samples, overflow/errors, persistence lag, and poller
notifications. Avoid logging every sample, which would alter the workload.

Compare repeated captures of the same steady stream and layout, then separately
test burst input, pan/zoom/resize, and historical hydration. Report CPU by thread,
p50/p95/p99 frame/update latency, throughput, data freshness, and correctness.
For a 60 Hz target, use 16.7 ms as the frame budget; this trace provides no current
frame percentile baseline. Require no regression in sample delivery or freshness.

Land configuration invalidation, plot subscription pacing, reader fast path,
reactor wake coalescing, optional persistence batching, and static ingest plans
as separate reviewable changes. Re-rank after each capture; their savings overlap
and cannot be summed into a credible total speedup estimate in advance.

For panel changes run the relevant behavioral tests and
`cargo test -p metor-panel`; for DB changes run `cargo test -p metor-db` and affected
integration tests. Build/lint affected crates and run the interactive scenarios
above. The original analysis made no implementation changes; subsequent section 1
work and its validation are recorded in Implementation status above.

### Reproducing the analysis

```sh
xcrun xctrace export --input ~/Desktop/metor-panel.trace --toc --output /tmp/metor-trace-toc.xml
xcrun xctrace export --input ~/Desktop/metor-panel.trace --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' --output /tmp/metor-time-profile.xml
xcrun xctrace export --input ~/Desktop/metor-panel.trace --xpath '/trace-toc/run[@number="1"]/data/table[@schema="potential-hangs"]' --output /tmp/metor-hangs.xml
```

Resolve XML `id`/`ref` elements before summing `weight`, demangle frame names,
and count a matching stack at most once per category. Local analysis outputs are
`/tmp/metor-summary.json`, `/tmp/metor-samples.json`, and
`/tmp/analyze_metor_trace.py`; temporary files are not durable project artifacts.
The trace and its exported process environment were not copied into the repo.
