# Ring Buffer (`metor-fsw-ring`)

The lock-free, shared-memory ring buffer is the transport over which
`metor-fsw-2` systems exchange data. Every system writes its output frames into
one or more ring buffers and reads its inputs from *views* into other systems'
buffers (cyclic systems view upstream outputs directly; async systems read
private input buffers the coordinator fills for them — see `DESIGN.md`). A system
may run as a dlopen'd library inside the coordinator's process *or* as a separate
process, so the buffer works identically in-process and across a process
boundary: it lives in one contiguous region addressed through a fixed
`#[repr(C)]` layout and contains no process-local pointers.

The crate generalizes the `metor-db` disruptor (`libs/db/src/disruptor.rs`)
along one axis — **shared-memory residence** (the disruptor uses heap
`Arc`/`Box` structures). Its unsafe core reuses the disruptor's proven
techniques: absolute monotonic `u64` cursors with `phys = abs & mask`, a
`committed` Release/Acquire publication handshake, a `high_water_mark` wrap
gap, and the slowest-reader in-use check that makes the writer backpressure
instead of overwrite.

## 1. Losslessness & the grant model

The buffer is **lossless**, unconditionally. The writer honors the disruptor's
in-use check on every write: `Writer::try_write` returns
`Err(WriteError::WouldBlock)` rather than overwrite bytes the slowest active
reader has not consumed, and the async `Writer::write` suspends until a reader
frees space. This is the read-soundness contract every view relies on — because
the writer can never scribble a record a reader still owns, reads are
**tear-free, zero-copy borrows**:

- `View::try_read()` returns an `Option<ReadGrant>` borrowing the next record's
  payload in place. Dropping the grant *consumes* the record: the view's cursor
  advances past it and a waiting writer is woken.
- `View::try_latest()` is the latest-wins read: it consumes every older unread
  record and returns a grant on the **newest** committed one, with the cursor
  parked at that record's *start* so it stays pinned (§6).
- `try_read_into`/`read_into` remain as copy conveniences for callers that must
  own the bytes.

An earlier layout (version 1) also offered a writer-chosen **overwrite** mode,
where the writer never blocked and lapped readers detected the loss via a
seqlock recheck. That whole policy axis was deleted: fsw-2's ports get
latest-wins semantics from `try_latest` on a lossless ring instead, which keeps
the data path free of atomic-per-byte copies and read revalidation. Layout
`version` was bumped to 2 (§2). The earlier `docs/design-ring-safety.md`
analysis predates this removal and describes the two-mode design.

## 2. Memory layout

One contiguous region (heap-allocated, mmap'd, or borrowed — all carried by the
one erased `Backing` struct, §10), four logical zones: a **header** (immutable after creation), a **control block**
(the live writer atomics), a **reader-cursor table** (a fixed array of slots),
and the **data region**. Multi-byte fields are stored in **native byte order**;
the `arch_tag` handshake (below) rejects regions written by a different pointer
width or endianness on attach, so cross-endian reinterpretation never happens.
Control words and reader slots are padded to 64-byte cache lines to avoid false
sharing.

In code, the layout is realized as three `#[repr(C)]` structs — `RegionHeader`
(a zerocopy type; its `IntoBytes` derive is a compile-time no-padding proof),
`Control`, and `ReaderSlot` (all-`AtomicU64` + pad, pointer-cast rather than
parsed) — with a `const` block of `offset_of!`/`size_of` assertions pinning
every offset in the tables below, so layout drift fails the build rather than
changing the wire format.

### What is shared-memory-resident vs process-local

- **In the region (shared):** the header, the control-block atomics, the
  reader-cursor table, and the data bytes. Addressed only by fixed offset.
- **In the process-local handle (never shared):** the region's base pointer and
  drop fn (`Backing`), the cached `capacity`/`mask`/`max_readers`/offsets, the
  reader-slot *index* a `View` holds, and the async `WakeSource`/`WakeSink`.
  These are reconstructed on attach (`attach` reads the geometry back out
  of the header), never stored in the region.

### Header (cache line 0, bytes `0x00..0x40`)

| Offset | Size | Type  | Field                 | Notes |
|--------|------|-------|-----------------------|-------|
| `0x00` | 4    | `u32` | `magic`               | `b"MFR1"` read as a native `u32`; validated on attach |
| `0x04` | 2    | `u16` | `version`             | layout version (currently `3`) |
| `0x06` | 2    | `u16` | `flags`               | always `0` in v3; bit 0 `FLAG_WAKE_SHARED` reserved for a future shared wake word |
| `0x08` | 8    | `u64` | `capacity`            | data-region size in bytes; a power of two |
| `0x10` | 8    | `u64` | `data_offset`         | region-relative offset of data byte 0 |
| `0x18` | 4    | `u32` | `max_readers`         | number of slots in the reader table |
| `0x1C` | 4    | `u32` | `reader_table_offset` | region-relative offset of slot 0 |
| `0x20` | 8    | `u64` | `total_size`          | full region size in bytes |
| `0x28` | 8    | `u64` | `arch_tag`            | pointer-width + endianness tag; validated on attach |
| `0x30` | 16   | —     | pad                   | to 64 B |

`arch_tag` is `((0x0102_0304u32 as u64) << 32) | size_of::<usize>()`. Its byte
pattern differs across endianness, and the low word carries the pointer width, so
a single equality check on attach rejects an incompatible producer
(`AttachError::ArchMismatch`).

### Control block (cache line 1, bytes `0x40..0x80`)

| Offset | Size | Type        | Field             | Notes |
|--------|------|-------------|-------------------|-------|
| `0x40` | 8    | `AtomicU64` | `committed`       | absolute bytes published; Release by the writer, Acquire by readers |
| `0x48` | 8    | `AtomicU64` | `high_water_mark` | absolute end of valid data before a pending wrap gap; `HWM_NONE = u64::MAX` means no gap |
| `0x50` | 8    | `AtomicU64` | `wake_word`       | reserved for a future cross-process futex/eventfd |
| `0x58` | 8    | `AtomicU64` | `writer_claim`    | single-writer enforcement: `0` = free, else the claimant's process-id tag (§4) |
| `0x60` | 32   | —           | pad               | to the next cache line |

> **Version-2 layout note.** Removing the overwrite mode also removed the
> `reserved_end` seqlock write-reservation word from the control block, shifting
> `wake_word` and `writer_claim` down one word each, and retired the
> `FLAG_LOSSLESS` flags bit (flags are now always `0`). `version` was bumped
> `1 → 2` so a stale mapping is rejected with `AttachError::BadVersion` —
> regions are ephemeral IPC state, not archives, so stale dev regions are simply
> recreated.
>
> **Version-3 layout note.** Cross-process systems made dead-peer cleanup real:
> each reader slot gained an `owner` word (carved from its pad) and the writer
> claim stores the claimant's pid instead of `1`, both stamped at claim time,
> feeding the `unsafe fn reclaim_owner(pid)` sweep described in §7.

`committed`, `high_water_mark`, and the wrap-gap mechanism are semantically
identical to the disruptor's `WriteHead`. There is **no** in-region mutex: there
is a single writer per buffer (§4), and a mutex would not be shared-memory-safe
across processes anyway. The header + control block together are `HEADER_SIZE =
0x80` bytes; the reader table starts immediately after.

### Reader-cursor table (starts at `reader_table_offset = 0x80`)

`max_readers` slots, each a 64-byte cache line (`READER_SLOT_SIZE = 64`):

| Slot offset | Size | Type        | Field    | Notes |
|-------------|------|-------------|----------|-------|
| `+0x00`     | 8    | `AtomicU64` | `cursor` | absolute read head, or `FREE_SLOT = u64::MAX` when unclaimed |
| `+0x08`     | 8    | `AtomicU64` | `epoch`  | generation bumped on claim and by reclaim; ABA-safe reuse hook |
| `+0x10`     | 8    | `AtomicU64` | `owner`  | the claiming process's id, for `reclaim_owner` (§7); diagnostic, never synchronization |
| `+0x18`     | 40   | —           | pad      | to 64 B |

### Data region (starts at `data_offset`, length `capacity`)

Raw bytes framed into **8-byte-aligned records** so payloads can be mapped as
`repr(C)` frames in place. Each record is:

```
[ len: u32 ][ _pad: u32 ][ payload: len bytes ][ tail_pad: round_up8(len) - len ]
└──── 8-byte record header ────┘
```

The 8-byte header (a `u32` length plus a `u32` pad word — together stored as one
`u64` whose low 32 bits are the length) puts the payload at an 8-byte boundary,
and the tail pad rounds the whole record up to a multiple of 8. The total is
`frame_len(len) = 8 + round_up8(len)`, always a multiple of 8. Because
`data_offset` is 64-byte aligned (header + control + table are all cache-line
multiples) and every record length is a multiple of 8, every record *start* is
8-byte aligned — and stays so across the wrap, because `capacity` is a power of
two and the wrap gap (`capacity - phys`) is therefore also a multiple of 8. This
is why payloads are mappable in place with no realignment step.

A record never straddles the wrap: if it would not fit contiguously in the tail,
the writer leaves a gap from the current physical offset to the end of the data
region (recording the boundary in `high_water_mark`) and writes the record at
offset 0 of the next lap. The `len` prefix makes records self-describing, so a
reader recovers boundaries with no external metadata.

```
region:
  0x00  ┌─────────────── header (64 B) ───────────────┐
  0x40  ├─────────────── control block (64 B) ────────┤
  0x80  ├─ reader slot 0 (64 B) ─┬─ slot 1 ─┬─ … ──────┤   max_readers slots
        ├──────────────── data region (capacity B) ────┤
        │ [hdr|payload|pad][hdr|payload|pad]…[gap][hdr… │   each record 8-aligned
        └──────────────────────────────────────────────┘
```

`frame_len` is public so buffer-sizing callers (fsw-2 system ports) can size a
ring from a frame's `MAX_SIZE` without re-deriving the header rule.

## 3. Cursor model & the backpressure invariant

All cursors are `u64` absolute, monotonically increasing byte counts; `phys = abs
& mask` where `mask = capacity - 1` (capacity is a power of two). The disruptor's
invariant carries over: a reader cursor is never ahead of `committed`, and the
in-flight (unread) bytes for a reader at `r` are `committed - r`. Absolute
counters make "caught up" (`r >= committed`) unambiguous and survive the physical
wrap. The single 16-EiB `u64` wrap at `committed` is not a concern in practice.

**The invariant.** Before every commit the writer proves

```
committed.wrapping_sub(slowest) + gap + rec <= capacity
```

where `slowest` is the minimum cursor over all registered readers (or
`committed` itself when there are none — an unread ring never blocks the
writer). Since every reader cursor is at or above `slowest`, this maintains
`committed - r <= capacity` for every registered reader at all times: the
physical slot of every byte in `[r, committed)` has not been reused, so every
unread record is intact. This is the theorem that makes borrowed reads (§5) and
the `try_latest` pin (§6) sound — the writer *cannot lap a reader*, ever.
Wrap-gap bytes consume absolute cursor space just like real bytes (the writer
advances `committed` across them), which is why the check charges `gap + rec`,
not `rec` alone.

**Worked example** (`capacity = 64`, 16-byte payloads → 24-byte records: 8-byte
header + 16-byte payload, already a multiple of 8 so no tail pad):

- The writer has published a long stream; `committed = 200` (`phys = 200 & 63 =
  8`). The slowest reader sits at `r = 160`: unread bytes `200 - 160 = 40`, and
  a 24-byte record fits (`40 + 24 = 64 ≤ 64`) — the writer publishes `200..224`.
- If the slowest reader were instead at `r = 136` (64 unread bytes — a full
  buffer), `64 + 24 > 64` → `try_write` returns `WouldBlock` and the async
  `write` suspends until that reader consumes a record.
- **Wrap gap.** With the writer at `committed = 240` (`phys = 240 & 63 = 48`), a
  24-byte record will not fit in the 16 tail bytes (`48 + 24 > 64`), so the
  writer sets `high_water_mark = 240`, leaves bytes `48..64` as a 16-byte gap,
  and writes the record at absolute `256..280` (`phys 0..24`) — provided the
  in-use check passes for the full `gap + rec = 40` bytes. A reader whose
  cursor reaches `240` exactly skips to `256`, the next lap boundary (`(r &
  !mask) + capacity`). The gap is a multiple of 8, so record-start alignment
  survives the wrap.

## 4. Write path

There is exactly one writer per buffer (each system owns its output buffer), and
the ring **enforces** it: `RingBuffer::writer()` CAS-claims the in-region
`writer_claim` word (`0 →` the claimant's pid, Acquire success / Relaxed
failure) and returns
`Err(WriterClaimed)` if a live writer already exists — the claim lives in the
region, so enforcement is cross-handle and cross-process. `Writer::drop` frees
the claim with a Release store, handing the whole region state to the next
claimer's Acquire CAS. A crashed process leaks its claim; the supervising host
reclaims it with `unsafe fn reclaim_owner(pid)` (or the blunter
`force_release_writer()`), asserting the claiming process is truly dead. The writer is the sole mutator of
`committed` and `high_water_mark`, and reads `committed` Relaxed.
`Writer::try_write(bytes)`:

1. `rec = frame_len(bytes.len())`. If `rec > capacity` →
   `Err(WriteError::InsufficientCapacity)`.
2. `c = committed` (Relaxed; sole writer). `reserve(c, rec)` computes the wrap:
   `phys = c & mask`; if `phys + rec > capacity` then `gap = capacity - phys` (a
   multiple of 8) and `start_abs = c + gap`, else `gap = 0` and `start_abs = c`.
3. **In-use check:** `fits(c, gap + rec)`: with `slowest =
   slowest_active_cursor()` (the min cursor over non-free slots, or `c` if no
   readers; the scan opens with a `SeqCst` fence — see §7), the write fits iff
   `c.wrapping_sub(slowest) + gap + rec <= capacity`. If it does not →
   `Err(WriteError::WouldBlock)`.
4. **Commit** (`commit(c, start_abs, gap, bytes)`):
   - Write the record header + payload at `phys' = start_abs & mask`
     (`write_record`) with **plain stores** — race-free because the in-use
     check guarantees no reader is inside these bytes, and publication
     happens-before every read via `committed` (exactly the disruptor's write
     path).
   - If `gap > 0`, store `high_water_mark = c` (Release) so readers skip the gap.
   - `committed.store(start_abs + rec, Release)`, then `data.notify()`.

The Release store of `committed` hands the freshly written bytes (and the
`high_water_mark` store) to readers, which Acquire-load `committed` before
touching any data byte — the same happens-before edge the disruptor relies on.

`Writer::write(bytes)` is the async form: it loops, and when the write does not
fit it awaits the **space-available** sink (`space.wait_until(...)`) with a
predicate that re-evaluates the in-use check, then retries. A reader frees space
whenever it advances — on grant drop, gap skip, copy-read, or view drop.

## 5. Read path & views

A `View` owns one reader-table slot (its index lives in the process-local
handle) and reads whole records. `locate()` finds the next readable record:

1. `r = slot.cursor` (Acquire), `c = committed` (Acquire), then `hwm =
   high_water_mark` (Acquire). The load order is load-bearing: seeing a
   post-wrap `committed` implies seeing that wrap's `hwm` store (it is
   sequenced before the `committed` Release), so a reader whose next bytes are
   a gap always sees the marker before it could misread stale gap bytes as a
   record header. (hwm-first would allow fresh `committed` + stale `hwm`.)
2. **Skip gap:** if `r == hwm`, store the cursor forward to the next lap
   boundary `(r & !mask) + capacity` (Release), notify the space sink, and
   retry from step 1. The skip never accepts data; the next iteration re-runs
   against a fresh `committed`. A given `r` can match at most one gap ever
   (`hwm` values strictly increase), so a stale `hwm` cannot re-trigger.
3. If `r >= c` → `Ok(None)` (caught up). Checked after the gap skip because
   the skip can transiently park the cursor *ahead* of `committed` — the
   wrap's `hwm` publishes before its `committed`.
4. Read `len` from the record header at `phys = r & mask` (`read_len` returns
   the raw `u32` widened to `u64`).
5. **Straddle/length guard, u64 math:** if `len > capacity - 8 - phys` (the
   `phys + rec > capacity` predicate rewritten overflow-free), the length field
   is not a real record's. A real record never straddles the wrap, and the
   writer can never lap a reader (§3), so no in-crate behavior can explain it →
   `Err(ReadError::Corrupt)` (external corruption of a shared mapping — degrade
   to an error, never an out-of-bounds borrow). Validating in `u64` *before*
   any `usize` conversion matters on 32-bit targets, where `frame_len(garbage)`
   would wrap `usize` and defeat the check itself.
6. Return the located record (`r`, `phys`, `len`, `rec`) — every offset now
   proven in-bounds, so the downstream copy/borrow is bounded even against a
   corrupted length.

**Borrows are sound.** The writer's in-use check provably keeps it from
touching the bytes of a record a reader still owns — the same guarantee that
lets the disruptor hand out a borrowed `&[u8]`. The Acquire load of `committed`
establishes happens-before against the writer's Release, and the in-use check
is the happens-after edge that keeps the bytes stable until the reader
advances. There is no lap to detect, no seqlock to recheck, and no revalidation
after the borrow.

`View::try_read()` therefore returns `Ok(Some(ReadGrant))` for the next record
(`Ok(None)` when caught up). The grant derefs to the payload bytes in place
(zero copy) and mutably borrows the `View`, so at most one grant is live per
view; while it lives, the view's cursor stays at or before the record start, so
the in-use check protects the borrowed bytes for the grant's whole lifetime. On
drop, the grant stores the cursor to the record's *end* (Release) and notifies
the space sink — consuming the record and waking a blocked writer.
`View::read()` is the async, event-driven form: it awaits the
**data-available** sink until `locate` finds a record, then borrows it. (The
await loop and the borrow are split for borrowck — a grant taken inside the
wait loop would pin the `self` borrow across every iteration; nothing can
consume between the successful locate and the borrow because the view is `&mut
self`.)

`View::try_read_into(buf)` / `View::read_into(buf)` are the copying
conveniences for callers that must own the bytes: a plain
`copy_nonoverlapping` of the payload, then advance. `try_read_into` returns
`Ok(true)` (a record was read) or `Ok(false)` (caught up). Prefer the grant
forms — the copy buys nothing in safety, only ownership.

`cursor()` / `committed()` accessors let the coordinator or a monitor inspect
lag without performing a read.

## 6. Latest-wins reads (`try_latest`)

`View::try_latest()` is the mode-1 overwrite semantics rebuilt as a *read-side*
policy on the lossless ring: "give me the newest value, I don't care about the
history." It:

1. Runs `locate()` in a loop. For each located record, it re-snapshots
   `committed` (Acquire) and asks: is anything committed past this record's
   end? 
2. **Older record** → consume it (`advance` past it, Release + space notify),
   freeing its bytes for the writer, and loop. A racing commit just means one
   more skip iteration.
3. **Newest record** (`end >= committed`) → return a `ReadGrant` on it, with
   the grant's `end_abs` set to the record's **start**, not its end.

That last detail is the pin. Dropping a `try_read` grant advances the cursor
past the record; dropping a `try_latest` grant re-stores the record's *start*.
The cursor therefore never moves past the newest record, and by the
backpressure invariant (§3) the writer counts its bytes as unread and can never
reclaim them — `try_write` returns `WouldBlock` before it would reuse the
pinned record's physical slot. Consequences:

- A later `try_latest` with no new data locates the same record and re-serves
  it: the caller always has *a* value once the first record lands, which is
  exactly what a cyclic consumer sampling a state vector wants.
- The borrow is stable for the same reason every borrow is (§5): the pinned
  cursor is the in-use check's floor.
- The pin bounds the writer: a consumer that only ever calls `try_latest` holds
  the writer to at most one buffer of progress past the pinned record. Size the
  ring so `capacity` comfortably exceeds one record, or interleave `try_read`
  to release history. (Dropping the view releases everything.)

`try_latest` returns `Ok(None)` only before the first record is committed — or
after the stream was fully consumed through `try_read`/`try_read_into`, which
advance past the newest record and leave nothing behind to re-serve.

## 7. Reader registration

Readers register in a **fixed-size reader table** (§2): `max_readers` slots at a
known offset, addressed by index. A heap Treiber list of `Arc<ReadNode>` like the
disruptor's cannot live in shared memory — it stores process-local heap pointers
and its `Arc` strong counts are meaningless cross-process — so the table mirrors
only the disruptor's *slot-reuse-by-CAS* idea over a flat array:

- **`RingBuffer::view(data, space)`:** Acquire-load `committed` as the start
  cursor (so a fresh view only sees data committed from now on — same rule as
  `Disruptor::reader`). Scan slots `0..max_readers`, CAS each `cursor` from
  `FREE_SLOT` to `start` (AcqRel success — Acquire pairs with `View::drop`'s
  Release for the slot-state handoff, Release publishes the claim; Relaxed
  failure). On success, bump `epoch` (`fetch_add(1, Release)`) and run the
  registration handshake below, then return a `View` holding that slot index.
  If no slot is free → `Err(FullReaderTable)`.
- **Registration handshake.** The fresh claim must be *provably visible* to the
  writer's `fits()` scan before the view is returned — until it is, the in-use
  check is vacuous for this reader and the writer could write past
  `start + capacity`, after which the borrow path would hand out overwritten
  bytes. Release/Acquire alone cannot prove it — reader
  (`store cursor; load committed`) vs. writer (`store committed; load cursor`)
  is Dekker's pattern, where both sides may read the older values (StoreLoad
  reordering). So `view()` loops: `fence(SeqCst)`, re-load `committed`
  (Acquire); if it moved, advance the claim to the new edge (a semantic no-op
  for a fresh view — it only sees data from now on anyway) and repeat, until
  `committed` is *stable* across the fence. The pairing `fence(SeqCst)` sits at
  the top of `slowest_active_cursor()`: in the fence total order, either the
  writer's scan is later and must observe the claim, or the reader's recheck is
  later and must observe that writer's `committed` — a stable `committed`
  therefore proves every unseen write was bounded by some other cursor at or
  below ours. The loop converges in 1–2 iterations (each extra one needs a
  commit inside a ~3-instruction window) and is deliberately unbounded —
  registration is a cold path.
- **`View::drop`:** `cursor.store(FREE_SLOT, Release)` — a single wait-free store
  that frees the slot for reuse.
- **`RingBuffer::reader_count()`** scans the table for non-free cursors.

In the two-mode design the handshake ran only on lossless buffers; it now
always runs — every buffer's reads are borrows, so every registration needs it.

Trade-offs versus the linked list: the maximum number of concurrent readers is
fixed at creation and registration is an `O(max_readers)` scan, but there is no
allocation, no `Arc`, no pointers, and the whole structure is addressable by
offset — exactly what shared memory requires. The `epoch` word guards against ABA
on slot reuse.

**Dead-owner reclamation.** Every claim — reader slots and the writer word —
is stamped with the claiming process's id, and `unsafe fn reclaim_owner(pid)`
frees everything a dead process left behind: each matching reader slot (epoch
bumped first, per the reserved discipline) and the writer claim if that pid
holds it. Without it, a killed peer's pinned cursor would backpressure the
writer forever. The safety contract is that the owner is dead (exited and
reaped, so none of its stores race) and that `pid` does not alias a live
claimant through pid reuse — reclaim promptly after reaping. The process-system
host (`docs/process-systems.md` §6) is the caller.

## 8. Async wake

Async systems are woken across two directions, both expressed through one trait
pair kept **out of the shared region**:

```rust
/// Signals that progress was made (new data committed, or space freed).
pub trait WakeSource {
    fn notify(&self);
}

/// Awaits progress: completes once `ready()` returns true. Implementations must
/// avoid lost wakeups by re-checking `ready` after arming.
pub trait WakeSink {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F);
}
```

- **Data-available** (writer → readers, on commit): the `Writer` holds a
  `WakeSource` (`data`) it `notify()`s after publishing; each async `View` holds
  a `WakeSink` (`data`) its `read` / `read_into` awaits.
- **Space-available** (readers → writer, when a reader advances): each `View`
  holds a `WakeSource` (`space`) it `notify()`s on advance / gap-skip / grant
  drop; the `Writer` holds a `WakeSink` (`space`) its async `write` awaits.

The predicate form (`wait_until(ready)`) lets the sink re-check the condition
after arming, closing the lost-wakeup window.

Implementations:

- **`NoWake`** — a no-op source/sink. The `try_*` paths never touch the wake
  hooks, so this is the right choice for synchronous consumers (the coordinator
  polling cyclic inputs once per cycle, and the Miri tests). Under `NoWake` the
  async paths degenerate to caller-driven polling.
- **`Notifier`** (behind the `async` feature) — backed by a `stellarator`
  `WaitQueue`, shared by clone between the writer and views for one direction.

Beside the trait pair, the crate ships the **`wake` module** (behind the
`futex` feature): free functions `wait`/`wait_timeout`/`wake_one`/`wake_all`
over a shared `AtomicU32`, implemented as a shared-memory futex (plain
`FUTEX_WAIT` on Linux, `os_sync_wait_on_address` + `SHARED` on macOS 14.4+).
It is not a `WakeSource`/`WakeSink` and no ring endpoint uses it; it is the
cross-process wake primitive the fsw-2 control block builds its step doorbell
on (`docs/process-systems.md` §2, including why the `atomic-wait` crate —
process-private on every platform — cannot serve). The per-ring `wake_word`
stays reserved (§14).

## 9. Public API surface

```rust
pub const fn round_up8(n: usize) -> usize;
pub const fn frame_len(payload_len: usize) -> usize; // 8 + round_up8(payload_len)

pub struct Config {
    pub capacity: usize,    // data-region bytes; power of two
    pub max_readers: usize, // reader-table slots (over-provision; see §11)
}

pub struct Backing { /* base, len, ctx, drop_fn — owned, type-erased region storage */ }

impl Backing {
    pub fn heap(size: usize) -> Self;   // zeroed leaked Box<[Word]>; Drop frees it
    /// # Safety: `base..base+len` satisfies the region contract (§10), outlives
    /// every Writer/View produced over it, and is not concurrently torn down.
    pub unsafe fn raw(base: *mut u8, len: usize) -> Self;   // non-owning: drop_fn None
    /// # Safety: if Some, `drop_fn` must be sound to call exactly once, from any
    /// thread, with exactly `(ctx, base, len)` — the only release of those resources.
    pub unsafe fn from_raw_parts(base: *mut u8, len: usize, ctx: *mut (),
        drop_fn: Option<unsafe fn(*mut (), *mut u8, usize)>) -> Self;
    pub fn base(&self) -> *mut u8;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

pub struct RingBuffer { /* Arc<Inner>: backing + cached geometry; Clone */ }

impl RingBuffer {
    pub fn create_in_memory(cfg: Config) -> Self;            // zeroes + writes header
    #[cfg(feature = "mmap")]
    pub fn create_mmap(path: &Path, cfg: Config) -> std::io::Result<Self>;
    /// # Safety: caller asserts `path` is a compatible region, not being torn down.
    #[cfg(feature = "mmap")]
    pub unsafe fn attach_mmap(path: &Path) -> std::io::Result<Self>;
    /// # Safety: `base..base+len` is a live, header-valid region outliving all
    /// writers/views produced from it and not torn down concurrently.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError>;
    /// # Safety: `backing`'s region is live and not concurrently torn down; the
    /// door for custom `Backing::from_raw_parts` regions (shared attach tail).
    pub unsafe fn attach(backing: Backing) -> Result<Self, AttachError>;
    pub fn region(&self) -> (*mut u8, usize);   // hand the bytes to a second handle
    pub fn committed(&self) -> u64;
    pub fn reader_count(&self) -> usize;
    pub fn writer<WD: WakeSource, WS: WakeSink>(&self, data: WD, space: WS)
        -> Result<Writer<WD, WS>, WriterClaimed>;   // claims the in-region writer word
    /// # Safety: the claiming writer no longer exists (crashed/leaked).
    pub unsafe fn force_release_writer(&self);
    pub fn view<RD: WakeSink, RS: WakeSource>(&self, data: RD, space: RS)
        -> Result<View<RD, RS>, FullReaderTable>;
}

impl<WD: WakeSource, WS: WakeSink> Writer<WD, WS> {
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
}

impl<RD: WakeSink, RS: WakeSource> View<RD, RS> {
    pub fn cursor(&self) -> u64;
    pub fn committed(&self) -> u64;
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_, RS>>, ReadError>;
    pub async fn read(&mut self) -> Result<ReadGrant<'_, RS>, ReadError>;
    pub fn try_latest(&mut self) -> Result<Option<ReadGrant<'_, RS>>, ReadError>;
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError>;
    pub async fn read_into(&mut self, buf: &mut Vec<u8>) -> Result<(), ReadError>;
}

pub struct ReadGrant<'a, RS: WakeSource> { /* Deref<Target = [u8]> */ }

pub enum WriteError  { InsufficientCapacity, WouldBlock }
pub enum ReadError   { Corrupt }
pub enum AttachError {
    BadMagic, BadVersion, ArchMismatch,
    TooSmall,        // region shorter than the fixed header
    Misaligned,      // attach_raw base not 8-byte aligned
    BadGeometry,     // header fields internally inconsistent (§10)
    RegionTruncated, // header self-consistent but total_size exceeds the backing
}
pub struct FullReaderTable;
pub struct WriterClaimed;
```

`ReadError` has a single variant: `Corrupt`, a structurally invalid region (a
record length that straddles the wrap or overruns the data region). It is
unreachable from the crate's own behavior — the writer can never lap a reader —
and exists so a corrupted shared mapping degrades to an error instead of an
out-of-bounds borrow. `ReadGrant` derefs to the payload bytes; on drop it moves
the view's cursor — past the record for a `try_read`/`read` grant (consume),
back to its start for a `try_latest` grant (pin) — and wakes a waiting writer.
`create_in_memory` / `create_mmap` panic if `capacity` is not a power of two of
at least 8 bytes (one record header) or `max_readers` is zero.

## 10. Backing storage

`Backing` is one concrete struct — an owned, type-erased `(base, len)` byte
range plus the destructor that releases it (`ctx: *mut ()` and `drop_fn:
Option<unsafe fn(*mut (), *mut u8, usize)>`). The backing kinds differ **only**
in how they drop, so a drop fn pointer replaces what used to be a `Backing`
trait with per-kind impls (`BoxBacking`/`MmapBacking`/`RawBacking`) — and with
it, the `B: Backing` generic that threaded through `RingBuffer`, `Writer`,
`View`, `ReadGrant`, and every fsw-2 port type above them.

**The region contract.** Every constructor asserts (and all the ring's `unsafe`
relies on) that `base..base+len` is a single allocation that is
interior-mutable (writes through `*mut` derived from `base` are sound), 8-byte
aligned, and stable for the backing's lifetime. The ring forms `&AtomicU64`
references and plain byte slices over this region, deriving every access
pointer from `base` — captured **once** at construction — so provenance stays
whole-allocation wide.

The constructors:

- **`Backing::heap(size)`** — in-process: a zeroed, leaked `Box<[Word]>` where
  `Word` is a `repr(C, align(8))` `UnsafeCell<u64>`: interior-mutable (sound to
  write through, Miri-clean) and 8-byte aligned on every target for the
  control/cursor atomics (`u64` alone is only 4-aligned on i686).
  `Box::into_raw` hands over the whole-allocation pointer and its provenance —
  no live `Box` is retained, so there is no per-access re-derivation from an
  owning `Box` (the old design's provenance dance). The `drop_fn` reconstructs
  the box via `Box::from_raw(slice_from_raw_parts_mut(base as *mut Word,
  len/8))` and frees it. Default for in-process use and tests.
- **`unsafe Backing::raw(base, len)`** — non-owning, over a region someone else
  owns (the host's backing, or another process's mmap): `drop_fn` is `None`, so
  dropping it frees nothing; the region's own atomics carry all
  synchronization. This is the same-process dlopen path: the host calls
  `region()` to read out `(base, len)` and a dlopen'd system reconstructs a
  ring over the very same bytes via `attach_raw`.
- **`Backing::mmap(map)`** (crate-internal, behind the `mmap` feature) — a
  `memmap2::MmapMut`, page-aligned and cross-process capable. The map is boxed
  behind `ctx`; the `drop_fn` drops that box, unmapping via `MmapMut`'s own
  `Drop`.
- **`unsafe Backing::from_raw_parts(base, len, ctx, drop_fn)`** — the open
  extension point for custom regions (a static arena, a foreign allocator, …),
  paired with `unsafe RingBuffer::attach(backing)`. Its safety contract: the
  region contract holds for the backing's whole lifetime, and if `drop_fn` is
  `Some` it must be sound to call **exactly once, from any thread**, with
  exactly `(ctx, base, len)` — that call being the only release of the
  resources they name.

**Send/Sync.** A `Backing` is `!Send + !Sync` by construction (raw pointers);
one consolidated `unsafe impl Send/Sync for Backing` carries the promise the
ring upholds through its own synchronization: region bytes are only ever
touched through atomics or the `committed` Release/Acquire handshake, and the
constructor contracts make `drop_fn` callable from whichever thread drops the
last handle. `Inner` (and with it `RingBuffer`) is then auto-`Send + Sync` —
its other fields are plain integers.

`create_*` lays out the region (`init_region`) and writes the header
single-threaded before any handle is published. `attach_mmap` / `attach_raw`
funnel into `attach`, which validates the header (`read_header`) and reads the
geometry back out of the region rather than from arguments, returning
`AttachError` on mismatch. Validation is exhaustive, in checked `u64`
arithmetic so a hostile header cannot overflow its way past a bound:

1. `region_len >= HEADER_SIZE` → `TooSmall` (shared path, so a sub-header mmap
   of a truncated file is rejected before any header read).
2. `attach_raw` base pointer 8-byte aligned → `Misaligned` (the heap/mmap
   backings are aligned by construction).
3. Magic / version / `arch_tag` → `BadMagic` / `BadVersion` / `ArchMismatch`.
4. `capacity` a power of two, `>= 8` (one record header — `read_len`'s bounds
   contract), and `<= usize::MAX` (a 32-bit target attaching a 64-bit-sized
   region) → `BadGeometry`.
5. `max_readers > 0` → `BadGeometry`.
6. Reader table in bounds: `>= HEADER_SIZE`, 8-aligned, ending at or before
   `data_offset` (checked mul/add) → `BadGeometry`.
7. Data region in bounds: `data_offset` 8-aligned, `data_offset + capacity <=
   total_size` (checked add) → `BadGeometry`.
8. `total_size <= region_len` → `RegionTruncated` (its own variant — the
   truncated-on-disk file is the diagnosable real-world failure).

After a successful attach, every offset the ring ever dereferences — control
words, all reader slots, `data_offset + phys` for `phys < capacity` — is inside
`[0, region_len)` and 8-aligned: attach restores the same geometry invariant
`layout()` + `init_region` establish at creation, which is what the `SAFETY`
comments on `control`/`slot`/`data_ptr` assume.

## 11. Memory ordering & Miri

The synchronization discipline, summarized. The data bytes themselves are
**plain** (non-atomic) stores and loads throughout — race-free because the
in-use check keeps the writer off in-flight bytes and the `committed` handshake
orders publication, exactly like the disruptor:

- **Publication.** The writer publishes with a Release store to `committed`;
  readers Acquire-load `committed` before touching any data byte. The
  `high_water_mark` Release store is ordered before that same `committed` Release,
  so a reader that observes the new `committed` also observes the gap boundary
  (readers therefore load `hwm` *after* `committed` — §5, the load order is
  load-bearing).
- **Reader registration** claims a slot with a CAS (AcqRel on success —
  Acquire for the slot-state handoff from `View::drop`'s Release, Release to
  publish the claim), then runs the `SeqCst` fence handshake against the
  writer's `slowest_active_cursor` scan (§7). Dekker's pattern needs the
  fences' total order; Release/Acquire alone admits the both-sides-stale
  outcome. Visibility of the claim to the writer's scan comes from the
  handshake, not from the claim CAS itself.
- **The in-use scan.** `slowest_active_cursor` opens with the pairing
  `fence(SeqCst)` and Acquire-loads each slot cursor. Cursor advances (record
  consume, gap skip, grant drop) are Release stores, so the writer's scan
  observes a cursor only at record boundaries the reader has truly finished
  with.
- **Writer claim.** `writer()` CAS (Acquire success / Relaxed failure) pairs
  with `Writer::drop`'s / `force_release_writer`'s Release store, handing the
  region state (`committed`/`hwm` and the data bytes) from one writer to its
  successor. The writer reads `committed` Relaxed — it is the sole mutator.
- **Slot free.** `View::drop` stores `FREE_SLOT` with Release — a single
  wait-free store pairing with the next claimant's Acquire CAS.

The `epoch` word is written (`fetch_add(1, Release)` on claim) but never yet
read; it is reserved for crash-slot reclamation (§14).

The unsafe core follows the `libs/db` Miri strategy (`libs/db/MIRI.md`;
mirrored in this crate's `MIRI.md`): the synchronous tests use only the `try_*`
APIs over a heap backing (`UnsafeCell` words) with `std::thread`, and pass under Miri
including Tree Borrows, many-seeds interleavings, and a 32-bit
(`i686-unknown-linux-gnu`) target run that executes the length-overflow
path. `concurrent_full_stream` exercises the backpressured zero-loss path
end to end; `concurrent_view_churn` hammers the registration handshake under a
live writer; `concurrent_reader_churn` / `concurrent_writer_claim_churn` stress
the slot and writer-claim CAS. The deterministic single-threaded tests cover
the edges: `reader_on_gap_start_reads_through` (the `r == hwm` skip),
`garbage_length_is_corrupt` (a scribbled length degrades to `Corrupt`, never an
out-of-bounds access),
`view_starts_stable` (the handshake converges), and `latest_pins_newest` /
`latest_pin_backpressures_writer` (the §6 pin: re-serving the newest record,
and the writer `WouldBlock`ing rather than reclaiming it). Only the async
runtime tests and the feature-gated `mmap` tests are skipped under Miri. Tests
live in `ring/src/tests.rs`; see `MIRI.md` for how to run.

## 12. Differences from the db disruptor

| Aspect | `metor-db` disruptor | `metor-fsw-ring` |
|---|---|---|
| Slow-reader handling | Backpressure (`try_grant` → `WouldBlock`) | Same in-use check: `try_write` → `WouldBlock`, async `write` suspends |
| Read model | Borrowed `&[u8]` | Borrowed `ReadGrant` (derefs to `[u8]`), plus copy conveniences and the `try_latest` pin |
| Reader registry | Heap Treiber list of `Arc<ReadNode>` | Fixed slot array, claimed by CAS, addressed by index |
| Internal references | `Arc`/`Box`/`AtomicPtr` (process-local) | Relative offsets only; no pointers in the region |
| Backing | `Box<[UnsafeCell<u8>]>` only | one erased `Backing` struct: heap, mmap, or borrowed raw (drop-fn is the only difference) |
| Cross-process | No | Yes (single mapped region, same layout both sides) |
| Record framing | caller's responsibility | `[len u32][pad u32][payload][tail pad]`, 8-byte aligned |
| Writer serialization | In-region `Mutex` (`write_lock`) | Single writer, enforced by the in-region `writer_claim` CAS (no lock) |
| Wake mechanism | `WaitQueue` baked into the core | `WakeSource`/`WakeSink` traits, out of the shared region |
| Carried over | — | Absolute cursors, `committed` Release/Acquire, `high_water_mark` wrap gap, in-use check, slot-reuse-by-CAS |

## 13. Crate placement

- **Location / name:** `libs/metor-fsw-2/ring/`, package `metor-fsw-ring`
  (consistent with the `metor-fsw-*` family; the `-2` is a workspace-path
  detail). `edition = "2024"`, `version.workspace`/`repository.workspace`,
  matching `libs/db`.
- **Features:** `mmap` (memmap2, already in tree via `libs/db`) gates the mmap
  backing (`create_mmap`/`attach_mmap`); `async` gates the `stellarator`-backed
  `Notifier`; `futex` gates the shared-futex `wake` module (§8). The in-memory
  heap path and the `try_*` APIs need none of them.
- The ring is byte-oriented: framing of `metor-proto` tables is a layer above it,
  so the crate has no `metor-proto` dependency. Keeping it standalone keeps the
  unsafe shared-memory core small and independently Miri-testable, and reusable
  by both the coordinator and out-of-process systems.

## 14. Reserved for future work

These are not implemented; layout room is reserved so they drop in without a
layout change:

- **Per-ring cross-process wake.** The `wake` module (§8) provides the
  shared-futex primitive, but no ring *endpoint* uses it yet: the `wake_word`
  control slot and the `FLAG_WAKE_SHARED` flag bit still reserve room for a
  per-ring shared wake behind the `WakeSource`/`WakeSink` seam. Cyclic
  cross-process systems don't need one (they are stepped through the fsw-2
  control-block doorbell and poll their inputs); it becomes interesting with
  cross-process *async* systems. Until then an out-of-process reader with no
  doorbell polls `try_read`.
- **Live-reader reclamation.** `reclaim_owner` (§7) handles a **dead** owner —
  the supervised, post-reap sweep. Reclaiming a merely *unresponsive* reader
  (owner alive, cursor stuck) remains future work: it would need cursor stores
  through a `View` to become epoch-checked, since a plain Release store on a
  reclaimed slot would corrupt the new owner's cursor. Callers still
  over-provision `max_readers` and supervise their readers.
- **Writer-death recovery.** `committed` gates readers off uncommitted bytes, so
  a dead writer simply stalls the stream. A replacement writer (after
  `force_release_writer`) resumes at `committed`: any bytes the dead writer
  scribbled past it were never published to a reader and are simply rewritten.
