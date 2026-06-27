# Work-Package 2 — VTable extensions (frames + dynamic components)

Status: **design only, pre-implementation**. Reviewer sign-off required before any code lands.

This document designs additions to the `metor-proto` VTable op set so that
`metor-fsw-2` can describe (1) **frames** — timestamped groups of components named by a
`ComponentId` — and (2) **runtime-dynamic** list/map fields whose data lives in a padded
trailer after the fixed portion of a table, including **nested** dynamic fields.

> Revision note (post-review): dynamic element identity is now **fully-qualified dotted
> names hashed exactly like every other component** (`ComponentId::new("processes.htop.pid")`)
> — *not* a folded/synthetic hash. Dynamic-frame **streaming** and **nested dynamics** are
> **in scope for v1**. Sections 3–9 reflect those decisions.

Relevant existing code:
- `libs/metor-proto/src/vtable.rs` — `Op`, `Field`, `VTable`, `OpRef`, `Offset`,
  `realize`, `realize_fields`, `apply`, the `builder` module, `_ASSERT_OP_SIZE`.
- `libs/metor-proto/src/com_de.rs` — `Componentize` / `Decomponentize`.
- `libs/metor-proto/src/types.rs` — `ComponentId` (fnv1a, top bit masked), `PrimType`,
  `ComponentView`, `Timestamp`.
- `libs/metor-fsw/src/path.rs` — `ComponentPath` / `ChainPath`; the dotted-name hashing
  and the `// TODO: chain hash functions` we factor out here.
- `libs/metor-proto/wkt/src/metadata.rs` — `ComponentMetadata` (+ `with_prefix`), the
  id→name channel.
- `libs/db/src/vtable_stream.rs`, `libs/db/src/lib.rs` (`insert_vtable`), `libs/db/src/main.rs`
  (`GenCpp`), `libs/db/cpp/vtable.hpp` — db consumers that `match` on `Op`/`RealizedOp`.

---

## 1. Purpose & the symmetry with metor-db

A `metor-fsw-2` system publishes its output as a `repr(C)` table plus a VTable that
describes it. That VTable is the *same* artifact metor-db already ingests
(`VTableMsg` → `insert_vtable` → `realize_fields`), so a frame "serializes" with no extra
step: the producer fills a struct, hands over the bytes plus its VTable, and both the
coordinator (for wiring/validation) and metor-db (for storage) interpret it through one
mechanism. Reusing the VTable op set — rather than inventing a second self-describing
format — is the core symmetry of the framework. The op set must grow to cover what frames
need that plain component tables do not: a **frame identity** and **dynamic
(runtime-sized) members** that still resolve to ordinary, dotted-named components.

### Recap of the current model

- `ops: Vec<Op>` is a flat pool; `OpRef(u32)` indexes it. Sub-parts of an op are always
  `OpRef`s, never inline payloads — this keeps `Op` small.
- `Field { offset: Offset, len: u32, arg: OpRef }`. `offset`/`len` address the **runtime
  table**; `arg` is the head of an **op chain**.
- `realize(op_ref, table)` evaluates one op into a `RealizedOp`. `Data` reads the VTable's
  own `data` buffer (static side-table); `Table` reads the runtime `table` argument.
- `realize_fields(table)` walks each field's chain in a loop: `Schema` records ty+dim,
  `Timestamp` records the timestamp, `Ext` passes through, and `Component` **terminates**,
  emitting a `RealizedField { component_id, shape, ty, offset, view, timestamp }`.
- `apply` drives `realize_fields(Some(table))` and pushes each `RealizedField` into a
  `Decomponentize` sink via the flat `apply_value(component_id, view, timestamp)`.

The extensions are expressed as **new ops in the chain / new chain terminals**, reusing
this loop.

---

## 2. Frame identity — `Op::Frame { component_id, arg }`

(Recommendation from the first revision; **unchanged**.)

A frame is a group of fields that share a timestamp and are collectively named by a frame
`ComponentId`. We express "this field belongs to frame X" with a metadata op in the
field's chain — like `Timestamp`. All fields of the frame reference the same `Frame` op
(dedup by `OpRef`), so the id is stored once. Chosen over adding a `frame` field to
`Field` (breaks the struct's `repr(C)`/postcard/C++ ABI) or a top-level `VTable` change.
The realize loop carries `frame: Option<ComponentId>` and emits it on `RealizedField`
(new field). Old VTables have no `Frame` op ⇒ `frame: None`, identical to today.

`Frame` is orthogonal to the dynamic-naming machinery below: static frames keep using
compile-time-hashed `Component` ids; `Frame` only adds the grouping key.

---

## 3. Dynamic identity model — fully-qualified dotted names

### The rule

Every dynamic element-member resolves to an **ordinary component** whose name is the
field path prefix, joined with the key/index, joined with the member name:

| shape | example component name | ComponentId |
|-------|------------------------|-------------|
| `FrameMap<Name, Process>` | `processes.htop.pid`  | `ComponentId::new("processes.htop.pid")` |
| `FrameList<Process>`      | `processes.0.pid`     | `ComponentId::new("processes.0.pid")` |
| nested (list of map)      | `processes.htop.threads.3.state` | `ComponentId::new("processes.htop.threads.3.state")` |

The id is **just the fnv1a hash of the full dotted string**, top bit masked, exactly like
`ComponentId::new` and exactly like the static dotted names the `path.rs` derive already
produces. There is **no new hashing scheme and no new sink method**:
`Decomponentize::apply_value(component_id, view, timestamp)` is unchanged, so db and UI
need zero value-path changes. A dynamic frame simply emits a runtime-variable *set* of
ordinary components.

### Where each name segment comes from

| segment | example | when known | stored where |
|---------|---------|-----------|--------------|
| **prefix** | `processes` | compile time | string in the VTable `data` buffer, referenced by the `List`/`Map` op's `name: OpRef` |
| **member name** | `pid` | compile time | string in `data`, referenced by the leaf terminal `PathComponent { name: OpRef }` |
| **map key** | `htop` | **runtime** | UTF-8 in the **trailer** (see §3.2) |
| **list index** | `0`, `3` | runtime (positional) | not stored — the element ordinal, formatted as decimal at realize time |

Compile-time segments live as `Data` ops (the existing `data` side-table; the "names
side-table" is just `Data` ops holding UTF-8, align 1 — no new buffer, no change to
`heapless` bounds). Runtime segments are produced by the `List`/`Map` expansion.

### 3.1 Chained (rolling) hashing — no full-string alloc on the hot path

fnv1a is a rolling hash: `h = OFFSET_BASIS_64; for b in bytes { h = (h ^ b).wrapping_mul(PRIME_64) }`
(`const-fnv1a-hash` constants `0xcbf29ce484222325` / `0x00000100000001B3`).
`ComponentId::new(s)` is this over `s.as_bytes()` with the final `& !(1 << 63)` Lua/i64
mask. Because it is a pure left-fold, feeding `"processes"`, then `"."`, then `"htop"`,
then `"."`, then `"pid"` in sequence yields **byte-for-byte** the same accumulator as
hashing the literal `"processes.htop.pid"`.

We factor a small rolling hasher into `metor-proto` and **share it with `path.rs`**
(resolving its `// TODO: we can do this without an alloc by chaining hash functions`):

```rust
// metor-proto
pub struct PathHasher(u64); // seeded with FNV_OFFSET_BASIS_64

impl PathHasher {
    pub const fn new() -> Self { Self(0xcbf2_9ce4_8422_2325) }
    /// Append one dotted segment. Replicates ChainPath's empty-segment rule:
    /// a leading/empty segment emits no separator (so "" + "a" == "a").
    pub fn push(&mut self, seg: &str) { /* feed '.' if non-first & seg non-empty, then seg bytes */ }
    pub fn finish(self) -> ComponentId { ComponentId(self.0 & !(1 << 63)) }
}
```

`realize` threads a `PathHasher` (a `Copy` `u64` accumulator) down the dynamic walk:
`List`/`Map` `push`es the prefix and then the key/index per element; the leaf
`PathComponent` `push`es the member name and `finish`es. The **hot id path never
allocates a `String`** — segments are fed directly. `ChainPath::to_component_id` is
re-expressed on `PathHasher` so static and dynamic paths share one definition (risk: the
two must stay bit-identical; covered by a cross-check test, see §9).

### 3.2 Map trailer: variable-length key names with fixed stride

The map key is now a variable-length **name**, but the trailer is parsed by a fixed
`stride` (`count = byte_len / stride`). Two options:

- **(A) fixed-size inline name buffer per entry** — `entry = { name: [u8; K], value }`.
  Simple, fixed stride, but caps names at `K` and wastes space on short names. Rejected.
- **(B) relative-slice key, names pooled after the entry array** — `entry = { key_off: u32,
  key_len: u32, value: <sub-frame> }`, fixed stride; the key *bytes* live in a **name pool**
  that follows the entry array in the trailer. `key_off`/`key_len` are table-absolute into
  that pool. **Recommended.**

Rationale for (B): arbitrary-length names with no cap, no per-entry waste, and it reuses
the exact relative-slice idiom already used for the field slot (rkyv style). Layout:

```
trailer = [ entry[0 .. N] ][ key-name pool (UTF-8, back-to-back) ]
entry   = repr(C) { key_off: u32, key_len: u32, <pad> value: <value sub-frame> }
```

The owning field's slot `{ trailer_off, byte_len }` delimits **just the entry array**
(so `count = byte_len / stride` is exact); the name pool is reached only through per-entry
`key_off`/`key_len` (bounds-checked against the table end). `value_offset` (on the `Map`
op) gives the byte offset of the value sub-frame within an entry. For **lists** no key is
stored — the index is the ordinal, formatted at realize time.

### 3.3 Names reach consumers via the metadata channel

The hashed id travels in the table; the **display name** travels on the existing
metadata channel (`ComponentMetadata { component_id, name, metadata }` via
`SetComponentMetadata`). For static components the derive already emits one `ComponentMetadata`
per component. For dynamic components the **producer** emits metadata **lazily, once per
newly-discovered key**: when a `FrameMap` first sees key `"htop"`, the framework emits, for
each member (and nested member) of that element, a `ComponentMetadata` whose `name` is the
composed dotted string and whose `component_id` is its hash. This reuses
`ComponentMetadata::with_prefix` (which already does `format!("{prefix}.{name}")` + rehash)
— the dynamic path is `prefix.key` applied to each member-template's metadata. Consumers
(UI/db) already map id→name through this channel, so `processes.htop.pid` displays with no
new mechanism. (The full `String` is built here, on the cold "new key discovered" path,
not per-sample.)

---

## 4. New `Op` variants

```rust
#[derive(Debug, Serialize, Deserialize, Clone, postcard_schema::Schema)]
#[repr(u8)]
pub enum Op {
    // ---- existing, unchanged ----
    Data { offset: Offset, len: u32 },
    Table { offset: Offset, len: u32 },
    None,
    Component { component_id: OpRef },              // static: precomputed dotted hash
    Schema { ty: OpRef, dim: OpRef, arg: OpRef },
    Timestamp { source: OpRef, arg: OpRef },
    Ext { arg: OpRef, id: PacketId, data: OpRef },

    // ---- new ----

    /// Frame identity / grouping (§2). Metadata op: records frame id, continues at `arg`.
    Frame { component_id: OpRef, arg: OpRef },

    /// Dynamic list terminal. The owning field's `offset`/`len` address an 8-byte slot
    /// `{ trailer_off: u32, byte_len: u32 }`. Elements are fixed-`stride`, back-to-back;
    /// `count = byte_len / stride`. `name` -> Data op holding the prefix ("processes").
    /// `members` are template fields (offsets relative to the element base); a member's
    /// chain may itself terminate in `List`/`Map` (nesting, §6.3).
    List { name: OpRef, members: ElementFields, stride: u32 },

    /// Dynamic map terminal. Entries are `{ key_off: u32, key_len: u32, <pad> value }`,
    /// fixed `stride`; key bytes live in the trailer name pool (§3.2). `value_offset` is
    /// the byte offset of the value sub-frame within an entry.
    Map { name: OpRef, members: ElementFields, stride: u32, value_offset: u32 },

    /// Dynamic leaf terminal. Appends `name` ("pid") to the running path hash and
    /// finalizes -> ComponentId. Replaces `Component` for dynamic members.
    PathComponent { name: OpRef },
}

/// A contiguous range of member-template fields in `VTable::fields`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, postcard_schema::Schema)]
#[repr(C)]
pub struct ElementFields { pub start: u32, pub count: u32 }
```

### Constraints addressed

- **`#[repr(u8)]` + `size_of::<Op>() <= 64`.** Largest new variant is `Map`:
  `name(u32) + members(8) + stride(u32) + value_offset(u32)` = 20 bytes payload, ~24 with
  the discriminant — far under 64 and comparable to the existing `Schema`. All multi-op
  sub-parts use `OpRef`; the only inline payloads are small fixed scalars, mirroring the
  existing `Data`/`Table` `{ offset, len }` u32 pairs. `_ASSERT_OP_SIZE` stays.
- **Derives / round-trip.** New variants and `ElementFields` use only `OpRef`/`u32`, all of
  which already implement `Serialize/Deserialize/Clone/postcard_schema::Schema`; postcard
  round-trips. Variants are **appended**, so existing discriminants do not shift.
- **`alloc` vs `heapless`.** No new container types in `Op`/`VTable`. Member templates and
  name strings live in the existing `fields`/`data` buffers (`Buf<…>`), so no-alloc bounds
  are unchanged. Only `builder` (already `#[cfg(feature = "alloc")]`) gains constructors.

---

## 5. Trailer encoding — worked byte-level examples

All examples use prefix `"processes"`, `Process { pid: u64, cpu_usage: f64 }` (size 16,
align 8), fixed region `{ timestamp: i64 @0, slot @8 }` (fixed size 16). Trailer is
8-aligned and starts at 16. Integers little-endian. Member templates (relative to the
element/value base): `{0, 8, schema(U64,[], path_component("pid"))}`,
`{8, 8, schema(F64,[], path_component("cpu_usage"))}`.

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

Length 48. Realize: slot→`(16,32)`→`count=2`. For `i in 0..2`, `base = 16 + i*16`; push
prefix `"processes"`, push index `i` (decimal); walk members ⇒
`processes.0.pid=1001`, `processes.0.cpu_usage=0.5`, `processes.1.pid=1002`,
`processes.1.cpu_usage=0.25`.

### 5b. `FrameMap<Name, Process>`, 2 entries (keys "htop", "init")

Entry `{ key_off: u32 @0, key_len: u32 @4, value: Process @8 }` ⇒ `stride = 24`,
`value_offset = 8`. Slot delimits the entry array: `{ trailer_off = 16, byte_len = 48 }`
⇒ `count = 2`. The name pool follows the entry array at offset 64.

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

Length 72. Realize: `count=2`; for `i`, `entry = 16 + i*24`; `key = str(table[key_off .. key_off+key_len])`;
`value_base = entry + 8`; push `"processes"`, push `key`, walk members ⇒
`processes.htop.pid=1001`, `processes.htop.cpu_usage=0.5`, `processes.init.pid=1002`,
`processes.init.cpu_usage=0.25`.

### 5c. Nested — `FrameMap<Name, Host>` where `Host { threads: FrameList<Thread> }`, `Thread { state: u8 }`

Target names like `processes.htop.threads.3.state`. The map's **value sub-frame** `Host`
is not a leaf — its single member is itself a `List` whose slot lives inside the entry's
value region and points into the **same global trailer**. (Design choice: **one global
trailer, all slots table-absolute**, so nested offset math is uniform; nested trailers are
*not* per-element-relative.)

```
fixed   { timestamp @0, slot_outer @8 }                       // size 16
entry[i] (stride 16) = { key_off: u32, key_len: u32, slot_inner: {off:u32,len:u32} }
                                                              // value sub-frame = the inner List slot
```

Worked layout for one host "htop" with a single thread (a 1-element list, ordinal 0 ⇒
`processes.htop.threads.0.state`):

| off | field | value |
|----:|-------|-------|
| 0  | timestamp                          | 1000 |
| 8  | slot_outer `{off=16, len=16}`      | one entry (stride 16) |
| 16 | entry[0].key_off = 40              | → name pool "htop" |
| 20 | entry[0].key_len = 4               | |
| 24 | entry[0].slot_inner `{off=44,len=1}` | inner list: 1 thread, stride 1 |
| 28 | (pad to 8 before pools)            | |
| 40 | name pool "htop"                   | (4 bytes) |
| 44 | thread[0].state u8 = 2             | inner-list trailer |

Realize recursion: outer `Map` pushes `"processes"`, then key `"htop"`; the value sub-frame
member is `List(name="threads", …)`, realized against the entry's value region — it reads
`slot_inner` at `entry + value_offset`, pushes `"threads"` then index `0`, walks `Thread`'s
members ⇒ leaf `PathComponent("state")` finalizes `processes.htop.threads.0.state = 2`. The
`PathHasher` accumulator is **passed down by value** through each level, so the dotted name
chains correctly with no string building on the sample path. (Producers are free to lay the
name pool and nested trailers in any order after the fixed region; only the offsets are
load-bearing — the table above interleaves them to keep the example compact.)

---

## 6. Realization — `realize` / `realize_fields` / `apply`

### 6.1 `realize`

Add `RealizedOp` cases (additive; no existing arm changes):

```rust
pub enum RealizedOp<'a> {
    Data(&'a [u8]), Table(RealizedTableSlice<'a>), Component(RealizedComponent),
    Schema(RealizedSchema<'a>), Timestamp(RealizedTimestamp), Ext(RealizedExt<'a>), None,
    // new:
    Frame(RealizedFrame),                 // { component_id, arg }
    List(RealizedDynamic<'a>),            // { name: &str, members, stride, arg-less }
    Map(RealizedDynamic<'a>),             // + value_offset
    PathComponent(RealizedPathLeaf<'a>),  // { name: &str }
}
```

- `Frame` resolves `component_id` (Data → ComponentId), returns `Frame { component_id, arg }`,
  **non-terminal** (loop follows `arg`).
- `List`/`Map` resolve `name` (Data → `&str`) and return the descriptor; they are
  **expanding terminals** — the slot is read by `realize_fields` (which holds `field.offset`).
- `PathComponent` resolves `name` (Data → `&str`); the loop finalizes the path hash.

### 6.2 `realize_fields`

The loop gains:
- `Frame(f)` → `frame = Some(f.component_id); realized_op = realize(f.arg)`.
- `List`/`Map` → **expand** one field into many `RealizedField`s. With `table = Some`: read
  the slot from `table[field.offset .. +8]`, compute `count`, and for each element seed a
  `PathHasher` with the prefix + key/index and walk the member-template fields (recursively,
  re-basing `table` to the element/value slice). With `table = None`: emit each member
  template **once** (no key/index, `view = None`) so consumers learn member ty/shape.
- A member template that itself terminates in `List`/`Map` recurses (§6.3), threading the
  parent's `PathHasher`.

Because one field can now yield several outputs, the per-field expansion moves into a
push-style driver `fn for_each_field(table, ctx, &mut impl FnMut(RealizedField))` carrying
the `PathHasher`/frame/schema state; `apply` is implemented on top of it. Under `alloc` the
existing `realize_fields(...) -> impl Iterator` is kept by `flat_map`ing each field's
outputs into a `SmallVec`. Top-level iteration **skips claimed member-template field
indices** (collected from every `List`/`Map` `members` range in one pre-pass, or via a
builder watermark — see §9).

`RealizedField` gains structural context (additive; defaulted for static fields):

```rust
pub struct RealizedField<'a> {
    pub component_id: ComponentId,  // for dynamic members: the composed dotted-name hash
    pub shape: &'a [usize], pub ty: PrimType, pub offset: usize,
    pub view: Option<ComponentView<'a>>, pub timestamp: Option<Timestamp>,
    // new:
    pub frame: Option<ComponentId>,
    pub element: Option<ElementKey>,   // Index(u32) | Key(&'a str); None for static
}
```

### 6.3 Nested recursion

Member templates are realized by the same loop, so nesting is free in the type system: a
member's chain ends in `PathComponent` (leaf) **or** another `List`/`Map` (nested dynamic).
Three invariants make the byte math work:
1. **One global trailer; all slots are table-absolute.** A nested slot is read from the
   element's value region but its value is an offset into the whole table, so the nested
   walk re-bases `table` to a table-absolute slice — no per-level relative arithmetic.
2. **The `PathHasher` is passed by value down each level**, so `processes` → `htop` →
   `threads` → `0` → `state` composes incrementally; no intermediate `String`.
3. **Depth is bounded** by the static op graph (the vtable is finite/acyclic), so recursion
   terminates; no-alloc builds get an explicit max-depth const to bound stack use.

### 6.4 What the consumer/db sees

Each dynamic element-member is an **ordinary flat component** (dotted-name hash + view +
timestamp) through the unchanged `apply_value`. db/UI need no value-path change; the only
new thing they observe is that the *set* of component ids for a dynamic frame varies at
runtime (handled by lazy registration, §8, and metadata emission, §3.3).

---

## 7. Builder API additions (`builder`, `#[cfg(feature = "alloc")]`)

```rust
/// Frame identity (§2).
pub fn frame(component_id: impl Into<ComponentId>, arg: Arc<OpBuilder>) -> Arc<OpBuilder>;

/// A name string in the data side-table (UTF-8, align 1). Used by list/map prefixes
/// and path_component leaves.
pub fn name(s: &str) -> Arc<OpBuilder>;

/// Dynamic leaf terminal: appends `member_name` and finalizes the dotted-name hash.
pub fn path_component(member_name: &str) -> Arc<OpBuilder>;

/// Runtime list of `Process`-like elements. `prefix` is the field name ("processes");
/// `members` are the element member templates (offsets relative to element base);
/// `stride` is the element size. Members may themselves be list/map (nesting).
pub fn list(
    prefix: &str,
    members: impl IntoIterator<Item = FieldBuilder>,
    stride: u32,
) -> Arc<OpBuilder>;

/// Runtime map keyed by a name string. Entry layout `{ key_off, key_len, <pad> value }`;
/// `value_offset` locates the value sub-frame within an entry.
pub fn map(
    prefix: &str,
    members: impl IntoIterator<Item = FieldBuilder>,
    stride: u32,
    value_offset: u32,
) -> Arc<OpBuilder>;
```

`OpBuilder` gains `Frame`, `List { prefix, members: Vec<FieldBuilder>, stride }`,
`Map { … value_offset }`, `PathComponent { name }`. `VTableBuilder::visit` for `List`/`Map`
**appends each member `FieldBuilder` to `vtable.fields`** (recording `ElementFields { start,
count }` and marking those indices claimed) and emits the op; templates are not top-level
fields. `offset_table_ops` gains pass-through arms for the new builder variants so nested
member offsets shift correctly. Optional `frame!`/`list!`/`map!` macros mirroring
`field!`/`table!` are a follow-up.

---

## 8. db impact / follow-ups

No existing op's behavior changes; adding variants forces exhaustive `match`es to gain
arms. Sites and required arms:

1. **`metor-proto/src/vtable.rs` — `realize` (exhaustive).** Add `Frame`/`List`/`Map`/
   `PathComponent` arms (the WP change). *Real handling.*
2. **`metor-proto/src/vtable.rs` — `realize_fields` loop (`_ => Err(InvalidOp)`).** Add
   `Frame` passthrough, `List`/`Map` expansion, `PathComponent` finalize. *Real handling.*
   The `RealizedOp` helper matches keep their `_` fall-through.
3. **`db/src/vtable_stream.rs:41` — `table_len` (`match op { Op::Table … _ => 0 }`).** The
   `_ => 0` covers the new ops, and the dynamic slot is an ordinary `Field` already counted
   by the `fields` map/`max`; the *fixed* length stays correct. The **trailer** is
   runtime-sized — handled by the new streaming path (§8.6), not this length calc. *Note,
   no struct change here.*
4. **`db/src/vtable_stream.rs:54` — streaming realize loop (`_ => Err(InvalidOp)`).** Add
   `RealizedOp::Frame(f)` passthrough; **dispatch** `List`/`Map`/`PathComponent` into the new
   dynamic-stream builder (§8.6). *Real handling — no longer a rejection.*
5. **`db/src/lib.rs:247` — `insert_vtable` via `realize_fields(None)`.** Now receives member
   templates (`view = None`, `frame = Some`, `element = None`). Follow-up: register the
   element member **schemas** (ty/shape) as a template under the prefix, but create concrete
   keyed/indexed components **lazily at apply/push time** when keys/indices appear. The
   ingest path must `get_or_insert` the component schema on first sample. *Real handling.*
6. **`db/src/lib.rs` ingest + metadata.** On first sight of a new dynamic component id, db
   accepts the lazily-created component and stores the matching `ComponentMetadata` (emitted
   by the producer, §3.3) so queries resolve `processes.htop.pid`. *Real handling.*
7. **`db/src/main.rs:102` — `vtable::Op::to_cpp()` + hand-written `db/cpp/vtable.hpp`.** New
   variants enter the generated postcard schema automatically, but the C++ `OpBuilder` mirror
   must gain `Frame`/`List`/`Map`/`PathComponent` builders (and the C++ table interpreter
   must learn the trailer/slot + name-pool walk, or explicitly reject). Silent gap if
   skipped (no compile error). *Real handling on the C++ side.*
8. **`metor-fsw/src/path.rs`.** Re-express `ChainPath::to_component_id` on the shared
   `PathHasher` (removes its alloc TODO); must stay bit-identical to `ComponentId::new`.
9. **Non-matching consumers (no change).** `metor-ui` plot consumes via a flat `apply_value`
   closure and the `builder` helpers; `metor-fsw/src/vtable.rs` only uses `builder`. No other
   crate `match`es metor-proto's `Op`/`RealizedOp`.

### 8.6 Streaming dynamic frames (in scope for v1)

The blocker is `FieldTable` in `vtable_stream.rs`: it pre-allocates **one fixed-offset shard
per field** in a single `LenPacket`, with an `AtomicBitVec` of "field filled" flags, and
sends the *same* buffer every tick. A dynamic trailer is runtime-sized and its component set
changes, so a fixed shard table cannot hold it.

Design — a **`DynamicFrameTable`** that **rebuilds the packet each tick** instead of
overwriting fixed shards:

- **Static fields keep the fast path.** A VTable with no `List`/`Map` uses today's
  `FieldTable` unchanged. A VTable that mixes static + dynamic uses `DynamicFrameTable`,
  which still writes static fields into fixed positions but appends a freshly built trailer.
- **Per-tick rebuild.** Each cycle the stage: (1) reads the **producer's latest dynamic
  table** to learn the **live key/index set** (the producer is authoritative about current
  membership — which processes exist now); (2) for each live element-member component, pulls
  its latest value from that component's WAL `Reader` (the same `RealTimeStage` source,
  keyed by the composed dotted-name id, created lazily as keys appear); (3) **re-serializes**
  the table: fixed region + slot + entry array + key-name pool, pushing into a growable
  `LenPacket` (`push`/`extend_aligned`, like `FieldTable::new` but length-varying); (4) sends.
- **Readiness.** The `AtomicBitVec` "all shards filled" gate is replaced for the dynamic
  portion by "rebuild on tick or on any contributing-component update"; static shards keep
  their gate. Backpressure/overrun semantics are unchanged (writers never block; lapped
  readers are detected exactly as today).
- **Membership churn.** Keys appearing/disappearing across ticks is expected: the live set is
  re-derived each tick from the producer table, so a vanished process simply drops out of the
  next packet; a new one is picked up once db has lazily registered its components (§8.5) and
  the producer has emitted its metadata (§3.3).
- **Nested frames** stream the same way: the rebuild walks the nested realize recursion
  (§6.3) to lay down nested slots + inner trailers into the same growable packet.

Cost: the dynamic path trades the zero-copy fixed-shard write for a per-tick re-serialization.
That is the necessary price of a runtime-sized payload and is bounded by the live element
count. This is a real lift in `vtable_stream.rs` (new `DynamicFrameTable`, dynamic-aware
plan construction) and is tracked as the largest WP-2 db task.

---

## 9. Open questions / risks for the reviewer

1. **`PathHasher` ↔ `ComponentId::new` bit-equality.** The rolling hasher must reproduce
   `ComponentId::new(full_dotted)` exactly, including `ChainPath`'s empty-segment/separator
   rules and the final top-bit mask. Proposed: a property test hashing random dotted paths
   both ways. Confirm the empty-segment rule (leading `.`? trailing `.`?) we must match.
2. **Map key uniqueness & name validity.** Keys are arbitrary producer strings. Dots inside
   a key (`"a.b"`) would alias the path grammar (`processes.a.b.pid`). Forbid `.` in keys, or
   escape? Also dedup policy if a producer emits the same key twice in one table.
3. **`trailer_off` / slot base.** Proposed table-absolute for both the field slot and nested
   slots (uniform nesting). Confirm vs trailer-relative.
4. **Name-pool placement.** §3.2 pools key names after the entry array, addressed by absolute
   offsets. Alternative: a single shared name pool for the whole table (dedup repeated keys
   across nested levels). Worth the dedup complexity?
5. **Claimed-template bookkeeping.** Skipping member templates in the top-level walk needs a
   pre-pass claimed-set or a builder watermark (templates after real fields). Watermark is
   cheaper but constrains builder ordering. Preference?
6. **Lazy dynamic registration & unbounded keys.** §8.5/§8.6 create components lazily and
   re-derive membership each tick. Churning keys (e.g. short-lived PIDs as map keys) grow the
   component set without bound. Need eviction/TTL or a "hidden after N idle ticks" policy?
7. **Streaming cost / max element count.** The per-tick rebuild is O(live elements). Do we
   need a cap or a slower update rate for very large dynamic frames?
8. **Metadata volume.** Emitting `ComponentMetadata` per member per new key can burst on
   startup (many processes at once). Batch into one message? Rate-limit?
9. **Nesting depth bound.** A `max-depth` const bounds no-alloc recursion. What depth must v1
   support (`processes.htop.threads.3.state` is depth 3)?

---

## Implementation Plan

Status: the **testable core landed** (this pass). The streaming rebuild is the
sequenced follow-up.

### Landed (metor-proto)
- `hash::PathHasher` — rolling fnv1a-64, bit-identical to `ComponentId::new`; `push`,
  `push_index`, `finish` (top-bit mask). Reused in `metor-fsw/src/path.rs` via a new
  `ComponentPath::hash_into`, killing the alloc TODO in `ChainPath::to_component_id`.
- `Op::{Frame, List, Map, PathComponent}` + `ElementFields` (`#[repr(C)]`, two `u32`);
  `Op` stays ≤ 64 bytes and keeps the serde/postcard_schema derives.
- `RealizedOp::{Frame, List, Map, PathComponent}`, `RealizedFrame`, `RealizedDynamic`,
  `RealizedPathLeaf`, `ElementKey`; `RealizedField` gains `frame` + `element`.
- `realize` arms for the new ops (`realize_str` helper for name data).
- `for_each_field` push driver (no-alloc; recursion bounded by `MAX_DYNAMIC_DEPTH = 8`)
  threading a `WalkCtx { base, path, element, frame, timestamp, depth }`; `apply`
  reimplemented on it; `realize_fields` kept as an `alloc`-gated collecting iterator.
  Dynamic expansion reads the `{trailer_off, byte_len}` slot, derives `count`, walks
  member templates, composes dotted names, and **inherits the frame timestamp** into
  elements. **Map keys containing `.` are rejected** (`Error::InvalidComponentData`).
- builder: `frame`, `name`, `path_component`, `list`, `map`; `OpBuilder` variants;
  `visit`/`visit_members` (appends member templates contiguously, records
  `ElementFields`); `offset_table_ops` pass-throughs; top-level walk skips template
  fields via `is_template_field`.

### Identity (decisions honored)
Dynamic component id = `ComponentId::new("<prefix>.<key|index>.<member>")`. Prefix and
member names are compile-time `Data` ops; the map key is a runtime trailer name (relative
slice `{key_off,key_len}` into a name pool after the fixed-stride entry array); list keys
are positional indices. One global table-absolute trailer; nesting via recursion (depth 8).

### Tests (all green)
`hash.rs`: deterministic + a 2000-iteration property test (`PathHasher` vs
`ComponentId::new`). `vtable.rs`: §5a list, §5b map (name pool), §5c nested
(`processes.htop.threads.0.state`), dot-in-key rejection, and `realize_fields(None)`
registration mode.

### Deferred (WP2b) — db follow-ups
- **Streaming (`vtable_stream.rs`)**: `Frame` passes through; `List`/`Map`/`PathComponent`
  return a clear error tagged `// TODO(streaming): DynamicFrameTable, WP2b`. The real
  `DynamicFrameTable` per-tick rebuild (§8.6) needs WP3 frame producers to integration-test.
- **`insert_vtable`**: registers member-template ty/shape from `realize_fields(None)`;
  concrete keyed components must be created lazily at apply/ingest (`// TODO(WP2b)`).
- **C++**: `ElementFields::to_cpp()` added to `gen-cpp`; the hand-written `cpp/vtable.hpp`
  builder intentionally omits the dynamic ops (documented), so no silent gap.
