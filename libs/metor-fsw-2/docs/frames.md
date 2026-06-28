# Work-Package 3 — Frames & component derives

Status: **design only, pre-implementation**. Reviewer sign-off required before any code
lands. No Rust in this WP — this document specifies what WP3 builds on top of the
**already-landed** WP2 vtable extensions.

This WP turns the WP2 primitives (`Op::{Frame, List, Map, PathComponent}`, `ElementFields`,
`PathHasher`, `RealizedField::{frame, element}`, the `frame`/`name`/`path_component`/`list`/
`map` builders) into an ergonomic, derive-driven **frame** surface for `metor-fsw-2`: a
struct annotated once becomes a timestamped, `ComponentId`-named group of components that
serializes to a metor-proto table with no extra step, and that can carry runtime-dynamic
`FrameList`/`FrameMap` members.

Relevant existing code (read before implementing):
- `libs/metor-proto/src/vtable.rs` — `Op`, `Field`, `VTable`, `RealizedField`, `WalkCtx`,
  `for_each_field`/`walk_field`/`expand_dynamic`, the `builder` module (`frame`, `name`,
  `path_component`, `list`, `map`, `FieldBuilder`, `VTableBuilder`, `offset_table_ops`).
- `libs/metor-proto/src/hash.rs` — `PathHasher` (`push`, `push_bytes`, `push_index`,
  `finish`).
- `libs/metor-fsw/src/vtable.rs` — the `AsVTable` trait (`vtable_fields(path)` / `as_vtable`).
- `libs/metor-fsw/src/metadata.rs` — the `Metadatatize` trait (std-gated).
- `libs/metor-fsw/src/path.rs` — `ComponentPath`, `ChainPath`, `hash_into`,
  `to_component_id`, `to_metadata`.
- `libs/metor-fsw/macros/src/{lib.rs, as_vtable.rs, metadatatize.rs, componentize.rs,
  decomponentize.rs}` — the derive crate; `componentize.rs`/`decomponentize.rs` are
  present but **not wired in** (see §2).
- `libs/metor-proto/src/com_de.rs` — `Componentize` (`sink_columns`, `MAX_SIZE`),
  `Decomponentize` (`apply_value`), `AsComponentView`, `FromComponentView`.
- `libs/metor-proto/src/types.rs` — `ComponentId`, `PrimType`, `ComponentView`,
  `Timestamp`, `LenPacket` (`push`, `push_aligned`, `extend_aligned`, `extend_from_slice`).
- `libs/metor-proto/wkt/src/metadata.rs` / `msgs.rs` — `ComponentMetadata` (`with_prefix`),
  `SetComponentMetadata`.
- `libs/metor-fsw-2/ring/src/lib.rs` — record framing (`round_up8`, `frame_len`); records
  are always **8-byte aligned**, which fixes the trailer alignment we adopt below.

---

## 1. Frame concept & trait

### 1.1 What a frame is

A **frame** is a `#[repr(C)]` struct that groups components sharing one logical timestamp
and is itself named by a `ComponentId` (the fnv1a-64 hash of the frame's dotted name, top
bit masked — identical to `ComponentId::new`). The canonical example (DESIGN.md):

```rust
use nox::{Quaternion, Vector, array::ArrayRepr};

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct IMU {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: Vector<f64, 3, ArrayRepr>,
    accel: Vector<f64, 3, ArrayRepr>,
    attitude: Quaternion<f64, ArrayRepr>,
}
```

Each field is a component (`IMU.omega` → `ComponentId::new("imu.omega")` when named under the
frame prefix); the shared `timestamp` is marked once and propagated to every field; and the
whole table is tagged with the frame id `ComponentId::new("imu")`.

### 1.1.1 Field types — scalars, nox vectors/quaternions, and `[T; N]` arrays

A frame field is one component whose primitive type is its scalar element type. Three field
shapes are supported:

- **Scalars** (`f64`, `u32`, `bool`, …) → one shape-`[]` component.
- **nox spatial types — the recommended pattern for vectors/quaternions.**
  `Vector<f64, 3, ArrayRepr>`, `Quaternion<f64, ArrayRepr>`,
  `SpatialMotion`/`SpatialTransform`/… each serialize to a single multi-element component
  (`imu.omega` of prim `f64`, shape `[3]`; a quaternion is shape `[4]`). nox types are
  representation-consistent: the vtable/`apply` path and the in-process
  `Componentize`/`Decomponentize` path produce the **same** single dotted component, so
  prefer them for any vector- or quaternion-valued field.
- **Plain `[T; N]` arrays** (`[f64; 3]`, `[bool; 3]`, …) are also supported as frame fields.
  The in-process `Componentize`/`Decomponentize` path treats `[T; N]` as one component of
  prim `T` and shape `[N]` (via `AsComponentView`/`FromComponentView`, mirroring nox).
  > Caveat: the byte-level vtable/`apply` path currently expands a `[T; N]` field into `N`
  > **indexed scalar** components (`imu.omega.0`, `imu.omega.1`, `imu.omega.2`), so the two
  > paths disagree on naming for arrays. If you need a single shape-`[N]` component on both
  > paths, use the nox `Vector`/`Quaternion` types above. (Unifying the `[T; N]` vtable
  > representation is tracked for a later pass.)

### 1.2 The `Frame` trait — a thin marker over existing traits

`Frame` is a **new but thin** trait. It does *not* replace `AsVTable`/`Metadatatize`/
`Componentize`/`Decomponentize`; it bundles them and adds the frame identity + timestamp
accessor. It is deliberately **not** `metor_proto::component::Component` — `Component`
requires a `schema()` for a single primitive array, which a multi-field frame is not.

```rust
// libs/metor-fsw-2 (new module, e.g. frame.rs)
pub trait Frame: AsVTable + Metadatatize + Componentize + Decomponentize {
    /// Frame name; the dotted prefix all member components hang off.
    const NAME: &'static str;
    /// fnv1a-64 of NAME, top-bit masked — same construction as every ComponentId.
    const FRAME_ID: ComponentId = ComponentId::new(Self::NAME);
    /// The shared timestamp marked with `#[metor_fsw(timestamp)]`.
    fn timestamp(&self) -> Timestamp;
}
```

Rationale:
- `AsVTable` already yields the table description; `Frame` only adds the `Op::Frame` wrap
  (§1.3) and pins `NAME`/`FRAME_ID`.
- `Componentize`/`Decomponentize` give the in-process struct↔components path (used by
  systems wiring and by the ring-buffer writer/reader) without going through bytes+vtable.
- `Metadatatize` emits the id→name records for the static members.
- The timestamp accessor lets the coordinator/ring writer stamp the frame uniformly.

### 1.3 Struct → VTable (the `Op::Frame` wrap)

The existing `AsVTable` derive (`as_vtable.rs`) already composes nested fields with
`vtable_fields(path)` and propagates the marked timestamp via `FieldBuilder::with_timestamp`.
WP3 adds exactly two things on top:

1. **Frame tag.** Each field's op chain is wrapped in the WP2 `frame(FRAME_ID, ...)` builder
   so `RealizedField::frame` carries `FRAME_ID` for every member. This needs a small
   `FieldBuilder::with_frame(component_id)` helper in `metor-proto::vtable::builder`
   (mirroring the existing `with_timestamp`), since `FieldBuilder.arg` is private. The
   derive then does `.map(|f| f.with_frame(FRAME_ID))` — one line alongside the existing
   `timestamp_map`.
2. **Frame name as the root prefix.** The frame derive seeds `vtable_fields` with the frame
   name (today `#[metor_fsw(parent = "...")]` is already used this way in cube-sat, e.g.
   `parent = "cube_sat"`). `NAME` defaults to the snake-cased ident and is overridable with
   `#[metor_fsw(name = "...")]` / `parent`.

Worked IMU vtable (what the derive expands to, using the landed builders):

```rust
fn as_vtable() -> VTable {
    let time = table!(IMU::timestamp);                       // raw_table(offset, size)
    vtable([
        field!(IMU::omega,
            schema(PrimType::F64, &[3],
                component(ComponentId::new("imu.omega"))))
            .with_timestamp(time.clone())
            .with_frame(ComponentId::new("imu")),
        field!(IMU::accel,
            schema(PrimType::F64, &[3],
                component(ComponentId::new("imu.accel"))))
            .with_timestamp(time)
            .with_frame(ComponentId::new("imu")),
    ])
}
```

`apply` then drives `for_each_field`, and each `RealizedField` arrives at the sink with
`component_id = imu.omega`, `timestamp = <marked>`, `frame = Some(imu)`. Because `with_frame`
wraps a single shared `frame(...)` Arc, `VTableBuilder`'s Arc-dedup stores the frame id once
(matching the WP2 "all fields reference one `Frame` op" decision).

> Note: the `timestamp` field itself is normally *not* emitted as a component (it is the
> source). Today the cube-sat `AsVTable` derive emits all non-skipped fields; WP3 should make
> the `#[metor_fsw(timestamp)]` field contribute the timestamp **source** only and not a
> standalone `timestamp` component, OR keep emitting it as a component — **open question Q1**.

---

## 2. Derives — finish & export `Componentize`/`Decomponentize`, add `#[derive(Frame)]`

### 2.1 Current state (what's actually there)

- `macros/src/lib.rs` registers **only** `#[derive(Metadatatize)]` and `#[derive(AsVTable)]`,
  and declares **only** `mod as_vtable; mod metadatatize;`.
- `componentize.rs` and `decomponentize.rs` exist and are written, but:
  - they are **not** `mod`-declared in `lib.rs` and have **no** `#[proc_macro_derive]` entry;
  - they reference `field.nest` and `field.component_id()`, **neither of which exists** on the
    current `Field` (which has only `ident`, `ty`, `component_id: Option<String>`,
    `timestamp: bool`, plus a `component_name()` method).
- `metor-fsw/src/lib.rs` re-exports `AsVTable`, `Metadatatize` from the macro crate but not
  Componentize/Decomponentize (the traits live in `metor_proto::com_de`).

So "finish + export" is concrete: extend `Field`, wire the modules, register the derives,
re-export, and reconcile naming.

### 2.2 Extend the shared `Field`

Add to `macros/src/lib.rs::Field`:

```rust
#[derive(FromField)]
#[darling(attributes(metor_fsw))]
struct Field {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    component_id: Option<String>,
    #[darling(default)] timestamp: bool,
    #[darling(default)] nest: bool,        // NEW: descend into a sub-frame instead of leaf
    #[darling(default)] max: Option<usize>, // NEW: max cardinality for FrameList/FrameMap (§3)
}
impl Field {
    fn component_name(&self) -> String { /* existing */ }
    fn component_id(&self) -> String { self.component_name() } // NEW: alias the two callsites use
}
```

`nest` distinguishes a field that is itself a frame/struct (recurse via the trait) from a
leaf scalar (emit `apply_value` / `as_component_view`). This matches how `AsVTable` already
recurses unconditionally through `<#ty as AsVTable>::vtable_fields`; for Componentize/
Decomponentize the recursion must be explicit because the trait methods differ for leaves vs
nested.

### 2.3 What each derive generates (after finishing)

| derive | generates | leaf behavior | nested (`#[metor_fsw(nest)]`) |
|--------|-----------|---------------|-------------------------------|
| `AsVTable` (extend) | `vtable_fields(path)` **and** new `element_fields()` (§4) | `component(path.chain(name).id)` leaf / `path_component(name)` leaf | `<Ty>::vtable_fields(path.chain(name)).map(offset_by)` |
| `Metadatatize` | `metadata(prefix)` | `prefix.chain(name).to_metadata()` | `<Ty>::metadata(prefix.chain(name))` |
| `Componentize` (finish) | `sink_columns(out)` + `MAX_SIZE` | `out.apply_value(id, self.f.as_component_view(), None)` | `self.f.sink_columns(out)` |
| `Decomponentize` (finish) | `apply_value(id, view, ts)` | match `id == ID const` → `FromComponentView` | `self.f.apply_value(id, view, ts)` |
| `Frame` (new, §2.4) | the four above + `impl Frame` | — | — |

`Componentize::MAX_SIZE` must change from the hard-coded `0` to the real sum: fixed-field
sizes plus, for each `FrameList`/`FrameMap` field, its declared `max` cardinality times the
element stride (+ key-pool budget for maps). This is what lets the coordinator size output
ring buffers (§3.4).

### 2.4 The unifying `#[derive(Frame)]`

`#[derive(Frame)]` is the one-annotation entry point. It expands to **all four** derives plus
`impl Frame`, so a user writes:

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]   // optional; defaults to snake_case(ident)
struct IMU { #[metor_fsw(timestamp)] timestamp: Timestamp, omega: Vector<f64, 3, ArrayRepr>, accel: Vector<f64, 3, ArrayRepr> }
```

Reconciliation with the existing standalone derives (**extend, don't fork**):
- `#[derive(Frame)]` is sugar that calls the same code paths as the four individual derives;
  the individual derives remain usable (cube-sat already stacks `AsVTable, Metadatatize,
  IntoBytes`). Implement `Frame` by having its proc-macro emit the other four token streams
  plus the `impl Frame` block (or, simpler, emit only `impl Frame` and require the user to
  also derive the four — **open question Q2: bundle vs require**; the task asks for "one
  annotation", so the bundling expansion is preferred).
- `AsVTable` is **extended** (add `with_frame` wrap + `element_fields`), not replaced, so
  existing `#[derive(AsVTable)]` users are unaffected unless they opt into `Frame`.
- The macro crate keeps a single shared `Field`/attribute surface (§2.2) so all five derives
  parse `#[metor_fsw(timestamp | nest | name | parent | component_id | max | group)]`
  consistently.

### 2.5 Wiring checklist (mechanical)

1. `macros/src/lib.rs`: add `mod componentize; mod decomponentize;`, `#[proc_macro_derive(
   Componentize, attributes(metor_fsw))]`, `#[proc_macro_derive(Decomponentize, ...)]`, and a
   `#[proc_macro_derive(Frame, ...)]` that emits the bundle.
2. Extend `Field` (§2.2); add `component_id()`; thread `nest`/`max`.
3. `metor-fsw/src/lib.rs`: re-export `Componentize`, `Decomponentize`, `Frame` from the macro
   crate (the *traits* stay in `metor_proto::com_de` / the new `frame.rs`).
4. New `metor-fsw-2` `frame.rs` defines the `Frame` trait (§1.2).

---

## 3. `FrameList<T>` / `FrameMap<K, V>` dynamic types

These are the Rust types a frame embeds to carry runtime-sized members. The decided dotted
model: list elements are addressed positionally (`processes.0.pid`), map entries by a
**name key** with **no `.`** (`processes.htop.pid`). Both are backed by the WP2 trailer.

### 3.1 In-struct representation = the 8-byte slot (not the data)

The element data lives in the trailer **after** the fixed region, exactly as the landed
`expand_dynamic` reads it. So inside the `#[repr(C)]` frame, a `FrameList`/`FrameMap` field
**is the slot** — 8 bytes, `{ trailer_off: u32, byte_len: u32 }` — and nothing more:

```rust
#[repr(C)]
pub struct FrameList<T, const MAX: usize> {
    slot: Slot,            // { trailer_off: u32, byte_len: u32 }  (8 bytes, the WP2 slot)
    _ty: PhantomData<T>,
}

#[repr(C)]
pub struct FrameMap<K, V, const MAX: usize, const MAX_KEY: usize = 32> {
    slot: Slot,
    _kv: PhantomData<(K, V)>,
}

#[repr(C)]
struct Slot { trailer_off: u32, byte_len: u32 }
```

This keeps the frame `#[repr(C)]`/`IntoBytes` fixed-size (so it still fits the ring's
fixed-shard fast path for the *static* part) while the variable data is appended past it.
The const generics carry the **max cardinality** (and max key length for maps) so `MAX_SIZE`
is computable (§3.4). `K` is a name-string key type (`&str`/`String`/a `Name` newtype),
**not** a `ComponentId` — the DESIGN.md `FrameMap<ComponentId, Process>` sketch is superseded
by the dotted-name decision in `vtable-dynamic.md §3`.

### 3.2 `AsVTable` for the dynamic types (emit `Op::List`/`Op::Map`)

`FrameList<T, MAX>::vtable_fields(path)` emits **one** field — the slot — whose arg is the
WP2 `list(prefix, members, stride)`:

```rust
impl<T: AsVTable, const MAX: usize> AsVTable for FrameList<T, MAX> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let prefix = path.to_name();                       // full static dotted prefix, e.g. "processes"
        core::iter::once(raw_field(
            0, size_of::<Slot>() as u32,                   // the slot, offset patched by offset_by upstream
            list(&prefix, T::element_fields(), size_of::<T>() as u32),
        ))
    }
}
```

- `prefix` is the **full static dotted path** from root to this field (`path.to_name()`),
  because `expand_dynamic` seeds the per-top-level-field `PathHasher` empty and pushes
  `dynm.name` first. This is why the top-level list in the landed test uses `name = "processes"`.
- `T::element_fields()` is the new **dynamic member-template** form of `AsVTable` (§4): the
  members with `path_component` leaves and offsets relative to the element base. The
  enclosing frame derive then `with_frame`/`with_timestamp`-wraps and `offset_by`-shifts the
  slot field into the parent struct, exactly as it does for static fields.

`FrameMap` is identical but emits `map(prefix, V::element_fields(), stride, value_offset)`,
where `stride`/`value_offset` come from the producer's chosen entry layout
`{ key_off: u32, key_len: u32, <pad> value }` (§3.3); the derive computes
`value_offset = align_up(8, align_of::<V>())` and `stride = value_offset + size_of::<V>()`.

### 3.3 Producer side — writing a frame with dynamic members

This is the hard part. A producing system fills the fixed region normally, then appends the
trailer and patches each slot. Two layers:

**(a) Low-level byte writer (`FrameWriter`).** Wraps a growable buffer — a `LenPacket`
(`push_aligned`, `extend_aligned`, `extend_from_slice`) or a `&mut [u8]` cursor. The fixed
region is written first (the `#[repr(C)]` frame struct, slots zeroed); then for each dynamic
field the writer lays down, **8-byte aligned** (consistent with the ring's `round_up8`/
`frame_len` record alignment):

- *List:* the element array back-to-back at the current trailer offset, then patches the
  field's slot to `{ trailer_off, byte_len = count * stride }`.
- *Map:* the fixed-stride entry array `{ key_off, key_len, <pad> value }` first (slot delimits
  **just the entry array**, so `count = byte_len / stride` stays exact), then a **name pool**
  of the key bytes back-to-back after the entry array; each entry's `key_off`/`key_len` point
  table-absolute into that pool. Matches the landed §5b layout and the `read_slot`/`key_off`/
  `key_len` reads in `expand_dynamic`.

Nested dynamics use **one global, table-absolute trailer** (the landed invariant): a nested
list's slot lives inside its parent element's value region but points table-absolute into the
same trailer, so the writer just keeps appending and patching. The writer must reject map
keys containing `.` (the consumer already errors with `Error::InvalidComponentData`; the
writer should fail earlier/louder).

**(b) Typed convenience (`FrameMap`/`FrameList` builder handles).** A producing system
mutates a typed builder that records pending elements, then a single `finish`/`serialize`
pass emits the bytes via `FrameWriter`:

```rust
let mut w = FrameWriter::<SysFrame>::new(&mut packet);   // packet: LenPacket
w.timestamp(now);
w.list_field(field!(SysFrame::processes), |l| {          // l: ListWriter<Process>
    for p in live { l.push(p); }                          // Process: IntoBytes element
});
w.map_field(field!(SysFrame::hosts), |m| {               // m: MapWriter<&str, Host>
    for (name, h) in hosts { m.insert(name, h); }         // rejects '.' in name
});
let bytes = w.finish();                                   // fixed region + trailer, slots patched
```

The static-only case is unchanged: if a frame has no dynamic fields the writer just emits the
`#[repr(C)]` bytes (zero trailer), so it stays on the existing fixed-shard fast path.

### 3.4 `MAX_SIZE` / max-cardinality declaration

Output ring buffers must be sized at construction, but dynamic frames are runtime-sized. We
bound them with the const generics + the `#[metor_fsw(max = N)]` attribute, surfaced through
`Componentize::MAX_SIZE`:

```
MAX_SIZE(frame) = size_of::<FixedRegion>()
                + Σ_lists ( MAX_i * stride_i )
                + Σ_maps  ( MAX_j * stride_j  +  MAX_j * MAX_KEY_j )   // entries + name pool
                + alignment padding (8-byte)
```

`FrameList<T, MAX>` / `FrameMap<K, V, MAX, MAX_KEY>` make `MAX` part of the type, so the
finished `Componentize` derive computes `MAX_SIZE` as a `const` expression. The coordinator
sizes the system's output ring to `MAX_SIZE` (rounded up via the ring's `frame_len`). If a
producer ever exceeds its declared `max`, the writer truncates and telemeters a health error
(systems report health as telemetry; DESIGN.md §Systems) rather than overflowing the buffer.

### 3.5 Consumer side — reading elements

Consumers reuse the **landed** realization unchanged. There are two access modes:

- **Sink / flat (db, UI, most systems):** call `vtable.apply(table_bytes, &mut sink)`. The
  landed `expand_dynamic` walks the slot → entry array → name pool, composes the dotted id
  via `PathHasher`, and pushes each element-member through `Decomponentize::apply_value`
  exactly like a static component. db/UI need **zero** value-path changes — a dynamic frame is
  just a runtime-variable *set* of ordinary components (vtable-dynamic.md §6.4).
- **Typed by index/key:** a consumer that wants `FrameList<T>`/`FrameMap<K, V>` ergonomics
  uses a thin reader that reads the slot from the fixed struct, derives `count`, and for a
  list indexes `trailer[trailer_off + i*stride ..]` as `T`, for a map scans entries and
  matches `key`. This reader is a presentation convenience over the same bytes; the
  authoritative semantics (including dotted ids, frame/timestamp inheritance, `.`-in-key
  rejection) are the landed `RealizedField`/`ElementKey` path, which the reader can also use
  via `realize_fields(Some(bytes))` and the `frame`/`element` fields on each `RealizedField`.

---

## 4. Nested dynamics support at the type level

Nesting (`processes.htop.threads.3.state`) is already correct at the **vtable/realize** level
(landed `test_dynamic_nested`). WP3's job is to make the **derives** emit those nested ops
naturally. The mechanism is a second derive output: `element_fields()`.

`#[derive(AsVTable)]` (extended) generates two member iterators:

1. `vtable_fields(path)` — *static* form, leaves are `component(path.chain(name).id)`. Used
   when the type is reached through compile-time struct nesting.
2. `element_fields()` — *dynamic member-template* form, leaves are `path_component(name)`
   with offsets **relative to the element base** (no `path` prefix baked in, because the
   runtime path is composed by the enclosing `list`/`map` at realize time). Used when the
   type is the element of a `FrameList`/`FrameMap`.

Composition rules (matching the landed tests):
- A leaf scalar member → `path_component(name)` in `element_fields()`.
- A nested **static** struct member → recurse `<Ty>::element_fields()` with the member name
  pushed onto the relative path and offsets shifted by the member offset.
- A nested **dynamic** member (`FrameList`/`FrameMap`) → emit `list(field_name, ...)` /
  `map(field_name, ...)` with the **relative** field name only (e.g. `"threads"`), because the
  parent dynamic path (`processes.htop`) is accumulated at runtime. This is exactly the
  `host_members()`/`thread_members()` shape in `test_dynamic_nested`.

Key distinction, stated once: a dynamic field reached **statically** uses its **full dotted
prefix** as the `Op::List`/`Op::Map` name (`path.to_name()`); a dynamic field reached as a
**member template** uses only its **own field name**. Both then rely on the landed
`PathHasher`-by-value threading in `walk_field`/`expand_dynamic`. Depth is bounded by the
landed `MAX_DYNAMIC_DEPTH = 8`.

---

## 5. Metadata emission for dynamic names

Static members already get one `ComponentMetadata` each from the `Metadatatize` derive
(`prefix.chain(name).to_metadata()` → `SetComponentMetadata`). Dynamic members can't be
enumerated at compile time, so the **producer emits metadata lazily, once per newly-seen
key**, on the existing metadata channel:

- The frame writer (§3.3) tracks the set of keys/indices it has already announced (per
  dynamic field). On first sight of a new key `"htop"` (or a new max index for a list), it
  emits, for each member-template (and nested member) of that element, a `ComponentMetadata`
  whose `name` is the composed dotted string (`processes.htop.pid`, `processes.htop.cpu_usage`,
  …) and whose `component_id` is its hash.
- This reuses `ComponentMetadata::with_prefix` (`format!("{prefix}.{name}")` + rehash): take
  each member-template's static metadata (`pid`, `cpu_usage`, produced by the element type's
  `Metadatatize`) and apply prefix `processes.htop`. The full `String` is built here, on the
  cold "new key" path — never per sample.
- Wire format: `SetComponentMetadata(ComponentMetadata)` (`wkt/src/msgs.rs`), the same
  channel db/UI already consume for id→name resolution, so `processes.htop.pid` displays with
  no new mechanism.

The set of member-templates to expand is available from `vtable.realize_fields(None)` /
`element_fields()` (registration mode emits each template once with `view = None`,
`frame = Some`), so the producer can derive "which members exist under this prefix" directly
from the vtable without hand-maintaining a list.

Open: metadata can burst at startup (many keys at once) — batching/rate-limiting is
Q (carried from vtable-dynamic.md §9.8). Eviction of stale keys (short-lived PIDs) is Q
(vtable-dynamic.md §9.6).

---

## 6. Reused vs. new

| Concern | Reused (landed) | New in WP3 |
|---------|-----------------|------------|
| Dotted-name hashing | `PathHasher`, `ComponentPath::hash_into`, `ChainPath` | — |
| Frame identity op | `Op::Frame`, `RealizedField::frame`, `frame()` builder | `FieldBuilder::with_frame` helper; frame derive wraps each field |
| Dynamic ops | `Op::{List, Map, PathComponent}`, `ElementFields`, `list`/`map`/`path_component`/`name` builders, `VTableBuilder::visit_members` | — |
| Realization | `for_each_field`, `walk_field`, `expand_dynamic`, `read_slot`, `RealizedField::{frame, element}`, `ElementKey`, `MAX_DYNAMIC_DEPTH` | — |
| `AsVTable` | `vtable_fields(path)`, `with_timestamp`, `offset_by`, the `as_vtable.rs` derive | add `element_fields()` (dynamic member-template form); `with_frame` wrap; `AsVTable for FrameList/FrameMap` |
| `Metadatatize` | the `metadatatize.rs` derive, `ComponentMetadata::with_prefix`, `SetComponentMetadata` | lazy per-key metadata emission in the producer |
| `Componentize`/`Decomponentize` | the *traits* (`metor_proto::com_de`), `AsComponentView`/`FromComponentView`, the **unexported** `componentize.rs`/`decomponentize.rs` | finish (`Field.nest`/`component_id()`), register derives, real `MAX_SIZE`, re-export |
| `Frame` | `Component`/`Timestamp`/`ComponentId` types | the `Frame` **trait** + `#[derive(Frame)]` bundle |
| Dynamic data types | the WP2 trailer layout + slot semantics | `FrameList<T, MAX>` / `FrameMap<K, V, MAX, MAX_KEY>` Rust types + `FrameWriter`/`ListWriter`/`MapWriter` producer API + typed index/key reader |
| Buffer sizing | ring `frame_len`/`round_up8`, `LenPacket` | `MAX_SIZE` formula from const-generic cardinalities + `#[metor_fsw(max)]` |

Genuinely new code is concentrated in: the `frame.rs` trait, the macro-crate finishing/wiring,
`FrameList`/`FrameMap` + their `AsVTable`/`Componentize` impls, and the `FrameWriter` producer
API. Everything below the vtable line is reuse.

---

## 7. Open questions / risks for the reviewer

1. **Q1 — timestamp field as a component?** Should the `#[metor_fsw(timestamp)]` field be
   emitted as a standalone component (`imu.timestamp`) in addition to being the timestamp
   *source*, or suppressed? The current `AsVTable` derive emits all fields; frames may want to
   suppress the duplicate. Affects db schema and the `Frame::timestamp` accessor.
2. **Q2 — `#[derive(Frame)]` bundling.** Expand `Frame` into the four sub-derives (one
   annotation, the task's stated goal) vs. require the user to also derive the four and have
   `Frame` emit only `impl Frame`? Bundling is more ergonomic but risks attribute/order
   surprises with `IntoBytes`/`Immutable`. Preference?
3. **Q3 — `FrameWriter` over `LenPacket` vs `&mut [u8]`.** The producer write API: build on the
   growable `LenPacket` (matches the ring path, allocates/extends) or a fixed `&mut [u8]`
   cursor sized by `MAX_SIZE` (no growth, bounded, but needs exact pre-sizing)? The streaming
   `DynamicFrameTable` (vtable-dynamic.md §8.6) leans toward `LenPacket`.
4. **Q4 — const-generic cardinality ergonomics.** `FrameList<T, MAX>` / `FrameMap<K, V, MAX,
   MAX_KEY>` make max-size computable but clutter the type (DESIGN.md shows bare
   `FrameList<Process>`). Alternative: keep the type bare and carry `max` only via
   `#[metor_fsw(max = N)]`, computing `MAX_SIZE` from the attribute. Which is the source of
   truth — the type or the attribute?
5. **Q5 — map key type `K`.** Settle on a single key type: `&str`/`String`, or a `Name`
   newtype that enforces "no `.`" at construction. Enforcement point: writer-time (loud) and
   already realize-time (`InvalidComponentData`). Do we also forbid empty keys (they'd vanish
   per `PathHasher`'s empty-segment rule, aliasing `processes..pid`)?
6. **Q6 — `MAX_SIZE` for maps.** The name-pool budget `MAX * MAX_KEY` can dominate; is a single
   shared/deduped name pool worth the complexity (vtable-dynamic.md §9.4)? And how is the
   per-frame fixed-region alignment padding accounted so the coordinator never under-sizes?
7. **Q7 — metadata burst / key churn.** Lazy per-key metadata (and lazy db component
   registration, vtable-dynamic.md §8.5) grow unboundedly with churning keys (short-lived
   PIDs). Need a TTL/eviction or "hidden after N idle ticks" policy, and possibly batched
   `SetComponentMetadata` at startup (Q from §5).
8. **Q8 — `element_fields()` placement.** Should the dynamic member-template form live on the
   `AsVTable` trait (extra required method, default-implemented in terms of the static form
   where possible) or a separate `AsElementVTable` trait? Extra trait keeps `AsVTable` clean
   but adds a bound on every `FrameList<T>`.
9. **Q9 — nested static-vs-dynamic prefix rule.** The "full dotted prefix when reached
   statically, own-name-only when reached as a member template" rule (§4) is load-bearing and
   currently implicit in the tests. Confirm it is the contract, and that
   `realize_fields(None)` registration mode composes the registration ids consistently for
   nested dynamics.
