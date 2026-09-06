# Global time and replay selector

Status: implemented on 2026-09-05. The design below records the intended behavior; the implementation notes identify the current boundaries.

The proposed next step is the [reusable timeline widget](timeline-widget.md), covering panel/dashboard hosting, a shared inspector preview, scrubbing, range selection, event lanes, and adaptive navigation.

## Implementation notes

`src/temporal/` now owns the anchor parser, shared controller, selected-sample readers, and inspector completion provider. Titlebar controls, context menus, and the command palette share its rows and actions. Tab inserts, Enter applies, and Escape discards. Dates are text-completed; timezone and DST validation use Jiff. Both view time and range endpoints retain their anchors in the optional versioned `TileLayout.temporal` field.

The initial data scope is the panel DB, with an editable component-name prefix for narrowing it. Live defaults to the telemetry head in that scope. Wall time and a named reference component are explicit alternatives; component names resolve to their registered IDs. Named connection groups and mission epochs remain future extensions. Extent discovery and predecessor queries run on workers. Readers share leases, cache predecessor intervals, retain a bounded WAL tail, check remote coverage, and prefetch at most one upcoming span during playback. Hydration uses the existing bounded, deduplicated service; queued network requests may finish after a view detaches.

Instantaneous widgets, copies, vector plots, map positions, and 3D transforms consume selected samples. Sample-table follow mode anchors its head to view time. Plots use the global interval as reset bounds. Pan and zoom remain local, and double-click resets the plot. Explicit commands sync all plots or use one plot’s current zoom as the global range. XY traces use the global interval as an acquisition-time filter while retaining their existing aligned-sample pairing behavior. Historical trajectories and event flags remain interval context around the playhead. Operational alarms and sequence controls remain explicitly Live; historical value editing and condition toggles are disabled.

Missing stateless expression outputs are reconstructed directly from predecessor inputs in a bounded point query. This does not write back to the DB or change the live evaluator. Stored expression outputs have unknown provenance unless it is known by the active reader. Missing stateful outputs report that a checkpoint is required; general warmup/checkpoint replay remains deferred. A graphical calendar, overview scrubber, reverse playback, looping, and per-sample stepping are deferred as described below.

Validation includes the full panel library suite, both inspector hosts' keyboard workflows, strict predecessor/gap/remote coverage tests, WAL handoff and shared-lease tests, stateless reconstruction isolation, anchored range and DST tests, and Rust/Python preset compatibility checks. A manual GUI review and a production-scale archive benchmark remain recommended before release.

## Recommendation

Introduce one global **view time** for instantaneous telemetry and one global **visible range** for historical views. Put both in a single compact time control, but keep them independently editable. Resolve their anchors once in a shared temporal controller. Route displayed samples through a shared as-of reader built on `Binding` and `BoundHistory`.

Build the time editor as text-first pages in the existing shared inspector. Right-click menus, titlebar entry points, and the command palette must use the same row builders, completion provider, validation, and actions; only placement and initial context differ. Autocomplete and resolved previews are the primary way to discover and edit expressions, dates, times, and anchors. A graphical calendar is deferred and is not required for any workflow.

A range answers “what interval am I looking at?” View time answers “what instant do these values represent?” Replay advances view time. An endpoint anchored to Live continues following Live while view time is paused; an endpoint anchored to View time follows replay instead. Never silently exchange those anchors.

Design assumption: support both selecting an absolute instant versus following Live, and configuring an instant alongside a range. This covers the request's “a timestamp OR a global time” wording without blocking the design. If a mission-relative timestamp was intended, the anchor model can add a named epoch without changing the UI structure. Named mission epochs are not a first-phase requirement.

## What exists and what must change

| Current implementation | Consequence for this design |
| --- | --- |
| `src/views/time_series/time_range.rs`: `Offset::{Earliest, Latest, Fixed}`, `TimeRangeBehavior`, preset rows, `GlobalTimeRange` | Preserve anchored intent; replace directional duration variants with named anchors plus signed offsets. Add a separate instant and transport state. |
| Each plot resolves `GlobalTimeRange` using its own extents | A shared rule currently produces different resolved timestamps. Following panels must consume one resolved global range. |
| `clamp_range` falls back to full history if requested bounds are inverted or outside coverage | Reject invalid edits and preserve valid out-of-coverage intervals; show empty coverage instead of changing the request. |
| `src/app.rs` renders a titlebar label and inspector rows | Extend those entry points with shared time pages and query completion; retain the existing inspector host. |
| `InspectorMode::{Anchored, Centered}`, `InspectorRow::query_rows`, `RowAction::{CascadeWith, ReplaceQuery}`, and `ExpressionRow` | Reuse the same page stack and keyboard contract on right-click and palette surfaces. Add a time-domain completion provider, with the existing expression picker as the interaction reference. |
| `src/views/binding.rs` seeds `time_series.latest()` and consumes live updates | A window repaint alone cannot update cached widget values to a historical instant. Subscriptions must react to time revisions. |
| `StreamUpdate::{Ready, Value, Stale}`; scalar/vector wrappers discard some status information | Add loading, missing, error, reset, timestamp, and quality to the shared display contract so seeking cannot retain a future value. |
| `Freshness` uses wall time and a three-second threshold | Historical age must use view time; pausing cannot age historical samples against today's wall clock. Preserve live freshness behavior separately. |
| `src/data_binding.rs`: owned `Binding`, `InputChanges`, `BoundHistory`, `watch_history`, registration watchers | Extend these facilities for point queries and dependency hydration. Do not create another expression ownership or history subsystem. |
| Map marker, list-plot GPU conversion, copy actions, sample-table head, and alarm state have independent latest paths | Audit and migrate these explicitly; changing the seeded stream helper is insufficient. |
| `src/dynamic/ops/replay.rs` reconstructs expressions from history with cold-start state | Display replay must not restart the live evaluator or claim that cold-start reconstruction exactly reproduces recorded state. |
| `TileLayout.global_time_range` is a string used by Rust and Python preset producers | Add a compatible persistence bridge and typed optional state; do not change the meaning of saved strings. |

## Temporal model and invariants

Maintain these concepts explicitly:

- **Data start / Data end:** earliest and latest known sample times in the selected data scope, including remote manifests. These describe the available data timeframe; fetching older data must not change an already-known extent. Discovering previously unknown history may legitimately extend it.
- **Live:** the current session clock in the same timestamp domain as the data. Default to the latest telemetry timestamp in the selected scope. UTC wall time is an explicit option. An explicit source-clock mode is needed for simulated or non-wall-clock streams; it follows that source's published time and does not advance during source silence. The session-head clock intentionally follows recorded telemetry, including simulated and archived timestamps. Select a reference component when multiple clock domains share the DB.
- **View time:** the timestamp all following instantaneous displays query. Its source is Live, a fixed timestamp, or an anchored expression. Replay temporarily advances a concrete view time.
- **Visible range:** two anchored endpoint expressions resolved against the same temporal snapshot. It does not itself select a sample for instantaneous displays.
- **Replay bounds:** a concrete interval captured when playback starts, normally from the visible range. They stay fixed for that playback session even if the visible range follows view time.

All timestamp and offset arithmetic uses checked integer microseconds, matching `Timestamp`; formatting and screen coordinates are the conversion boundaries. Overflow is an error. A range requires start < end. A single timestamp belongs in the view-time field, not in a zero-width range.

DB query intervals are half-open. Model Data end as the latest sample instant, separately from a half-open query's exclusive end (manifest seal/coverage endpoints are inclusive). Full-range queries ending at Data end adapt that terminal sample to a checked `end + one timestamp tick`; never display an artificial one-microsecond offset to the user. Fixed timestamp and duration windows retain their exclusive query end; plotting may fetch an additional boundary sample for drawing without counting it in interval statistics. A dataset containing one sample has a valid instant and extent; add display-only plot padding without inventing history or mutating saved anchors.

### Anchors

Use `TimeExpr { anchor, offset }` where offset is a signed duration. Legal endpoint anchors are Data start, Data end, Live, View time, and Timestamp. The timestamp variant stores the absolute instant. The view-time expression may use Data start, Data end, Live, or Timestamp, but cannot reference itself or the visible range. That restriction prevents cycles.

| Choice | Start expression | End expression |
| --- | --- | --- |
| Full range | Data start | Data end |
| First 5m | Data start | Data start + 5m |
| Last 2.5m | Live − 150s | Live |
| Last 5m of data | Data end − 5m | Data end |
| 5m ending at view time | View time − 5m | View time |
| 1m around view time | View time − 30s | View time + 30s |
| Absolute interval | Timestamp A | Timestamp B |
| Mixed interval | Timestamp A | Live |

An anchored endpoint is a formula, not a cached timestamp. “Pin timestamp” replaces one formula with its currently resolved instant. “Pin both” freezes the visible interval atomically. The picker previews the result before applying. “First 5m” retains its five-minute span even if only two minutes of data exist; the remaining three minutes show no coverage.

### Scope and synchronization

Default the data scope to the selected connection/session; allow an explicit group of connections. Derive a union extent across the selected sources, excluding synthetic expression outputs where their input provenance already supplies bounds. Do not derive the scope from currently visible widgets: opening a gauge must not move Data start. For multiple sources, the union preserves all history; missing coverage is shown per source. Offer a selected reference source when a user needs that source's extent or source clock.

Resolve scope metadata, Live, view time, and the global range in that order, once per controller update. Publish the resulting immutable snapshot with a revision. Every following plot receives identical endpoints and every following instantaneous widget receives identical view time, even though their held sample timestamps differ. No synchronization barrier across unavailable sensors is required: each value carries its sample time and completeness status.

Scope changes are explicit picker actions. Preview changed anchored values; fixed timestamps stay fixed. During replay, changing the scope pauses transport. Loss of metadata produces an unresolved/loading state, not a guessed epoch or a fallback to full range.

## UI design

### Compact control

Keep the titlebar compact. The first segment communicates display mode and view time, the second communicates the range; clicking either opens the corresponding shared inspector page anchored to that segment.

```text
Live:
  [● Live ▾]  [Last 5m ▾]  [Ⅱ]

Paused:
  [Ⅱ 2026-09-05 14:32:10.250 UTC ▾]  [First 5m ▾]  [▶]  [Go live]

Playing:
  [▶ 14:32:12.250 UTC ▾]  [5m ending at view time ▾]  [Ⅱ] [2× ▾] [Go live]
```

Show the full date whenever crossing days or when the chosen instant is not today; retain the full timestamp in the tooltip and picker. Mode is expressed with text and icons, not only color. Narrow windows abbreviate the timestamp before hiding transport; Go live stays available through the mode segment.

Pause in Live freezes view time at the controller's current instant. It does not rewrite the visible range. If the range remains live-anchored, its label stays “Last 5m” and the picker explains “Range follows Live; values are paused.” Offer the adjacent explicit action “Range follows view time.” This makes the mixed state understandable without changing saved intent.

### Shared inspector pages

The time UI is a set of ordinary inspector pages, not a separate popover form. The same page factory supplies `InspectorRequest.rows` in Anchored and Centered modes. Register searchable palette entries such as “Time: Set view time…”, “Time: Set visible range…”, “Time: Pause”, and “Time: Go live”. Titlebar clicks open the corresponding editor directly; right-click menus expose those same actions through navigation rows. A plot-context action such as “Set view time here” adds its clicked timestamp as an argument to the same seek action.

The top-level Time page is a fuzzy-searchable list of current values and actions. Navigation opens focused pages for View time, Visible range, Start, End, Timezone, Scope, Clock, Playback speed, and Step size. Endpoint pages are an optional way to build an expression incrementally; no dropdown-only functionality is necessary. Breadcrumbs, selection, scrolling, dismissal, and focus restoration remain owned by `Inspector`.

```text
Time
  Search…
  View time          Live                              ›
  Visible range      Last 5m                           ›
  Pause
  Play from range start
  Go live
  Pin both range endpoints
  More time settings                                   ›

Time › Visible range
  > the last 2.5m
  Apply range: Last 2.5m                        Enter
    Live − 150s → Live · 2m 30s
    Resolved: 14:30:40 → 14:33:10 UTC
  Last 2.5m of data                 ends at Data end
  2.5m ending at view time          follows replay

Time › View time
  > 2026-09-05 14:32:10.250
  Set view time: 2026-09-05 14:32:10.250 UTC      Enter
    Uses UTC · pauses view time
  UTC                                               Tab
  America/Los_Angeles                                Tab
```

The indented preview lines are nonselectable rows using the existing row-list layout, not a custom form. In the anchored menu, labels elide to the available width and full expressions/timestamps remain accessible through the query and tooltip. Both modes expose the same candidates and actions.

### Completion provider and keyboard contract

Add a time provider row implementing `InspectorRow::query_rows(query, cursor, cx)`, following `src/inspector/rows/expression.rs`. Each editor page has one provider parameterized by its target: instant, whole range, start endpoint, or end endpoint. Leave the root command palette's ordinary fuzzy search intact; time-expression interpretation begins only after entering a time editor. This avoids intercepting component names or unrelated commands that happen to contain dates or “last”.

The provider uses a pure time parser/completer rather than the component-expression compiler. Return token replacement spans, insertion text, caret position, description, and candidate kind. Reuse the existing matching/rendering helpers where they fit; factor a small common candidate adapter only if needed, without coupling time syntax to `metor_expr` types or creating another completion host.

- Empty editor queries show common presets, current state, bounded recent selections, and examples of valid syntax. Open an existing value with its canonical expression prefilled, preserving anchor intent instead of substituting resolved timestamps.
- Partial input offers context-specific anchors, keywords, durations, date/time segments, and timezones. For example, `fir` completes to `first `; `last 2.5` offers `m`, `s`, and `h` with duration descriptions; `data start + ` offers durations; after `..` offer valid end expressions. Filter out View time as an anchor when editing view time itself.
- Complete valid input puts the explicit commit row first, followed by the resolved preview and alternative completions. The commit label states its target and effect, such as “Apply range” or “Set view time … · pauses”. A preset row may commit on Enter/click only when it displays a complete expression and its meaning.
- Up/Down selects rows. Tab inserts the selected candidate using `RowAction::ReplaceQuery` and never commits. Enter/click on a partial completion inserts it and keeps the page open; Enter/click on a commit row applies it. Show this distinction in row labels/help rather than introducing a second keyboard model.
- Completion replaces only the relevant token span at the current UTF-8 byte cursor, preserving the rest of a mixed-endpoint expression. Recompute candidates after insertion. Exact valid input must remain easy to commit instead of requiring traversal through many suggestions.
- Errors and partial-input hints stay visible as nonselectable rows, while applicable completions remain selectable. Invalid input has no executable commit action. Use the shared header/noninteractive-row behavior so arrow navigation skips diagnostics.

Use `CascadeWith` to open child editors with their query prefilled. If direct titlebar opening needs an initial query that `InspectorRequest` does not yet carry, add a small shared inspector initialization option; both placement modes must support it. Do not implement a time-only text field or overlay to work around that API. Reuse `NavRow`/`CommandRow` and the existing `RowAction` dispatch for settings and transport.

### Drafts, previews, and actions

Typing and completion insertion edit only the page query. A commit parses and validates again against one current temporal snapshot, then atomically changes the targeted property. A range expression commits both endpoints together. A Start or End editor previews the resulting whole range and commits against the unchanged counterpart; reject an invalid combined interval. Use existing Escape/back behavior to abandon the uncommitted page. There is no separate modal Apply/Cancel footer.

Track the edited property's base revision and semantic dependencies. Merge unrelated changes: applying a range cannot undo a newer seek or transport update. If the same property, scope, clock, timezone, or opposite endpoint was explicitly edited elsewhere, refresh the preview and require an explicit commit against the new context. Normal Live/replay ticks update resolved previews without being conflicts or rewriting typed text. Date shorthand uses a captured, labeled reference date during the edit. At commit, fixed date/time input remains fixed; anchored expressions resolve against the current snapshot.

Observe controller changes while a page is open. The inspector currently requests provider rows when its query changes; add a narrow shared provider-invalidation/refresh path for dynamic previews if needed. Preserve the query, byte cursor, and selected candidate by stable identity across refreshes; a Live tick must not move focus onto a different action. Avoid rebuilding the full inspector on each clock update.

Use one typed temporal action layer for inspector rows, palette commands, toolbar buttons, shortcuts, and plot gestures. It applies target/dependency checks and emits the same state changes regardless of entry point. Pin start/end, Pin both, Follow Live, Follow view time, Play, Pause, Go live, rate, and step remain ordinary searchable commands; choosing a graphical control is never necessary to express these actions.

### Date and time completion

Support precise date selection through text and suggestions in the same inspector field. ISO timestamps, an explicit date plus time, and fractional seconds are first-class input. A partial date such as `2026-09-` offers a bounded list of day completions labeled with weekday; partial time offers its missing segments. Suggest recorded-data dates and recently used instants where metadata is already available, without loading the archive to populate a menu. Directly typed dates outside coverage remain valid.

For convenience, support `today 14:32`, `yesterday 14:32`, and a time-only query such as `14:32:10.250`. Today/yesterday use the selected display timezone's civil date; time-only input uses the date of the current view time. Capture and show that reference date when editing starts. These forms normalize to explicit fixed timestamps on commit, so they do not move the next day. A bare date in an instant field remains incomplete until a time is supplied or the user explicitly selects a “Start of day” completion. A range-only `day 2026-09-05` expression selects that civil day and normalizes to fixed endpoints.

Use UTC initially and expose Timezone as a searchable settings page with UTC, Local, and IANA names. Zone-less input uses that explicitly visible zone, and the commit preview always includes the complete timestamp and UTC offset. Explicitly disambiguate repeated DST times with offset-labeled candidates and reject nonexistent local times. Editing a date token preserves existing time/fraction tokens; timezone changes must state whether they affect interpretation of the draft or only display of a committed instant. Store absolute timestamps and persist the display zone.

Duration `1d` always means 24 elapsed hours. A date-only range such as `2026-09-05` resolves from local midnight to the next local midnight, exclusive, and can be 23 or 25 hours. Never construct a day end as `23:59:59.999`. Use the existing `jiff` and `hifitime` dependencies behind one tested conversion boundary; verify UTC/leap-second conversion before accepting leap-second entry. Unsupported `:60` produces an error rather than rolling over.

A calendar grid may be considered later only as an optional inspector-hosted page using `CascadeView`, reachable in both modes and backed by the same parser/model/actions. It must not become the only way to select dates or introduce a second time-selection implementation. The first release and its acceptance criteria require no graphical calendar.

### Expression grammar and errors

Use one parser shared by completion, presets, and endpoint editors. This is a small deterministic grammar, not an open-ended natural-language interpreter. Completion and validation must agree on the same typed representation; an offered complete expression must parse successfully.

Supported forms:

```text
full / full range / the full range
first 5m / the first 5M
last 2.5m / the last 2.5m
last 5m of data
data start + 30s .. data start + 5m
view time - 5m .. view time
2026-09-05T14:00:00Z .. 2026-09-05T14:05:00Z
2026-09-05T14:00:00Z .. live
day 2026-09-05
```

The instant field also accepts `live`, `data start + 30s`, `data end`, or an absolute timestamp. Accept legacy `+duration`, `-duration`, `=epoch`, and `↔` syntax through a compatibility parser with its original Data start/Data end meanings. New canonical text spells anchors explicitly where ambiguity matters.

Keywords and fixed-duration units are case-insensitive; `M` and `m` both mean minutes. Support decimals and compound durations (`2.5m`, `1h 30m`, `250ms`, `1d`). Exclude months and years from elapsed-duration expressions. Require positive duration for first/last presets; permit signed offsets in endpoint editors. Parse decimal input to checked integer ticks rather than through a lossy float. Reject values finer than the timestamp resolution with an actionable message.

Show diagnostic rows and withhold the commit action for unknown anchors, invalid durations, ambiguous civil time, overflow, or start at/after end. Zone-less timestamps use the editor's explicit display zone. Keep the current committed state active. A valid request outside available data is an informational coverage result and remains selectable. Preset matching uses typed equality, not the spelling the user entered.

### Timeline and gestures

An optional overview strip below the toolbar shows data coverage, visible-range handles, a separate view-time marker, and playback bounds. It supplements the shared inspector pages and is not required to set an instant, range, or transport option. Distinct handles and labels prevent confusion between selecting an interval and seeking an instant; every gesture dispatches the same temporal actions as its text/command equivalent.

- Drag the view-time marker or use “Set view time here” on a plot to seek and pause. Ordinary hover and existing measurement cursors remain local measurement tools.
- Drag overview range handles to set fixed endpoints; changing range does not seek. An offscreen view-time marker gets an edge indicator and “Show view time” action.
- Pan and zoom remain local. Double-click resets to the configured bounds. Explicit commands sync all plots to the global range or use the current plot zoom as that range.
- Go live changes view time to Live and stops replay. It preserves the visible range. If the playhead is outside it, offer “Show live in range”; do not overwrite a carefully chosen absolute interval.
- Keyboard transport operates only when a time control or plot has focus, never while a text editor consumes keys. Give all icon controls accessible names and tooltips.

### Interaction walkthroughs

1. **Investigate startup:** enter “The first 5M.” All following plots show Data start to Data start + 5m. Values remain Live until a user picks an instant. Click “Set view time here” at an event; every gauge and map marker queries at that instant. Play advances through the captured five-minute interval.
2. **Monitor a rolling window:** choose “The last 2.5m.” The range is Live − 150s to Live. Pause freezes values while the range continues to move. Choose “150s ending at view time” to make the range follow paused/replaying values instead.
3. **Open an incident:** find “Time: Set view time…” in the command palette or open View time from a right-click menu. Type `2026-09-05 14:32:10.250 UTC`, use completion as needed, and accept the commit row. It becomes a fixed instant and triggers as-of loading. Select “1m around view time” to see context. Later changing view time shifts this window because its anchors say so. Both entry points use the same editor and produce identical state.
4. **Freeze a comparison interval:** start with Full range, then Pin both. Later ingestion extends Data end but the visible interval remains fixed. Go live returns values to current time while retaining that interval.

## Shared data mechanism

### Controller and snapshot

Add a temporal module separate from the time-series renderer, with pure model/parser/resolver code and a GPUI `TemporalController` entity. A small GPUI global holds the entity handle. Keep one shared controller per application initially, matching existing `GlobalTimeRange`; a future workspace scope can replace that owner without changing consumer APIs.

Conceptual types, subject to repository conventions:

```rust
TimeExpr { anchor: TimeAnchor, offset_micros: i64 }
TimeRangeSpec { start: TimeExpr, end: TimeExpr }
ViewTimeSpec = Live | At(TimeExpr)
Transport = Paused | Playing { base_time, base_monotonic, rate, bounds }
TemporalSnapshot {
    revision, scope_revision, view_time, visible_range,
    data_extent, live_time, mode,
}
```

Publish separate dirty information for view time, range, and source coverage so a range-only edit does not restart every scalar read. Resolution happens before broadcast, and consumers never each call `Timestamp::now()` for a selected timestamp. Rendering stays synchronous against cached state; storage/hydration/replay work stays off the UI thread.

### Selected samples

Extend `BoundHistory` with point-read orchestration and a reusable reader/subscription adapter beside the existing `views/binding.rs` decoding helpers. Preserve owned `Binding` and its expression lifetime. Avoid a new global map of mutable widget values; share query/cache work keyed by DB/source identity, binding/component identity, time revision, and coverage revision. A selected sample has a stable identity and owned/stably referenced bytes reused by display, tooltip, and copy. Reference-count shared subscriptions, cancel demands when the last consumer detaches, and bound cached bytes and historical intervals; retain expression owners only while their bindings or active requests need them.

The read contract is **latest sample at or before view time**. Hold-last is the default for numeric, boolean, enum, vector, and quaternion values; never pick a nearer future sample. Return both requested time and actual sample time. Interpolation, including quaternion slerp, can be an explicit later per-view option with gap/quality rules.

The active DB implementation is `../db/src/time_series_2.rs`, aliased as `metor_db::time_series`. Its `get` is exact and `binary_search_nearest` does not promise predecessor semantics across nodes. Add or factor a strict `at_or_before` query; the `last_before` scan in `src/dynamic/ops/replay.rs` is a useful local precedent but needs inclusive equality and completeness checks. Implement a bounded panel adapter first if a DB API extension cannot be included in the same change.

The selected sample envelope must distinguish:

- Ready: decoded value, sample timestamp, selected timestamp, sample age, quality, and historical provenance.
- Loading: pending local scan, remote coverage, or expression reconstruction for this requested instant.
- No sample: proven no sample at/before the instant, or known unavailable history.
- Error: source unavailable, decoding error, or failed history request with retry support.

On a time revision, immediately invalidate the previous selection. A displayed value from a later timestamp must never remain presented as belonging to the new instant. Loading may show the previous value dimmed only with its old timestamp and an explicit loading label; the default is a placeholder. Clear derived transforms/markers or mark them unavailable rather than leaving an apparently valid future pose. Drop stale asynchronous results using request revisions, including after rebinding or Go live.

A result is valid only when coverage proves that no newer sample at or before the instant is hidden in an unloaded span. If a resident sample is at `s < t` and a remote span intersects `(s, t]`, load the necessary span before reporting a complete selection. Request the newest relevant span first, search backward only as needed, and stop at a proven predecessor or proven start-of-history. Do not fetch the entire archive to answer one gauge. Track empty-but-checked coverage separately from missing coverage.

Use existing hydrators and Backfiller through `BoundHistory`; coalesce duplicate demands and maintain bounded caches. Observe registration and history installation in addition to live arrivals. A historical read must complete even if no new live sample ever arrives. During fast scrubbing, update the marker immediately, throttle expensive demands, and execute a final exact query on release. Capture generation/revision checks across purge/install races; raw sample bytes must remain owned or backed by a stable snapshot.

### Freshness and live behavior

Historical sample age is `view_time - sample_time`, checked and nonnegative. It stays constant while paused and advances with replay time. Preserve the existing default three-second threshold initially through the new envelope, while allowing the separately planned per-binding freshness policy later. Metadata may identify held/discrete states that remain semantically valid longer; do not conflate sample age with decode failure or missing history.

Live ingestion continues while historical displays are paused. Keep live receipt/clock-health information separate from historical sample age. Existing future-stamp and repeated-stamp detection remains live source health; future-dated samples are not eligible for a strict as-of query until the selected clock reaches them. A simulated source-clock mode must state its clock basis so age is not accidentally measured against wall time.

Preserve the transition from the live WAL tail to committed history. The reader keeps a bounded tail of timestamped live samples and merges it with committed samples under one predecessor contract, deduplicating as commits arrive. Pause captures the last published controller snapshot and eligible displayed sample identities atomically: a sample already visible through the live stream must not disappear or jump backward because its history commit is still pending. Include this race in the first end-to-end test slice.

### Consumer migration

| Consumers | Required change |
| --- | --- |
| ValueStrip, Monitor, component/browser/table values, text, gauge, meter, state chip, traffic lights | Consume the selected sample envelope and clear/reset on time changes; keep formatting and metadata adapters. |
| Attitude, 3D position/orientation/markers, dashboard connector styling | Resolve all inputs at the same view time; propagate missing status and actual sample times instead of combining a historical input with a live input. |
| Map | Marker uses the as-of sample; trail uses the visible interval, defaulting to history through view time. If later points remain visible for context, visually distinguish them from the replayed trail. Camera follow tracks the selected marker. |
| Time-series plots, spectrograms, execution timeline, XY time windows | Consume one resolved range and draw the global playhead; retain independent-range override with an explicit badge. Temporal trajectory data after the playhead is context, not the current state. |
| List plot and latest-vector GPU bounds/conversion | Replace direct latest reads with the same selected vector sample; include time/coverage revisions in bounds and GPU upload keys. |
| Samples table | Preserve its paginated history role; selected/latest head becomes anchored to view time in follow mode. Explicit historical paging remains possible and labeled. |
| Copy value and value tooltips | Copy the displayed selected sample and its formatting, not a fresh `latest()` lookup. Timestamp copying is a separate option. |
| Logs/events | Retain range-based lists and add a playhead marker or as-of filter; events may be visible after the playhead as explicit context. |

Audit remaining `latest()`, `Timestamp::now()`, caches keyed only by live head, and stream callbacks. Not every occurrence is wrong: ingestion, acquisition diagnostics, range metadata, and live control still require live state. Classify each call rather than mechanically replacing it.

### Expressions and operational state

Prefer stored expression outputs when present, but do not infer recorded-live provenance merely from storage: the existing Backfiller writes reconstructed outputs into the same DB. Track provenance and reconstruction origin for newly reconstructed spans, and treat existing spans without provenance as unknown rather than asserting they were recorded live. For missing output history, use `BoundHistory` and the existing replay plan to hydrate input spans and reconstruct through the requested instant. Current expression replay cold-starts window buffers, declared state, and random generators at the requested range start. Therefore arbitrary tiny replay windows cannot promise consistent stateful as-of values.

Make the reconstruction origin stable per expression history session, normally the selected data start, and include it in reconstruction/cache provenance. Stateless expressions and available stored outputs can support immediate historical selection with their provenance displayed; completing a general stateful warmup/checkpoint system is not a prerequisite for that first useful slice. Finite windows require their documented warmup; reconstructing general state and random generation requires a known origin/checkpoint or must display “Historical reconstruction unavailable” until that policy is implemented. Never present a cold-start value as equivalent to recorded live output without identifying reconstruction provenance. Scrubbing must not mutate the live evaluator or its state, and backfill must preserve its existing boundary around the live writer.

Alarms, latches, sequence state, and command controls are a separate boundary. Historical value colors cannot read today's active alarm latch and pretend it applied then. Static threshold comparison may use the selected value, identified as a threshold result. Historical alarm/sequence state requires persisted event reconstruction. Until available, show that status as unavailable in historical views; keep the live operational alarm summary explicitly labeled Live.

Display replay does not replay commands, edits, alarm acknowledgments, sequence starts, or control-system side effects. Historical ValueStrip editing is disabled. Existing command panels may remain live with an explicit Live label and live feedback, or offer a read-only historical view; do not feed a historical held value into a command default or permissive check.

## Replay transport

Use a monotonic clock and a captured base timestamp: `view_time = base_time + elapsed * rate`. Advance one controller clock, rather than creating a timer per widget. Start with forward rates 0.1×, 0.25×, 0.5×, 1×, 2×, 5×, and 10×; reverse playback is deferred. Pause commits the currently resolved instant. Seeking pauses, invalidates outstanding requests, and establishes a new base for the next Play.

If view time lies outside the selected playback bounds when Play is pressed, show the explicit action “Play from range start.” At the captured end, pause; do not automatically jump into Live. Offer looping as a subsequent small enhancement with the same fixed bounds. A fixed-duration step, configurable and initially one second, is deterministic across sensors. A “next sample” mode requires an explicitly selected reference trace and can follow later.

Prefetch a bounded window in the direction of travel and prioritize the current instant. The clock may continue while an individual component loads, with its loading state visible. If the session cannot provide history at all, pause and show a clear unavailable/buffering state; avoid a global barrier held by a disconnected ancillary sensor. Coalesce refreshes to the UI frame cadence, cache predecessor intervals, and avoid full archive scans per frame.

## Persistence and compatibility

Add an optional, versioned typed temporal configuration to `TileLayout` using its existing `#[serde(default)]` compatibility pattern. Store the range expressions, paused instant or live selection, scope/clock selection, timezone, and playback preferences. Persist a running replay as a paused resolved instant, never as an instruction to start playback on load. Do not persist transient hydration, monotonic time, or request revisions.

Continue accepting `global_time_range` and old per-panel range strings. Legacy `Latest` and `LAST 5m` mean Data end, matching the current implementation; migrate them to explicit Data end anchors. New UI “Last 5m” means Live. Label a migrated selection “Last 5m of data” so there is no silent behavioral change. Full range retains Data start/Data end anchors. Empty old state restores Full range and Live view time.

New writers emit the typed configuration as authoritative and a legacy-compatible range fallback for old readers, freezing unsupported anchors to the currently resolved interval when necessary. Document that older readers cannot restore replay intent. Update Rust struct literals and Python preset-builder behavior deliberately; optional fields need not force existing producers to emit the new schema. A future producer wanting Live or View time anchors must use the typed configuration.

This requires coordinated changes outside this crate in `../metor-proto/wkt/src/tile.rs` and related preset producers/tests. The plan does not assume those paths are writable in the current task. Existing unsaved edits and layouts must be preserved during implementation.

## Implementation sequence and validation

1. **Pure model and compatibility:** introduce anchor types, checked resolver, parser/formatter/completer, legacy adapter, and unit tests. Cover first/last decimals, uppercase M, compounds, fixed/mixed endpoints, date shorthand, civil days, invalid/empty/overflow cases, precision, anchor motion, single-sample data, and out-of-coverage requests. Check completion replacement spans and parser/completer agreement, including a cursor in the middle of a Unicode expression.
2. **Controller and scope:** add the shared temporal snapshot, clock abstraction, union/reference bounds, and observations. Bridge `GlobalTimeRange` so existing consumers can migrate incrementally. Verify two panels with disjoint source extents receive identical global endpoints, and independent ranges remain independent.
3. **As-of reader and first end-to-end slice:** implement strict predecessor selection, manifest completeness, loading/reset semantics, history observation, and shared caching. Migrate ValueStrip plus a gauge and a time-series plot first. Use a fake clock and two asynchronous signals to verify seeking backward, equality, no earlier sample, pauses, live ingestion during pause, late hydration, cancellation, and Go live.
4. **Shared time pages and autocomplete:** implement the time provider row, common page builders, and typed actions; register palette commands and connect titlebar/right-click entry points to the same pages. Add date/time and timezone completion, endpoint pages, presets, previews, coverage, and diagnostics. Add shared initial-query/provider-refresh support only where existing inspector APIs need it. Verify Tab inserts without committing, Enter respects the selected row's action, Escape/back discards uncommitted text, existing anchors reopen intact, and clock ticks preserve query/cursor/selection. Test same-field conflicts, unrelated-change merging, DST disambiguation, and fractional-second preservation. Run equivalent keyboard-only workflows through Anchored and Centered modes; no calendar or standalone time form is required.
5. **Complete consumer migration and historical semantics:** migrate map, attitude/3D, connectors, list-plot GPU path, tables, copy, expressions, and alarm/sequence presentation. Verify no future sample leaks through marker, pose, color, clipboard, or cached bounds. Stateful reconstruction limitations must be visible before enabling those outputs in historical mode.
6. **Transport and navigation:** implement shared play/pause/rate/step commands, captured replay bounds, linked range gestures, and prefetch. The optional overview scrubber uses those same actions and may follow the text workflow. Use deterministic clock tests for pause/resume/rate changes/end-of-range and rapid scrubbing under delayed remote responses. Compare toolbar, palette, and context-menu invocations of the same action.
7. **Persistence and cleanup:** add typed layout state and migration coverage for existing saved layouts and Python presets. Save/load anchored expressions without resolving them away; restore playing sessions paused. Replace the old preset-only rows with the shared completion pages and remove the temporary time bridge after all consumers migrate. Retain the shared inspector host and update the existing universal-binding documentation to describe temporal reads.

Run the crate's appropriate focused tests and `cargo check -p metor-panel` during implementation, then the relevant layout/proto and preset tests for changed shared schemas. Add performance measurements using representative multi-panel layouts: current-time scrubbing must not trigger unbounded remote fetches, duplicate expression replay per widget, or one full-history scan per frame. Documentation-only planning needs no code-test run.

Completion means a user can enter “The first 5M,” type or complete an instant inside it, see all following instantaneous values at that instant, play through the interval, and Go live with the chosen range/anchor behavior preserved. The entire workflow must be possible through both the command palette and right-click inspector using their shared rows and keyboard behavior, without a graphical calendar. Verify that opening time pages does not alter ordinary palette search or component-expression completion. The same workflow must work when history is remote, a signal has gaps, sources update at different rates, or data continues arriving during pause.


## UX revision after interactive review

Restore free-form local zoom; global changes must not continually override it.
Use the explicit plot-menu commands to coordinate views. Remove the additional
inline status/stale badges, especially from outline value strips; sample details
remain available on demand. Play/Pause in the titlebar use icons with tooltips.

“Last 5m” follows telemetry time by default, avoiding a wall-clock mismatch with
simulations or recorded sessions. Pause followed by Play resumes live monitoring.
“Play from range start” starts at the first available point inside that range,
skipping empty space before the archive begins. Date suggestions say “Entire day ·
YYYY-MM-DD” and insert the date, which resolves to that civil day in the selected
timezone. Complete suggestions apply with Enter and remain editable with Tab.

Timestamp labels use exactly three fractional digits (milliseconds) and short zone abbreviations (UTC, PST/PDT). Exact
microsecond values remain in editor inputs, saved state, and the view-time
tooltip. The existing double-chevron icon provides Go live directly beside view
time. Shared time settings and palette commands offer timestamp or elapsed/T0
display for view time, fixed range endpoints, and previews. T0 defaults to data
start, with an optional fixed reference timestamp saved in the layout. Inputs
accept `T0 + 2.5m`, `T-30s`, `T+00:05:00`, and ranges such as `T-30s .. T+1m`;
these resolve to exact fixed timestamps when applied. Selecting a display format
or reference leaves the selected time and range unchanged.

Execution timelines retain one scan worker and combine incoming range changes
while it is busy. Dropping a task handle cannot interrupt a synchronous history
scan already running, so restarting workers on each live tick can accumulate
CPU work. Live bounds keep updating independently; the next scan uses the newest
range. User edits and topology changes invalidate older results. Unchanged clock
snapshots no longer force window refreshes, and selected-value readers can use
the resident data head directly when it proves the requested predecessor.
