# WP2b — metor-db dynamic-frame ingest + streaming

Status: **design only, pre-implementation**. Reviewer sign-off required before any code
lands. No Rust in this WP.

WP2 (`libs/metor-proto/src/vtable.rs`) added the dynamic op set — `Op::{Frame, List, Map,
PathComponent}`, the `for_each_field` / `expand_dynamic` / `walk_field` realization engine,
`RealizedField { component_id, ty, shape, view, timestamp, frame, element }`, and the shared
`PathHasher`. It left the db side stubbed: `insert_vtable` registers member *templates* but
not concrete keyed components (`// TODO(WP2b)`, `lib.rs:254`), and `handle_vtable_stream`
*rejects* a field plan that reaches `List`/`Map`/`PathComponent` (`// TODO(streaming)`,
`vtable_stream.rs:117–125`). This document designs the db side: **lazy ingest** of dynamic
frames (concrete components created on first sample) and **streaming** them to subscribers.

## Review decisions (gate passed)

- **Borrow wrinkle → `with_state_mut` + clone the vtable** out of the registry, single
  `for_each_field` pass (§2.2). Per-sample vtable clone accepted for v1.
- **Live key set → the producer's latest table is authoritative** (§3/§4 adjusted): the live
  stream reflects exactly the keys in the most recently ingested table for the frame, so a
  vanished key drops out next tick (the original `vtable-dynamic.md` §8.6 intent). db **caches
  the last table bytes per dynamic frame**; the stream re-emits/re-derives from that cache each
  subscriber-tick (sampled latest-wins at the subscriber rate), with the global-`vtable_gen`
  watch resending the VTable structure on bump. The grow-only `dynamic_members` structure (§3)
  is therefore **not** the streaming source — keep it only if it earns its place for
  lazy-creation bookkeeping/queries; the live set comes from the cached table.
- Other open questions defaulted: hide the never-sampled template component; resend the VTable
  on gen bump; accept global-gen churn + unbounded growth (no eviction) for v1; batch metadata
  later.

Grounding signatures (all paths absolute under `libs/`):
- `db/src/vtable_stream.rs` — `handle_vtable_stream` (29), the realize loop's dynamic reject
  (117–125), `FieldTable` (185–237: `new`/`field`/`wait_ready`/`take`/`replace_pkt`/
  `notify_writers`), `RealTimeStage::next` (331), `AtomicBitVec` (483).
- `db/src/lib.rs` — `handle_fixed_stream` (1884–1943, the proven gen-watch rebuild),
  `insert_vtable` (247–271), the `Packet::Table` ingest arm (1215–1230), `DBSink::apply_value`
  (756–781), `State` (88–101) + `insert_component`/`insert_component_inner` (319–368),
  `with_state`/`with_state_mut` (133–141), `Component`/`push_buf` (600, 730),
  `ComponentSchema::new` (467), `DB.vtable_gen: AtomicCell<u64>` (75).
- `metor-proto/src/vtable.rs` — `for_each_field` (563), `expand_dynamic` (692), `apply` (796),
  `RealizedField` (361), `ElementKey` (323), `realize_fields(None)` (776).
- `metor-proto/src/types.rs` — `ComponentView::prim_type`/`shape`/`as_bytes` (336/304/408),
  `Table::sink` (842).
- `db/cpp/vtable.hpp` — the documented parse-only builder gap (`NOTE(WP2b)`, 21–29).

---

## 1. Purpose + the gen-watch pattern db already proves

`handle_fixed_stream` (`lib.rs:1884`) is the template the dynamic path mirrors. It is a
single per-tick loop, not a spawn-per-shard fan-out:

```text
let mut current_vtable_gen = db.vtable_gen.latest();          // 1890
loop {
    let vtable_gen = db.vtable_gen.latest();                  // 1901  poll the GLOBAL gen
    if vtable_gen != current_vtable_gen {                     // 1902  structure changed
        components = db.with_state(|s| s.visible_components());
        // re-synthesize + RESEND the VTable structure
        stream.send(VTableMsg { id, vtable }.with_request_id(req_id)) ...
        current_vtable_gen = vtable_gen;                      // 1908
    }
    table.clear();
    DBVisitor.populate_table(&components, &mut table, current_timestamp);  // 1911 re-fill values
    stream.send(table ...);                                   // 1938
    state.wait_for_tick(...).await;                           // 1940
}
```

Two moves: **resend structure only when `vtable_gen` bumps**; **re-populate values every
tick**. `vtable_gen` is the global `AtomicCell<u64>` bumped by `insert_vtable` (265) and by
the ingest arm when a never-before-seen time series is sunk (1226). Per the WP2b **decision**
we reuse this *same global gen* as the dynamic stream's rebuild trigger — a lazily created
keyed component bumps it exactly like any new series, so a subscribed dynamic stream notices
"membership changed" through the existing signal. (A per-frame membership gen is a noted
future refinement, §8.)

---

## 2. Ingest: lazy concrete-component creation

### 2.1 The hook

`DBSink::apply_value` (`lib.rs:756`) is where a sample meets a component:

```rust
let Some(component) = self.components.get(&component_id) else {
    return Err(Error::ComponentNotFound(component_id));   // 766–768
};
```

For a dynamic frame the concrete id (`processes.htop.pid`) is **not** registered up front
(only the template `processes.pid` is, §5). The first sample must *create* it. The view
carries everything `ComponentSchema` needs — no template reverse-lookup:

```rust
let schema = ComponentSchema::new(view.prim_type(), view.shape());   // types.rs 336 / 304
```

Idempotency is free: `insert_component_inner` (347–355) early-returns `Ok(())` when the id
already exists and only errors on a genuine `SchemaMismatch`. On creation we must bump
`vtable_gen` (so subscribers re-derive membership, §1) and set `sunk_new_time_series` — the
existing ingest arm already bumps the gen when `sunk_new_time_series` is set (1225–1227), so
lazy creation can ride that path or bump directly.

### 2.2 The borrow wrinkle and its resolution

Today the `Packet::Table` arm runs the sink under **`with_state` (read lock)** and `DBSink`
holds `components: &'a HashMap<…>` (an *immutable* borrow):

```rust
db.with_state(|state| {                                   // 1217  READ lock
    let mut sink = DBSink { components: &state.components, ... };
    table.sink(&state.vtable_registry, &mut sink)??;      // 1224
});
```

Lazy creation needs to **insert into `state.components`** — impossible under a read lock and
through a shared `&` borrow. A second, independent problem: `Table::sink` → `VTable::apply`
(`vtable.rs:796`) calls `sink.apply_value(rf.component_id, view, rf.timestamp)` and **drops
`rf.frame` and `rf.element`** on the floor. Membership tracking (§3) needs both. So the
dynamic ingest cannot go through `Decomponentize::apply_value` at all — it must iterate
`for_each_field` directly, where `frame`/`element` survive.

**Recommended resolution — clone the VTable out, single mutable pass.** Replace the
`with_state` + `Table::sink` call in the ingest arm with a `with_state_mut` block that drives
`for_each_field` over the table:

```rust
db.with_state_mut(|state| {
    // Decouple the vtable's borrow from `state` so the closure can hold `&mut state`.
    let vtable = state.vtable_registry.get(&table.id).ok_or(Error::VTableNotFound)?.clone();
    let received = Timestamp::now();
    let mut new_series = false;
    vtable.for_each_field(Some(buf), &mut |rf| {
        // get-or-create concrete component (idempotent; existing schema re-checked)
        if !state.components.contains_key(&rf.component_id) {
            let schema = ComponentSchema::new(rf.ty, rf.shape);
            state.insert_component(rf.component_id, schema, &self.path)?;   // mirrors insert_vtable
            if let (Some(frame), Some(key)) = (rf.frame, rf.element) {
                state.dynamic_members.record(frame, key, rf.component_id);  // §3
            }
            new_series = true;
        }
        let view = rf.view.expect("table provided");
        let ts = rf.timestamp.unwrap_or(received);
        state.components[&rf.component_id].push_buf(ts, view.as_bytes())?;  // 730
        Ok(())
    })?;
    if new_series { self.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst); }
    Ok::<_, Error>(())
})?;
```

Why this over the alternatives:
- *Why not stay on `with_state` + collect-then-insert?* You could pre-scan into a local
  `Vec<(ComponentId, ComponentSchema, frame, key)>`, drop the read lock, then `with_state_mut`
  to insert — but that doubles the realize walk and splits the push (existing components push
  under the read lock, new ones after re-acquiring the write lock), with an awkward window
  between. Not worth it.
- *Why not an interior-mutable `components` map?* That ripples a `RefCell`/lock into every
  `state.components` reader across the crate for one call site. Too invasive.
- *Why clone the VTable?* `registry.get(&id)` returns `&VTable` borrowing **all** of `state`,
  which collides with `&mut state` in the closure. Cloning the (small, op-list-sized) VTable
  breaks that borrow — and `handle_vtable_stream:48` and `handle_fixed_stream` already clone
  vtables per use, so the cost is precedented. **The per-sample clone is the one real cost; it
  is the headline open question (§8).** A clone-free variant (split-borrow `State` so
  `vtable_registry` and `components` are disjoint locals, or cache the cloned vtable per
  `table.id`) is a follow-up optimization.

**Locking/ordering note.** The ingest arm now takes the **write** lock for the whole table
(today it took the read lock). `push_buf` (730) writes the component's lock-free WAL
`Disruptor` and is unchanged; the write lock only newly guards `components`/`dynamic_members`
mutation. Streams read membership under `with_state` (read lock) so they serialize against
ingest exactly as `handle_fixed_stream`'s `visible_components()` snapshot does today.

---

## 3. Membership tracking

You cannot recover a dotted prefix from a hashed id, so to assemble a frame's trailer the db
must remember *which* element keys/indices are live and which concrete component ids they map
to. `RealizedField` gives us exactly the two facts to record this, at creation time:

- `rf.frame: Option<ComponentId>` — the enclosing frame's id (the `Op::Frame` tag, e.g.
  `ComponentId::new("imu")`; for the process example, whatever frame wraps the field).
- `rf.element: Option<ElementKey>` — `ElementKey::Index(u32)` for list elements,
  `ElementKey::Key(&str)` for map entries (`vtable.rs:323`). `None` for static fields.
- `rf.component_id` — the fully-qualified concrete id (`processes.htop.pid`).

`expand_dynamic` (`vtable.rs:692`) is what produces these: it pushes the prefix + key/index +
member name through `PathHasher` and emits one `RealizedField` per element-member, in
`members_start..members_end` template order (762).

### 3.1 Structure (new, on `State`)

```rust
// State (lib.rs:88)
dynamic_members: HashMap<ComponentId /* frame id */, FrameMembership>,

struct FrameMembership {
    /// Insertion-ordered element keys → that element's member component ids,
    /// recorded in template order as realization emits them.
    elements: IndexMap<ElementKeyOwned, Vec<ComponentId>>,
}
enum ElementKeyOwned { Index(u32), Key(String) }   // owned form of ElementKey
```

`record(frame, key, member_id)` appends `member_id` to `elements[key]` (creating the entry,
preserving first-seen order). Populated only on the lazy-creation branch in §2, so it grows
once per `(key, member)` rather than every tick.

### 3.2 How the stream consumes it

When `handle_vtable_stream`'s field plan reaches a `List`/`Map` op it has the
`RealizedDynamic { name (prefix), members, stride, value_offset, is_map }` and the field's
inherited frame id (from the `Op::Frame` passthrough). Keyed by that frame id it pulls
`FrameMembership.elements`, and for each `(key, member_ids)`:
1. emits a trailer **entry** at the element's stride slot (list: members back-to-back; map:
   `{key_off, key_len, <pad> value}` with the key bytes appended to the name pool);
2. fills each member's value from `state.components[member_id].time_series.latest()`;
3. the prefixed name (`processes.htop.pid`) is already the component id — its
   `ComponentMetadata` (emitted by the producer) resolves the human name; the stream needs no
   string reconstruction.

**Caveat (open question, §8):** keying by frame id is ambiguous if one frame owns *two*
dynamic fields with different prefixes — both prefixes' keys land under one frame entry. The
stream disambiguates by recomputing each member's expected id from *this* op's prefix +
templates (the same `PathHasher` walk `expand_dynamic` uses) and only emitting ids that exist
in `state.components`; foreign-prefix keys recompute to non-existent ids and drop out.
Cleaner long-term: key membership by `ComponentId::new(prefix)` instead of the frame id (but
the prefix string is consumed during realization and isn't on the leaf `RealizedField`, so
that needs a small enrichment of the walk).

---

## 4. Dynamic streaming — `DynamicFrameTable`

Replaces the reject at `vtable_stream.rs:117–125`. When the realize loop (54) hits
`RealizedOp::List(_) | RealizedOp::Map(_)` for a field, the stream is **dynamic** and uses a
`DynamicFrameTable` instead of (or alongside) `FieldTable`. `RealizedOp::Frame` already
passes through (112–116); `PathComponent` only appears inside member templates, never as a
top-level field's terminal, so it is reached via `expand_dynamic`, not this loop.

### 4.1 Why `FieldTable` cannot hold it

`FieldTable` (`vtable_stream.rs:185`) pre-allocates **one fixed-offset shard per field** in a
single `LenPacket`, gated by an `AtomicBitVec` of "field filled" flags (200), and ships the
*same* buffer every tick (`take`/`replace_pkt`, 223/228). A dynamic trailer is runtime-sized
and its member set changes, so a fixed shard table cannot express it.

### 4.2 The model — mirror `handle_fixed_stream`, not the shard fan-out

`DynamicFrameTable` runs a single per-tick rebuild loop like `handle_fixed_stream`
(1884–1943), allocating the packet's *fixed* region once and re-serializing the *trailer*
each tick into a growable `LenPacket`:

```text
let mut current_gen = db.vtable_gen.latest();
loop {
    let gen = db.vtable_gen.latest();
    if gen != current_gen {                         // structure (membership) may have changed
        snapshot = db.with_state(|s| s.dynamic_members.snapshot_for(frame_ids)
                                       .with_components(...));   // re-derive the live key set
        // re-derive entry-array shape for the new key set + RESEND the VTable
        // (dynamic ops + member templates), mirroring handle_fixed_stream:1902–1908
        stream.send(VTableMsg { id, vtable }.with_request_id(req_id)) ...;
        current_gen = gen;
    }
    pkt.clear();
    // (a) static fields: fixed region, written from each component's latest (or kept shard-style)
    // (b) dynamic trailer, rebuilt from `snapshot`:
    for each dynamic field:
        write slot { trailer_off, byte_len } into the fixed region;
        for (key, member_ids) in snapshot.elements (insertion order):
            push entry (list: values; map: {key_off,key_len, value}), reading each
            member via state.components[id].time_series.latest();   // non-blocking snapshot
        append key-name pool; back-patch trailer_off / byte_len / key_off.
    stream.send(pkt.with_request_id(req_id)) ...;
    wait_for_tick(...).await;                        // same cadence source as the static stream
}
```

Key points spelled out:

- **Per-element latest values.** Read `component.time_series.latest()` per member each tick —
  the same non-blocking snapshot `handle_fixed_stream` uses (cf. `lib.rs:1865`) — **not** the
  blocking `RealTimeStage`/`Reader::next().await` (`vtable_stream.rs:341`). The realtime
  shard model gates "ready" on *every* shard filling via `AtomicBitVec::all_set` (216), which
  cannot express a membership that grows and shrinks between ticks. §8.6 of `vtable-dynamic.md`
  is explicit: for the dynamic portion, "all shards filled" is replaced by "rebuild on tick or
  on any contributing-component update."

- **Trailer serialization into a growable `LenPacket`.** The `{trailer_off, byte_len}` slot
  sits in the fixed region (8 bytes, written like a static field). The entry array + name pool
  are *appended* per tick via `LenPacket` `push`/`extend_aligned` (as `FieldTable::new` builds
  its zeroed body, 191–193, but length-varying). This is **not** the fixed-shard `FieldTable`
  — that struct stays for fully-static streams. Layout matches what `expand_dynamic` parses
  (`vtable.rs:724–766`): list entries are `stride`-spaced member blocks; map entries are
  `{key_off:u32, key_len:u32, <pad> value}` with key bytes in the pool, addressed table-
  absolute. Nested dynamics recurse the same rebuild into the same packet (write inner slot,
  append inner trailer).

- **Static + dynamic mix.** Recommended for v1: a VTable containing any `List`/`Map`
  re-serializes the *whole* packet each tick (drop the shard fast-path for that stream),
  exactly like `handle_fixed_stream` re-fills `table` every tick. Keeping the static part
  shard-style while only the dynamic part rebuilds is a possible optimization but complicates
  readiness (two gates, two writers into one buffer); flagged in §8.

- **Where the gen check sits.** Top of the loop, identical placement to
  `handle_fixed_stream:1901–1909`: poll `db.vtable_gen.latest()`, and on bump re-derive the
  membership snapshot + resend the VTable, then fall through to the per-tick trailer rebuild.
  Membership churn that *doesn't* change the component set (a count change carried purely in
  the slot's `byte_len`) is parsed by the consumer from the data; the gen-bump resend keeps
  the consumer's structural/metadata view in sync when keyed components are lazily created.

### 4.3 Dispatch site

In the realize loop (`vtable_stream.rs:54`), the `RealizedOp::List(_) | RealizedOp::Map(_)`
arm (currently 117–125) stops returning `InvalidOp` and instead marks the stream dynamic
(records the dynamic field's plan: prefix, member templates, stride, frame id) so the outer
function builds a `DynamicFrameTable` rather than the static `FieldTable` + `handle_plan`
fan-out (156, 168–175).

---

## 5. `insert_vtable` — template registration (confirm as-is)

`insert_vtable` (`lib.rs:247`) already does the right thing and needs no structural change:
`realize_fields(None)` runs `expand_dynamic`'s **schema/registration mode** (`vtable.rs:707`),
emitting each member *template* once — `view = None`, `frame = Some`, `element = None`,
`component_id = ComponentId::new("processes.pid")` (the prefix + member name, no key) — so its
`ty`/`shape` register up front via `insert_component` (264). Concrete keyed components
(`processes.htop.pid`) are created lazily (§2).

What template registration buys: the `ty`/`shape` carrier for the eventual concrete
components, and a stable schema the stream can consult. Note the template component
(`processes.pid`) never receives samples — it should likely be marked **hidden** (like LoD
components, via `is_component_hidden`, `lib.rs:386`) so it doesn't pollute listings or
`visible_components()`. Flagged in §8.

---

## 6. C++ gap; test fixture

**C++ (out of scope).** `db/cpp/vtable.hpp` documents the gap (`NOTE(WP2b)`, 21–29): the
generated `Op`/`ElementFields` cover dynamic ops on the *parse* side, but the hand-written
builder deliberately does **not** expose `Frame`/`List`/`Map`/`PathComponent` — C++ producers
cannot *emit* dynamic frames. This WP keeps that parse-only gap as-is; emitting dynamic frames
from C++ stays out of scope.

**Test fixture.** End-to-end exercise needs a dynamic-frame producer pushing into db. A WP3
`FrameList`/`FrameMap`-bearing system is the real source, but a hand-built table suffices for
a unit/integration test now — reuse the exact shape proven in `vtable.rs`'s `test_dynamic_list`
(1499–1530):

1. Build `vtable([raw_field(8, 8, timestamp(raw_table(0,8), list("processes",
   process_members(), 16)))])` and `insert_vtable` it (registers `processes.pid` /
   `processes.cpu_usage` templates).
2. Hand-assemble a table buffer with the trailer (timestamp @0, slot `{trailer_off, byte_len}`
   @8, two 16-byte elements) and feed it as `Packet::Table`. Assert `state.components` now has
   `processes.0.pid` / `processes.0.cpu_usage` / `processes.1.*` and `dynamic_members[frame]`
   lists indices 0,1.
3. Subscribe `handle_vtable_stream` with the same VTable; assert the streamed packet
   round-trips back through `apply` to the same concrete ids/values, and that pushing a table
   with a *new* key bumps `vtable_gen` and the next streamed packet includes the new element.

---

## 7. Reused vs new

**Reused unchanged:**
- `DB.vtable_gen: AtomicCell<u64>` gen-watch (`lib.rs:75`, 1890/1901) — the dynamic rebuild
  trigger (per decision).
- `handle_fixed_stream`'s gen-watch + per-tick re-populate loop (1884–1943) — the
  `DynamicFrameTable` template.
- `for_each_field` / `expand_dynamic` / `realize_fields(None)` / `RealizedField`
  (ty/shape/view/frame/element) (`vtable.rs:563/692/776/361`).
- `insert_component` idempotent schema check (`lib.rs:347`), `ComponentSchema::new` (467),
  `Component::push_buf` (730), `time_series.latest()` (1865), `LenPacket`, `PathHasher`.
- `DBSink` (756) — still used for fully-static streams.
- `FieldTable` (`vtable_stream.rs:185`) — still used for fully-static streams.

**New:**
- `State.dynamic_members: HashMap<frame_id, FrameMembership>` + `ElementKeyOwned` (`lib.rs`).
- Lazy-creation + membership-record branch replacing the `Table::sink` call in the ingest arm
  (1215–1230), driven by `for_each_field` under `with_state_mut` with a cloned VTable.
- `DynamicFrameTable` + per-tick trailer serializer in `vtable_stream.rs`, dispatched from the
  realize loop's `List`/`Map` arm (replacing 117–125).

---

## 8. Open questions / risks for the reviewer

1. **Borrow-wrinkle choice (headline).** Recommended: `with_state_mut` + **clone the VTable**
   out of the registry per ingested table, single `for_each_field` pass. The cost is a
   per-sample vtable clone. Acceptable, or should we (a) split-borrow `State` so
   `vtable_registry`/`components` are disjoint, or (b) cache the cloned VTable per `table.id`?
   Cloning is the simplest correct option and is precedented (`vtable_stream.rs:48`).
2. **Static shard-style vs full re-serialize.** Recommended: any VTable with a `List`/`Map`
   re-serializes the whole packet each tick (simple, matches `handle_fixed_stream`). Keeping
   the static part shard-style while only the dynamic part rebuilds saves a copy but needs two
   readiness gates writing one buffer. Worth it for v1?
3. **Global-gen churn under fast key turnover.** Reusing the *global* `vtable_gen` means every
   lazily created keyed component (every new PID-as-key) bumps it, forcing every subscribed
   stream — even static ones — to re-snapshot and resend. Fast churn = gen thrash. The noted
   per-frame membership gen would scope this; out of scope for v1 but the reviewer should
   accept the churn cost.
4. **Stale membership / no liveness with grow-only `dynamic_members`.** §8.6 says a vanished
   process "drops out of the next packet," but with grow-only membership + global gen there is
   no per-tick liveness signal, so a dead key's last value lingers in the trailer until
   evicted. Do we need per-tick liveness (the producer table is authoritative) or is
   last-value-sticky acceptable for v1?
5. **Unbounded membership growth (no eviction, v1 decision).** Churning keys grow
   `dynamic_members` + `components` without bound. Confirm no TTL/"hidden after N idle ticks"
   for v1 and that this is a tracked follow-up.
6. **Membership key: frame id vs prefix.** Frame id is ambiguous when one frame owns multiple
   dynamic fields with different prefixes (§3.2). The existence-filter workaround works but
   over-iterates. Prefer enriching the realize walk to surface the prefix and keying by
   `ComponentId::new(prefix)`?
7. **Template component visibility.** `processes.pid` is registered but never sampled. Mark it
   hidden (`is_component_hidden`) so it stays out of listings/`visible_components()`?
8. **VTable resend vs slot-count parsing.** A consumer parses the live element count from the
   slot's `byte_len` every packet, so a membership *count* change needs no resend. The
   gen-bump resend (mirroring `handle_fixed_stream`) is belt-and-suspenders + keeps metadata in
   sync as keyed components are created. Confirm we want the resend on every gen bump.
9. **Metadata burst.** Lazy creation of many keys at once (process list on startup) bursts
   `ComponentMetadata` emission. Batch/rate-limit (cross-refs `vtable-dynamic.md` §9.8)?
