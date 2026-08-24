# 20 — Manual plot annotations

## Summary

Let operators place their own markers, notes, and shaded regions on the
time axis. One `Annotation` shape (a labelled `t_start..t_end` span, where
`t_start == t_end` is a point marker) covers all three prior-art forms. Point
annotations reach the plot as a fourth `EventSource`, so the entire flag
gutter — clustering, chips, hover and pinned popovers, and per-plot opt-in via
the existing `EventOverlay` list — works with no new rendering. Spans get one
new underlay painter. Authoring reuses the measurement cursor: a cursor already
selects a time range and already opens the inspector, so "Save as annotation"
is a row on the cursor inspector rather than a new gesture.

## Reuse vs. new

Extended:

- `src/plot_events/mod.rs` — `EventKindKey` gains `Annotations`, `EventDetail`
  gains `Annotation(Annotation)`, and `EventSourceRegistry` learns the
  built-in. `kind_key_to_string`/`from_string` gain `"annotations"`.
- `views/time_series/event_overlay.rs` — unchanged; annotations are enabled on
  a plot by adding an `EventOverlay` with the new key, so
  `EventOverlayConfig`, the overlay wizard, and the visibility toggle all
  apply unchanged.
- `views/time_series/mod.rs` — `sync_event_sources`, `build_event_clusters`,
  `event_cluster_at`, `event_hover_popover`, `event_pinned_popover`, and
  `observe_event_target` all pick up annotations for free; only
  `observe_event_target` needs a new downcast arm for the store.
- `views/time_series/event_flags.rs` — `mix`, `fit_label`, and the `CHIP_*`
  geometry constants go from private to `pub(super)` so the band painter draws
  an identical chip. No duplicated chip code.
- `views/time_series/cursor_inspector.rs` — one new `CommandRow`
  ("Save as annotation"), beside the existing "Delete cursor" precedent.
- `inspector/registry/defaults.rs` — `register_annotation_builder`, a direct
  copy of the shape of `register_measurement_cursor_builder`.
- `inspector/palette.rs` — an `ItemRegistry` provider for the standalone
  "Add annotation at now" command (a palette entry, not a content right-click:
  the plot's right-click already belongs to the cursor inspector).
- `libs/metor-proto/wkt/src/tile.rs` — `TileLayout` gains `annotations`.

New: `src/annotations/mod.rs` (`Annotation`, `AnnotationId`,
`AnnotationStore`, `AnnotationSource`) and
`views/time_series/annotation_bands.rs` (`AnnotationBand`,
`paint_annotation_bands`).

## Design

### One shape

```rust
// src/annotations/mod.rs
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct Annotation {
    #[facet(skip)] pub id: AnnotationId,
    #[facet(skip)] pub t_start: Timestamp,
    /// Equal to `t_start` for a point marker; a span otherwise.
    #[facet(skip)] pub t_end: Timestamp,
    /// Chip text on the plot and the popover header.
    pub label: SharedString,
    /// Longer body, shown only in the popover.
    pub note: SharedString,
    /// `Auto` resolves to `theme.text_secondary` — annotations are operator
    /// chrome, and the alarm palette is never reused decoratively.
    pub color: Override<Hsla>,
    pub visible: bool,
}
```

A "vertical marker", a "text note", and a "shaded region" are the same record
with a different span and a different amount of text. Timestamps are edited
through the inspector as absolute values, or set by the authoring gestures
below.

### Store

`crate::annotations::AnnotationStore` is an app-global `Entity`, mirroring
`crate::logs`, `crate::alarms`, and `crate::sequences` exactly — `init(cx)`,
`try_global(cx)`, a `generation: u64` bumped on every mutation, and
`add`/`remove`/`update`/`in_range`. Every plot already knows how to observe a
store of this shape.

### Rendering: points through the event pipeline, spans through one painter

```rust
pub struct AnnotationSource;   // key = EventKindKey::Annotations

impl EventSource for AnnotationSource {
    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent> {
        // Zero-width annotations only; spans are drawn as bands.
    }
    fn observe_target(&self, cx: &App) -> Option<AnyEntity>;  // the store
    // name/default_color/generation as usual
}
```

`EventDetail::Annotation` lets `event_detail_element` render the note and, in
place of the JSON tree, an "Edit annotation" `NavRow` into the annotation's own
inspector page.

Spans can't be flags, so `views/time_series/annotation_bands.rs` adds:

```rust
pub(super) struct AnnotationBand { pub x0: Pixels, pub x1: Pixels, pub color: Hsla, pub label: String }
pub(super) fn paint_annotation_bands(outer, view, bands, window, cx);
```

Painted in the **underlay** canvas, after the alarm tint and before the
gridlines, so traces stay on top: a translucent quad (`mix()` from
`event_flags.rs`, the same blend the chips use), a 1 px rule at each edge in
the full color, and one chip at the band's top-left truncated to the band width
by `fit_label`. Bands are gated by the same `EventOverlay` visibility check as
flags, so a plot opts into annotations once and gets both forms.

Hit-testing and the popover: a band's chip joins the gutter hit test —
`event_cluster_at` already scans a `GUTTER_H` strip and picks the nearest
chip; band chips are appended to the same per-frame list with their span
carried through, so hover/pin/edit behave identically for markers and regions.

### Authoring, without a new gesture

The measurement cursor already does the hard part: alt+left-drag selects
`(t_start, t_end)`, snaps to samples, pins the view for the drag, and opens the
inspector on release. Annotations ride it:

- `cursor_inspector::build_cursor_rows` gains **"Save as annotation"**, which
  writes `MeasurementCursor::ordered()` into the store as a new `Annotation`
  and removes the cursor. A zero-length drag is already discarded as a click,
  so point markers come from the palette command instead.
- `inspector/palette.rs` registers `Category::Command` entries **"Add
  annotation at now"** and **"Add annotation…"** (the latter pushes an
  inspector page for a fresh annotation with editable timestamps).
- Existing annotations are editable and deletable from their popover
  (`NavRow` → the annotation's page) or from a palette submenu listing them,
  built the way `ItemRegistry` builds its other `SubMenu`s. Deletion mirrors
  `cursor_inspector::delete_action_row` — a `CommandRow::action` returning
  `RowAction::Dismiss`.

### Persistence: the layout document

Annotations go in `TileLayout`, next to `global_time_range` — which is the
existing precedent for app-global view state riding the layout doc:

```rust
// libs/metor-proto/wkt/src/tile.rs
pub struct TileLayout {
    pub version: u32,
    #[serde(default)] pub global_time_range: String,
    #[serde(default)] pub annotations: Vec<TileAnnotation>,
    pub root: TileNode,
}
```

`TileGroup::serialize` snapshots the store; `TileGroup::deserialize` restores
it. Rationale: an annotation is not a property of one pane (the same test point
is meaningful on every plot), it must travel with a per-target saved layout,
and a target preset can then ship test points alongside its panes.

Rejected: **per-plot config** (`PlotPanelConfig.annotations`) — it duplicates
one marker into every plot that shows it and makes "the same event" N records.
**DB-backed** — the right long-term home once annotations need to be shared
between operators or aligned with a recording rather than a layout; leave the
seam by keeping every mutation behind `AnnotationStore`'s methods so a
pluggable backend can be introduced later (the `NodeStore` trait-backend
precedent) without touching a single call site.

Bump `TILE_LAYOUT_VERSION`; old layouts are rejected, not migrated.

## Implementation steps

Each step ends with `cargo build -p metor-panel` green.

1. **Store.** New `src/annotations/mod.rs`: `AnnotationId` (atomic counter, the
   `CursorId` idiom), `Annotation`, `AnnotationStore` with `init`/`try_global`/
   `generation`/`in_range`, registered in `src/app.rs` beside the other global
   stores. Unit-test `in_range` inclusion at the span endpoints.
2. **Event source.** `AnnotationSource` implementing `EventSource`;
   `EventKindKey::Annotations` and its string round trip in
   `src/plot_events/mod.rs` (extend the existing `kind_key_*` tests);
   registration in `EventSourceRegistry`; `EventDetail::Annotation` and its arm
   in `event_detail_element`. Add the store downcast to
   `TimeSeriesPlot::observe_event_target`. At this point point-annotations
   already render as flags on any plot with the overlay enabled.
3. **Bands.** Promote `mix`, `fit_label`, and `CHIP_*` to `pub(super)` in
   `views/time_series/event_flags.rs`; add
   `views/time_series/annotation_bands.rs`; snapshot spans in the underlay
   canvas prepare closure in `views/time_series/mod.rs::Render` and paint them.
4. **Band hit-testing.** Append band chips to the per-frame chip list consumed
   by `event_cluster_at` so hover and pin work on regions.
5. **Inspector.** `register_annotation_builder` in
   `inspector/registry/defaults.rs` (page with label/note/color/visible plus a
   Delete action); the "Edit annotation" `NavRow` from the popover.
6. **Authoring.** "Save as annotation" row in
   `views/time_series/cursor_inspector.rs`; the two palette commands in
   `inspector/palette.rs::register_builtin_providers`.
7. **Persistence.** `TileAnnotation` in `libs/metor-proto/wkt/src/tile.rs`;
   snapshot/restore in `TileGroup::serialize`/`deserialize`
   (`src/tiles/mod.rs`, `src/tiles/serial.rs`); bump `TILE_LAYOUT_VERSION` and
   add the history line — plans 02, 09, and 13 bump too, so take one shared
   bump per release. Round-trip test in `tiles/serial.rs`.
8. **Docs.** Module doc on `src/annotations/mod.rs` stating the one-shape
   design and the store-not-pane ownership rule; a line in
   `src/plot_events/mod.rs`'s module doc noting that the fourth source is
   operator-authored rather than store-derived.

## Open questions

- **Scope.** Annotations are global to the layout; per-plot visibility is the
  existing `EventOverlay` opt-in. Does anyone actually want "this note belongs
  only to this pane"? If so it is an `Option<PaneId>` filter on the source,
  not a second storage location.
- **Y-anchored notes.** Ignition callouts can be pinned to a `(t, value)`
  point, not just a time. That needs an axis reference and a leader line — the
  dashboard's `connectors.rs` already draws leaders and is the machinery to
  reuse if it is wanted. Out of scope here.
- **XY and list plots.** Annotations are timestamps, so they only apply to the
  time axis. The XY analog is plan 13's `XyReference` (a reference point is a
  one-point table).
- **Overlap.** Nested or overlapping bands will stack their tints. Cap at a
  fixed alpha per pixel column, or offset chips vertically like the flag
  gutter does horizontally — decide once real usage exists.
- **Recording alignment.** Annotations are stamped in panel time
  (microseconds, the same clock traces use). Under a simulated FSW clock the
  record and payload stamps diverge — `PlotEvent`'s doc comment already flags
  this for logs, and annotations inherit the same caveat.
- **Sharing.** Layout-scoped storage means annotations travel with a saved
  layout file, not between live operators. The DB-backed backend is the answer
  when that matters; the store interface is shaped so it does not become a
  rewrite.
