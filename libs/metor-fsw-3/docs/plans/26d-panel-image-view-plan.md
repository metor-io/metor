# 26d: Panel `image_view`, plan

Implements the panel side of `26-star-tracker.md` §3: P3 (minimal view), the
panel parts of P6 (histogram, curves, overlays, preset), and the panel parts
of P7 (RGB, Bayer, float). The record type, metadata, and the db/link fixes
are in 26a (the image-record framework plan). This plan only states what it
needs from 26a.

Chunks A–E deliver P3. F is P6, G is P7, H is an outline. Each chunk lands as
one PR, gated on `cargo test -p metor-panel`, `cargo clippy -p metor-panel`,
and an antagonistic review before the next chunk starts.

Order: A → B → C → D → E → F → G. A and B do not depend on each other and can
be written in parallel. C needs A's layout parser. A needs 26a's chunk A
(the `metor-proto-wkt` image metadata).

All code lives in a new `libs/metor-panel/src/views/image_view/` module. It
is not named `image`, because `views/mod.rs` would then shadow the `image`
crate the panel already uses.

## Facts the plan relies on

Re-verified against this worktree (`sphw/star-tracker` at `9fdb7a6b5`).

Reading samples:

- `SelectedReader` (`temporal/samples.rs:55-75`) is one WAL subscription plus
  at most one history query per (db, component). `acquire` (`:77`) shares
  readers through a weak global. `refresh` (`:246`) follows `view_time`, so
  live and scrubbed views take the same path. History is read by
  `at_or_before` (`:617`).
- `SelectedSample.bytes` is an owned `Arc<[u8]>` (`:16-20`). The WAL task
  copies each arrival into it (`:119`). The tail keeps at most 256 samples
  or 4 MiB, whichever binds first, but always keeps one (`:141`). With
  2 MiB frames that means one or two frames.
- `publish` decides whether to notify by comparing
  `(timestamp, bytes, reconstructed)` by value (`:214-217`). When the
  timestamps match, that is a full memcmp of the frame. This happens on
  every clock tick that re-selects the same sample.
- The head fast path copies `latest.data()` into a new `Arc` (`:579`).
- Views follow a reader with `cx.observe(&reader, …)`. `spawn_seeded_stream`
  (`views/binding.rs:186-235`) is the reference.
- The tests build a DB in a tempdir, insert a component, install samples,
  `Seek`, and `run_until_parked` (`temporal/samples.rs:831-1235`). The
  scrubbing test in D copies this fixture.
- `Component::push_buf` (`libs/db/src/lib.rs:1052`) writes the WAL. The
  panel's persist op uses it (`dynamic/ops/persist.rs:60-118`), together
  with `insert_component`, a `vtable_gen` bump, and `set_component_metadata`.
  The synthetic source in C does the same.

Drawing:

- `RenderImage` holds BGRA (`gpui/src/assets.rs:42-49`) and is immutable.
  `Window::paint_image` (`gpui/src/window.rs:5071`) uploads it into the
  sprite atlas on first paint. `drop_image` (`:5193`) removes it. An image
  that is never dropped stays in the atlas until the window closes.
  `has_image_atlas_entry` (`:5207`) exists under `test-support`.
- The atlas is `BGRA8Unorm` (`gpui_apple/src/metal_atlas.rs:150`,
  `gpui_wgpu/src/wgpu_atlas.rs:441`). Its sampler is linear on both backends
  (`gpui_apple/src/shaders.metal:716`, `gpui_wgpu/src/wgpu_renderer.rs:1234`).
  Sprite bounds are snapped to device pixels (`window.rs:5108`). An image
  rendered at device resolution and painted 1:1 is therefore not blurred.
- `paint_surface` takes a `CVPixelBuffer` and exists only on macOS and iOS
  (`window.rs:5170-5190`). The wgpu backend skips surfaces
  (`wgpu_renderer.rs:1606-1608`). gpui has no custom-shader or
  updatable-texture primitive.
- `on_release_in(window, …)` (`gpui/src/app/context.rs:370`) gives a release
  callback that receives `&mut Window`. `BackgroundExecutor::scoped`
  (`gpui/src/executor.rs:135`) runs borrowed futures on gpui's pool.
- The panel has three frame-swap patterns:
  - The plots keep `current_frame` and `pending_release`, and drop the old
    frame in the canvas prepaint (`views/time_series/gpu.rs:752-879`;
    used at `views/spectrogram/plot.rs:636-655`). They never release their
    last frame when the view closes.
  - The 3D viewer orphans its frames on release (`views/viewer_3d/mod.rs:312-323`).
  - The map orphans its tiles to a global store (`views/map/tiles.rs:13-17`).
- Map tiles swizzle RGBA to BGRA before building a `RenderImage`
  (`views/map/tiles.rs:445-455`). The static `ImageWidget` does not
  (`views/dashboard/widgets.rs:973-981`), so its red and blue are swapped.
  That fix is a separate small PR, outside these chunks (decision 4).

Registration:

- One `WidgetSpec` with `.with_tile(key, …)` registers a view on both the
  dashboard and the tiles (`views/dashboard/widgets.rs:133-178`).
  `ItemRegistry::register_view` (`tiles/serial.rs:84`) adapts it into a
  pane deserializer. Map, samples table, and attitude use this path
  (`widgets.rs:515-576`). `WidgetKind::image()` is the static PNG widget
  (`views/dashboard/mod.rs:82`, `widgets.rs:372-383`).
- The new-panel menu is built in `new_panel_rows` (`tiles/panels.rs:1385`).
  It appends every spec's `tile_add_flows`. `menu.component` lists every
  component (`:1314-1335`, through `trace_picker::component_picker_rows`
  at `inspector/trace_picker.rs:57`). `list_components` (`:27`) reads names
  and metadata from the DB state.
- Palette commands are built in `register_command_provider`
  (`inspector/palette.rs:376`). "Plot Component" is the model for a
  submenu (`:423-443`).
- The outline's component context menu builds rows at
  `views/outline/mod.rs:792-815`. "Plot component" dispatches
  `PlotComponentAction` (`tiles/pane.rs:23`), which `AppRoot` handles at
  `app.rs:369-383`.

Hazards:

- The outline creates a value strip and a sparkline with one trace per
  element for every component (`views/outline/mod.rs:1050-1075`). For an
  image component that is a million elements. "Plot component" would plot
  a million traces. `element_names` (`trace_picker.rs:785`) would build a
  million strings.

Metadata and types:

- Component metadata is a `HashMap<String, String>` read through
  `MetadataExt::get` (`metor-proto/wkt/src/metadata.rs:110-129`).
- An array field emits one component per element, named `path.i.field`
  (`metor-component/src/vtable.rs:37-47`, `metadata.rs:51-62`).
- `rayon` is already in the panel's build graph (through `image` →
  `ravif` → `rav1e`).

## Decisions

Settled after the first draft:

1. The metadata types live in `metor-proto-wkt`, as 26a defines them
   (`26a-image-records-plan.md`, chunk A, "Metadata"). The panel parses no
   strings of its own.
2. Overlays bind to the tracker's shaped components: one component per
   field of `StarCentroids` (`u`, `v`, `flux`: `[f32; 32]`; `cat_id`;
   `count`), as in `26b-star-tracker-sensor-plan.md` chunk J.
3. Auto levels are computed over the visible area by default, and over the
   ROI when one is set.
4. The red/blue swap in the static `image` widget is fixed in its own small
   PR, outside these chunks.
5. The synthetic source is a debug-only palette command.
6. The outline gets no thumbnails; image rows show a text label.
7. The performance floor is the Linux x86 host (run through `orb`), with a
   5 ms budget per render.
8. `StarTruth` uses the same shape as `StarCentroids`: one component per
   field (`u`, `v`, `mag`, `cat_id`, plus `count`). Plan 26c owns that
   record.
9. Hiding image components from the plot pickers is a panel concern, owned
   by this plan's chunk A. 26a only provides the `image.format` metadata
   that chunk A keys on.

## What this plan needs from 26a

1. From `metor-proto-wkt` (`wkt/src/metadata.rs`, 26a chunk A):
   - the keys `IMAGE_FORMAT_KEY`, `IMAGE_BITS_KEY`, `IMAGE_BLACK_KEY`,
     `IMAGE_WHITE_KEY`, `IMAGE_STRETCH_KEY` and `LOD_KEY`
   - `ImageFormat { Mono8, Mono16, MonoF32, Rgb8, Rgba8 }` with `as_str` and
     `FromStr`
   - the getters `image_format()`, `image_bits()` and `lod()`, plus
     `with_image(format, bits)` and `with_lod(bool)`

   The panel builds its `FrameLayout` from these getters and the schema.
2. Bits come from the pixel type (`Mono16<BITS>`, 8 for 8-bit types, 32 for
   `MonoF32`) and arrive as `image.bits`. The panel trusts `image.bits` and
   checks it against the element width.
3. `lod = "false"` on every image component. The panel does not read `lod`;
   it is the db's concern.
4. The schema:
   - `U8`, `U16` or `F32`
   - `dim` `[H, W]`, or `[H, W, C]` with C ∈ {3, 4} for `rgb8`/`rgba8`
   - row-major, no row padding, little-endian
5. Display hints (`black`, `white`, `stretch`) have keys and getters but no
   producer in P1. The panel's defaults are auto levels over the visible
   area (decision 3), a linear curve, and gray. When a hint is present, it
   overrides that default.
6. The db blockers from 26 §3.3, which are in 26a:
   - the WAL byte budget (`libs/db/src/lib.rs:921,948,976`)
   - LoD skipping image components (`libs/db/src/lod.rs:138`)
   - `NODE_SIZE` (`time_series.rs:61`)

   Until they land, a 1024² frame allocates a 2 GiB WAL, so the synthetic
   source defaults to 256² (a 128 MiB WAL).
7. A frame's fields share a parent path: 26a's integration test shows
   `…image.pixels` next to `…frame_id`. The sibling readout in F relies on
   this.
8. Bayer formats are not in 26a; it notes they would be additive. G's Bayer
   path waits for `Bayer8<P>`/`Bayer16<P, BITS>` and their `ImageFormat`
   variants.

## Crate trade study

### Benchmark

The prototype is at
`/private/tmp/claude-501/-Users-sphw-code-metor-metor-examples-adcs-fsw3/23e312df-4f1d-4d0c-96fe-d4a53e7e1674/scratchpad/panel-image-trade/src/main.rs`.

Setup:
- Input: a synthetic 1024² Mono16/12 star field (300 Gaussian stars, 50 hot
  pixels).
- Output: BGRA through a 4096-entry inferno LUT.
- Machine: Apple M4 Max, release build.
- Each figure is the median of 30 runs after 3 warm-up runs, in ms.
- `fast_image_resize` (fir) was built with its `rayon` feature, so its
  numbers are not single-threaded.

| Case | hand-rolled, 1 thread | hand-rolled, rayon | fir 6.1 + LUT pass | pic-scale 0.7 + LUT | `image` 0.25 + LUT |
|---|---|---|---|---|---|
| fit 1200², nearest (0.85 src px per px) | 0.78 | 0.12 | 0.62 | 0.86 | 10.85 |
| 1:1 1024² | 0.57 | 0.08 | | | |
| zoom 4×, 1600×1200 out | 1.03 | 0.12 | | | |
| down to 512² (2.0), hand max-pool | 0.33 | 0.10 | box 0.34, nearest 0.19 | | |
| down to 700² (1.46), hand max-pool | 0.58 | 0.13 | box 0.45, nearest 0.32 | | |
| down to 256² (4.0), hand max-pool | 0.13 | 0.08 | box 0.15, nearest 0.08 | | |

| Statistics and colour | ms |
|---|---|
| histogram, 4096 bins, hand-rolled, 1 thread / rayon | 0.36 / 0.10 |
| two percentiles from that histogram | 0.001 |
| `hdrhistogram` 7.6: record every pixel, then two quantiles | 1.14 |
| `ndarray-stats` 0.7: copy, then two `quantile_mut` | 61.9 |
| LUT build, 4096 entries: gray / `colorous` / `colorgrad` | 0.005 / 0.030 / 0.028 |
| `demosaic` 0.3 on 1024² f32 planar: bilinear / MHC | 16.8 / 21.1 |

Quality check: a single 4095 hot pixel on a background of 200, reduced 4×.
- Max-pool keeps it at full white.
- fir's box filter reduces it to 444, which is lost in the background.

This is the reason for max-pool, and it holds for stars at the same
reduction.

### Max-pool

No crate checked offers a max or min reduction:
- fir 6.1 (`ResizeAlg`: `Nearest`, `Convolution`, `Interpolation`,
  `SuperSampling`)
- pic-scale 0.7 (`ResamplingFunction`: convolution kernels only)
- `image` 0.25 (`FilterType`)

`imageproc` 0.27 has `grayscale_dilate`, but only for `GrayImage` (u8). It
also dilates at full resolution with a mask, rather than pooling into an
output grid. Pooling therefore has to be written by hand.

The kernel is small:
1. Take the column-wise max of the rows in the output row's source span.
2. Take the max over each output column's source span.
3. Look the result up in the LUT.

Nearest uses the same span tables. Writing both kernels is less code than
adapting a crate, and it avoids an intermediate buffer.

### Recommendations

| Concern | Pick | Reason |
|---|---|---|
| Mono resample (nearest, max-pool) fused with the LUT | Hand-rolled, about 150 lines in `resample.rs` | No crate does max-pool. The fused loop writes BGRA once. Hand-rolled nearest matches or beats fir plus a LUT pass and needs no scratch image. |
| Reading pixels from `Arc<[u8]>` | `zerocopy` (already a dependency): `[U16<LE>]`, `[F32<LE>]` | Correct for endianness and alignment-free, so a sample buffer's alignment never matters. |
| RGB downsample (G) | `fast_image_resize` 6 (`Box`, `crop`) | SIMD for `U8x3` and `U8x4`. `crop` is the viewport. A mean is the right reduction for colour. Zoom-in stays on the hand-rolled nearest. |
| Colormaps | `colorous` 1.0 | No dependencies. Gives d3's inferno, viridis, magma, cividis and turbo through `eval_continuous`. A LUT builds in 30 µs. `colorgrad` is as fast but pulls in `csscolorparser` and a larger API. `scarlet` is heavy and unmaintained. The panel's `Colormap` (`views/time_series/heatmap.rs:44`) derives from theme stops and is not perceptual, so the image view does not reuse it. |
| Histogram and percentiles | Hand-rolled, 2^bits bins | Exact for integer data, 0.36 ms, and the same bins feed the histogram UI. `hdrhistogram` is 3× slower and approximate by design. `ndarray-stats` needs a copy and a selection (60 ms). For `mono_f32`: min/max, then 4096 bins over that range. |
| Bayer (G) | Hand-rolled 2×2 superpixel when the view shows ≥ 2 source px per device px. `demosaic` 0.3 (bilinear, MHC; no dependencies, `no_std`, maintained) at closer zoom, on the visible region only, cached per sample. | Superpixel is free and exact when zoomed out. `bayer` 0.1.5 has a 2018-era `&mut Read` API. `rawloader` and `imagepipe` are pipelines for camera raw files (decoders, colour management), which is the wrong scope. |
| Parallelism | None in P3. Each render is one task on gpui's background executor. | Every case is ≤ 1.1 ms single-threaded. The kernels take a row range, so splitting into bands with `BackgroundExecutor::scoped` is a local change if a slower host or the 2592×1944 sensor needs it. `rayon` costs nothing to build, but it adds a third pool next to gpui's and Bevy's, and a gpui test cannot drive it deterministically. |
| Display primitive | `RenderImage` at device resolution, with `drop_image` | The only cross-platform option. `paint_surface` is macOS-only and takes YCbCr `CVPixelBuffer`s; it is kept for a later video source (H). A wgpu pass on the panel's own device (`gpu_context.rs`, as in the plots) was rejected: it adds a staging readback and a frame of latency to a job that takes 1 ms on the CPU. No gpui patch is needed (26 open question 6). |
| `image` crate | Only as the `RgbaImage`/`Frame` container that `RenderImage::new` takes | Its resize is about 14× slower than the alternatives here. |

The measurements come from one fast machine. The floor is the Linux x86
host (decision 7). Chunk B adds an `#[ignore]`d timing test. D runs it on
that host through `orb` (nixos) and records the numbers in the PR; E does
not land until every case is under 5 ms there. If one is not, the fix is
splitting `render_into` into bands with `BackgroundExecutor::scoped`.

## Module layout

`libs/metor-panel/src/views/image_view/`:

| File | Contents | Chunk |
|---|---|---|
| `layout.rs` | `FrameLayout` from schema and metadata; typed pixel access | A |
| `lut.rs` | `Display`, `Curve`, `Colormap`, `Lut` | B |
| `histogram.rs` | `Histogram`, percentiles, ROI statistics | B, F |
| `resample.rs` | `Viewport`, span tables, nearest, max-pool, `render_into` | B |
| `synthetic.rs` | test-image generator and dev source | C |
| `source.rs` | `FrameSource`, `RecordSource` | D |
| `render.rs` | `RenderKey`, background job, frame swap and release | D |
| `view.rs` | `ImageView` entity, config, painting, interaction | D, E |
| `overlay.rs` | `Overlay::Points` | F |

## A. Image components: layout and guards

Files:
- `views/image_view/{mod,layout}.rs`, `views/mod.rs`
- `views/outline/mod.rs`
- `views/component_browser/mod.rs`
- `inspector/trace_picker.rs`
- `temporal/samples.rs`

Changes:

- `FrameLayout { width, height, channels, format: ImageFormat, bits, black,
  white, stretch }`. `ImageFormat` and the getters are
  `metor_proto_wkt`'s (26a dependency 1).
  - `FrameLayout::from_component(&ComponentSchema, &ComponentMetadata) ->
    Result<FrameLayout, LayoutError>` reads `image_format()`,
    `image_bits()` and the display-hint getters. It checks that the
    element type, `dim`, and channel count agree with the format, and that
    `bits` fits the element.
  - `layout.pixels(&[u8]) -> Result<Pixels<'_>, LayoutError>` borrows
    `Pixels::{U8(&[u8]), U16(&[U16<LE>]), F32(&[F32<LE>])}`. It returns an
    error, not a panic, when the length is wrong.
- `image_layout(db, id) -> Option<FrameLayout>` and
  `list_image_components(db)`. The second filters `list_components` by
  `image.format`.
- Guards for image components:
  - The outline shows a text cell such as `1024×1024 mono16/12` in place of
    the value strip and sparkline.
  - The outline and the component browser omit "Plot component".
  - The trace pickers leave image components out.
- `SelectedReader::publish` checks `Arc::ptr_eq` on `bytes` before comparing
  by value. The reader is otherwise unchanged.

Tests (`views/image_view/layout.rs`, plus existing modules):

- Nominal:
  - `mono16` with `bits = 12` over `U16 [4, 6]` parses to width 6 and
    height 4.
  - `rgb8` over `U8 [2, 2, 3]` parses.
  - Metadata built with `with_image(Mono16, 12)` and `with_lod(false)`
    parses, so the test uses 26a's writer, not hand-typed strings.
  - `pixels` reads little-endian values back.
- Edge: a 1×1 frame; `bits = 16`; and `image.black > image.white`, which is
  kept as written because the LUT handles it (B).
- Failure, each a distinct `LayoutError` and none a panic:
  - `mono16` over `U8`
  - `rgb8` with C = 4
  - `bits = 17`
  - an unknown or missing `image.format`
  - a missing `image.bits`
  - bytes one short
- The outline builds no strip for an image component (a gpui test against
  the outline's delegate). `list_image_components` returns only tagged
  components.
- Publishing the same `Arc` twice notifies once.

## B. Pixel kernels

Files: `views/image_view/{lut,histogram,resample}.rs`, `Cargo.toml` (adds
`colorous = "1"`).

Changes:

- `Display`:
  - `levels: Levels::{Manual { black, white }, Auto { low, high, lock }}`
  - `curve: Curve::{Linear, Sqrt, Log, Asinh { softening }, Gamma(f32)}`
  - `colormap: Colormap::{Gray, Inferno, Viridis, Magma, Cividis}`
  - `saturation: bool`

  It derives facet, serde and `PartialEq` for the inspector.
- `Lut::build(bits, black, white, &Display, saturated: [u8; 4]) -> Lut`:
  - one `u32` BGRA per code, `2^bits` entries
  - `black == white` makes a step function, not a division by zero
  - `black > white` inverts
  - with `saturation`, the code `2^bits − 1` maps to the theme's saturation
    colour, which the caller passes in
- `Histogram::{of_u8, of_u16(bits), of_f32(lo, hi, bins)}`, then
  `percentile(p) -> Option<u32>` and `count()`. Each takes the pixels and
  a source `Rect`, so the same code serves the visible area, the ROI, and
  the whole frame. Codes above `2^bits − 1`
  land in the last bin, so bad data reads as saturated rather than wrapping.
  NaN is skipped.
- `Viewport { origin: (f64, f64), step: f64 }`. `origin` is the source point
  at the device pixel (0, 0); `step` is source pixels per device pixel.
  - `spans(origin, step, n, limit)` gives `[a, b)` source spans that
    partition the covered range.
  - `Viewport::visible(img_w, img_h, dst_w, dst_h) -> Rect` is the part of
    the output the image covers. Only that rectangle is rendered; the
    backdrop is a gpui quad.
- `render_into(pixels, layout, &Lut, viewport, rows: Range<usize>, dst:
  &mut [u8])` picks nearest when `step ≤ 1` and max-pool when `step > 1`.
  For `mono_f32`, an affine map to 4096 codes comes first.

Tests (pure, `#[cfg(test)]` in each file):

- LUT:
  - black maps to the first colormap colour and white to the last
  - below black and above white clamp
  - every curve is monotonic over all codes for each colormap
  - `Gamma(1.0)` equals `Linear`
  - `asinh` is fixed at both ends
  - the length is `2^bits` for 8, 12 and 16
  - the saturation entry is replaced only when enabled
  - `black == white` and `black > white` do not panic and behave as stated
- Histogram:
  - the counts sum to the pixel count
  - `p = 0` gives min and `p = 1` gives max
  - on a constant image every percentile is the constant
  - an empty slice gives `None`
  - out-of-range codes land in the last bin
  - NaN is ignored
  - a `Rect` counts only its pixels, and a `Rect` clipped by the frame
    edge counts only the overlap
- Resample:
  - spans cover every source pixel exactly once for steps 1, 1.463, 2,
    4 and 7.3, at origins 0, 0.5, −3.2 and past the edge
  - at step 1, `render_into` is the identity through a gray LUT
  - at step 0.25, every source pixel becomes a 4×4 block
  - max-pool keeps a single hot pixel at full value for every step in
    1.0–8.0 and every phase of 0–7 pixels
  - a viewport fully outside the image renders nothing
  - zero-sized outputs return early
  - rendering only `rows` touches only those rows, which is the
    band-split contract
- Perf: an `#[ignore]` test runs the benchmark cases and prints the
  timings. It is not part of the gate.

## C. Synthetic source

Files: `views/image_view/synthetic.rs`, `inspector/palette.rs`.

Changes:

- `synthetic::frame(kind, w, h, n) -> Vec<u8>` is pure and deterministic
  (xorshift seeded by `n`). It has four kinds:
  - `StarField`: Mono16/12. Background 200 with noise, Gaussian stars that
    drift one pixel per frame, fixed hot pixels, one saturated star, and a
    slow gradient.
  - `Ramp`: Mono8.
  - `Float`: `mono_f32`, a signed field.
  - `TestCard`: RGB8 with colour bars.

  The star positions are a pure function of `n`, so tests can compute the
  expected pixel.
- `synthetic::install(db, name, kind, w, h) -> Component` inserts the
  component the way `persist.rs:60-90` does. Its metadata comes from
  `with_image(format, bits)` and `with_lod(false)`, which is exactly what
  26a's `FrameImage` emits.
  `synthetic::run(db, component, kind, rate, cx) -> Task<()>` calls
  `push_buf` at the wall-clock rate.
- The palette gets "Debug: Synthetic Image Source", compiled only under
  `cfg(debug_assertions)` (decision 5). Release builds have no dev source.
  It offers the four kinds, a size (256² by default; 1024² once 26a's WAL
  budget lands), and a rate (10 Hz). The task lives as long as the palette
  command's handle in a global; a second invocation stops it.

Tests:
- `frame` is byte-identical across calls.
- The saturated star reaches `2^bits − 1`.
- `install` produces a component whose `FrameLayout` parses.
- After `run` and `run_until_parked`, `time_series.latest()` advances.

This is the way to develop without the simulator. It is also the contract
check: the generator writes the same keys that 26a's `FrameImage` emits.
When 26a lands, its link test asserts that the panel's `FrameLayout` parses
a real `StarImage`.

## D. `ImageView` core: reader, render, paint, registration

Files:
- `views/image_view/{source,render,view}.rs`, `views/mod.rs`
- `views/dashboard/{mod,widgets}.rs`
- `inspector/palette.rs`
- `views/outline/mod.rs`, `views/component_browser/mod.rs`
- `tiles/pane.rs`, `app.rs`
- `theme.rs`

Source:

- `Frame { timestamp, bytes: Arc<[u8]>, layout: FrameLayout }`.
- `trait FrameSource { fn frame(&self, cx: &App) -> FrameState; fn
  entity(&self) -> AnyEntity }`. `FrameState` is `Ready(Frame, stale)`,
  `Loading`, `Missing`, or `Error`. `entity` is what the view observes.
- `RecordSource` wraps `samples::acquire` and maps `Selection` to
  `FrameState`. It re-reads the layout when `metadata_gen` moves. A
  `StaticSource` (one fixed `Frame`) serves the view tests that have no
  DB. The video source in H is the third implementation.

Render:

- `RenderKey { frame: (timestamp, Arc ptr), display, levels, viewport,
  roi, out: (w, h), theme }`. With auto levels, a pan or zoom changes the
  region, so it changes the levels too.
- At most one job is in flight. A key that changes during a job marks the
  view dirty, and the next job starts when the current one finishes. A
  result whose key is no longer wanted is dropped without being painted.
- The job runs on `cx.background_executor()`:
  1. histogram, when the levels are auto or the histogram is shown. Its
     region is the ROI when one is set, and otherwise the source
     rectangle under the visible output (decision 3).
  2. levels
  3. LUT, cached in the view by `(bits, display, levels)`
  4. `render_into`
  5. `RenderImage`

  It returns the image, the histogram, and the levels it used.
- `Frames { current, pending_release }` as in `PlotRenderState`. The canvas
  prepaint drains `pending_release` through `drop_image`. On its first
  prepaint the view registers `cx.on_release_in(window, …)`, which drops
  `current` and `pending`. This fixes the last-frame leak the plots have.
- The output size is the snapped device-pixel size of the visible image
  rectangle, `(bounds ∩ image) × scale_factor`. The image is painted with
  `paint_image(rect, rect, …)` so each texel lands on one device pixel.

View:

- `ImageViewConfig { component, display, view: ViewMode::{Fit, Free {
  center, step }}, roi, histogram, crosshair, overlays }`. Serde and
  `Default`. It is shared by the tile and the dashboard. P3 ships
  `ViewMode::Fit` only; E adds `Free`.
- The `ImageView` entity follows the `Map` pattern (`views/map/mod.rs:104-140`):
  - `source: Binding`, rebound by comparison on render
  - the editable `display` fields carry facet `inspect::` attributes
  - status text for `Loading`, `Missing` (no frame at or before the view
    time) and `Error`, and a stale badge
- Defaults:
  - auto levels at 0.5% / 99.95% over the visible area, recomputed every
    frame
  - linear curve, gray colormap
  - when the producer sets `image.black` and `image.white`, they give
    manual levels; `image.stretch` picks the curve (26a dependency 5)
- Theme tokens: `image_backdrop`, `image_saturated`,
  `image_overlay_detected`, `image_overlay_truth`, `image_crosshair`. They
  go in every theme in `theme.rs`, with the existing all-themes tests
  extended.

Registration:

- `WidgetKind::image_view()` (`"image_view"`) with `.with_tile("image_view",
  …)` and a live tile title (the component name).
  - `.with_add_flow` and `.with_tile_add_flow` both open a picker built on
    `list_image_components`, so neither surface lists non-image
    components.
  - The static `image` widget is unchanged.
- `OpenImageAction { component_id }` in `tiles/pane.rs`, handled in
  `AppRoot` like `PlotComponentAction`. It deserializes `"image_view"`
  through the `ItemRegistry`.
- "Open as image" for image components in the outline menu
  (`outline/mod.rs:792`) and the component browser menu.
- "Open Image" in the palette: a submenu of image components, next to
  "Plot Component".

Tests:

- **Scrubbing selects the right sample** (gpui test, the `samples.rs`
  fixture). Setup:
  - A `U16 [4, 4]` component tagged `mono16`/12.
  - Frames at t = 10, 20 and 100. Every pixel of frame t equals `t * 10`.

  Checks:
  - After `Seek` to 15, the source's frame is t = 10 and pixel (0, 0) reads
    100.
  - 20 → 20 (inclusive).
  - 99 → 20.
  - 5 → `Missing`.
  - Seek back to 100 → 100.
  - A `push_buf` at 120 while live → 120.
  - While paused at 99, a `push_buf` at 50 → 50, because a late
    predecessor wins.

  The same test, through `add_window_view` and a draw, checks that the
  rendered key's timestamp follows each seek, and that the hover readout
  at (0, 0) gives the frame's raw value (once E adds the readout).
- Render scheduling:
  - redrawing with an unchanged key starts no job
  - a key change during a job produces exactly one follow-up job
  - a superseded result is never installed
- Atlas: after three frames, `has_image_atlas_entry` holds only for the
  current image. After the view is released, it holds for none.
- Auto levels: with a frame whose left half is 100 and right half is
  4000, fitting the left half in view gives levels inside 100, and a ROI
  on the right half overrides the view.
- Config: serde round trip, and tile serialize/deserialize through
  `ItemRegistry` gives the same config.
- Failure:
  - a component whose layout fails shows the `LayoutError` text
  - a sample with the wrong length shows an error, not a panic
  - a component that disappears shows `Loading`

## E. Interaction and display settings (completes P3)

Files: `views/image_view/{view,resample}.rs`.

Changes:

- `ViewMode::Free { center, step }`.
  - The wheel zooms about the cursor, by a factor of 1.25 per notch,
    clamped to 1/64…64 source px per device px.
  - Dragging pans, clamped so that at least 32 device px of the image stay
    in view.
  - `F` fits, `1` sets 1:1 (step = 1 device px per source px), and a double
    click toggles between fit and the last free view.
- Hover readout: `(x, y)` in source pixels and the raw value (DN, or the
  float for `mono_f32`), drawn in a corner chip. The value comes from the
  frame bytes, never from the LUT output.
- Levels:
  - Manual black and white rows in the inspector.
  - Auto levels with editable `low` and `high` percentiles, over the
    visible area or the ROI.
  - `lock` freezes the levels computed on the frame shown when it is
    turned on; turning it off recomputes per frame.
  - "Reset to metadata" restores the layout's defaults.

Tests (pure, on `Viewport`):
- Zooming about a point keeps that source point under the cursor at scale
  factors 1 and 2.
- Pan clamping holds at every zoom.
- 1:1 gives step = 1/scale_factor in logical px.
- `to_source(point)` is the inverse of the forward map, to within half a
  pixel.

Tests (gpui, with the D fixture):
- The readout at a chosen point after zoom and pan matches the expected
  raw value.
- With `lock`, the levels hold across a new frame; without it they follow.

## F. Histogram, ROI, overlays, preset (P6)

Files:
- `views/image_view/{view,histogram,overlay}.rs`
- `theme.rs`
- `examples/adcs-fsw3` (preset only)

Changes:

- Histogram strip under the image:
  - log-y bars from the job's `Histogram`, drawn with `PathBuilder`
  - draggable black and white handles that write manual levels
  - a marker at `2^bits − 1`
- Crosshair at the cursor, showing the row and column values.
- ROI:
  - shift-drag draws a rectangle, stored in config
  - `Histogram::roi_stats(pixels, rect)` gives min, max, mean, σ and sum
    for that rectangle, in raw units
  - while a ROI is set, auto levels use it (decision 3); clearing it
    returns to the visible area
- Sibling readout: scalar components under the image component's parent
  path (26a dependency 7), each read with `samples::acquire` at the same
  view time and shown in the hover chip.
- `Overlay::Points { u, v, count, size, color: OverlayColor::{Detected,
  Truth} }` (decision 2). Each field names one shaped component:
  - `u`, `v`: `[f32; 32]` pixel coordinates (`star_centroids.u`,
    `star_centroids.v`)
  - `count`: the scalar count; rows at or past `count.min(32)` are ignored
  - `size`: optional, `star_centroids.flux`, mapped to a radius
  - `cat_id` is shown in the hover chip for the point under the cursor

  Five components, so at most five `SelectedReader`s per overlay. Each is
  read at the view time, so the overlay follows scrubbing and stays aligned
  with the image. `u` and `v` must have the same length; a mismatch shows
  an error on the overlay, not a panic. `StarTruth`'s true centroids are
  bound the same way, as `color: Truth`, with `size` from `mag`
  (decision 8).
  - Points are drawn as circles in the paint closure, through the same
    `Viewport`.
  - Points whose timestamp differs from the frame's are drawn dashed.
- Preset: a "Star tracker" layout in `adcs-fsw3`:
  - an `image_view` of `StarImage` with overlays from `StarCentroids` and
    `StarTruth`
  - plots of `StarSolution`

  It is shipped through the target's `Presets(...)` (`presets.rs:8-13`).

Tests:
- `roi_stats` against hand-computed values.
- An ROI that is empty or partly outside the image is clamped.
- Dragging a handle sets manual levels.
- Overlay points map to the expected device positions under zoom and pan.
- Only the first `count` rows draw; `count > 32` clamps; mismatched `u`
  and `v` lengths give an error.
- A point timestamp that differs from the frame's is flagged.
- The overlay follows a seek, using the D fixture plus `[f32; 32]` `u`
  and `v` components and a `count`.

## G. Formats (P7)

Files: `views/image_view/{layout,lut,resample}.rs`, `Cargo.toml` (adds
`fast_image_resize = "6"` and `demosaic = "0.3"`).

Changes:

- `rgb8` and `rgba8`:
  - a per-channel levels LUT
  - zooming in uses the hand-rolled nearest
  - zooming out uses fir `Box` with `crop`, followed by the LUT
- `mono_f32`: done in B. G adds a symmetric diverging colormap option
  (`colorous::RED_BLUE`) for signed fields.
- Bayer, once 26a adds `Bayer8<P>`/`Bayer16<P, BITS>` and their
  `ImageFormat` variants (26a dependency 8):
  - At ≥ 2 source px per device px, the hand-rolled 2×2 superpixel feeds
    the RGB path.
  - At closer zoom, `demosaic` bilinear runs on the visible region plus a
    2 px border, and is cached per (sample, region).
  - The readout shows the raw CFA value and its colour.
- Histogram equalisation as a `Levels` mode: the LUT is the normalised
  cumulative histogram.

Tests:
- RGB 1:1 is the identity, apart from the channel swizzle.
- Every 2×2 block of the superpixel output matches its RGGB inputs.
- The demosaic region at the frame edges does not panic.
- The equalised LUT is monotonic and uses the full range.

## H. Video source (outline)

The 26 §3.4 `FrameSource` trait takes a decoder-backed source.

- On macOS, `libs/video-toolbox` decodes H.264 into a `CVPixelBuffer`.
  `paint_surface` draws it with no CPU conversion, but only for YCbCr and
  only at the surface's own scaling.
- Elsewhere, the decoder writes planes that feed `render_into`.

This needs its own design doc.

## Ordering

| Order | Chunk | Depends on |
|---|---|---|
| 1 | A | 26a chunk A |
| 1 | B | nothing |
| 2 | C | A |
| 3 | D | A, B (C for manual testing) |
| 4 | E | D |
| 5 | F | E. Overlays need P4's `StarCentroids` and the preset needs P6's wiring; the histogram and ROI can land first. |
| 6 | G | B, D |

P3's gate ("view CPU-rendered frames in sim.py") needs A–E plus 26a. Until
26a lands, the synthetic source at 256² stands in for it.

## Open questions

None open.
