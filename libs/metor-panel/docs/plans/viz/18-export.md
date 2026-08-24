# 18 — Export

Item 18 of `docs/plans/telemetry-viz-additions.md`.

## Summary

Add CSV export for a plot's visible traces (raw DB samples over the visible time
window, not the decimated LOD copy) and for the component table / data table's
current rows, both reached as command rows in the existing inspector/palette
system rather than a new toolbar or right-click menu. PNG export of a pane is
**not implemented** — gpui 0.2.1 exposes no way to read back a rendered
element's or window's pixels; see the feasibility verdict below. This plan
covers CSV only and scopes PNG out explicitly rather than hand-waving it.

## Reuse vs. new

- **File dialog**: `Window::prompt_for_new_path`, already used for preset save
  (`src/inspector/palette.rs:484`, `preset_save_rows`). CSV export reuses the
  identical `receiver.await` → `cx.update` → `std::fs::write` shape; no new
  dialog plumbing.
- **Command surfacing**: reuses the whole-type inspector builder pattern
  (`InspectorRegistry::register_type_builder`), the same mechanism behind
  "Add Model" / "Reset Camera" on `Viewer3d` (`src/inspector/registry/defaults.rs:435`,
  `register_viewer3d_builder`) and the tab-bar toggles on `Pane` (`:461`). This
  is the standing "pane-level actions live in the palette/inspector, not
  right-click on content" pattern — no new UI surface, no gesture competing
  with drag/drop.
- **Raw sample decode**: reuses `read_element_f64` (`src/views/time_series/gpu.rs:1717`),
  the exact per-sample scalar decoder the GPU line-draw path already uses, so
  CSV values match what the plot renders (modulo LOD, which export
  deliberately skips — see Design).
- **History query**: reuses `TimeSeries::get_range` (`libs/db/src/time_series_2.rs:940`)
  and `TimeSeriesNodeSlice` (`:255`), the same API `LinePlot::effective_view`
  and `update_lod_state` already call for bounds tracking. No new metor-db
  surface.
- **Remote hydration**: reuses `TimeSeries::coverage` (`libs/db/src/time_series_2.rs:393`)
  and `crate::hydration::hydrator` (`src/hydration.rs`), the same pair
  `LinePlot::gap_bands` (`:286`) uses to request remote-only spans for the LOD
  gap overlay.
- **Table value formatting**: reuses `current_value_string`
  (`src/views/component_table.rs:92`, private today — promote to
  `pub(crate)`) instead of a second value-to-string path.
- **New**: a small `src/export.rs` module (CSV assembly + prompt/write helper)
  and one new dependency, `csv` — already resolved at 1.4.0 in the workspace
  lockfile as a transitive dependency, so this is a direct `[dependencies]`
  line, not a new version to negotiate.
- **New, minimal, non-trait surface**: one `export_rows` method added
  directly to `ComponentTableDelegate` and to `DataTableGrid`, not a new
  `TableDelegate` trait method — only these two concrete types are in scope
  and their row models (flat metas vs. grouped instances) don't share a
  natural shape.

## Design

### CSV: plot traces

`LinePlot` (`src/views/time_series/line_plot.rs:124`) owns `traces:
Vec<Entity<Trace>>`, `db: Arc<DB>` (via `LinePlot::db()`), and the methods
that already answer "what's visible": `effective_view(cx)` (`:414`) returns
`PlotView { x: (min_x, max_x), .. }` in the same f64-microsecond space
`update_lod_state` converts to `Timestamp` for LOD/coverage decisions
(`Range<Timestamp>` built as `Timestamp(min_x as i64)..Timestamp(max_x as i64)`,
`:299-300`). Export reuses that exact conversion — the visible range is not
new plumbing, it already drives what the plot fetches.

For each visible `Trace` (`cfg.visible`, `Trace::component_id`,
`Trace::element_index` — `src/views/time_series/mod.rs:1085`):

1. Resolve the live `Component` the same way `TraceTracking` does (already
   held in `LinePlot`'s per-trace tracking map keyed by `EntityId`).
2. Call `component.time_series.coverage(range.clone(), &mut gaps)`
   (`libs/db/src/time_series_2.rs:393`). For any `RemoteOnly` gap, call
   `crate::hydration::hydrator(cx)?.request(component_id, gap.range)` — the
   same call `gap_bands` makes (`line_plot.rs:314-321`) — then poll
   `component.time_series.wait()` in a loop bounded by a timeout (proposed:
   10s). A "Preparing export…" label covers the wait; on timeout, export
   what's resident and log `tracing::warn!(component_id, ?gaps, "export
   missing remote-only span")` rather than blocking indefinitely.
3. Call `component.time_series.get_range(range.clone())` (`:940`) — **always
   the raw `component.time_series`, never a `lod_levels` companion.** Direct
   application of "never hide a glitch": the on-screen plot may be rendering
   the min/max LOD envelope because the window is over `RAW_SAMPLE_BUDGET`,
   but the export must be real samples, not synthetic envelope points.
4. Walk `TimeSeriesSlice::as_iter()` (`:282`, newest-first per node) into a
   `Vec<TimeSeriesNodeSlice>`, reverse it, and decode `(Timestamp, f64)` per
   node with `read_element_f64(&schema, slice.data(), i, element_index)`
   rather than `iter_values` — `iter_values` parses the whole tensor into a
   `ComponentView`; export only needs the one scalar element the trace plots,
   and `read_element_f64` is the exact decode path the renderer already uses.

**Row model.** Traces are independent time series and are not resampled onto
a common grid for export (resampling would fabricate values — the same
"never hide a glitch" reasoning). Instead, k-way merge every trace's
`(Timestamp, f64)` stream into one ascending-timestamp sequence and emit one
CSV row per distinct timestamp, with a blank cell for any trace that has no
sample at that exact timestamp — the sparse-union shape Foxglove and
PlotJuggler both use for multi-topic CSV export. Header: `timestamp_us`, then
one column per trace using `Trace::label` (falling back to `"component#element"`
when empty, matching how the plot itself labels an untitled trace). Timestamps
are emitted as raw microseconds (`Timestamp.0`), not formatted — a CSV
consumer should not have to guess a timezone.

**Size guard.** A trace over `RAW_SAMPLE_BUDGET` (`line_plot.rs`, 1,000,000)
is exactly the case the plot itself refuses to draw raw. Export honors the
"raw always" rule but should not silently write a multi-hundred-MB file: before
writing, sum `estimate_samples(range)` (`libs/db/src/time_series_2.rs:802`)
across visible traces and, above a threshold (proposed: 2,000,000 total
samples), show a confirm row ("Export N samples across M traces — this may
take a while and produce a large file") before proceeding.

### CSV: tables

`ComponentTableDelegate` (`src/views/component_table.rs:117`) holds `metas:
Vec<RowMeta>` (`id`, `name`, `component`) independent of the visible-row
entity cache (`row_cache`), so a full-table export needs no live
`ComponentRow` entities — it walks `metas` directly and calls
`current_value_string(&self.db, &meta.component)` (`:92`, promote to
`pub(crate)`) per row. Columns: `Name`, `Value` — the `Sparkline` column
(`columns()`, `:166`) has no textual representation and is dropped for CSV.

`DataTableGrid` (`src/views/data_table/grid.rs:24`) holds `rows: Vec<RowState>`,
each a `GroupInstance` (`name`, `field_ids: Vec<Option<ComponentId>>`,
`src/views/data_table/grouping.rs:15`) plus the active `Group` (`fields: Vec<SharedString>`,
`:9`) for column names. Export walks `rows`, resolving each `field_ids[i]` to
a `Component` via `db` and formatting with the same `current_value_string` (or
directly `format_value`, `src/views/format.rs:27`, for a field that's `None`
→ blank cell). Columns: `Name` plus one column per `Group::fields` entry.

Both delegates get a small `pub(crate) fn export_rows(&self, cx: &App) ->
(Vec<String>, Vec<Vec<String>>)` (header, rows) rather than a new
`TableDelegate` trait method — `ColumnBrowserDelegate` (which `DataTableGrid`'s
owner, `DataTable = ColumnBrowser<DataTableDelegate>`, implements) and
`TableDelegate` are different traits, and only these two concrete types are in
scope. `DataTableDelegate` (`src/views/data_table/mod.rs:33`) needs a
`pub(crate) fn grid(&self) -> &Entity<Table<DataTableGrid>>` accessor (its
`grid` field, `:37`, is currently private) so the inspector builder for
`DataTablePanel` can reach the nested grid entity through
`DataTablePanel.inner.read(cx).delegate().grid()`.

### PNG feasibility verdict: not feasible with gpui 0.2.1's public API

gpui 0.2.1 (`~/.cargo/registry/.../gpui-0.2.1`) has no element-to-image or
window-readback API: no `read_pixels`, no `copy_texture_to_buffer`, no
`render_to_image`, nothing named `screenshot` anywhere in the crate. The only
image-producing capability is `ScreenCaptureSource` (`src/platform.rs:314`),
a *desktop screen-share* primitive (macOS ScreenCaptureKit, X11/Wayland
portals, Windows) that streams frames from an entire physical display, not a
window or an element. Using it for "PNG of a pane" would require: OS
screen-recording permission (a user-facing prompt, different per platform);
resolving the window's on-screen bounds (`Window::bounds()`, `window.rs:1688`)
and the pane's bounds within it; starting a capture stream for whatever
display the window sits on; cropping one frame in physical pixels (accounting
for `scale_factor()`, `:1815`, and multi-monitor offsets); and living with the
fact that this backend has no z-order or damage concept, so any window
overlapping the pane at capture time corrupts the result. That is a
fundamentally different, far heavier feature than "export what this pane is
showing" — closer to "take a screen recording and hope nothing occludes it"
— with separate code paths per platform (`platform/mac/screen_capture.rs`,
`platform/scap_screen_capture.rs`, `platform/linux/{wayland,x11}/client.rs`).
**Recommendation: scope PNG out of this item entirely.** If it's worth doing
later it's a separate, harder plan built on `ScreenCaptureSource`, not an
extension of the CSV work here.

### Action surfacing

Both exports are inspector command rows, added via
`InspectorRegistry::register_type_builder` in
`src/inspector/registry/defaults.rs`, following the `Viewer3d`/`Pane`
convention: call the existing `default_rows_for_any_entity` (for `LinePlot`,
which is `#[derive(facet::Facet)]` and today has no type builder at all — its
inspector page is pure field reflection) or build the row list from scratch
(for `TablePanel`/`DataTablePanel`, which have no `Facet` derive and today
show nothing when inspected), then push one `CommandRow` at the end.

Reachability matches the existing per-panel property editor: `PlotPanel`
already routes `inspectable_entity()` (`src/tiles/panels.rs:893`) to its
`line_plot: Entity<LinePlot>`, so opening that panel's property page (however
it's opened today — the palette's "Panel" category, `register_panel_provider`,
`src/inspector/palette.rs:231`, or an anchored per-pane settings affordance)
surfaces "Export Visible Range (CSV)…" as the last row alongside the reflected
trace/axis fields. `TablePanel` and `DataTablePanel` currently override
neither `inspectable_entity()` nor register any builder, so their property
page is empty today; adding `register_type_builder::<TablePanel>` and
`register_type_builder::<DataTablePanel>` both establishes their first
inspector content and adds "Export Table (CSV)…". No new palette category, no
right-click on plot/table content.

## Implementation steps

1. **`libs/metor-panel/Cargo.toml`**: add `csv = "1"` to `[dependencies]`
   (already resolved at 1.4.0 in the workspace lockfile).
2. **`src/views/time_series/gpu.rs`**: change `read_element_f64` (`:1717`)
   from module-private to `pub(crate)`.
3. **`src/views/component_table.rs`**: change `current_value_string` (`:92`)
   to `pub(crate)`.
4. **`src/views/data_table/mod.rs`**: add `pub(crate) fn grid(&self) ->
   &Entity<Table<DataTableGrid>>` to `DataTableDelegate` (`:33`), returning
   `&self.grid`.
5. **`src/export.rs`** (new, `pub(crate) mod export;` in `src/lib.rs`):
   - `pub(crate) fn prompt_and_write_csv(window, cx, default_name:
     SharedString, header: Vec<String>, rows: Vec<Vec<String>>)` —
     synchronous-rows variant for tables: opens `cx.prompt_for_new_path(&initial_dir,
     Some(&default_name))` (mirroring `preset_save_rows`, `palette.rs:484`)
     and writes with the `csv` crate inside a `cx.spawn` task on confirm
     (matching the `std::fs::write` shape at `palette.rs:496`).
   - `pub(crate) fn prompt_and_export_plot_csv(line_plot: Entity<LinePlot>,
     window, cx)` — the async variant: prompts first (don't do work the user
     might cancel), then resolves the visible range, runs the
     coverage/hydrate/wait loop, decodes samples, and k-way-merges rows.
     Decode and merge run on `cx.background_executor().spawn(...)` (pattern
     already used at `line_plot.rs:689` for LOD bucket conversion), since a
     multi-trace, near-`RAW_SAMPLE_BUDGET` export is real CPU work that must
     not hitch the frame loop.
   - A pure `merge_traces(traces: Vec<Vec<(Timestamp, f64)>>, labels:
     Vec<SharedString>) -> (Vec<String>, Vec<Vec<String>>)` helper, unit
     tested directly (three traces, partially overlapping timestamps, at
     least one gap) without a `DB` or gpui context.
6. **`src/inspector/registry/defaults.rs`**: add
   `register_line_plot_export_builder` (called from `register_defaults`
   alongside `register_trace_builder`, `:93`) that wraps
   `default_rows_for_any_entity` and appends the CSV command row; add
   `register_table_export_builders` covering both `TablePanel` and
   `DataTablePanel`.
7. **Manual verification**: a plot with an over-`RAW_SAMPLE_BUDGET` window
   (LOD actively rendering) exports raw samples, not the envelope — diff
   sample count in the CSV against `estimate_samples` for that range. A
   component table and a grouped data table export with the field/name
   columns matching what's on screen. A plot pointed at a connected target
   with a remote-only span in its visible window hydrates before exporting
   (or times out and warns) rather than silently omitting data.
8. **Tests**: `merge_traces` unit tests (step 5); a `metor-db` integration
   test asserting `get_range` + `read_element_f64` over a seeded
   `TimeSeries` reproduces the values written by the ingest side (guards the
   raw-decode path independent of gpui).

## Open questions

- **Hydration timeout UX.** A hard 10s timeout with a warn log is a
  placeholder; is a blocking modal ("waiting for remote history…") with a
  cancel button worth the extra state machine for v1, or is silent
  best-effort acceptable given this only affects a *connected-target* (not
  local) workflow?
- **Size-guard threshold.** 2,000,000 total samples before confirming is a
  guess; should it instead key off estimated *file size* (samples × trace
  count × ~20 bytes/cell), which is closer to what the user actually cares
  about?
- **Timestamp formatting.** Raw microseconds is the least lossy choice but
  the least friendly one to open in a spreadsheet. Worth a second, human
  `timestamp_utc` column redundant with `timestamp_us`, the way some
  telemetry tools emit both?
- **XY plot / list plot.** This plan covers `LinePlot` (the time-series plot)
  only. `XyLinePlot` and `ListLinePlot` have their own trace types
  (`XyTrace`, `ListTrace`) and would need the same treatment separately —
  worth doing in the same pass, or a follow-up once the CSV helper in
  `src/export.rs` proves out on the time-series case?
- **Dashboard / free-form canvas.** No table or plot lives at the dashboard's
  top level (`DashboardPanel`) — only individual bound widgets. Is
  "export everything on this dashboard" a real ask, or is per-widget export
  (once widgets wrap `LinePlot`/`Table` internally) sufficient?
