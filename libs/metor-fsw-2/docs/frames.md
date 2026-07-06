# Frames

A **frame** is the unit of data a `metor-fsw-2` system publishes: a `#[repr(C)]` struct
whose fields are components sharing one logical timestamp, the whole group named by a
`ComponentId`. A single `#[derive(Frame)]` turns such a struct into something that
serializes straight to a metor-proto table — no separate encode step — and that can carry
runtime-sized `FrameList`/`FrameMap` members past its fixed region.

This document is the reference for the frame format and the read/write APIs. The crate
layout it describes:

| Concern | File |
|---------|------|
| The `Frame` trait | `src/frame.rs` |
| `FrameList`/`FrameMap`/`Slot`/`Name` + their vtable/Componentize impls | `src/dynamic.rs` |
| `FrameWriter`/`ListWriter`/`MapWriter` (producer) | `src/writer.rs` |
| `ListReader`/`MapReader` (typed consumer) | `src/reader.rs` |
| `Output`/`Input`/`FrameRef` (typed ports over a ring) | `src/port.rs` |
| `#[derive(Frame)]` and the four sub-derives | `../metor-fsw/macros/src/{frame,as_vtable,componentize,decomponentize,metadatatize}.rs` |
| Acceptance tests (byte layouts pinned here) | `src/tests.rs` |

It builds on the metor-proto primitives — the vtable `Op::{Frame, List, Map, PathComponent}`
ops, `RealizedField`, `for_each_field`/`expand_dynamic`, `PathHasher`, and the
`list`/`map`/`path_component`/`component`/`frame` builders — plus the ring's record framing
(`round_up8`, `frame_len`), all of which records are 8-byte aligned. Frames add the typed
surface on top.

---

## 1. The frame concept

### 1.1 What a frame is

A frame groups components that share one timestamp and is itself named by a `ComponentId`
(the fnv1a-64 hash of the frame's dotted name, top bit masked — `ComponentId::new`). The
canonical example:

```rust
use nox::{Quaternion, Vector, array::ArrayRepr};

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]   // optional; defaults to snake_case(ident)
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: Vector<f64, 3, ArrayRepr>,
    accel: Vector<f64, 3, ArrayRepr>,
    attitude: Quaternion<f64, ArrayRepr>,
}
```

Each non-timestamp field is a component named under the frame prefix (`Imu.omega` →
`ComponentId::new("imu.omega")`); the `timestamp` field is the shared timestamp source and
is propagated to every component; and every component is tagged with the frame id
`ComponentId::new("imu")`.

The frame name comes from `#[metor_fsw(name = "...")]` (or the equivalent `parent = "..."`),
defaulting to `snake_case` of the struct ident. An **empty** name means "no prefix" — the
member components sit at the root.

### 1.2 Field types — scalars, nox spatial types, and `[T; N]` arrays

A frame field is one component whose primitive type is its scalar element type. Three field
shapes are supported:

- **Scalars** (`f64`, `u32`, `u64`, `bool`, …) → one shape-`[]` component.
- **nox spatial types — the recommended shape for vectors/quaternions.**
  `Vector<f64, 3, ArrayRepr>`, `Quaternion<f64, ArrayRepr>`, `SpatialMotion`/
  `SpatialTransform`/… each serialize to a single multi-element component (`imu.omega`,
  prim `f64`, shape `[3]`; a quaternion is shape `[4]`). nox types are
  representation-consistent: the byte-level vtable/`apply` path and the in-process
  `Componentize`/`Decomponentize` path produce the **same** single dotted component, so
  prefer them for any vector- or quaternion-valued field.
- **Plain `[T; N]` arrays** (`[f64; 3]`, `[bool; 3]`, …) are supported with a known split
  between the two serialization paths (see the limitation below).

### 1.3 The `Frame` trait

`Frame` (`src/frame.rs`) is a thin marker bundling the four component traits and adding the
frame identity plus the shared-timestamp accessor. It does **not** replace
`AsVTable`/`Metadatatize`/`Componentize`/`Decomponentize` — it requires all four — and it is
deliberately not `metor_proto::component::Component` (a `Component` has a single primitive
`schema()`, which a multi-field frame is not):

```rust
pub trait Frame: AsVTable + Metadatatize + Componentize + Decomponentize {
    /// Frame name; the dotted prefix all member components hang off (and the
    /// `Op::Frame` tag id). Empty means "no prefix".
    const NAME: &'static str;
    /// fnv1a-64 of NAME, top-bit masked — the same construction as every ComponentId.
    const FRAME_ID: ComponentId = ComponentId::new(Self::NAME);
    /// The shared timestamp marked with `#[metor_fsw(timestamp)]`.
    fn timestamp(&self) -> Timestamp;
}
```

The roles of the bundled traits:

- `AsVTable` yields the table description; the frame derive adds the `Op::Frame` wrap (§1.4)
  and the dynamic member-template form (§2.3).
- `Componentize`/`Decomponentize` give the in-process struct↔components path (used by systems
  wiring) without going through bytes+vtable.
- `Metadatatize` emits the id→name records for the static members.
- `timestamp()` lets a port/coordinator read the frame's timestamp uniformly. With no marked
  field it returns `Timestamp::default()`.

### 1.4 Struct → VTable: the frame tag and the timestamp source

`#[derive(AsVTable)]` composes nested fields with `vtable_fields(path)`. The frame derive
generates the same `AsVTable` impl with two additions:

1. **Frame tag.** Every field's op chain is wrapped via `FieldBuilder::with_frame(FRAME_ID)`
   (`.map(move |field| field.with_frame(frame_id))`), so each `RealizedField` carries
   `frame = Some(FRAME_ID)`. Because a single shared `frame(...)` Arc is referenced, the
   builder's Arc-dedup stores the frame id once.
2. **Timestamp source, not a component.** The `#[metor_fsw(timestamp)]` field becomes the
   timestamp *source* (`with_timestamp(raw_table(offset, size))`) propagated to every other
   field, and is itself **suppressed** — it is never emitted as a standalone component on
   either the vtable path or the `Componentize` path. (Tests assert that `imu.timestamp` does
   not appear.)

What the derive expands `Imu` (with scalar fields, for brevity) to:

```rust
fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
    let timestamp_source = raw_table(offset_of!(Self, timestamp) as u32, size_of::<Timestamp>() as u32);
    std::iter::empty()
        .chain(<f64 as AsVTable>::vtable_fields(path.chain("imu.omega"))
            .map(|f| f.offset_by(offset_of!(Self, omega) as u32)))
        .chain(<f64 as AsVTable>::vtable_fields(path.chain("imu.accel"))
            .map(|f| f.offset_by(offset_of!(Self, accel) as u32)))
        .map(move |f| f.with_timestamp(timestamp_source.clone()))
        .map(move |f| f.with_frame(ComponentId::new("imu")))
}
```

`as_vtable().apply(bytes, &mut sink)` then drives `for_each_field`, and each component arrives
with `component_id = imu.omega`, `timestamp = <marked>`, `frame = Some(imu)`. The `frame` id is
carried on `RealizedField`, not handed to `apply_value`, so tests inspect it via
`realize_fields(Some(bytes))`.

---

## 2. The derives

### 2.1 `#[derive(Frame)]` — the one-annotation entry point

`#[derive(Frame)]` expands to **all four** sub-derives (`AsVTable` + `Metadatatize` +
`Componentize` + `Decomponentize`) plus `impl Frame`. A frame author writes one fsw derive
and the standard zerocopy derives:

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu { #[metor_fsw(timestamp)] timestamp: Timestamp, omega: f64, accel: f64 }
```

The individual derives remain usable standalone (a sub-frame element type, for instance,
derives only `AsVTable` — see §4). `AsVTable` is shared, not forked: the only difference under
`Frame` is the `with_frame` wrap (driven by passing `Some(FRAME_ID)` into the shared
generator) and the timestamp suppression. The macro crate keeps one `Field`/attribute surface
so every derive parses `#[metor_fsw(timestamp | nest | name | parent | component_id | max |
group)]` consistently.

### 2.2 What each derive generates

| derive | generates | leaf field | nested / dynamic field |
|--------|-----------|------------|------------------------|
| `AsVTable` | `vtable_fields(path)` **and** `element_fields(prefix)` (§2.3) | recurse into the field type's `vtable_fields(path.chain(name))`, offset-shifted | same recursion; `FrameList`/`FrameMap` emit `Op::List`/`Op::Map` (§3.2) |
| `Metadatatize` | `metadata(prefix)` | `prefix.chain(name).to_metadata()` | recurse / empty for dynamic types (§5) |
| `Componentize` | `sink_columns(out)` + `MAX_SIZE` | `out.apply_value(id, self.f.as_component_view(), None)` | `self.f.sink_columns(out)` (a dynamic slot is a no-op) |
| `Decomponentize` | `apply_value(id, view, ts)` | match `id == ID const` → `FromComponentView` | recurse |
| `Frame` | the four above + `impl Frame` | — | — |

A field recurses (rather than emitting a scalar leaf) when it is marked `#[metor_fsw(nest)]`
or when its type is a `FrameList`/`FrameMap` (whose 8-byte slot carries no in-struct value).
`Componentize::MAX_SIZE` is computed from the fixed size plus each dynamic field's budget
(§3.5).

### 2.3 `element_fields` — the dynamic member-template form

`AsVTable` has two member iterators:

1. `vtable_fields(path)` — the **static** form. Leaves are absolute
   `component(path.chain(name).id)` ops, used when the type is reached through compile-time
   struct nesting.
2. `element_fields(prefix: String)` — the **dynamic member-template** form. Leaves are
   `path_component(name)` ops with offsets relative to the element base and no absolute path
   baked in, because the runtime path is composed by the enclosing `list`/`map` at realize
   time. `prefix` is the dotted name of the value *relative to the element base* (empty at the
   element root); it is taken by value so the returned iterator borrows no caller temporary.

`element_fields` is defaulted to empty on the trait, so hand-written `AsVTable` impls (e.g. the
nox spatial types) keep compiling; only types actually used as dynamic elements override it.
The derive builds relative names with a `child(name)` closure (`prefix.field`, or just `field`
at the root) and recurses through `<Ty>::element_fields(child(name))`.

---

## 3. Dynamic members: `FrameList` / `FrameMap`

These are the types a frame embeds to carry runtime-sized members. Elements are addressed by
dotted name: list elements positionally (`processes.0.pid`), map entries by a name key with no
`.` (`processes.htop.pid`). Both live in the table trailer, after the fixed region.

### 3.1 In-struct representation is the 8-byte slot

Inside the `#[repr(C)]` frame, a `FrameList`/`FrameMap` field **is** an 8-byte slot — a
table-absolute offset to the element block in the trailer plus its byte length — and nothing
more. Each is `#[repr(transparent)]` over `Slot`, so the field is exactly 8 bytes and stays
trivially `IntoBytes`/`FromBytes`:

```rust
#[repr(C)]
pub struct Slot { pub trailer_off: u32, pub byte_len: u32 }   // 8 bytes

#[repr(transparent)]
pub struct FrameList<T, const MAX: usize> { slot: Slot, _ty: PhantomData<T> }

#[repr(transparent)]
pub struct FrameMap<K, V, const MAX: usize, const MAX_KEY: usize = 32> { slot: Slot, _kv: PhantomData<(K, V)> }
```

Keeping the static part fixed-size means a frame with dynamic members still has a `#[repr(C)]`/
`IntoBytes` fixed region; the variable data is appended past it. The const generics carry the
**max cardinality** (`MAX`) and, for maps, the max key length (`MAX_KEY`, default 32), so
`Componentize::MAX_SIZE` is a `const` (§3.5). Both default to an `EMPTY` slot
(`trailer_off = 0, byte_len = 0`); construct a frame with `FrameList::EMPTY` / `FrameMap::EMPTY`
and let the writer patch the slots.

`K` is a name-key type, not a `ComponentId`. The `Name<'a>` newtype validates the grammar at
construction (non-empty, no `.`); the tests use `FrameMap<Name<'static>, V, MAX>`.

### 3.2 `AsVTable` for the dynamic types

`FrameList<T, MAX>` emits exactly **one** field — the slot at offset 0 — whose arg is the
`list(prefix, members, stride)` op:

```rust
impl<T: AsVTable, const MAX: usize> AsVTable for FrameList<T, MAX> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let prefix = path.to_name();                       // full static dotted prefix, e.g. "processes"
        core::iter::once(raw_field(
            0, size_of::<Slot>() as u32,
            list(&prefix, T::element_fields(String::new()), size_of::<T>() as u32),
        ))
    }
    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        // Reached as a member template: name is the relative own name only.
        core::iter::once(raw_field(0, size_of::<Slot>() as u32,
            list(&prefix, T::element_fields(String::new()), size_of::<T>() as u32)))
    }
}
```

- When reached **statically**, the list's name is the **full dotted prefix** from root
  (`path.to_name()`), because `expand_dynamic` seeds each top-level field's `PathHasher` empty
  and pushes the dynamic name first. When reached as a **member template**, the name is the
  field's own relative name only (§4).
- The enclosing frame derive `with_frame`/`with_timestamp`-wraps and `offset_by`-shifts this
  single slot field exactly as it does a static field.
- `FrameMap` is identical but emits `map(prefix, V::element_fields(...), map_stride::<V>(),
  map_value_offset::<V>())` — the entry stride and value offset of the entry layout (§3.3).

### 3.3 Trailer byte layout

The table bytes are `LenPacket::table` bytes: an 8-byte header (`TABLE_BASE` = 4-byte length +
1 ty + 2 id + 1 req_id), then the table itself. **Table offset 0 is the fixed-region start**;
all slot `trailer_off`/map `key_off` values are table-absolute (relative to that start) — the
one-global-trailer invariant, which holds across nesting. Each dynamic block is laid down
**8-byte aligned** (matching the ring's `round_up8`).

**List block.** Element bytes back-to-back at the (8-aligned) trailer offset. The slot is
`{ trailer_off, byte_len = count * size_of::<T> }`; a reader recovers `count = byte_len /
size_of::<T>`.

**Map block.** A fixed-stride **entry array** followed by a **name pool**:

```
entry[i] = { key_off: u32, key_len: u32, <pad to value_offset>, value: V }   // stride bytes
...
name pool: key bytes back-to-back; entry[i].key_off points table-absolute into here
```

The slot delimits the **entry array only** (`{ entry_array_off, count * stride }`), so
`count = byte_len / stride` stays exact even with the pool after it. Each `key_off` is
table-absolute into the pool; `key_len` is the byte length. In code the
`{ key_off, key_len }` pair is the zerocopy struct `MapEntryHeader`
(`src/dynamic.rs`, next to `Slot`); the entry geometry:

- `entry_align::<V>() = max(align_of::<V>(), 8)` — entries stay at least 8-aligned.
- `map_value_offset::<V>() = align_up(size_of::<MapEntryHeader>(), entry_align::<V>())` —
  the value sits after the header pair (8 bytes for any value of alignment ≤ 8).
- `map_stride::<V>() = align_up(map_value_offset + size_of::<V>, entry_align::<V>())`.

For a 16-byte, 8-aligned `Process`, that is `value_offset = 8`, `stride = 24`. For a nested
`Host` whose value is an inner 8-byte slot, `value_offset = 8`, `stride = 16` (the layout the
nested test pins).

### 3.4 Producer side — `FrameWriter`, `ListWriter`, `MapWriter`

`FrameWriter<F>` (`src/writer.rs`) builds a frame's table bytes over a growable `LenPacket`. It
writes the fixed `#[repr(C)]` region first (slots zeroed), then for each dynamic field appends
its 8-aligned block to the trailer and patches the slot:

```rust
let frame = SysList { timestamp: Timestamp(1000), processes: FrameList::EMPTY };
let mut w = FrameWriter::new(&frame);                       // seeds the fixed region
w.list(offset_of!(SysList, processes), |l| {                // l: &mut ListWriter<Process>
    l.push(Process { pid: 1001, cpu_usage: 0.5 });
    l.push(Process { pid: 1002, cpu_usage: 0.25 });
});
let table = w.table();                                      // &[u8]: fixed region + trailer, slots patched
SysList::as_vtable().apply(table, &mut sink).unwrap().unwrap();
```

API surface:

- `FrameWriter::new(fixed: &F)` allocates a `LenPacket::table([0, 0], F::MAX_SIZE.min(1 << 16))`
  and copies `fixed.as_bytes()` after the table base.
- `FrameWriter::from_packet(packet, fixed)` reuses an existing `LenPacket` (cleared back to the
  table base) instead of allocating a fresh one — the per-write pooling path used by
  `Output::write_with` to avoid a malloc+free on every dynamic publish. The cleared buffer is
  byte-equivalent to a fresh `LenPacket::table([0, 0], _)`.
- `list(slot_off, build)` — `build` drives a `ListWriter<T>` (`push`, `len`, `is_empty`); the
  writer 8-aligns, appends the element bytes, and patches the slot at `slot_off` (obtained with
  `core::mem::offset_of!`).
- `map(slot_off, build) -> Result<(), WriteError>` — `build` drives a `MapWriter<V>` (`insert`).
  The writer 8-aligns, lays the entry array (each entry zeroed, `key_off`/`key_len` set, value
  copied at `value_offset`), then appends the name pool; it patches the slot to the entry array
  only. Keys are validated on `insert` and the first rejection is surfaced here as
  `Err(WriteError::DotInKey | EmptyKey)`.
- `finish() -> LenPacket` returns the backing packet; `table() -> &[u8]` returns the table bytes
  (offset 0 at the fixed region) — feed either to `VTable::apply` or to the ring.

The static-only case needs none of this: a frame with no dynamic fields has its `#[repr(C)]`
bytes equal to its table bytes, so it is written directly.

**Through a port (`src/port.rs`).** `Output<F>` wraps the single ring `Writer` a system owns:

```rust
out.write(&fixed_frame)?;                                   // fixed: one try_write, no serialize
out.write_with(&fixed, |fw: &mut FrameWriter<F>| {          // dynamic: build trailer, write one record
    fw.list(offset_of!(F, processes), |l| { /* ... */ });
})?;
```

`write_with` keeps a reused `LenPacket` scratch buffer across calls (via `from_packet`) so a
per-cycle dynamic publish does not reallocate.

### 3.5 `MAX_SIZE` and buffer sizing

Output rings are sized at construction, but dynamic frames are runtime-sized, so the const
generics bound them. `Componentize::MAX_SIZE` (generated by the derive) is:

```
MAX_SIZE(frame) = size_of::<Self>()                    // fixed region (already includes every 8-byte slot)
                + Σ_dynamic  <Field>::MAX_SIZE          // each list/map trailer budget
                + 8                                     // alignment pad
```

where the dynamic types contribute:

```
FrameList<T, MAX>::MAX_SIZE          = round_up8( MAX * size_of::<T>() )
FrameMap<K, V, MAX, MAX_KEY>::MAX_SIZE = round_up8( MAX * map_stride::<V>() + MAX * MAX_KEY )   // entries + name pool
```

Worked (the `max_size_formula` test): a frame with `procs: FrameList<Process, 8>` and
`hosts: FrameMap<Name, Process, 4, 16>` is `24` fixed `+ 128` list `+ 160` map `+ 8` pad =
`320` bytes. `buffer_capacity::<F>(depth)` / `capacity_for(max_size, depth)` (`src/port.rs`)
round this up through the ring's `frame_len` to a power-of-two ring capacity.

The const generic on the type is the source of truth for `MAX`. `#[metor_fsw(max = N)]` is
accepted on the field for forward-compatibility but is not consulted by the derives.

### 3.6 Consumer side — flat `apply` and typed readers

Two access modes, over the same bytes:

**Flat (db, UI, most systems).** `vtable.apply(table_bytes, &mut sink)` — or `FrameRef::apply`
— walks the slot → entry array → name pool, composes the dotted id via `PathHasher`, inherits
the frame id and timestamp, and pushes each element-member through `Decomponentize::apply_value`
exactly like a static component. A dynamic frame is just a runtime-variable *set* of ordinary
components; consumers on this path need no dynamic-specific code.

**Typed by index/key.** `FrameRef<'a, F>` (`src/port.rs`) is a zero-copy view of one record's
table bytes:

- `get() -> &F` reads the fixed `#[repr(C)]` region directly (`ref_from_prefix`) — the producer
  wrote `fixed.as_bytes()` there, so no per-field decode.
- `list::<T>(slot_off) -> ListReader<'a, T>` and `map::<V>(slot_off) -> MapReader<'a, V>` read
  the slot at `slot_off` and index/scan the trailer.
- `apply::<D>(sink)` is the uniform escape hatch onto the vtable path above.

`ListReader` (`src/reader.rs`) derives `len = byte_len / size_of::<T>()` and reads element `i`
at `trailer_off + i * size_of::<T>()`. `MapReader` derives `len = byte_len / map_stride::<V>()`,
reads each entry's `key_off`/`key_len` (resolving the key in the pool) and the value at
`map_value_offset::<V>()`, and offers `entry(i)`, `get(key)`, and `iter()`. These readers are a
presentation convenience; the authoritative dotted-id/frame/timestamp semantics remain the
`apply` path.

---

## 4. Nested dynamics and the prefix rule

Nesting (`processes.htop.threads.0.state`) is handled by the derives emitting the right ops; the
realize side (`expand_dynamic`) walks it unchanged, bounded by `MAX_DYNAMIC_DEPTH = 8`. The
load-bearing rule, exercised by `nested_dynamic_prefix_rule`:

- A dynamic field reached **statically** uses its **full dotted prefix** (`path.to_name()`) as
  the `Op::List`/`Op::Map` name.
- A dynamic field reached as a **member template** (the element type of an enclosing list/map)
  uses only its **own relative field name** — the parent dynamic path (`processes.htop`) is
  accumulated at runtime by the enclosing op.

Composition in `element_fields` (matching the test):

- a leaf scalar member → `path_component(name)`;
- a nested **static** struct member → recurse `<Ty>::element_fields(prefix.field)`, offsets
  shifted by the member offset;
- a nested **dynamic** member (`FrameList`/`FrameMap`) → emit `list`/`map` named by the
  **relative** field name only (e.g. `"threads"`).

The `nested_dynamic_prefix_rule` test constructs the exact trailer for
`SysNested { processes: FrameMap<Name, Host, 4> }` where `Host { threads: FrameList<Thread, 4> }`,
and asserts `apply` yields `processes.htop.threads.0.state` with the shared frame timestamp.
Its byte layout is the contract:

```
@0   timestamp i64 = 1000
@8   slot_outer.trailer_off = 16
@12  slot_outer.byte_len    = 16          // one 16-byte map entry
@16  entry[0].key_off = 40                // -> "htop" in the pool
@20  entry[0].key_len = 4
@24  Host value (entry@16 + value_offset 8): inner list slot
@24  slot_inner.trailer_off = 44
@28  slot_inner.byte_len    = 1           // one 1-byte Thread
@32  (pad to 40)
@40  "htop"                               // name pool
@44  thread[0].state = 2
```

---

## 5. Metadata for dynamic names

Static members each get one `ComponentMetadata` from the `Metadatatize` derive
(`prefix.chain(name).to_metadata()` → `SetComponentMetadata`), so db/UI resolve their dotted
ids with no extra mechanism.

Dynamic members cannot be enumerated at compile time: `Metadatatize for FrameList`/`FrameMap`
returns an empty iterator. The intended model is that the **producer announces a dynamic
member's metadata lazily, once per newly-seen key**, by composing each element member-template's
name under the live prefix (`processes.htop.pid`, …) — `ComponentMetadata::with_prefix` rehashes
a member-template's static metadata under `processes.htop` — and emitting it on the same
`SetComponentMetadata` channel db/UI already consume. The set of member-templates to expand is
recoverable from the vtable (`element_fields` / `realize_fields(None)` registration mode), so it
need not be hand-maintained.

**Limitation:** this lazy per-key emission is a producer responsibility that the `FrameWriter`
does not yet implement (it does not track seen keys or emit metadata). Until it does, a dynamic
member's dotted ids carry no id→name record. Related open behavior: metadata can burst at
startup when many keys appear at once (batching/rate-limiting), and stale keys (e.g. short-lived
PIDs) are not evicted.

---

## 6. Other limitations and future work

- **`[T; N]` representation split.** The in-process `Componentize`/`Decomponentize` path treats
  a `[T; N]` field as one component of prim `T`, shape `[N]` (via
  `AsComponentView`/`FromComponentView`). The byte-level vtable/`apply` path, via the blanket
  `AsVTable for [T; N]` (`metor-fsw/src/vtable.rs`), expands it into `N` **indexed scalar**
  components (`arr.v.0`, `arr.v.1`, `arr.v.2`). The two paths therefore disagree on naming for
  plain arrays — `array_field_frame_round_trip` pins both halves. For a single shape-`[N]`
  component on both paths, use a nox `Vector`/`Quaternion` instead.
- **Dynamic-member metadata** is not emitted by the producer yet (§5).
- **`FrameWriter` backing** is the growable `LenPacket` only; there is no fixed `&mut [u8]`
  cursor variant.
