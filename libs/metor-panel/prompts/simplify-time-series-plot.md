# Plan: simplify `time_series_plot`

Scope: `src/elements/time_series/{mod.rs, line_plot.rs}`, plus the
inspectable-entity wiring in `src/tiles/{item.rs, panels.rs}`.

Non-goals: changes to `bounds.rs`, `gpu.rs`, shaders, `Trace` field
shape (the `Trace` struct is already a good reflection target).

## Current shape

**`LinePlot`** owns rendering + data tracking:
```
entries: Vec<Entry>              // config: Entity<Trace>, component, y_bounds, last_scan_ts
view_override, y_min_override, y_max_override, x_range
gpu_state, _tasks
```
Used by `Monitor`, `ComponentRow`, and `TimeSeriesPlot`.

**`TimeSeriesPlot`** owns interaction + chrome:
```
db, line_plot: Entity<LinePlot>                     // opaque
custom_title, x_range, y_min_override, y_max_override, traces: Vec<Entity<Trace>>   // facet
bound_trace_ids, drag_*, last_plot_area, on_open_page  // opaque
```

## Problems worth fixing

1. **Dead inspector fields.** `TimeSeriesPlot::{x_range, y_min_override,
   y_max_override}` are reflected to the inspector, but `on_notify`
   only detects trace-id changes and `rebind_traces` only rebinds
   traces — edits to these fields via reflection are never propagated
   to `LinePlot`. Either wire them up or delete them.
2. **Trace vec is duplicated.** `TimeSeriesPlot.traces` and
   `LinePlot.entries[i].config` store the same `Entity<Trace>` list.
   Kept in sync via an `on_notify` EntityId diff. Any inspector edit
   that adds/removes a trace triggers a full `bind_trace_entities`
   (which drops every tracker task and y-bounds watermark — even for
   unchanged traces).
3. **`LinePlot.entries` conflates three roles.** `config` (reflection
   target), `component` (lazy DB handle), and `y_bounds/last_scan_ts`
   (tracker state). Moving traces in/out of the Vec invalidates the
   latter two even though the work to recompute them is redundant.
4. **Title recomputed every frame.** `TimeSeriesPlot::title()` clones
   every `Trace` and does DB metadata lookups from inside `Render`.
   `tab_title` calls it too, so the same work happens at least twice
   per repaint.
5. **Render-time trace clone.** `render()` clones all `Trace`s into
   `trace_configs` solely to read `visible`, `color`, and `label` for
   the legend. The legend only renders when `len() >= 2`.
6. **Inspector can't reach `LinePlot`.** `inspectable_entity` returns
   `TimeSeriesPlot`, and `line_plot: Entity<LinePlot>` is `#[facet(opaque)]`.
   This forces every inspectable knob to be duplicated up to
   `TimeSeriesPlot` (see problem 1/2).

## Options

I see three viable directions. They're not mutually exclusive — you
can mix them.

### Option A — consolidate on `LinePlot`, make it the inspectable root

Change `PlotPanel::inspectable_entity` to return the `Entity<LinePlot>`
and make `LinePlot` itself `Facet`. The inspectable fields (`traces`,
`x_range`, `y_min_override`, `y_max_override`) live in one place;
`TimeSeriesPlot` loses its duplicates and its sync logic.

Pros: kills problems 1, 2, 6 outright. Monitor/ComponentRow are
unaffected (they don't use reflection). Smallest line-count win.

Cons: `custom_title` either moves to `LinePlot` (not great — Monitor
doesn't want a title) or we accept that the plot-level title lives
on the wrapper. I'd move it to `LinePlot` with a `None` default —
Monitor can just ignore it. The fields `entries`, `gpu_state`,
`_tasks`, `view_override` become `#[facet(opaque)]`.

### Option B — keep the split, fix the sync

Leave `TimeSeriesPlot` as the inspectable root, but actually route
the reflected fields to `LinePlot` on every `on_notify` (not just
when trace ids change). Store no state on `TimeSeriesPlot` other
than the reflected fields — compute `bound_trace_ids` diffs the same
way but also diff the scalars.

Pros: minimal structural change.

Cons: preserves the duplication. Problem 2 and 3 remain. Every
inspector knob still needs two places to live.

### Option C — unify `LinePlot` and `TimeSeriesPlot` into one type

Collapse into one element with a `Chrome` flags struct (legend, grid,
axes, interactivity). Monitor/ComponentRow pass `Chrome::none()`.

Pros: one type instead of two.

Cons: drags interaction state (`drag_*`, `last_plot_area`,
`on_open_page`) into Monitor's element. Bloats a simple sparkline
with handlers it never fires. I don't recommend this — the split
between "rendering engine" and "interactive container" is the one
part of the current design that earns its complexity.

**Recommendation: Option A.** It addresses the most problems, keeps
Monitor simple, and is a small-to-medium refactor. The rest of this
plan assumes Option A.

## Plan (Option A)

### Step 1 — split `Entry` tracking state into a keyed map

In `line_plot.rs`:

```rust
pub struct LinePlot {
    traces: Vec<Entity<Trace>>,                    // canonical, reflected
    tracking: HashMap<EntityId, TraceTracking>,    // opaque
    view_override: Option<PlotBounds>,             // opaque
    y_min_override: Option<f64>,                   // reflected
    y_max_override: Option<f64>,                   // reflected
    x_range: TimeRangeBehavior,                    // reflected
    custom_title: Option<SharedString>,            // reflected
    gpu_state: PlotRenderState,                    // opaque
    _tasks: Vec<gpui::Task<()>>,                   // opaque
}

struct TraceTracking {
    component: Option<Component>,
    y_bounds: Option<(f64, f64)>,
    last_scan_ts: Option<Timestamp>,
}
```

Keying by `EntityId` means:
- Inserting or reordering a trace no longer invalidates already-resolved
  `component` / `y_bounds` for unchanged traces.
- `bind_trace_entities` becomes "diff the set of ids, drop tracking for
  removed ids, spawn trackers for added ids". Tasks become a map too.
- The existing `bind_traces(Vec<Trace>)` helper (used by Monitor) wraps
  this by constructing `Entity<Trace>` handles.

Trackers keyed by `EntityId` instead of `idx` fix a latent bug: today,
if the inspector removes trace[0], tracker for old trace[1] now writes
to trace[0]'s slot.

### Step 2 — make `LinePlot` the reflection target

- `#[derive(facet::Facet)]` on `LinePlot`.
- `#[facet(opaque)]` on `tracking`, `view_override`, `gpu_state`,
  `_tasks`.
- Reflected fields: `traces`, `x_range`, `y_min_override`,
  `y_max_override`, `custom_title`.
- Observe self: when reflected state changes, reconcile `tracking` with
  `traces` (add/remove entries) and re-invalidate `view_override` if
  the y-overrides changed. Replaces today's `on_notify` on
  `TimeSeriesPlot`.

### Step 3 — slim `TimeSeriesPlot`

```rust
pub struct TimeSeriesPlot {
    db: Arc<DB>,                                    // opaque (still needed for title lookup)
    line_plot: Entity<LinePlot>,                    // opaque, the real state
    drag_start, drag_start_view, drag_zone,         // interaction
    last_plot_area,
    on_open_page,
}
```

No `Facet` derive needed anymore. Remove `traces`, `custom_title`,
`x_range`, `y_min_override`, `y_max_override`, `bound_trace_ids`,
`rebind_traces`, `on_notify`, `set_traces`. `set_custom_title` forwards
to `line_plot`. `title()` moves to `LinePlot` (or becomes a free
function that takes `&LinePlot` + `&DB`).

### Step 4 — change `inspectable_entity`

```rust
// tiles/panels.rs
fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
    Some(self.inner.read(cx).line_plot().clone().into_any())
    //            ^^^ need &App access — see question below
}
```

`PaneItem::inspectable_entity(&self)` has no `&App`, so we need the
`TimeSeriesPlot` to expose `line_plot()` without reading itself. It
already does (`line_plot()` returns `&Entity<LinePlot>`). So this
becomes `self.inner.line_plot_unchecked()` or we change the trait
signature. Simplest: add a `line_plot(&self) -> Entity<LinePlot>`
getter that doesn't need `&App` by caching the handle on `PlotPanel`
itself at construction.

Actually, cleanest: store `Entity<LinePlot>` directly on `PlotPanel`
alongside `inner` — it's a cheap clone — then `inspectable_entity`
returns that without any borrowing gymnastics.

### Step 5 — cache the title

Compute `title(&self)` from `traces` inside `LinePlot` and cache a
`SharedString`. Recompute only when the trace set changes or
`custom_title` changes — both are already hooks in step 2's
reconcile path. `tab_title` and `Render` both read the cached
value. Removes the per-frame DB lookup + `Trace` clones.

### Step 6 — render-path cleanups

- Legend: read `visible`, `color`, `label` directly from
  `Entity<Trace>` inside the loop; drop `trace_configs` clone.
- `custom_title` / `title()` calls already use cached value.
- Remove the `println!("on notify")` in `on_notify` if it survives
  anywhere (it shouldn't — `on_notify` is gone).

## What stays the same

- `Trace` struct — already clean, already `Facet`, good reflection target.
- GPU state, shaders, `bounds.rs`.
- `Monitor` and `ComponentRow` — they only use `bind_traces` and
  `Render`, both of which are unchanged.
- `Entity<Trace>` remains the unit of sharing with the inspector and
  with the trace picker.

## Open questions before I start

1. **Where does `custom_title` belong?** My preference is on
   `LinePlot` so it's inspectable alongside the traces. Monitor
   doesn't set it and there's no visual element for it inside
   `LinePlot::render` — the title is painted by `TimeSeriesPlot`.
   The field would exist only to be read back out. Acceptable?
2. **Do the reflected `y_min_override` / `y_max_override` / `x_range`
   fields on `TimeSeriesPlot` work today?** My reading says no — the
   `on_notify` sync only fires on trace-id change. If that's right,
   step 2 is a bug fix, not just a refactor. Want me to verify by
   editing those fields via the inspector before touching anything?
3. **Is `PlotPanel` storing the `Entity<LinePlot>` directly (step 4)
   acceptable**, or do you want the `PaneItem::inspectable_entity`
   trait signature to take `&App`? The former is a one-line change;
   the latter touches every `PaneItem` impl.

## Rough size

- `line_plot.rs`: +~40 lines (map + reconcile), gains Facet fields.
- `mod.rs`: −~80 lines (rebind/on_notify/duplicated fields/title gone;
  title stays in a thin wrapper).
- `panels.rs`: +2 lines (store `Entity<LinePlot>`), `inspectable_entity`
  changes one line.
- `item.rs`: unchanged.

Net: smaller, and the inspector reaches the real state instead of a
shadow copy.
