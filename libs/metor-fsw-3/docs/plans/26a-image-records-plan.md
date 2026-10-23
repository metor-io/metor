# 26a: Image records through rings, links, and the db, plan

Implements phase P1 of `26-star-tracker.md` (§0, §3.3): a fixed-size image
travels as one Frame field from a system, through a `Publish`/`Subscribe`
link, into the db, and back out through `GetTimeSeries`. The `StarImage` and
`StarTruth` contracts are 26c's, in a small contracts PR right after chunk A;
26b chunk J consumes them. "What 26b and 26d need" below maps their requests to chunks.

Decisions taken in review: bits live on the pixel type (`Mono16<12>`); `lod`
keeps the existing `"true"`/`"false"` convention; rings are capped at 8 MiB
and rings of 1 MiB or more (`LARGE_RING`) get 2× mirrors; the db stores images at full rate for now;
`GetTimeSeries` clamps oldest-first with paging; the batcher rotates ports;
Bayer is deferred.

Each chunk lands as one PR, gated on `cargo test`, `cargo clippy`, the Kani
proofs it touches, and `pytest` for chunks that change Python. A chunk ends
with an antagonistic review before the next one starts.

Order: A, B, C, and E do not depend on each other and can be written in
parallel. D follows C (both edit `db/src/lib.rs`). F needs all of them.

## Facts the plan relies on

Checked against the `sphw/star-tracker` tree. Where a fact differs from the
design doc, the plan follows the fact.

Records and derives:

- `FrameBytes<N>` is `#[repr(transparent)]` over `[u8; N]` and emits one
  `U8` component of shape `[N]` (`metor-component/src/bytes.rs:16-37`). A
  plain array field emits one component per element
  (`metor-component/src/vtable.rs:37-47`, `metadata.rs:51-62`).
- A field type's `Metadatatize::metadata` receives only the field's path.
  The derive has no per-field hook to add metadata keys
  (`derive-impl/src/metadatatize.rs:52-61`). `#[frame(...)]` on a field is
  parsed by the shared `Field` (`derive-impl/src/field.rs:6-30`), which knows
  `timestamp`, `nest`, `max`, `skip`, and `component_id`.
- The derived `Decomponentize` builds each leaf field by value and assigns it:
  `self.#ident = val` (`derive-impl/src/decomponentize.rs:43-46`). Nothing in
  `metor-fsw-3`, `db`, or `metor-panel` calls `Decomponentize` on a frame at
  run time; `Frame` only requires it as a bound (`metor-fsw-3/src/frame.rs:13-24`).
- Prototype (scratch crate, not committed): a `#[repr(transparent)]
  FrameImage<P, const W, const H>([[P; W]; H])` with zerocopy derives and a
  `Mono16<const BITS: u8 = 16>` pixel compiles under `#[derive(Frame)]` as
  `FrameImage<Mono16<12>, 1024, 1024>`. `size_of::<StarImage>()` is
  2,097,176, `MAX_LEN` equals it, `decode` borrows it in place, and the
  vtable has one field for the pixels. 2592×1944 (10,077,704 bytes) also
  compiles. A shape built as `const { &[H, W] }` is `'static`.
- The same prototype's derived `apply_value` overflows a 3 MiB thread stack
  in a debug build, even when `from_component_view` returns `Err` at once;
  it passes on a 1 MiB stack in release.
- `Frame` encode returns `self.as_bytes()` and never touches the scratch
  buffer (`macros/src/frame.rs:94-96`), yet every typed `Output` allocates
  `vec![0; T::MAX_LEN]` of scratch (`port/mod.rs:141`).
- `Sample<T: Frame + Copy>` stores the record by value in a `Cell`
  (`port/sample.rs:13-30`).
- `zerocopy`'s `FromZeros::new_box_zeroed` needs its `alloc` feature, which
  `metor-component` and `metor-fsw-3` do not enable (`metor-component/Cargo.toml`,
  `metor-fsw-3/Cargo.toml`).
- `ComponentMetadata.metadata` is a free `HashMap<String, String>`; the
  helpers are `with_string`, `with_hidden`, `with_enum`, `with_element_names`
  (`metor-proto/wkt/src/metadata.rs:15-89`).

Rings:

- A ring holds `port.depth * ring_depth` records (`coordinator/build.rs:778-795`),
  `ring_depth` defaults to 8 (`coordinator/config.rs:24`), and `Record::DEPTH`
  already defaults to 1 (`record.rs:61`). `ring_capacity` rounds up to a
  power of two (`port/mod.rs:33-41`). A 2 MiB frame therefore gets a
  32 MiB ring today, not the 8 MiB the design doc expects.
- A system on a thread (every async system, `fsw.db` included) reads and
  writes through mirror rings four times the real ring
  (`thread/os.rs:184`, `thread/os.rs:268-282`). A wasm guest's copy is twice
  the real ring (`wasm/pack.rs:27`). One 2 MiB image port feeding the db
  costs 32 MiB + 128 MiB today.
- A full ring refuses the write (`WriteError::WouldBlock`); the output
  counts it as dropped (`port/mod.rs:109-120`).
- `Python Target(ring_depth=8)` emits `ring_depth` (`_target.py:108,274`).

Links:

- `LinkParams::conn_cap` already exists per link, defaulting to 1 MiB
  (`link/mod.rs:65-68,80-82`). Python already passes it: `Publish(...,
  conn_cap=)` and `Subscribe(..., conn_cap=)` (`_builtins.py:45,200,274`).
  What is missing is a check, not the setting.
- Each slot allocates three `conn_cap` buffers: one receive, two send
  (`link/conns.rs:144-154`). `Publish` also allocates a `conn_cap` batch
  buffer (`link/publish.rs:32-39`). `Publish` ignores every body it receives
  (`publish.rs:66`), and `Subscribe` only sends its seed
  (`subscribe.rs:64-74`).
- `Batcher::fill` reads a port only while `max_len + PACKET_OVERHEAD` fits
  the spare scratch (`link/batch.rs:35-43`). A port whose `max_len + 8`
  exceeds `conn_cap` is never read: its records sit in the ring and nothing
  reports it. `PACKET_OVERHEAD` is 8 (`link/wire.rs:108`).
- The deframer returns `Oversized` for a body past `cap - 4`
  (`link/deframe.rs:67-75`), and the connection closes with only a
  `Closed` event (`link/conns.rs:380-385`). A dialing subscriber then
  reconnects and fails the same way.
- `PublishInputs::append_defs` and `SubscribeOutputs::append_defs` already
  run per-instance checks on the dynamic ports, with `cx.params` in hand
  (`publish.rs:99-107`, `subscribe.rs:131-138`). The dynamic ports' `max_len`
  is known there: inputs from the producer's `PortDef`
  (`coordinator/build.rs:515-533`), outputs from `OutputSource::Def` or the
  record table.
- A `DefError` from a def fails the build as `BuildError::Def`
  (`coordinator/build.rs:534-537`).

DB:

- Each component's WAL is `Disruptor::new(schema.size() * 1024)` on create
  and in `from_time_series`, and `* 256` on open (`db/src/lib.rs:921,948,976`).
  `Disruptor::new` touches every byte (`disruptor.rs:18-22`), so a 2 MiB
  component costs 2 GiB resident at create. Entries are `size + 8` bytes
  (`lib.rs:1056`, `lib.rs:1021`); a full WAL drops the sample with a warning
  (`lib.rs:1056-1062`).
- `insert_vtable` creates a component before its metadata arrives
  (`link/db.rs:239-252`), so a WAL size chosen at create can only use the
  schema.
- Nodes are `NODE_SIZE` = 32 MiB for both index and data
  (`time_series.rs:61,71-72`). A write that does not fit rolls a new node at
  the same timestamp (`time_series.rs:1084-1096`); a sample larger than a
  node fails there, since `AppendLog::with_size` uses `create_new`
  (`append_log.rs:94-99`). Installed nodes already size with
  `NODE_SIZE.max(len + 64)` (`store/mod.rs:184`).
- LoD runs on every visible, non-string, non-LoD component
  (`lod.rs:138-145`). The key `lod` already exists as an opt-in for hidden
  components, with value `"true"` (`lod.rs:67`). Eligibility is re-read after
  the first seal, when metadata has arrived (`lod.rs:168-174`).
- `GetTimeSeries` writes `len * (size + 8)` bytes and casts the packet length
  to `u32` (`lib.rs:1358-1372`). Each connection's buffers are 1 GiB
  (`lib.rs:1156-1157`). `limit` keeps the oldest samples of the range.
- The panel never sends `GetTimeSeries`. Its mirror subscribes with
  `Stream { RealTime }` (`db/src/remote/db.rs:165-168`), which sends a
  component's latest sample each time it changes, so a slow reader skips
  frames (`lib.rs:2218-2266`).
- `FixedRate` packs the nearest sample of every visible component into one
  table every tick (`lib.rs:2268-2320`). No Rust client uses it; the C++
  header does (`db/examples/db.hpp:1271`). UDP unicast streams go through
  the same handlers (`lib.rs:1274-1288,2137-2149`).

## A. `FrameImage` and image metadata

Files: `metor-component/src/image.rs` and `array.rs` (new),
`metor-component/src/lib.rs`, `metor-component/Cargo.toml`;
`metor-proto/wkt/src/image.rs` (new), `wkt/src/{lib,metadata}.rs`;
`metor-proto/src/com_de.rs`; `metor-component/derive-impl/src/decomponentize.rs`;
`metor-fsw-3/Cargo.toml`, `metor-fsw-3/src/lib.rs` (re-exports).

Pixel types:

- `trait Pixel: FromBytes + IntoBytes + Immutable + KnownLayout + Copy +
  'static { type Sample: FromBytes + IntoBytes + Immutable + Copy; const
  PRIM: PrimType; const CHANNELS: usize; const BITS: u8; const FORMAT:
  ImageFormat; fn view<'a>(samples: &'a [Self::Sample], shape: &'a [usize])
  -> ComponentView<'a>; }`. `Sample` is `u8`, `u16`, or `f32`. `view` wraps
  the slice; no copy.
- `Mono8(u8)`, `Mono16<const BITS: u8 = 16>(u16)`, `MonoF32(f32)`,
  `Rgb8([u8; 3])`, `Rgba8([u8; 4])`, each `#[repr(transparent)]`. `Mono16`
  asserts `1 <= BITS <= 16` in a `const` block. `BITS` is 8 for 8-bit types
  and 32 for `MonoF32`.
- Bits live on the pixel type, so the design's `#[frame(image(bits = 12))]`
  is not needed: the bits follow the type into nested structs, `FrameList`
  elements, and wasm packs with no derive change.

`FrameImage<P: Pixel, const W: usize, const H: usize>([[P; W]; H])`:

- `#[repr(transparent)]`, zerocopy derives including `FromZeros`. Not
  `Clone`/`Copy`, so it cannot sit in a `Sample` or be moved by value by
  accident. No `Default`. A struct holding one, such as `StarImage`, derives
  `FromZeros` too and is built with `new_box_zeroed()` at init (enable
  zerocopy's `alloc` feature in `metor-component` and `metor-fsw-3`).
- Alignment is the sample's (2 for `Mono16`), never above 16, so
  `Output::new`'s ring-alignment assertion passes.
- `AsVTable`: one `raw_field(0, size_of::<Self>() as u32, schema(P::PRIM,
  shape, component))`, shape `[H, W]` when `CHANNELS == 1`, else
  `[H, W, CHANNELS]`. Same empty-path fallback as `FrameBytes`
  (`"frame_image"`).
- `Metadatatize`: `path.to_metadata().with_image(P::FORMAT, P::BITS)`, which
  also sets `lod = "false"`.
- `AsComponentView`: `P::view(self.as_bytes(), const { &[H, W] })` (or the
  three-element shape).
- `FromComponentView`: checks prim type and shape, copies into a new value.
  It also overrides `assign_component_view` (below) to copy in place.
- Accessors: `pixels() -> &[P::Sample]` and `pixels_mut()`, row-major,
  length `W * H * CHANNELS`. For `Mono16<B>` that is a `&[u16]` of length
  `W * H`, which 26b's `star_tracker` borrows from the ring sample with no
  copy. Also `rows() -> &[[P; W]; H]`, `rows_mut()`, and `get(x, y) ->
  Option<P>` with `x` the column and `y` the row. `const WIDTH`, `HEIGHT`,
  `BYTES`. The `pixels` casts are zerocopy `ref_from_bytes` on
  `self.as_bytes()`, which cannot fail for these layouts (`PANIC Safety`
  note).
- Layout contract (26d need 2): element `U8`, `U16`, or `F32`; `dim` `[H, W]`,
  or `[H, W, C]` with C ∈ {3, 4}; row-major, no row padding, little-endian
  (every target we build for); `image.bits` at most the element width.
  Pixel (0, 0) is the first sample. 26a sets no pixel-centre convention;
  that belongs to `metor-star::camera` (26b).
- Field naming (26d need 5): a frame's fields are announced as
  `{port root}.{field}` (prototype: `st.frame_id`, `st.exposure_us`,
  `st.pixels`), so `pixels`, `exposure_us`, and `frame_id` share a parent
  path. A test pins it.

`FrameArray<T, const N: usize>([T; N])`, for `T` in `u8`, `u16`, `u32`,
`i32`, `f32`, `f64`:

- One component of shape `[N]`, like `FrameBytes`, with no `image.*` keys.
  It is the shaped type 26d's open question 2(b) asks for, so P4 can publish
  `StarCentroids` `u` and `v` as one component each. About 60 lines; it shares
  the view and in-place assign code with `FrameImage`.
- A `const` assertion that `H * W * CHANNELS * PRIM.size() ==
  size_of::<Self>()` and that `size_of::<Self>() <= u32::MAX`.

In-place decomponentize:

- `FromComponentView` gains a defaulted `fn assign_component_view(&mut self,
  view) -> Result<(), Error> { *self = Self::from_component_view(view)?;
  Ok(()) }`. The derive calls it instead of `self.#ident = val`. Small types
  are unchanged; `FrameImage` overrides it with `copy_from_slice` and never
  builds a stack temporary.

Metadata (new `metor-proto/wkt/src/image.rs`, re-exported from the crate
root). This is the single typed definition 26d need 1 asks for; the panel,
fsw-3, and the db all parse with it.

- Keys: `IMAGE_FORMAT_KEY = "image.format"`, `IMAGE_BITS_KEY = "image.bits"`,
  `IMAGE_BLACK_KEY = "image.black"`, `IMAGE_WHITE_KEY = "image.white"`,
  `IMAGE_STRETCH_KEY = "image.stretch"`, and `LOD_KEY = "lod"`. The db's
  `LOD_OPT_IN_KEY` becomes an alias of `LOD_KEY`. `lod` keeps the existing
  convention, `"true"` or `"false"`.
- `enum ImageFormat` with `as_str`/`FromStr` for `mono8`, `mono16`,
  `mono_f32`, `rgb8`, `rgba8`, and the Bayer spellings
  `bayer_{rggb,grbg,gbrg,bggr}{8,16}`. The Bayer variants are spellings only:
  no pixel type emits them while Bayer is deferred, but 26d's G chunk can
  parse them. `element() -> PrimType` and `channels() -> usize`.
- `enum Stretch { Linear, Sqrt, Asinh, Log, Auto }`; an unknown value parses
  as `Auto` (26d need 3).
- `struct ImageMeta { format, bits: u8, black: Option<f64>, white:
  Option<f64>, stretch: Stretch }`. `ImageMeta::parse(&ComponentMetadata) ->
  Result<Option<ImageMeta>, ImageMetaError>` gives `Ok(None)` without
  `image.format`; a missing `bits` is the element width; `black` and `white`
  are raw DN. `ImageMeta::check(&self, prim: PrimType, dim: &[u64])` checks
  a schema against the format: element type, rank, channel count, and bits
  within the element. 26d's `FrameLayout::from_component` builds on these
  two.
- `ComponentMetadata::with_image(format, bits)` (sets `lod = "false"` too),
  `image()`, `with_lod(bool)`, and `lod() -> Option<bool>` (anything other
  than `"true"`/`"false"` is `None`).
- No producer sets the display hints in P1. The panel defaults to `0` and
  `2^bits - 1`.

Crates considered: `zerocopy` (kept; already the layout crate, and
`new_box_zeroed` avoids a 2 MiB stack value); `bytemuck` (no: a second layout
crate for nothing new); `rgb` (its pixel types derive `bytemuck`, not
`zerocopy`, and the orphan rule keeps us from adding the derives); `imgref`
and `ndarray` (2D views with strides; useful to consumers such as
`metor-star`, not needed in the record type); `image` (heap-backed buffers,
large dependency; fit for the panel's PNG export, not for records);
`nox::ArrayView` (kept, through `ComponentView`, for the shaped view).

Tests (`metor-component/src/image.rs` tests, `wkt` tests):

- Nominal: `FrameImage<Mono16<12>, 4, 2>` emits one field of shape `[2, 4]`,
  `U16`, with `image.format = mono16`, `image.bits = 12`, `lod = false`;
  `Rgb8` emits `[2, 4, 3]` `U8`; view and `from_component_view` round-trip
  a pattern; `assign_component_view` copies in place.
- Nominal (`metor-fsw-3/src/tests`): a `#[derive(Frame)]` struct with a
  `FrameImage<Mono16<12>, 1024, 1024>` field has `MAX_LEN` 2,097,176, one
  vtable field per scalar plus one for the pixels, decodes in place from
  `new_box_zeroed` bytes, and its derived `apply_value` on the pixels runs
  on a 512 KiB thread stack in a debug build.
- Nominal: `ImageMeta::parse` on the metadata of a `#[derive(Frame)]`
  struct's image field gives `mono16`, bits 12, `Stretch::Auto`, and
  `check` accepts its schema; `pixels()` on `Mono16<12>` is a `&[u16]` of
  length `W * H` whose `[y * W + x]` equals `get(x, y)`; the field-naming
  test sees `root.pixels` and `root.exposure_us` under one parent;
  `FrameArray<f32, 32>` emits one `F32 [32]` component with no `image.*`
  keys.
- Edge: `W = 1` or `H = 1`; `MonoF32` with NaN pixels round-trips bit for bit;
  `ImageFormat::from_str` on every `as_str`, Bayer spellings included; a
  missing `image.bits` parses as the element width; `image.stretch = "foo"`
  parses as `Auto`.
- Failure: a view of the wrong prim type, of the wrong shape, or of the wrong
  length is `InvalidComponentData` and leaves the target unchanged;
  `Mono16<0>` and `Mono16<17>` fail to compile (trybuild in
  `metor-fsw-3/tests/ui`); `lod()` on `"off"` is `None`; `check` rejects
  `mono16` over `U8`, `rgb8` with C = 4, and bits 17, each with a distinct
  `ImageMetaError`; an unknown `image.format` is an error, not `None`.

Kani: none. The size relation is a `const` assertion.

## B. Ring sizing for large records

Files: `coordinator/config.rs`, `coordinator/build.rs`, `port/mod.rs`,
`record.rs`, `macros/src/frame.rs`, `mirror.rs`, `thread/os.rs`,
`wasm/pack.rs`, `wasm/tests.rs`; Python `_target.py`; goldens.

- `CoordinatorConfig::ring_bytes: NonZeroUsize`, default 8 MiB,
  `#[serde(default)]`. It caps the records a ring holds, never below two:
  `ring_records(max_len, depth, ring_bytes) = min(depth, max(2, ring_bytes /
  frame_len(max_len)))`. `RingSpec::allocate` uses it before
  `ring_capacity`. Small records are unaffected (8 MiB holds thousands); a
  2 MiB frame gets 3 records in an 8 MiB ring.
- Mirrors for large rings: `fn mirror_factor(capacity: usize, factor: usize)
  -> usize` returns `2` when `capacity >= LARGE_RING` (1 MiB) and `factor`
  otherwise. `Mirror::ring_for` (thread, `factor` 4) and the wasm copy
  (`COPY_FACTOR` 2, so unchanged) both call it. Why 2× is enough: the copy
  wraps at a different offset than the real ring, so holding everything a
  full real ring holds needs at most `capacity + frame_len(max_len)` bytes,
  and a ring always fits two records, so that is at most 1.5× `capacity`;
  2× is the next power of two. The 4× today is slack for an async system
  that falls a few cycles behind. For a large ring that slack becomes
  counted drops, which the mirror already reports every 100 cycles. One
  image port feeding the db drops from 32 + 128 MiB to 8 + 16 MiB.
- `Payload::SCRATCH_LEN: usize = Self::MAX_LEN`. `#[derive(Frame)]` sets it
  to 0, and `Output::counting` allocates `SCRATCH_LEN` bytes, so a frame
  output holds no 2 MiB scratch.
- Python: `Target(ring_bytes=8 << 20)` emits `ring_bytes`; a value below 1 is
  a `ConfigError`.

Wasm guests (26b need 4). The guest never reads the host ring. Each cycle
`WasmStep` copies every new record from the real ring into the guest's copy
in guest memory (`wasm/step.rs:15-21`), and `Input<StarImage>::latest()` in
the guest borrows from that copy, as it does natively. So a guest reading
images pays one 2 MiB `memcpy` per new frame and holds a 16 MiB copy ring
(2× the 8 MiB ring) in linear memory, well under the 4 GiB limit
(`wasm/guest.rs:24`). No read puts a frame on the guest stack. A guest that
writes images builds the frame with `new_box_zeroed()`, since the wasm32
stack is 1 MiB, and with `SCRATCH_LEN = 0` its output holds no scratch
copy.

Tests:

- Nominal (`coordinator/build.rs` tests): a 2 MiB frame port gets a ring of
  at most `next_power_of_two(ring_bytes)` that takes three writes without a
  read; a 64-byte port's ring is unchanged from today.
- Nominal (`thread` tests): an async system reading a 2 MiB port through a
  2× mirror receives every frame at 30 Hz when it keeps up.
- Nominal (`wasm/tests.rs`): a guest reads a 2 MiB frame with `latest()` and
  reports `pixels()[0]` and the last pixel; a guest writes one built with
  `new_box_zeroed()`. Both run with the guest's default stack. The test logs
  the per-frame copy time for 26b's budget.
- Edge: `ring_bytes` smaller than one record still gives two records;
  `ring_depth = 1` with a 2 MiB record; a frame output's scratch is empty
  and a postcard output's is `MAX_LEN`; a ring just under `LARGE_RING` keeps
  the 4× mirror.
- Failure: a record whose two-record ring overflows `usize` is
  `RingTooLarge`, as today; Python rejects `ring_bytes=0`; an async reader
  that sleeps for 10 cycles loses frames, and the mirror's drop report counts
  each one.

Kani (`coordinator/build.rs` and `mirror.rs` proofs): for any
`max_len <= u32::MAX`, `depth`, and `ring_bytes`, `ring_records` is at least
2 and at most `max(depth, 2)`, and when `ring_capacity(max_len, records)` is
`Some` it is at most `max(ring_bytes, 2 * frame_len(max_len))
.next_power_of_two()`. For any ring of `capacity` that holds at least two
records of `max_len`, `mirror_factor(capacity, f) * capacity >= capacity +
frame_len(max_len)`.

## C. DB storage for large components

Files: `db/src/lib.rs`, `db/src/time_series.rs`, `db/src/lod.rs`.

WAL:

- `fn wal_capacity(size: usize, entries: usize) -> usize`: with `entry =
  size + 8`, `entry * clamp(WAL_BUDGET / entry, WAL_MIN_ENTRIES, entries)`.
  `WAL_BUDGET = 64 MiB`, `WAL_MIN_ENTRIES = 4`. Create and
  `from_time_series` pass 1024, open passes 256. The capacity is a multiple
  of the entry, so a grant never leaves a wrap gap.
- Small components keep about today's sample count (an f64 goes from 8 KiB to
  16 KiB, since today's `size * 1024` ignores the timestamp). A 2 MiB
  component gets 31 entries, about 64 MiB.
- The size is chosen from the schema only, since metadata arrives later.

Nodes:

- `fn node_data_size(element: u64) -> u64 = NODE_SIZE.max(element *
  NODE_MIN_ELEMENTS + 64)`, `NODE_MIN_ELEMENTS = 64`.
  `TimeSeriesNode::create` sizes `data` with it; `index` stays `NODE_SIZE`.
  Files are sparse, and `AppendLog::open` reads the size from the file, so
  old nodes open unchanged. A 2 MiB component rolls a 128 MiB node every
  64 samples.

LoD:

- `eligible` also returns false when `meta.lod() == Some(false)`, or when
  `meta.image_format().is_some()` and `meta.lod() != Some(true)`.

Crates considered: `memmap2::MmapMut::map_anon` (already a db dependency)
would make the WAL's zeroing lazy. Deferred: with the budget the eager
fill is at most 64 MiB per component, and the change touches every
`Disruptor` reader.

Tests (`db/src/lib.rs`, `time_series.rs`, `lod.rs` tests, tempdirs):

- Nominal: a `[1024, 1024]` `U16` component takes 100 pushes at 2 MiB with
  the persist task running, and every sample reads back; its WAL is at most
  64 MiB; an f64 component's WAL holds at least 1024 entries.
- Nominal: a component with `image.format` and no `lod` key gets no LoD
  levels after sealing; the same schema without it gets levels.
- Edge: a 40 MiB element (past the old `NODE_SIZE`) pushes and reads back; a
  node created at 32 MiB before the change opens and appends; `lod = "true"`
  on an image re-enables LoD.
- Failure: with the persist task stopped, pushes past the WAL's entries are
  dropped with the existing warning and none corrupts the WAL; `lod =
  "false"` on an ordinary f64 component disables LoD.

Kani (`lib.rs` proofs): for any `size <= u32::MAX` and `entries` in `1..=1024`,
`wal_capacity` does not overflow, is a multiple of `size + 8`, holds at least
`min(WAL_MIN_ENTRIES, entries)` entries and at most `entries`, and is at most
`max(WAL_BUDGET, WAL_MIN_ENTRIES * (size + 8))`. `node_data_size(e) >= e + 64`.

## D. DB serving large components

Files: `db/src/lib.rs`.

- `GetTimeSeries`: `fn series_len(range_len, limit, size) -> Result<usize,
  Error>` returns `min(range_len, limit, (SERIES_BUDGET - 8 -
  PacketHeader) / (size + 8))`, `SERIES_BUDGET = 256 MiB`. A component whose
  one sample does not fit is `Error::ComponentTooLarge`. The packet length is
  `u32::try_from`, which cannot fail after the clamp. The response still
  keeps the oldest samples, so a client pages by asking again from the last
  timestamp plus one.
- `FixedRate`: `visible_components` gains a sibling, `fixed_rate_components`,
  that also drops components with `image.format`. They stay reachable
  through `RealTime` and `GetTimeSeries`. `FixedRateBehavior` is unchanged,
  so the C++ header keeps working.
- UDP: a unicast stream skips components whose table packet exceeds
  `MAX_UDP_PAYLOAD` (65,507 bytes), and logs each skipped component once,
  instead of failing every send.
- `RealTime` is unchanged: it already sends only the latest sample.

Tests (`db/src/lib.rs` tests and `db/tests`):

- Nominal: 300 samples of 2 MiB; a `GetTimeSeries` with no limit returns the
  oldest 127, and paging from the last timestamp returns the rest in order;
  a `FixedRate` stream over a db holding an image and an f64 carries only the
  f64; a `RealTime` stream carries both.
- Edge: `limit = Some(0)` returns an empty series; a range with one sample;
  an f64 component with 10^6 samples is not clamped.
- Failure: a component whose single sample exceeds `SERIES_BUDGET` answers
  `ErrorResponse`, not a truncated packet; a UDP stream over an image
  component sends the small components and logs the image once.

Kani: for any `range_len`, `limit`, and `size <= u32::MAX`, `series_len`'s
`Ok(len)` satisfies `len <= range_len`, `len <= limit`, and `8 + header + len *
(size + 8) <= SERIES_BUDGET <= u32::MAX`, and it is `Ok(n >= 1)` whenever
`range_len >= 1`, `limit >= 1`, and one sample fits.

## E. Link capacity checks

Files: `link/{mod,conns,publish,subscribe,batch,wire}.rs`, `def.rs`; Python
`_builtins.py`; `tests/link.rs`.

Config-time check:

- `wire::fits(max_len, conn_cap) -> bool`: `max_len.checked_add(PACKET_OVERHEAD)
  .is_some_and(|n| n <= conn_cap)`.
- `PublishInputs::append_defs` and `SubscribeOutputs::append_defs` decode
  `conn_cap` from `cx.params` (default 1 MiB) and reject the first dynamic
  port that does not fit with `DefError::PortExceedsConnCap { port,
  max_len, conn_cap }`. The message names the `conn_cap` that would fit
  (`max_len + 8`, rounded up to a MiB). The build fails as
  `BuildError::Def` with the system's location.
- `Batcher::fill`'s silent skip stays as a backstop; with the check it cannot
  happen.

Batcher fairness (about 30 lines):

- `Batcher` keeps `next: usize`, the port the next batch starts at.
  `DynInputs::iter_mut_from(start)` yields `(index, def, input)` from `start`
  to the end and then from 0, using `split_at_mut` (no allocation). `fill`
  zips it with `ids` by index. After a batch, `next` is the first port that
  still had a record waiting when the buffer filled, or `next + 1` if every
  port drained. Records within a port keep their order; batches never
  promised an order across ports.
- This stops small ports listed before an image port from crowding the image
  out of every batch.

Buffers by direction:

- `ConnsConfig { rx_cap, tx_cap, .. }` replaces `conn_cap`. `Publish` takes
  `tx_cap = conn_cap` and `rx_cap = CONTROL_CAP`; `Subscribe` takes `rx_cap =
  conn_cap` and `tx_cap = CONTROL_CAP`, where `CONTROL_CAP = min(conn_cap,
  64 KiB)`. `Conns::new` checks the seed against `tx_cap`. A 4 MiB image
  link with one peer drops from 16 MiB to about 12 MiB (`Publish`: two send
  buffers plus the batch) and the listener default of 8 peers from 100 MiB
  to 68 MiB.

Oversized bodies:

- `Event::Oversized { len }` replaces the bare close. The deframer reports
  the prefix's length, and the link logs a fault `oversized_packet` with the
  length and its `conn_cap`, then closes as today.

Python:

- `Publish` and `Subscribe` reject a `conn_cap` below 64 KiB or above
  `u32::MAX` with `ConfigError`. The docstrings say an image link needs
  `conn_cap` of at least its largest record plus 8 bytes, and that the build
  checks it.

Tests:

- Nominal (`tests/link.rs`): a `Publish` with `conn_cap = 4 MiB` serving a
  2 MiB frame delivers it to a raw client; a dialing `Subscribe` with the same
  cap mirrors it.
- Edge: `max_len + 8 == conn_cap` builds and sends; a `Publish` with no inputs
  builds with any cap; a dialing subscriber's seed for 32 message records
  fits `CONTROL_CAP`.
- Failure: `max_len + 8 == conn_cap + 1` fails the build with
  `PortExceedsConnCap` naming the port, for `Publish` (input) and for
  `Subscribe` (output); a raw client sending a 2 MiB packet to a 1 MiB
  subscriber is closed and logged `oversized_packet`; Python rejects
  `conn_cap=0` and `conn_cap=1 << 32`.
- `link/conns/tests.rs`: the steady-state zero-allocation test runs with
  split caps.
- Batcher (`link/batch.rs` tests): nominal, three ports that each fill half
  the buffer every cycle are all sent within two batches; edge, one port
  rotates to itself; failure, a port listed first that alone fills the
  buffer every cycle no longer starves a 2 MiB port listed last (it is sent
  within two batches), and records within each port stay in order.

Kani (`link/deframe.rs` proofs): for any `max_len` and `conn_cap`, if
`fits(max_len, conn_cap)` then a packet of `max_len` payload appended to an
empty `conn_cap` buffer is written whole, and a deframer of `rx_cap =
conn_cap` yields its body rather than `Oversized`.

## F. Integration: 1024² Mono16 through link, db, and `GetTimeSeries`

Files: new `tests/image.rs`, `tests/common` (a `camera` test system).

- `camera` writes an `ImageFrame { timestamp, frame_id: u64, pixels:
  FrameImage<Mono16<12>, 1024, 1024> }` every `1/rate` seconds from a
  `Box` allocated at make, filling pixel `i` with `(frame_id + i) & 0xfff`.
- Target `cam` (`Clock::Wall`, 100 Hz): `camera` and a `Publish` listening
  with `conn_cap = 4 MiB`, `max_connections = 1`.
- Target `gw`: a dialing `Subscribe` with `conn_cap = 4 MiB` whose `image`
  output carries the published def, and a `fsw.db` reading it.
- A client waits for the db, then pages `GetTimeSeries` over
  `gw...image.pixels` and `...frame_id`.

Tests:

- Nominal: at 10 Hz and 30 Hz for 3 s each, every `frame_id` in the window
  arrives once, in order; each frame's pixels match the pattern;
  `link_status` shows no `batches_dropped` or `inbound_dropped`;
  `db_status.refused` is 0; the metadata dump has `image.format = mono16`,
  `image.bits = 12`, `lod = false` on the pixels component;
  `ImageMeta::parse` and `check` accept the dumped metadata and schema, and
  `pixels`, `frame_id`, and `exposure_us` share a parent path. If 26d chunk A
  has landed, the test also builds its `FrameLayout` from the dump (26d
  dependency 1).
- Edge: reconnecting the subscriber mid-run loses only frames sent while it
  was down, and the series stays ordered.
- Failure: the same targets with the subscriber's `conn_cap` at 1 MiB fail to
  build with `PortExceedsConnCap`; a `GetTimeSeries` over 300 frames returns
  127 and pages to the end.
- The test records peak RSS growth on Linux (`/proc/self/status`) and asserts
  it stays under 512 MiB, to catch a return of eager 2 GiB buffers.

Run it under `orb` on nixos as well as macOS, since the link uses io_uring
there.

## What 26b and 26d need

26d ("What this plan needs from 26a"):

| # | Need | Where |
|---|---|---|
| 1 | One typed metadata definition in a crate the panel uses: `ImageMeta` with `parse`, key constants, `ImageFormat` with the mono, RGB, and Bayer spellings | A, in `metor-proto-wkt` (`image.rs`). Bayer is spellings only. F checks it against a real frame. |
| 2 | Schema: `U8`/`U16`/`F32`, `[H, W]` or `[H, W, C]` with C ∈ {3, 4}, row-major, no padding, little-endian, bits within the element, missing bits means full width | A, layout contract, enforced by `ImageMeta::check` |
| 3 | `black`/`white` in raw DN; `stretch` ∈ {linear, sqrt, asinh, log, auto}, unknown means auto | A, `ImageMeta`, `Stretch` |
| 4 | WAL byte budget, LoD skipped for images, node size | C. Once C lands, 26d's synthetic source can default to 1024². |
| 5 | `pixels` and `exposure_us` share a parent path | A, field-naming test; F checks it end to end |
| 6 | `StarCentroids` shape for overlays (26d open question 2) | A adds `FrameArray<T, N>`, so P4 can take option (b). The record itself is P4's. |

Hiding image components from plot pickers before P3 is 26d chunk A. 26a only
supplies `image.format`, which that chunk filters on.

26b ("What this plan needs from 26a"):

| Need | Where |
|---|---|
| `FrameImage<Mono16, W, H>`, `#[repr(transparent)]`, alignment ≥ 2, `pixels() -> &[u16]` row-major of length W·H | A. `pixels()` returns `&[P::Sample]`, which is `&[u16]` for `Mono16<B>`. Alignment is 2. |
| `FromZeros` on `FrameImage` and `StarImage`, built with `new_box_zeroed()` | A. `FrameImage` derives `FromZeros`, so `StarImage` can too. zerocopy's `alloc` feature is turned on. |
| `StarImage` and `StarTruth` definitions | Not in 26a. 26c defines them in a contracts PR right after 26a chunk A; 26b chunk J consumes them. |
| `Input<StarImage>::latest()` at 2 MiB, native and in the wasm guest, whether the guest copies, and the cost | B. The guest copies each new frame once (2 MiB `memcpy`) into a 16 MiB copy ring, then borrows it. B's wasm test measures the copy time. |
| Pixel-centre convention | Not 26a's. Pixel (0, 0) is the first sample, and `metor-star::camera` sets the centre convention. |

## Follow-ups

1. **Image storage and decimation.** P1 stores every frame: 60 MB/s at
   30 Hz, about 216 GB/h. That is accepted for now. Revisit db storage for
   images later: decimated or on-demand storage, retention, and whether
   offload and tiering should treat image components differently.
2. **Bayer pixel types.** Deferred. `ImageFormat` already has the spellings;
   `Bayer8<P>`/`Bayer16<P, BITS>` would be additive.
3. **Display hints from producers.** Producers cannot set `image.black`,
   `white`, or `stretch` in P1. A general per-field `#[frame(meta(...))]`
   attribute would allow it.
