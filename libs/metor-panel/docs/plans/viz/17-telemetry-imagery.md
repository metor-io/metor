# 17. Telemetry imagery view

## Summary

`ImageWidget` (`src/views/dashboard/widgets.rs:804`) only ever decodes a
file/inline-base64 image once at construction. This plan turns it into a view
that can *also* bind to a component and redecode on every new sample, reusing
`ComponentStream`/`binding.rs` exactly the way `Monitor`/`Meter`/`TrafficLight`
do, plus a small freshness affordance. It lands as an extension of the
existing type — moved into `src/views/` and renamed to drop the "Widget"
suffix — not a new view, and it closes an existing anomaly where `image` is
the only shared widget kind with no tile-pane surface.

## Reuse vs. new

**Decision: extend `ImageWidget`, do not add a new view.** Rename it
`ImageView` (file `src/views/image_view.rs`), config `ImageConfig`. Rationale:

- The task is additive to what `ImageWidget` already does (decode bytes →
  `gpui::RenderImage` → `paint_image`); a telemetry-bound mode is a second way
  to *obtain* the bytes, not a different rendering problem.
- Every other shared instrument in this crate (`Monitor`, `Meter`, `Gauge`,
  `StateChip`, `TrafficLight`, `AttitudeIndicator`) already carries this exact
  "static-friendly config, `from_config(cfg, db, cx)` optionally spawns a
  stream" shape — see `MeterConfig`/`Meter::from_config`
  (`src/views/meter.rs:52,119`) and the doc comment on `MeterConfig`: "shared
  by the tile and dashboard surfaces — the two differ only in how they host
  the view, not in what an operator can configure." `ImageWidget` is the one
  straggler still living inside `dashboard/widgets.rs` instead of `src/views/`.
- **Rename justified**: `ImageWidget` is the only "*Widget"-named type in the
  shared-instrument family (the rest are bare nouns — `Monitor`, `Meter`,
  `TrafficLight` — with "Widget"/"Panel" reserved for the two *host* wrappers
  around them). It also currently lives in `dashboard/`, implying
  dashboard-only, which stops being true. Moving it to `src/views/image_view.rs`
  as `ImageView`/`ImageConfig` matches both the naming and the module-
  organization convention ("logically distinct subsystems get their own
  file", STYLE.md) other instruments follow.
- **No new tile-pane `PaneItem` struct.** `TrafficLightPanel`/
  `TrafficLightGridPanel` in `src/tiles/panels.rs` look like the precedent for
  a hand-written wrapper, but they are dead code today — grep finds zero
  constructors, and `src/app.rs`'s `register_pane_item_deserializers`
  (`:1288`) never registers them. The live bridge is generic: any
  `WidgetSpec` with `.with_tile(key, tab_title_fn)` (`widgets.rs:122`) is
  auto-adapted into `ItemRegistry` by the boot-time loop over
  `WidgetRegistry::tile_specs()` (`src/app.rs:1119-1125` →
  `ItemRegistry::register_view`, `src/tiles/serial.rs:84`). `image`'s
  `WidgetSpec` registration (`widgets.rs:301-312`) is simply missing
  `.with_tile(...)` — that's the entire reason Image has no tile-pane surface
  today, and the entire fix.

## Design

### Value encoding

metor-db has no variable-length blob primitive. A component's wire shape is
fixed at registration: `ComponentSchema { prim_type: PrimType, dim: SmallVec<[usize;4]> }`
(`libs/db/src/lib.rs:600-604`), `size() == dim.product() * prim_type.size()`,
and `WalComponentStream::next()` frames every WAL message as exactly
`[Timestamp][schema.size() bytes]` (`src/lib.rs:156-169`, this crate). There is
a variable-length byte channel in metor-db (`DB::push_msg` / `MsgLog`,
`libs/db/src/lib.rs:397`, the same path `LogEvent`/alarms/sequences ride), but
it is not a time-series component and the task explicitly wants this to "ride
the existing stream/time machinery" — i.e. `ComponentStream`, live-tail +
seed-from-latest, subscribe-before-producer-registers. So imagery is a
component whose `PrimType` is `U8`, and the frame lives inside its fixed-size
tensor. Two shapes are supported, dispatched on `ComponentView::shape()`
(`libs/metor-proto/src/types.rs:310`):

- **Compressed frame** — `U8` with `dim = [N]` (a capacity the producer
  picks, e.g. `[65536]`). The first 4 bytes are a little-endian `u32` giving
  the actual encoded length; `image::load_from_memory` decodes
  `bytes[4..4+len]`. This is the direct analog of COSMOS `IMAGEVIEWER` —
  arbitrary format (PNG/JPEG/etc.) bytes out of a fixed-capacity slot — and
  reuses the exact decode already in `ImageWidget::load`
  (`widgets.rs:816-834`).
- **Raw pixel tensor** — `U8` with `dim = [h, w, c]`, `c ∈ {1, 3, 4}`. Fed
  straight into `ImageBuffer::from_raw` with no format sniffing: the cheapest
  possible path for a fixed-resolution sensor (star tracker, thermal array)
  that would rather spend bandwidth than a codec. This is also the shape that
  needs the least new code, since metor-db's tensor model already matches it
  exactly (design.md: "each piece of telemetry is an N dimensional tensor").

Both funnel into one helper:

```rust
// src/views/image_view.rs
fn decode_component_image(view: ComponentView<'_>) -> Option<Arc<RenderImage>>
```

which matches on `(view, view.shape())`, and for the compressed case reads
the length prefix then delegates to a shared
`fn rgba_to_render_image(img: image::DynamicImage) -> Arc<RenderImage>` — the
tail end of today's `ImageWidget::load` (`widgets.rs:826-834`), factored out
so both the static path (file/inline bytes) and the telemetry path call it.

### Decode path

`ImageView::from_config(cfg: &ImageConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self`
branches on `cfg.component.is_empty()`, mirroring every other
`from_config` in `src/views/` (`Meter`, `Gauge`, `StateChip`,
`AttitudeIndicator` all check this exact sentinel today in
`dashboard/widgets.rs::build_meter/build_gauge/build_state_chip`):

- **empty** → today's static behavior unchanged: decode `cfg.data` (base64 /
  `data:` URI, via the existing `decode_inline`) or `cfg.path` eagerly, no
  stream, no `db`/`cx` dependency beyond satisfying the shared constructor
  signature.
- **non-empty** → `binding::spawn_seeded_stream` (`src/views/binding.rs:155`)
  with `decode = decode_component_image`; `apply` sets
  `self.render_image = Some(img)`, `self.last_sample = Some(Timestamp::now())`,
  `cx.notify()`. Seeding (paint the last committed frame immediately) and
  late-binding (subscribe before the producer registers) come for free from
  that helper — the same guarantee `Monitor`/`Meter` already give any
  component placed before its producer starts.

### Config

```rust
// src/views/image_view.rs
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageConfig {
    pub path: String,
    pub data: String,
    pub component: String,
}
```

`component` empty ⇒ static (today's behavior, unchanged shape/back-compat —
old dashboard blobs with only `path`/`data` still parse via `#[serde(default)]`).
Non-empty ⇒ telemetry-bound, `path`/`data` ignored. This is the same
"empty-string-as-unbound" sentinel `ComponentId::new(&cfg.component)` callers
already use crate-wide, not a new convention.

### Fresh-image indication

No crate-wide staleness primitive exists yet (survey item #8 is still an open
Tier-1 gap — `binding.rs` has no age/staleness helper today). Rather than
build that generalization here, `ImageView` gets a narrow, local version that
should be folded into #8's eventual `binding.rs` support rather than
duplicated:

- Track `last_sample: Option<Timestamp>`, set in the stream `apply` closure.
- `render()` computes age the same way `format_age` already does in
  `src/views/alarm_panel.rs:53-62` / `src/views/sequence_panel.rs:96`:
  `(Timestamp::now().0 - last_sample.0) / 1_000_000.0` seconds.
- Past a threshold (a local constant to start — e.g. 5s — not yet
  alarm-store-derived; there is no "expected update rate" declared anywhere
  in the control system today), tint the frame's border/corner badge with
  `theme.text_tertiary`-style desaturation, per STYLE.md's ban on ad hoc
  `Hsla` — reuse an existing `Theme` field rather than invent one.
- Because a slow camera's frames arrive far apart, the badge needs to age
  *between* samples, not just repaint on new data. Spawn one extra periodic
  task only while telemetry-bound: a loop of
  `cx.background_executor().timer(Duration::from_secs(1)).await; cx.notify();`
  (the `timer` API is `gpui::BackgroundExecutor::timer`,
  `gpui-0.2.1/src/executor.rs:345` — no existing crate precedent for a
  periodic UI tick, so this is the one genuinely new idiom in this plan).
- No latching/first-out semantics (that's #6, Annunciator panel) — this is
  purely "does the frame look current," matching Open MCT's fresh-image
  indicator, not an alarm.

### Thumbnail strip / scrub — explicitly out of MVP

COSMOS/Open MCT-style scrubbing through recent frames needs either (a)
retaining several decoded frames client-side (the WAL `Disruptor` is a ring
buffer sized for *one* seed + live tail, not a scrollback of decoded images —
`WalComponentStream` only ever exposes the latest message in a grant,
`src/lib.rs:159-169`) or (b) re-querying `Component::time_series` (the
persisted arrow store) for past samples keyed by a scrub position, which is a
real feature (would want to share machinery with the plot's time-range
scrubbing) but is its own chunk of work orthogonal to "get imagery on the
stream/time machinery." Left as an open question / natural follow-up, not
MVP.

## Implementation steps

1. **`src/views/image_view.rs` (new).** Move `ImageWidgetConfig` → `ImageConfig`
   (add `component: String`), `ImageWidget` → `ImageView` (add `last_sample`,
   telemetry branch), `decode_inline` (unchanged), and a new
   `decode_component_image` + `rgba_to_render_image` helper factored out of
   `ImageWidget::load`'s tail. `ImageView::from_config(cfg, db, cx)` replaces
   `ImageWidget::load(cfg)`; keep a thin `ImageView::static_only(cfg)` shim
   only if some caller needs construction without a `db`/`cx` (check; likely
   not — `build_image` already has both). Freshness badge lives in `Render`.
2. **`src/views/mod.rs`.** Add `pub mod image_view;` and re-export
   `pub use image_view::{ImageView, ImageConfig};` alongside the other
   instrument re-exports.
3. **`src/views/dashboard/widgets.rs`.** Delete the inline `ImageWidget`
   struct/impl/`decode_inline`/tests (moved). Add
   `pub use crate::views::ImageConfig as ImageWidgetConfig;` (matches the
   existing `TrafficLightConfig as TrafficLightWidgetConfig` alias pattern,
   `:488-489`). Rewrite `build_image` (`:700-703`) to
   `as_live(cx.new(|cx| ImageView::from_config(&cfg, db.clone(), cx)))` (now
   needs `db`/`cx`, matching `build_meter`/`build_gauge`). Add
   `.with_tile("image", |blob| { let cfg = parse_or_default::<ImageWidgetConfig>(blob); ... a short label ... })`
   to the `WidgetKind::image()` registration (`:301-312`) — this is what
   actually gets Image onto the tile-pane surface, via the existing
   `WidgetRegistry::tile_specs()` → `ItemRegistry::register_view` boot loop
   (`src/app.rs:1119-1125`); no new `PaneItem` type needed.
4. **`src/views/dashboard/mod.rs`.** Extend the add-flow: `image_path_rows`
   (`:1126`) grows a sibling "Bind to telemetry…" `NavRow` that opens
   `component_picker_rows(dashboard, db, WidgetKind::image())`; add an
   `else if kind == WidgetKind::image()` arm to `component_picker_rows`
   (`:1036`, alongside the existing `monitor`/`traffic_light` arms) building
   `ImageWidgetConfig { component, ..Default::default() }`.
5. **Tests.** Port the existing `decode_inline`/`image_config_supports_local_path_only`
   tests into `image_view.rs`. Add: `decode_component_image` on a synthetic
   1-D `U8` `ComponentView` (build via `ComponentSchema::new(PrimType::U8, &[N]).parse_value(&bytes)`)
   with a length-prefixed tiny PNG, and on a 3-D `U8[h,w,3]` raw-pixel view;
   assert both round-trip to a `RenderImage` of the right pixel size. A
   round-trip test for `ImageConfig` with only `path` set (back-compat: no
   `component` field in an old saved blob still parses via `#[serde(default)]`).

## Open questions

- **Compressed-frame capacity vs. actual frame size.** The fixed-capacity
  `U8[N]` shape means a producer that emits a frame larger than `N` silently
  truncates. Is there a validator/warning worth adding on the producer side
  (probably a `metor-db`/FSW concern, out of this crate), or is "the operator
  picks `N` generously" sufficient for v1?
- **Freshness threshold.** Fixed constant vs. something derived from observed
  inter-frame interval vs. waiting for the crate-wide staleness work (#8) and
  making `ImageView` a second consumer of it instead of carrying its own
  timer. Recommend revisiting when #8 lands rather than blocking this on it.
- **Thumbnail strip + scrub** (explicitly out of MVP above) — worth scoping
  once/if it's wanted, likely sharing time-range machinery with the plot
  rather than being imagery-specific.
- **`ImagePanel`/tiles parity for other dashboard-only widgets.** This plan
  fixes `image`'s missing `.with_tile`, but `monitor` has the same gap
  (`docs/plans/widget-kind-registry.md` flags it as "optional parity"). Not
  this plan's job, but worth doing in the same pass if someone's already in
  `widgets.rs`.
- **Color channel order.** `ImageWidget::load` already builds an
  `ImageBuffer<Rgba<u8>, _>` and wraps it directly in a `Frame` despite
  `gpui::RenderImage`'s doc comment saying it stores "BGRA format"
  (`gpui-0.2.1/src/assets.rs:41`); this plan inherits whatever that existing,
  already-shipped behavior does (right or wrong) rather than fixing it here.
