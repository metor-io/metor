# Dynamic frames & the VTable op set

A `metor-fsw-2` system publishes its output as a `#[repr(C)]` table plus a **VTable**
describing it. The VTable is the same self-describing artifact `metor-db` ingests
(`VTableMsg` → `insert_vtable` → `realize_fields`), so a frame "serializes" with no extra
step: the producer fills a struct, hands over the bytes plus its VTable, and the
coordinator (for wiring/validation) and `metor-db` (for storage) both interpret it through
one mechanism.

Most frame members are ordinary, fixed-offset components. This document describes the part
of the model that is **runtime-sized**: list/map members whose element data lives in a
padded trailer after the fixed portion of a table, including **nested** dynamic members.
These are described with four VTable ops — `Frame`, `List`, `Map`, `PathComponent` — and a
shared rolling path-hasher, all defined in `metor-proto` (`libs/metor-proto/src/vtable.rs`,
`libs/metor-proto/src/hash.rs`). `metor-fsw-2` consumes them through the
[`FrameList`]/[`FrameMap`] field types ([`src/dynamic.rs`](../src/dynamic.rs)), the
[`FrameWriter`] producer ([`src/writer.rs`](../src/writer.rs)), the wiring/compatibility
check ([`src/descriptor.rs`](../src/descriptor.rs)), and the dlopen prefix rewrite
([`src/abi/mod.rs`](../src/abi/mod.rs)).

---

## 1. The VTable op model

A VTable is `{ ops, fields, data }`:

- `ops: Vec<Op>` is a flat pool; `OpRef(u32)` indexes it. Sub-parts of an op are always
  `OpRef`s, never inline payloads — this keeps `Op` small (`size_of::<Op>() <= 64`, asserted
  by `_ASSERT_OP_SIZE`).
- `Field { offset: Offset, len: u32, arg: OpRef }`. `offset`/`len` address the **runtime
  table**; `arg` is the head of an **op chain**.
- `data: Vec<u8>` is a static side-table holding constants (component ids, schema `ty`/`dim`
  blobs, UTF-8 names), addressed by `Op::Data`.

`realize(op_ref, table)` evaluates one op into a `RealizedOp`: `Op::Data` reads the VTable's
own `data` buffer; `Op::Table` reads the runtime `table` argument. Realizing a whole VTable
walks each field's op chain: metadata ops (`Schema`, `Timestamp`, `Frame`, `Ext`) record
state and continue at `arg`; terminal ops (`Component`, `PathComponent`, `List`, `Map`) end
the chain and emit one or more `RealizedField`s. The dynamic members are expressed entirely
as **new ops in the chain / new chain terminals**, reusing this loop.

---

## 2. Frame identity — `Op::Frame { component_id, arg }`

A frame is a group of fields that share a timestamp and are collectively named by a frame
`ComponentId`. "This field belongs to frame X" is expressed with a metadata op in the
field's chain, exactly like `Timestamp`. All fields of a frame reference the same `Frame`
op (deduplicated by `OpRef`), so the id is stored once.

`Frame` is a metadata op: `realize` resolves `component_id` (a `Data` op holding the
`ComponentId`) and returns `RealizedFrame { component_id, arg }`; the realize loop records
`frame = Some(component_id)` and continues at `arg`. Each emitted `RealizedField` carries
`frame: Option<ComponentId>`. A VTable with no `Frame` op yields `frame: None`.

This keeps the grouping key out of the `Field` struct (which would break its
`repr(C)`/postcard/C++ ABI) and off the top-level `VTable`. The builder exposes it as
`FieldBuilder::with_frame(component_id)`, mirroring `with_timestamp`; the `Frame` trait
([`src/frame.rs`](../src/frame.rs)) supplies `FRAME_ID = ComponentId::new(NAME)`.

`Frame` is orthogonal to the dynamic-naming machinery: static frames keep using
compile-time-hashed `Component` ids; `Frame` only adds the grouping key.

---

## 3. Dynamic identity model — fully-qualified dotted names

Every dynamic element-member resolves to an **ordinary component** whose name is the field
path prefix, joined with the key/index, joined with the member name:

| shape | example component name | ComponentId |
|-------|------------------------|-------------|
| `FrameMap<Name, Process>` | `processes.htop.pid`  | `ComponentId::new("processes.htop.pid")` |
| `FrameList<Process>`      | `processes.0.pid`     | `ComponentId::new("processes.0.pid")` |
| nested (map of list)      | `processes.htop.threads.3.state` | `ComponentId::new("processes.htop.threads.3.state")` |

The id is the fnv1a-64 hash of the full dotted string, top bit masked — exactly like
`ComponentId::new` and exactly like the static dotted names the `metor-fsw` derive produces.
There is no separate hashing scheme and no new sink method:
`Decomponentize::apply_value(component_id, view, timestamp)` is unchanged, so `metor-db` and
the UI need zero value-path changes. A dynamic frame simply emits a runtime-variable *set*
of ordinary components.

### Where each name segment comes from

| segment | example | when known | stored where |
|---------|---------|-----------|--------------|
| **prefix** | `processes` | compile time | UTF-8 in the VTable `data` buffer, referenced by the `List`/`Map` op's `name: OpRef` |
| **member name** | `pid` | compile time | UTF-8 in `data`, referenced by the leaf terminal `PathComponent { name: OpRef }` |
| **map key** | `htop` | **runtime** | UTF-8 in the table **trailer** (§5.2) |
| **list index** | `0`, `3` | runtime (positional) | not stored — the element ordinal, formatted as decimal at realize time |

Compile-time segments are `Data` ops holding UTF-8 (align 1) in the existing `data`
side-table — no new buffer. Runtime segments are produced by the `List`/`Map` expansion.

### 3.1 `PathHasher` — chained hashing, no full-string allocation

fnv1a-64 is a rolling hash:
`h = OFFSET_BASIS; for b in bytes { h = (h ^ b).wrapping_mul(PRIME) }`
(constants `0xcbf29ce484222325` / `0x00000100000001B3`). `ComponentId::new(s)` is this fold
over `s.as_bytes()` with a final `& !(1 << 63)` top-bit mask (keeping the id `i64`/Lua-safe).
Because the fold is left-associative, feeding `"processes"`, then `"."`, then `"htop"`, then
`"."`, then `"pid"` in sequence yields **byte-for-byte** the same accumulator as hashing the
literal `"processes.htop.pid"`.

`PathHasher` (`metor-proto/src/hash.rs`) is the rolling hasher, shared with the static
`ComponentPath`/`ChainPath` derive in `metor-fsw` (via `ComponentPath::hash_into`) so static
and dynamic paths produce bit-identical ids:

```rust
pub struct PathHasher { hash: u64, has_content: bool }

impl PathHasher {
    pub const fn new() -> Self;          // seeded with the fnv1a-64 offset basis
    pub fn push(&mut self, segment: &str);   // a '.' is fed before a non-empty segment
                                             // only if a prior non-empty segment exists
    pub fn push_bytes(&mut self, bytes: &[u8]);
    pub fn push_index(&mut self, idx: u32);  // formats the decimal into a stack buffer
    pub fn finish(self) -> ComponentId;      // applies the top-bit mask
}
```

The `has_content` flag implements the empty-segment rule: an empty segment contributes
nothing and emits no separator, so `"" + "a" == "a"` and `["", "a", "", "b"] == "a.b"`,
matching the `ChainPath` join rules. `PathHasher` is `Copy` (a `u64` plus a `bool`), so it is
threaded down the dynamic walk **by value**: `List`/`Map` push the prefix and then the
key/index per element; the leaf `PathComponent` pushes the member name and finishes. The hot
id path never allocates a `String`. A 2000-iteration property test in `hash.rs` checks
`PathHasher` against `ComponentId::new` of the joined string for random dotted paths.

### 3.2 Display names reach consumers via the metadata channel

The hashed id travels in the table; the human-readable **display name** travels on the
existing metadata channel (`ComponentMetadata { component_id, name, metadata }`). Static
components emit one `ComponentMetadata` per component from the derive. Dynamic components are
announced **lazily, once per newly-discovered key**, by the producer — `FrameList`/`FrameMap`
emit an empty static metadata set (`Metadatatize::metadata` returns
`core::iter::empty()`), because their concrete element-members do not exist until runtime
keys appear. Consumers already map id→name through this channel, so `processes.htop.pid`
displays with no new mechanism.

---

## 4. The op set

```rust
#[repr(u8)]
pub enum Op {
    // fixed-table members
    Data { offset: Offset, len: u32 },
    Table { offset: Offset, len: u32 },
    None,
    Component { component_id: OpRef },        // static: precomputed dotted-name hash
    Schema { ty: OpRef, dim: OpRef, arg: OpRef },
    Timestamp { source: OpRef, arg: OpRef },
    Ext { arg: OpRef, id: PacketId, data: OpRef },

    // frame identity & dynamic members
    Frame { component_id: OpRef, arg: OpRef },
    List { name: OpRef, members: ElementFields, stride: u32 },
    Map  { name: OpRef, members: ElementFields, stride: u32, value_offset: u32 },
    PathComponent { name: OpRef },
}

#[repr(C)]
pub struct ElementFields { pub start: u32, pub count: u32 }
```

- **`Op::List`** — dynamic list terminal. The owning `Field`'s `offset`/`len` address an
  8-byte slot `{ trailer_off: u32, byte_len: u32 }` in the fixed region. Elements are laid
  out back-to-back in the trailer with byte `stride`; `count = byte_len / stride`. `name`
  references a `Data` op holding the path prefix (`"processes"`). `members` is a contiguous
  range of member-template `Field`s whose offsets are relative to each element's base.
- **`Op::Map`** — dynamic map terminal. Like `List`, but each trailer entry is
  `{ key_off: u32, key_len: u32, <pad> value }`: the key *bytes* live in a name pool
  elsewhere in the trailer (addressed by `key_off`/`key_len`), and the value sub-frame begins
  at `value_offset` within the entry.
- **`Op::PathComponent`** — dynamic leaf terminal, used in member templates instead of
  `Component`. Appends `name` (a `Data` op holding the member name, `"pid"`) to the running
  dotted path accumulated by the enclosing `List`/`Map` expansion, then finalizes it into a
  `ComponentId`.
- **`ElementFields { start, count }`** names a contiguous block of member-template `Field`s
  in `VTable::fields`. These describe one element and are **not** iterated as top-level
  fields — they are claimed templates, realized only as part of their owning dynamic field.

A member's chain may itself terminate in `List`/`Map`, giving nested dynamics.
`MAX_DYNAMIC_DEPTH = 8` bounds the nesting honored during realization (and therefore stack
use on the no-alloc path); exceeding it is reported as `Error::InvalidOp`.

`Op` stays `repr(u8)` and `<= 64` bytes: the largest new variant is `Map`
(`name + members(8) + stride + value_offset` ≈ 20 bytes payload). All multi-op sub-parts use
`OpRef`; the only inline payloads are small fixed scalars. Variants are **appended**, so
existing discriminants do not shift; everything is `OpRef`/`u32`, so the serde / postcard /
`postcard_schema` round-trip is preserved. `VTable::is_dynamic()` reports whether any
`List`/`Map` op is present.

---

## 5. Trailer encoding

All examples use prefix `"processes"`, `Process { pid: u64, cpu_usage: f64 }` (size 16,
align 8), fixed region `{ timestamp: i64 @0, slot @8 }` (fixed size 16). The trailer is
8-aligned and starts at 16. Integers little-endian. Member templates (offsets relative to
the element/value base): `{0, 8, schema(U64, [], path_component("pid"))}`,
`{8, 8, schema(F64, [], path_component("cpu_usage"))}`.

### 5a. `FrameList<Process>`, 2 elements

`stride = 16`. Slot `{ trailer_off = 16, byte_len = 32 }` ⇒ `count = 2`.

| off | field | bytes (LE) | value |
|----:|-------|-----------|-------|
| 0  | `timestamp` i64        | `E8 03 00 00 00 00 00 00` | 1000 |
| 8  | `slot.trailer_off` u32 | `10 00 00 00`             | 16 |
| 12 | `slot.byte_len` u32    | `20 00 00 00`             | 32 |
| 16 | `[0].pid` u64          | `E9 03 00 00 00 00 00 00` | 1001 |
| 24 | `[0].cpu_usage` f64    | `00 00 00 00 00 00 E0 3F` | 0.5 |
| 32 | `[1].pid` u64          | `EA 03 00 00 00 00 00 00` | 1002 |
| 40 | `[1].cpu_usage` f64    | `00 00 00 00 00 00 D0 3F` | 0.25 |

Length 48. Realize: slot → `(16, 32)` → `count = 2`. For `i in 0..2`, `base = 16 + i*16`;
push prefix `"processes"`, push index `i` (decimal); walk members ⇒ `processes.0.pid=1001`,
`processes.0.cpu_usage=0.5`, `processes.1.pid=1002`, `processes.1.cpu_usage=0.25`.

### 5b. `FrameMap<Name, Process>`, 2 entries (keys "htop", "init")

Entry `{ key_off: u32 @0, key_len: u32 @4, value: Process @8 }` ⇒ `stride = 24`,
`value_offset = 8`. The slot delimits **just the entry array**: `{ trailer_off = 16,
byte_len = 48 }` ⇒ `count = 2`. The name pool follows the entry array at offset 64.

| off | field | bytes (LE) | value |
|----:|-------|-----------|-------|
| 0  | `timestamp` i64        | `E8 03 …`     | 1000 |
| 8  | `slot.trailer_off` u32 | `10 00 00 00` | 16 |
| 12 | `slot.byte_len` u32    | `30 00 00 00` | 48 (entry array only) |
| 16 | `[0].key_off` u32      | `40 00 00 00` | 64 → "htop" |
| 20 | `[0].key_len` u32      | `04 00 00 00` | 4 |
| 24 | `[0].value.pid` u64    | `E9 03 …`     | 1001 |
| 32 | `[0].value.cpu` f64    | `… E0 3F`     | 0.5 |
| 40 | `[1].key_off` u32      | `44 00 00 00` | 68 → "init" |
| 44 | `[1].key_len` u32      | `04 00 00 00` | 4 |
| 48 | `[1].value.pid` u64    | `EA 03 …`     | 1002 |
| 56 | `[1].value.cpu` f64    | `… D0 3F`     | 0.25 |
| 64 | name pool `"htop"`     | `68 74 6F 70` | "htop" |
| 68 | name pool `"init"`     | `69 6E 69 74` | "init" |

Length 72. Realize: `count = 2`; for `i`, `entry = 16 + i*24`;
`key = str(table[key_off .. key_off+key_len])`; `value_base = entry + 8`; push `"processes"`,
push `key`, walk members ⇒ `processes.htop.pid=1001`, `processes.htop.cpu_usage=0.5`,
`processes.init.pid=1002`, `processes.init.cpu_usage=0.25`.

A key containing `.` would alias the dotted-path grammar; realization rejects it with
`Error::InvalidComponentData`. (Keys are also validated at write time — §7, §8.)

### 5.2 Why the relative-slice key layout

The map key is a variable-length name, but the trailer is parsed by a fixed `stride`
(`count = byte_len / stride`). The entry stores a **relative slice** `{ key_off, key_len }`
into a name pool that follows the fixed-stride entry array, rather than an inline fixed-size
name buffer per entry. This allows arbitrary-length keys with no per-entry cap and no wasted
space on short keys, and it reuses the same relative-slice idiom as the field slot itself.
`key_off`/`key_len` are table-absolute into the pool (bounds-checked against the table end).
Lists store no key — the index is the element ordinal, formatted at realize time.

### 5c. Nested — `FrameMap<Name, Host>` where `Host { threads: FrameList<Thread> }`, `Thread { state: u8 }`

Target names like `processes.htop.threads.3.state`. The map's **value sub-frame** `Host` is
not a leaf — its single member is itself a `List` whose slot lives inside the entry's value
region and points into the **same global trailer**. There is **one global trailer; all slots
are table-absolute**, so nested offset math is uniform — a nested trailer is not
per-element-relative.

```
fixed     { timestamp @0, slot_outer @8 }                        // size 16
entry[i]  (stride 16) = { key_off: u32, key_len: u32, slot_inner: {off:u32,len:u32} }
                                                                 // value sub-frame = the inner List slot
```

One host "htop" with a single thread (a 1-element list, ordinal 0 ⇒
`processes.htop.threads.0.state`):

| off | field | value |
|----:|-------|-------|
| 0  | timestamp                            | 1000 |
| 8  | slot_outer `{off=16, len=16}`        | one entry (stride 16) |
| 16 | entry[0].key_off = 40                | → name pool "htop" |
| 20 | entry[0].key_len = 4                 | |
| 24 | entry[0].slot_inner `{off=44,len=1}` | inner list: 1 thread, stride 1 |
| 28 | (pad to 8 before pools)              | |
| 40 | name pool "htop"                     | (4 bytes) |
| 44 | thread[0].state u8 = 2               | inner-list trailer |

Realize recursion: outer `Map` pushes `"processes"`, then key `"htop"`; the value sub-frame
member is `List(name="threads", …)`, realized against the entry's value region — it reads
`slot_inner` at `entry + value_offset`, pushes `"threads"` then index `0`, walks `Thread`'s
members ⇒ leaf `PathComponent("state")` finalizes `processes.htop.threads.0.state = 2`. The
`PathHasher` accumulator is passed down by value through each level, so the dotted name
chains with no string building on the sample path. Producers may lay the name pool and nested
trailers in any order after the fixed region; only the offsets are load-bearing.

---

## 6. Realization

### 6.1 `realize` and the realized ops

`realize(op_ref, table)` adds these `RealizedOp` cases:

- `Frame` resolves `component_id` (`Data` → `ComponentId`) and returns
  `RealizedFrame { component_id, arg }` — **non-terminal** (the loop follows `arg`).
- `List`/`Map` resolve `name` (`Data` → `&str`, via the `realize_str` helper) and return a
  `RealizedDynamic { name, members, stride, value_offset, is_map }` — **expanding terminals**
  (the slot is read by the field walk, which holds `field.offset`). `value_offset` is `0` for
  a list.
- `PathComponent` resolves `name` (`Data` → `&str`) and returns
  `RealizedPathLeaf { name }`; the field walk finalizes the path hash.

### 6.2 The field walk

Because a dynamic field can yield several components, field realization is a push-style
driver rather than a one-field-one-component iterator:

```rust
pub fn for_each_field<'a>(
    &'a self,
    table: Option<&'a [u8]>,
    f: &mut dyn FnMut(RealizedField<'a>) -> Result<(), Error>,
) -> Result<(), Error>;
```

`for_each_field` is the shared engine behind both `VTable::apply` (which sinks each realized
component into a `Decomponentize`) and, under `alloc`, `VTable::realize_fields` (which
collects into a `Vec<Result<RealizedField, _>>` iterator). It is no-alloc friendly: no heap,
recursion bounded by `MAX_DYNAMIC_DEPTH`. No-alloc callers use `for_each_field` directly.

It iterates top-level fields, **skipping member templates** (any field index inside some
`List`/`Map` op's `members` range, identified by `is_template_field`), and walks each
remaining field with a default `WalkCtx`:

```rust
struct WalkCtx<'a> {
    base: usize,                    // containing element's base offset
    path: PathHasher,               // accumulated dotted-name prefix
    element: Option<ElementKey<'a>>,// Index(u32) | Key(&str); None for static
    frame: Option<ComponentId>,     // inherited frame
    timestamp: Option<Timestamp>,   // inherited timestamp
    depth: usize,
}
```

`walk_field` runs the op chain: `Schema` records ty+dim, `Timestamp`/`Frame` record (and
override the inherited) timestamp/frame, `Ext` passes through. A `Component` terminal emits a
static leaf; a `PathComponent` terminal pushes its name onto a copy of `ctx.path`, finishes
the hash, and emits a dynamic leaf; a `List`/`Map` terminal calls `expand_dynamic`. Every
leaf goes through `emit_leaf`, which requires a `Schema` to have been seen
(`Error::SchemaNotFound` otherwise), computes `offset = ctx.base + field.offset`, builds a
`ComponentView` from the table slice (when `table` is `Some`), and invokes the callback.

`expand_dynamic` pushes the dynamic op's `name` onto the path, then:

- With `table = Some` (ingest mode): reads the slot from `table[field.offset .. +8]` (rebased
  by `ctx.base`), computes `count = byte_len / stride` (rejecting `stride == 0` with
  `InvalidOp`), and for each element derives a child `WalkCtx` with the element base, the
  extended path (`push_index` for a list, `push` for a validated map key), the `ElementKey`,
  and the inherited frame/timestamp, then walks each member-template field. For a map it reads
  `{ key_off, key_len }`, slices the key from the name pool, and rejects a `.` in the key.
- With `table = None` (schema/registration mode): the element count is unknown, so it emits
  each member template **once** with no element key and no view — enough to surface the member
  ty/shape (e.g. `processes.pid`). Nested dynamics recurse the same way.

### 6.3 `RealizedField`

```rust
pub struct RealizedField<'a> {
    pub component_id: ComponentId,        // dynamic members: the composed dotted-name hash
    pub shape: &'a [usize],
    pub ty: PrimType,
    pub offset: usize,
    pub view: Option<ComponentView<'a>>,  // None in registration mode
    pub timestamp: Option<Timestamp>,
    pub frame: Option<ComponentId>,       // the frame this field belongs to, if Frame-tagged
    pub element: Option<ElementKey<'a>>,  // the dynamic element this came from; None for static
    pub dynamic: bool,                    // realized through a PathComponent terminal
}
```

`dynamic` distinguishes a member *template* (`processes.pid`, registration mode, never sampled
directly) and a concrete element-member (`processes.0.pid`, ingest mode) from a static field.
Each dynamic element-member is an ordinary flat component (dotted-name hash + view +
timestamp) through the unchanged `apply_value`; the only new thing a consumer observes is that
the *set* of component ids for a dynamic frame varies at runtime. Element-members **inherit
the frame timestamp** unless they carry their own `Timestamp` op.

---

## 7. Builder API

The `alloc`-gated `builder` module gains:

```rust
pub fn frame(component_id: impl Into<ComponentId>, arg: Arc<OpBuilder>) -> Arc<OpBuilder>;
pub fn name(s: &str) -> Arc<OpBuilder>;            // UTF-8 Data op, align 1
pub fn path_component(member_name: &str) -> Arc<OpBuilder>;
pub fn list(prefix: &str, members: impl IntoIterator<Item = FieldBuilder>, stride: u32)
    -> Arc<OpBuilder>;
pub fn map(prefix: &str, members: impl IntoIterator<Item = FieldBuilder>,
           stride: u32, value_offset: u32) -> Arc<OpBuilder>;
```

`FieldBuilder::with_frame(component_id)` wraps a field's chain in a `Frame` op (deduplicated
so a frame id shared by every field is stored once). `OpBuilder` gains `Frame`, `List
{ name, members: Vec<FieldBuilder>, stride }`, `Map { …, value_offset }`, and
`PathComponent { name }`.

`VTableBuilder::visit` for `List`/`Map` calls `visit_members`, which appends each member
`FieldBuilder` to `vtable.fields` as a contiguous block and records the `ElementFields
{ start, count }` range — templates are not top-level fields. Each member's op chain is
visited first (so a *nested* template block is appended earlier), then the direct member
`Field`s are pushed together to keep the range contiguous. `offset_table_ops` has
pass-through arms for the new builder variants: dynamic terminals carry no shiftable table ops
(`name` is a `Data` op; member offsets are relative to the element base, not the table), and
`Frame`'s `component_id` is a `Data` op, so only its `arg` can shift.

---

## 8. The `metor-fsw-2` surface

[`src/dynamic.rs`](../src/dynamic.rs) provides the two field types a frame author drops into a
`#[repr(C)]` struct. Each **is** the 8-byte slot — nothing more:

```rust
#[repr(C)]
pub struct Slot { pub trailer_off: u32, pub byte_len: u32 }

#[repr(transparent)]
pub struct FrameList<T, const MAX: usize> { slot: Slot, _ty: PhantomData<T> }

#[repr(transparent)]
pub struct FrameMap<K, V, const MAX: usize, const MAX_KEY: usize = 32> {
    slot: Slot, _kv: PhantomData<(K, V)>,
}
```

`#[repr(transparent)]` over `Slot` (the `PhantomData` is a ZST) keeps the in-struct field
exactly the 8-byte slot and trivially `IntoBytes`. Both default to `EMPTY`
(`trailer_off = 0, byte_len = 0`); the producer patches the slot through the `FrameWriter`.

- **`AsVTable`** emits the field's single `Op::List`/`Op::Map`. Reached statically
  (`vtable_fields`), the op `name` is the full dotted prefix (`path.to_name()`); reached as a
  member template (`element_fields`), it is the relative own name only. The element member
  templates are `V::element_fields(String::new())` (offsets relative to the element base; the
  enclosing frame `offset_by`s the slot).
- **`Componentize`** sinks nothing directly (the slot holds no in-struct value; elements are
  sunk through the vtable/trailer path) and sets the worst-case trailer budget:
  `FrameList::MAX_SIZE = round_up8(MAX * size_of::<T>())`;
  `FrameMap::MAX_SIZE = round_up8(MAX * map_stride::<V>() + MAX * MAX_KEY)` (entry array plus
  name pool).
- **`Metadatatize`** is empty — dynamic members are announced lazily, per new key, by the
  producer (§3.2).

The const helpers fix the map entry layout: `entry_align::<V>()` is `max(align_of::<V>(), 8)`
so each entry's `{ key_off, key_len }` pair and value stay 8-byte aligned;
`map_value_offset::<V>() = align_up(8, entry_align::<V>())`;
`map_stride::<V>() = align_up(value_offset + size_of::<V>(), entry_align::<V>())`.

`Name<'a>` is a map-key newtype enforcing the dotted-name grammar at construction: `Name::new`
returns `None` for an empty key (an empty segment vanishes under the `PathHasher` rule, which
would alias `a..b`) or a key containing `.` (which would alias the path separator). This is one
of three guards: the `Name` newtype and the `FrameWriter`'s `validate_key` reject bad keys
loudly at write time; `expand_dynamic` rejects a `.`-containing key at realize time with
`Error::InvalidComponentData`.

---

## 9. The producer — `FrameWriter`

[`FrameWriter<F>`](../src/writer.rs) builds a frame's table bytes (fixed region + trailer)
over a growable `LenPacket`. It writes the fixed `#[repr(C)]` region first (slots zeroed —
authored as `FrameList::EMPTY` / `FrameMap::EMPTY`), then each dynamic field appends its
element block to the trailer (8-byte aligned via the ring's `round_up8`) and patches its slot
`{ trailer_off, byte_len }`. All trailer offsets are table-absolute (relative to the
fixed-region start), matching the one-global-trailer invariant.

- `list(slot_off, build)` collects elements through a `ListWriter<T>`, aligns to 8, appends
  the back-to-back element bytes, and patches the slot to span them.
- `map(slot_off, build)` collects `(key, value)` entries through a `MapWriter<V>` (rejecting
  empty / `.`-containing keys, surfaced as `WriteError::EmptyKey` / `WriteError::DotInKey`),
  then lays the fixed-stride entry array followed by the name pool: each entry's `key_off`
  points table-absolute into the pool, and the slot delimits **only the entry array** so
  `count = byte_len / stride` stays exact.

The result is fed to `VTable::apply`, with table offset 0 at the fixed region (`writer.table()`).

---

## 10. Wiring & compatibility

[`src/descriptor.rs`](../src/descriptor.rs) drives compatibility checking in **registration
mode**. `realize_set(vtable)` calls `vtable.realize_fields(None)` and collects every
`(component_id, ty, shape)` — including dynamic member *templates* (e.g. `processes.pid`),
which is the registration-mode contract. `compatible(producer, consumer)` requires the same
`frame_id` and that the consumer's component set is a **subset** of the producer's with
matching `ty`/`shape`, so a producer may emit extra fields a consumer ignores. Because a
dynamic frame's template set is fixed at compile time (only the concrete keyed/indexed members
vary at runtime), two ports agree on a dynamic member by agreeing on its template.

---

## 11. dlopen and prefix rewriting

A statically-linked port bakes instance-prefixed component ids by re-deriving its vtable under
the instance name (`AsVTable::vtable_fields(prefix)`). A `dlopen`'d system has no static frame
type, so [`src/abi/mod.rs`](../src/abi/mod.rs) carries the **unprefixed** vtable plus per-port
`ComponentMetadata` across the boundary and reconstructs the prefixed vtable on the host with
`prefix_announce_vtable`: it builds an unprefixed→prefixed id map from the metadata and
rewrites every 8-byte `Op::Data` blob whose value is a known leaf id.

Dynamic member templates need no rewrite there: they use `Op::PathComponent`, which composes
its id at realize time from the runtime path and bakes **no** id into `data`. The frame-tag id
(baked bare by `with_frame`) and the schema `ty`/`dim` blobs are likewise absent from the map
and left untouched. So a dlopen'd dynamic frame keys its components the same way as a static
one once the runtime prefix flows through the same `PathHasher` walk.

---

## 12. Status of the consumer side

The producer (`FrameWriter`), the realization engine (`for_each_field` / `walk_field` /
`expand_dynamic`), the op set, `PathHasher`, the `FrameList`/`FrameMap` surface, and the
wiring/compatibility and dlopen paths are all implemented and tested (see the dynamic-frame
tests in `metor-proto/src/vtable.rs` covering §5a/§5b/§5c, dot-in-key rejection, and
registration mode).

`metor-db`'s ingest and streaming of dynamic frames is the one piece not yet built: lazy
creation of concrete keyed/indexed components on first sample, and re-serializing a
runtime-sized trailer per subscriber tick. That db-side design lives in
[`db-dynamic-streaming.md`](db-dynamic-streaming.md). Until it lands, `metor-db` registers the
member templates from `realize_fields(None)` but does not stream concrete dynamic components;
no part of the `metor-fsw-2` model above depends on it.
