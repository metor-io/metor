# Plot shell unification

## Goal

Collapse the triplicated machinery *above* the shared GPU core into one generic
plot shell, so that:

- `xy_plot` and `list_plot` stop carrying near-verbatim copies of the
  interactive wrapper, the legend builder, and the line-plot entity scaffold;
- adding the next plot type (histogram, spectrogram) is a matter of writing a
  small backend (trace shape + tracker + `effective_view` + draw builder), not
  copy-pasting ~700 lines of pan/zoom/legend/GPU-readback boilerplate;
- `time_series` shares what is genuinely shared (the legend) now, and is kept
  out of the generic wrapper on purpose (justified below), with the shell's
  seams designed so it *can* join later without a reshape.

The GPU core (`src/views/time_series/gpu.rs`, `AxisSource`, `LineDraw`,
`PlotRenderState`) is already correctly shared across all three plot types and
is **out of scope** — we build strictly on top of it.

## Current state

### Layout of the three plots

| plot | wrapper (`mod.rs`) | entity (`line_plot.rs`) | view type | axes |
|------|--------------------|--------------------------|-----------|------|
| time_series | `TimeSeriesPlot` (1855 LOC file) | `LinePlot` (in `time_series/line_plot.rs`) | `PlotView` (multi-axis) | N |
| xy_plot | `XyPlot` (491 LOC file) | `XyLinePlot` (424 LOC) | `PlotBounds` (single) | 1 |
| list_plot | `ListPlot` (321 LOC file) | `ListLinePlot` (344 LOC) | `PlotBounds` (single) | 1 |

`src/views/plot_common.rs` already exists (34 LOC) and holds exactly one shared
helper, `reconcile_trackers`. Per the brief and prior guidance, this is the
home to extend rather than inventing a new module.

### Measured duplication

All counts from `diff`/`wc` on the current tree (post the timestamp/GPU-index
bug-fix wave).

**1. Interactive wrapper — `xy_plot/mod.rs` vs `list_plot/mod.rs`.**
`diff src/views/xy_plot/mod.rs src/views/list_plot/mod.rs` reports 234 changed
lines, but almost all of those are the doc comments, the trace struct, and the
~149-line block of `paint_xy_underlay`/`paint_xy_overlay` that only lives in
`xy_plot`. The actual interactive wrapper — `struct XyPlot`/`ListPlot`, its
`new`/`line_plot`/`title`/`view`/`reset_view`, and the entire `Render` impl
(mouse-down/up/move/scroll handlers, the underlay/overlay canvas pair, the
child insets, and the legend) — is **~260 lines that are identical except for
the type names `XyPlot`/`XyLinePlot`/`XyTrace` → `ListPlot`/`ListLinePlot`/
`ListTrace`.** The `reset_view` bodies are byte-identical; the four mouse
handlers are byte-identical; the two canvas children are byte-identical.

**2. Legend row — all three `mod.rs`.**
The legend block is **69 lines, present in three copies**:

- `xy_plot/mod.rs` (269–337) and `list_plot/mod.rs` (249–317) are **byte-for-byte
  identical** (`diff` reports zero differences).
- `time_series/mod.rs` (1710–1778) differs by **exactly one line**:
  `.pl(px(Y_LABEL_WIDTH + PADDING))` (xy/list) vs `.pl(chrome_left)` (ts). And
  `chrome_left == px(left_margin(axis_count))`, where `left_margin(1) ==
  Y_LABEL_WIDTH + PADDING` — so the two are the same value at `axis_count == 1`.

That is ~138 duplicated lines (two redundant copies) that collapse to one
parameterized helper.

**3. Line-plot entity scaffold — `xy_plot/line_plot.rs` vs
`list_plot/line_plot.rs`.**
Of ~400 lines each, roughly **150 lines are near-verbatim duplicate**:

- `OverrideSnapshot` struct + `capture` (18 lines) — identical modulo the
  `XyLinePlot`/`ListLinePlot` receiver type;
- the struct field block (`traces`, `x_min/x_max/y_min/y_max_override`,
  `custom_title`, `db`, `tracking`, `tasks`, `view_override`, `last_overrides`,
  `title_cache`, `gpu_state`) — identical modulo the `Tracking` element type;
- `new` (~24 lines) — identical modulo the default title string;
- `bind_traces`, `set_view_override`, `db`, `traces`, `trace_count`,
  `is_empty`, `title` (~40 lines) — identical;
- the `reconcile` override-snapshot + title-cache tail (~12 lines) — identical;
- the `Render` canvas/readback closure (~55 lines) — identical **except** the
  `filter_map` that builds `Vec<LineDraw>` and the `*_gpu_state` fn name;
- `derive_title` (~14 lines) — identical modulo the title strings.

The genuinely per-plot parts are: the `Tracking` type, `spawn_tracker`,
`effective_view`, and the `LineDraw` mapping in `Render`. Everything else is
scaffold.

### Downstream coupling (must keep compiling / behaving)

- `src/tiles/panels.rs` imports and constructs `XyPlot::new`, `ListPlot::new`,
  `TimeSeriesPlot::new/from_component`, and holds `Entity<XyLinePlot>` /
  `Entity<ListLinePlot>` in `XyPlotPanel`/`ListPlotPanel` for inspection and
  `to_config`/`from_config`.
- `src/inspector/registry/{builders,defaults}.rs` `downcast::<XyLinePlot>()`,
  `downcast::<ListLinePlot>()`, and `register_entity_list::<XyLinePlot,
  XyTrace>()` / `::<ListLinePlot, ListTrace>()`. These match on **`TypeId`**, so
  they keep working as long as the public names resolve to the same concrete
  type — which a `type` alias guarantees.
- `src/views/mod.rs` re-exports `ListPlot/ListLinePlot/ListTrace`,
  `XyPlot/XyLinePlot/XyTrace`, `TimeSeriesPlot/Trace`.

## Proposed design

Everything below lands in `src/views/plot_common.rs`. Four pieces, from lowest
risk to highest.

### 1. `legend_row` (+ `LegendTrace`)

```rust
pub trait LegendTrace: 'static {
    fn color(&self) -> Hsla;
    fn visible(&self) -> bool;
    fn set_visible(&mut self, v: bool);
    fn label(&self) -> SharedString;
}

/// Build the wrap-flow legend footer. `left_pad` is the plot's left chrome
/// width so labels line up with the plot area. `line_plot` is notified after a
/// visibility toggle so the wrapper repaints.
pub fn legend_row<Tr, L>(
    traces: &[Entity<Tr>],
    left_pad: Pixels,
    line_plot: &Entity<L>,
    cx: &App,
) -> Div
where Tr: LegendTrace, L: 'static
```

The per-trace `on_mouse_down` handlers only ever touch the trace entity, the
`line_plot` entity, `window`, and `cx` — never the wrapper `self` — so they can
be plain `move` closures instead of `cx.listener(...)`. That makes `legend_row`
independent of the parent wrapper type, so all three wrappers call it, including
`time_series` (passing `px(left_margin(axis_count))`).

`XyTrace`, `ListTrace`, and `Trace` all already have `color`/`visible`/`label`
fields, so the three `impl LegendTrace` blocks are trivial.

### 2. `paint_numeric_underlay` / `paint_numeric_overlay`

Move the two chrome painters currently in `xy_plot/mod.rs` (and imported
cross-module by `list_plot`) into `plot_common` under axis-neutral names — both
axes are numeric for xy *and* list, so the `xy_` prefix is a misnomer once list
shares them. This deletes the `list_plot -> xy_plot` import.

### 3. `PlotViewOps` — view-type abstraction

The mouse handlers differ between plots only in how they pan/zoom the view given
a hit `AxisZone`. Extract that into a trait so the shell's handlers are
view-agnostic:

```rust
pub trait PlotViewOps: Clone + 'static {
    fn axis_count(&self) -> usize;
    /// Pan given a pixel drag delta from the drag-start view.
    fn pan(self, zone: AxisZone, pa: Bounds<Pixels>, dx: Pixels, dy: Pixels) -> Self;
    /// Zoom about the pointer given a wheel factor.
    fn zoom(self, zone: AxisZone, pa: Bounds<Pixels>, pos: Point<Pixels>, factor: f64) -> Self;
}
```

`impl PlotViewOps for PlotBounds` is a mechanical lift of the existing xy/list
match arms (`offset_by_norm`/`offset_x`/`offset_y`, `zoom_at`/`zoom_x`/`zoom_y`,
`axis_count == 1`). This is what makes the shell "parameterized over view type"
per the brief: `time_series`'s multi-axis `PlotView` can later get its own
`impl PlotViewOps` (`offset_axis_y`/`zoom_y_all`/dynamic `axis_count`) and drop
into the same shell without touching the handler code.

### 4. `PlotShell<L>` + `ShellPlot` — the generic wrapper

```rust
pub trait ShellPlot: Render + Sized + 'static {
    type Trace: LegendTrace;
    type View: PlotViewOps;

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self;
    fn bind_traces(&mut self, traces: Vec<Self::Trace>, cx: &mut Context<Self>);
    fn traces(&self) -> &[Entity<Self::Trace>];
    fn title(&self) -> SharedString;

    fn effective_view(&self, cx: &App) -> Option<Self::View>;
    fn set_view_override(&mut self, v: Option<Self::View>, cx: &mut Context<Self>);
    fn reset_overrides(&mut self, cx: &mut Context<Self>);

    fn axis_count(&self) -> usize { 1 }
    fn paint_underlay(bounds: Bounds<Pixels>, view: &Self::View, window: &mut Window, cx: &mut App);
    fn paint_overlay(bounds: Bounds<Pixels>, view: &Self::View, window: &mut Window, cx: &mut App);
}

pub struct PlotShell<L: ShellPlot> {
    line_plot: Entity<L>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<L::View>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
}

pub type XyPlot = PlotShell<XyLinePlot>;
pub type ListPlot = PlotShell<ListLinePlot>;
```

`PlotShell` owns the identical `new` (build inner, observe it), `line_plot`,
`title`, `view`, `reset_view`, and the whole `Render` impl (the four mouse
handlers routed through `L::View: PlotViewOps`, the underlay/overlay canvases
routed through `L::paint_*`, the child inset, and a `legend_row(...)` call).
`XyLinePlot`/`ListLinePlot` already expose every method `ShellPlot` requires;
implementing it is thin.

Because `XyPlot`/`ListPlot` become **type aliases** to the same concrete
monomorphizations, `panels.rs`, the inspector `downcast`s, and the `views::`
re-exports keep compiling unchanged.

### 5. Line-plot entity core — `LinePlotCore<B>` (preferred) with fallback

`XyLinePlot` and `ListLinePlot` have **identical struct shapes** and — critically
— **identical inspector-reflected fields** (`traces`, the four overrides,
`custom_title`; everything else is `#[facet(opaque)]`). That invites collapsing
them into one generic entity:

```rust
pub trait LineBackend: Sized + 'static {
    type Trace: LegendTrace + facet::Facet<'static>;
    type Tracking: 'static;
    const DEFAULT_TITLE: &'static str;

    fn new_tracking() -> Self::Tracking;
    fn spawn_tracker(id: EntityId, trace: Entity<Self::Trace>, db: Arc<DB>,
                     cx: &mut Context<LinePlotCore<Self>>) -> Task<()>;
    fn effective_view(core: &LinePlotCore<Self>, cx: &App) -> Option<PlotBounds>;
    fn build_draws<'a>(core: &'a LinePlotCore<Self>, view: PlotBounds, cx: &App)
                       -> Vec<LineDraw<'a>>;
    fn derive_title(traces: &[Entity<Self::Trace>], cx: &App) -> SharedString;
}

#[derive(facet::Facet)]
pub struct LinePlotCore<B: LineBackend> {
    pub traces: Vec<Entity<B::Trace>>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    pub custom_title: Override<SharedString>,
    #[facet(opaque)] db: Arc<DB>,
    #[facet(opaque)] tracking: HashMap<EntityId, B::Tracking>,
    #[facet(opaque)] tasks: HashMap<EntityId, Task<()>>,
    #[facet(opaque)] view_override: Option<PlotBounds>,
    #[facet(opaque)] last_overrides: OverrideSnapshot,
    #[facet(opaque)] title_cache: SharedString,
    #[facet(opaque)] gpu_state: PlotRenderState,
}

pub type XyLinePlot = LinePlotCore<XyBackend>;
pub type ListLinePlot = LinePlotCore<ListBackend>;
```

The scaffold (`new`, `bind_traces`, `set_view_override`, `db`, `reconcile`,
accessors, the `Render` readback closure, `OverrideSnapshot`) lives once on
`LinePlotCore`; only `XyBackend`/`ListBackend` carry the ~200 lines of genuinely
different tracker/`effective_view`/`build_draws`.

**Pivotal risk:** this hinges on `#[derive(facet::Facet)]` working on a generic
struct where `B::Trace: Facet` (the workspace runs a *pinned facet fork*,
`metor-io/facet` @ `sphw/facet-gpui`, 0.44.x). A grep found **no existing
generic `Facet` derive in the tree**, so this is unproven. Hence Step 0 is a
spike, and there is a fallback that captures most of the win without any
facet-on-generic dependency:

**Fallback (5b):** keep the two concrete structs; extract the byte-identical
non-`Facet` pieces into `plot_common` free functions and call them from each:
`OverrideSnapshot` (moved), a `reconcile_overrides_and_title(...)` helper, a
`render_gpu_canvas(weak, gpu_state_fn, view_fn, build_draws_fn)` helper, and a
`derive_single_title(...)` helper. This still removes ~120 of the ~150
duplicated lines; it just leaves the field block + `new` + trivial accessors
concrete (≈30 lines/plot).

### Why not the alternatives

- **A `macro_rules!` that stamps out the three wrappers.** Rejected: the crate's
  style is "functions over macros; question duplication," and generic
  monomorphization gives better IDE/inspector ergonomics and keeps `TypeId`
  identity for the downcasts. A macro would hide exactly the code the inspector
  and serialization paths depend on.
- **`Box<dyn ShellPlot>` trait objects instead of generics.** Rejected: the
  inspector downcasts to the concrete entity type; erasing to a trait object
  breaks `TypeId` identity and adds vtable overhead for zero benefit.
- **A brand-new `plot_shell` module.** Rejected per the brief and prior
  guidance: extend `plot_common`, which already owns `reconcile_trackers`.
- **Folding `time_series` into `PlotShell` in this refactor.** Rejected — see
  the decision below.

### Decision: `time_series` joins the shared legend now, the shared *wrapper*
### later

Recommendation: **`time_series` adopts only `legend_row` in this pass; its full
wrapper unification is a follow-up.** `TimeSeriesPlot` carries four kinds of
complexity that `XyPlot`/`ListPlot` lack, and forcing them into the generic
shell now would make the abstraction leaky and tax the two simple callers:

1. **Different view type.** It uses multi-axis `PlotView`, not `PlotBounds`;
   pan/zoom go through `offset_axis_y`/`zoom_y_all`/`zoom_axis_y` with a *dynamic*
   `axis_count`. (The `PlotViewOps` seam is designed so this becomes just another
   `impl` later.)
2. **Extra input modes** interleaved in the mouse handlers: alt+left-drag opens a
   measurement cursor, right-click hit-tests/opens the cursor inspector, and a
   measurement-panel drag runs through `advance_panel_drag`/`end_panel_drag`.
   Each is an early-return woven into `on_mouse_down/up/move`.
3. **Extra overlays**: a cursor canvas, the measurement-panel div tree, alarm
   tint + limit lines, and per-axis left-edge value markers — extra child slots
   the shell would have to grow.
4. **Extra observers** (the alarm store).

Absorbing all four now means giving `PlotShell` pluggable per-event pre-handlers
and a variable set of overlay slots — abstraction the two simple plots would pay
for with no benefit. The higher-value, lower-risk move is: unify xy+list
completely, share the legend with time_series, and revisit time_series's wrapper
once histogram/spectrogram reveal which extension points are genuinely needed.
The `PlotViewOps` + `ShellPlot::paint_*` seams are shaped so that adoption is
additive, not a rewrite.

## Step-by-step migration

Each step ends with `cargo build -p metor-panel` green.

**Step 0 — Facet-on-generic spike (gates Step 5).**
In a scratch module, `#[derive(facet::Facet)]` a generic `struct Probe<B: Bar> {
xs: Vec<Entity<B::T>>, o: Override<f64> }` and build. If it compiles and the
`reflect` metadata is well-formed, take path 5a (generic core); else 5b
(helpers). Delete the scratch module. *Touches:* none permanent.

**Step 1 — Shared legend.**
Add `LegendTrace` + `legend_row` to `plot_common.rs`; `impl LegendTrace` for
`XyTrace`, `ListTrace`, `Trace`; replace the three inline legend blocks with
`legend_row(...)` calls. *Touches:* `views/plot_common.rs`,
`views/xy_plot/mod.rs`, `views/list_plot/mod.rs`, `views/time_series/mod.rs`.

**Step 2 — Shared numeric paint.**
Move `paint_xy_underlay`/`paint_xy_overlay` into `plot_common` as
`paint_numeric_underlay`/`paint_numeric_overlay`; update references in
`xy_plot` and `list_plot`; drop the `list_plot -> xy_plot` import. *Touches:*
`views/plot_common.rs`, `views/xy_plot/mod.rs`, `views/list_plot/mod.rs`.

**Step 3 — `PlotViewOps`.**
Add the trait + `impl PlotViewOps for PlotBounds` (lifting the existing match
arms). No call sites change yet. *Touches:* `views/plot_common.rs` (may
`use` `bounds::PlotBounds`).

**Step 4 — `PlotShell<L>` + `ShellPlot`.**
Add `PlotShell` and `ShellPlot` to `plot_common`; `impl ShellPlot for
XyLinePlot` and `for ListLinePlot`; replace the `XyPlot`/`ListPlot` structs with
`pub type XyPlot = PlotShell<XyLinePlot>` / `pub type ListPlot =
PlotShell<ListLinePlot>`; delete the old wrapper structs, their impls, and their
`Render` impls from the two `mod.rs`. Re-export the aliases from
`views/xy_plot/mod.rs` / `list_plot/mod.rs` so `views::mod.rs` and `panels.rs`
are untouched. *Touches:* `views/plot_common.rs`, `views/xy_plot/mod.rs`,
`views/list_plot/mod.rs`; verify only (no edits expected) `tiles/panels.rs`,
`inspector/registry/*`, `views/mod.rs`.

**Step 5a — Generic entity core (if Step 0 passed).**
Add `LineBackend` + `LinePlotCore<B>` to `plot_common` (or a new
`plot_common`-adjacent submodule if the file gets large); move
`OverrideSnapshot` in; define `XyBackend`/`ListBackend` carrying `Tracking`,
`spawn_tracker`, `effective_view`, `build_draws`, `derive_title`; replace the
two concrete structs with `pub type XyLinePlot = LinePlotCore<XyBackend>` /
`ListLinePlot = LinePlotCore<ListBackend>`. *Touches:*
`views/plot_common.rs`, `views/xy_plot/line_plot.rs`,
`views/list_plot/line_plot.rs`; verify `inspector/registry/*` still downcasts.

**Step 5b — Helper extraction (fallback).**
Move `OverrideSnapshot` into `plot_common`; add
`reconcile_overrides_and_title`, `render_gpu_canvas`, and `derive_single_title`
helpers; call them from the two concrete `line_plot.rs`, keeping the field block
+ `new` + accessors concrete. *Touches:* same files as 5a.

**Step 6 — Cleanup.**
Delete dead imports; rewrite the module docs on the three `mod.rs` and
`plot_common.rs` to describe the new shared shell and how to add a plot type;
`cargo clippy -p metor-panel` clean. *Touches:* the four files.

## Risks and how to test

- **Facet-on-generic (highest).** Gated by Step 0; 5b fallback if it fails.
  *Test:* build, then open the inspector on an XY plot and a list plot and
  confirm the trace list and the four override fields render and edit (exercises
  `register_entity_list::<XyLinePlot, XyTrace>` and the `downcast::<XyLinePlot>`
  path). Alias `TypeId` identity must hold — verify the "Add trace" wizard still
  fires.
- **Pan/zoom parity.** The `PlotViewOps` lift must reproduce the exact arms
  (`Plot`/`XAxis`/`YAxis`). *Test:* on both xy and list — drag-pan in the plot
  body, over the X axis, over the Y axis; scroll-zoom in each zone;
  double-click-to-reset.
- **Legend parity.** *Test:* toggle a trace's visibility (opacity dims + auto-fit
  re-fits the remaining trace); right-click a legend entry opens the inspector at
  the pointer — on all three plots, including `time_series` (its one-line pad
  difference).
- **GPU readback ordering.** The canvas closure's `weak.update` →
  `take_pending_release` → `drop_image` sequence is subtle; if extracted
  (`render_gpu_canvas`) keep the exact order. *Test:* plots paint, resize
  repaints, no `drop_image` panic and no image-handle leak over a minute of live
  data.
- **Serialization.** `XyPlotPanel`/`ListPlotPanel` `to_config`/`from_config` are
  untouched and names are preserved, so no `SUPPORTED_LAYOUT_VERSION` bump.
  *Test:* save a layout with xy + list panels, reload, confirm traces/overrides
  round-trip.
- **Legend closure form.** `legend_row` uses plain `move` closures on
  `.on_mouse_down`. If gpui's `Div` rejects the non-listener form here, fall back
  to passing a `WeakEntity<L>` and `cx.listener` — note, not a blocker.
- **Regression net.** `cargo test -p metor-panel` (the existing `time_series`
  tick tests must stay green); add a unit test asserting `PlotViewOps::pan/zoom`
  on `PlotBounds` matches the pre-refactor results for a Plot/XAxis/YAxis drag.

## Estimated scope

| file | before | after (5a) | delta |
|------|-------:|-----------:|------:|
| `views/plot_common.rs` | 34 | ~360 | +326 |
| `views/xy_plot/mod.rs` | 491 | ~95 | −396 |
| `views/list_plot/mod.rs` | 321 | ~70 | −251 |
| `views/xy_plot/line_plot.rs` | 424 | ~200 | −224 |
| `views/list_plot/line_plot.rs` | 344 | ~130 | −214 |
| `views/time_series/mod.rs` | 1855 | ~1795 | −60 |
| **net** | | | **≈ −820** |

Verify-only (expected zero edits): `tiles/panels.rs`,
`inspector/registry/{builders,defaults}.rs`, `views/mod.rs`.

Gross deletion ≈ 1140 LOC of duplication; ≈ 360 LOC of shared shell added; **net
≈ −800 LOC**, with one place to add histogram/spectrogram. With the 5b fallback
the numbers are ~40 LOC less favorable (the field block + `new` stay concrete)
but the wrapper + legend + paint wins are unchanged.
