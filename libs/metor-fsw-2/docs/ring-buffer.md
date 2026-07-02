# Ring Buffer (`metor-fsw-ring`)

The lock-free, shared-memory ring buffer is the transport over which
`metor-fsw-2` systems exchange data. Every system writes its output frames into
one or more ring buffers and reads its inputs from *views* into other systems'
buffers (cyclic systems view upstream outputs directly; async systems read
private input buffers the coordinator fills for them — see `DESIGN.md`). A system
may run as a dlopen'd library inside the coordinator's process *or* as a separate
process, so the buffer works identically in-process and across a process
boundary: it lives in one contiguous region addressed by fixed byte offsets and
contains no process-local pointers.

The crate generalizes the `metor-db` disruptor (`libs/db/src/disruptor.rs`)
along two axes — a **writer-chosen overrun policy** (the disruptor only ever
backpressures) and **shared-memory residence** (the disruptor uses heap
`Arc`/`Box` structures). Its unsafe core reuses the disruptor's proven
techniques: absolute monotonic `u64` cursors with `phys = abs & mask`, a
`committed` Release/Acquire publication handshake, and a `high_water_mark` wrap
gap.

## 1. Overrun policy

A buffer is created in one of two modes (`Overrun`), fixed at creation and
recorded in the header. The mode is the read-soundness contract every view
relies on, so a buffer cannot mix writes of different modes — that would silently
void the borrow guarantee.

- **`Overrun::Overwrite` (default).** The writer never blocks on readers. When a
  write would reuse data-region bytes a slow reader has not consumed, the writer
  overwrites them and proceeds; the lapped reader *detects* this via
  `View::is_lapped()` / `Err(ReadError::Lapped)` rather than being waited on.
  This is the framework default for cyclic-system outputs: a producing system
  always makes progress, and a reader that falls more than a buffer behind is a
  hard error the coordinator telemeters (per `DESIGN.md`). Because the writer can
  race a reader on the same bytes, reads in this mode copy the payload out and
  re-validate — see §6.
- **`Overrun::Lossless`.** The writer honors the disruptor's in-use check and
  never overwrites in-flight bytes. `Writer::try_write` returns
  `Err(WriteError::WouldBlock)` rather than clobbering the slowest active reader;
  the async `Writer::write` suspends until a reader frees space. A view on a
  lossless buffer can never become lapped, so reads are tear-free and may borrow
  the bytes in place (`View::try_read`), exactly as the disruptor does.

`is_lapped()` is always `false` on a lossless buffer.

## 2. Memory layout

One contiguous region (`BoxBacking`, `MmapBacking`, or a borrowed `RawBacking`),
four logical zones: a **header** (immutable after creation), a **control block**
(the live writer atomics), a **reader-cursor table** (a fixed array of slots),
and the **data region**. Multi-byte fields are stored in **native byte order**;
the `arch_tag` handshake (below) rejects regions written by a different pointer
width or endianness on attach, so cross-endian reinterpretation never happens.
Control words and reader slots are padded to 64-byte cache lines to avoid false
sharing.

### What is shared-memory-resident vs process-local

- **In the region (shared):** the header, the control-block atomics, the
  reader-cursor table, and the data bytes. Addressed only by fixed offset.
- **In the process-local handle (never shared):** the mapping base pointer
  (`Backing`), the cached `capacity`/`mask`/`max_readers`/`overrun`/offsets, the
  reader-slot *index* a `View` holds, and the async `WakeSource`/`WakeSink`.
  These are reconstructed on attach (`from_validated` reads the geometry back out
  of the header), never stored in the region.

### Header (cache line 0, bytes `0x00..0x40`)

| Offset | Size | Type  | Field                 | Notes |
|--------|------|-------|-----------------------|-------|
| `0x00` | 4    | `u32` | `magic`               | `b"MFR1"` read as a native `u32`; validated on attach |
| `0x04` | 2    | `u16` | `version`             | layout version (currently `1`) |
| `0x06` | 2    | `u16` | `flags`               | bit 0 `FLAG_WAKE_SHARED` (reserved, never set); bit 1 `FLAG_LOSSLESS` (set iff lossless) |
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
| `0x50` | 8    | `AtomicU64` | `reserved_end`    | seqlock write reservation: the absolute end of the record the writer has *started* writing, stored (with a Release fence) before any data byte; `committed <= reserved_end` at every instant (§4/§5) |
| `0x58` | 8    | `AtomicU64` | `wake_word`       | reserved for a future cross-process futex/eventfd |
| `0x60` | 8    | `AtomicU64` | `writer_claim`    | single-writer enforcement: `0` = free, `1` = a live `Writer` exists (§4) |
| `0x68` | 24   | —           | pad               | to the next cache line |

> **Pre-1.0 layout note.** `reserved_end` going live and `writer_claim` being
> added changed the control block's meaning **without a `version` bump** —
> regions are ephemeral IPC state, not archives, so stale dev regions are simply
> recreated. Post-1.0, any such change bumps `version`.

`committed`, `high_water_mark`, and the wrap-gap mechanism are semantically
identical to the disruptor's `WriteHead`. There is **no** in-region mutex: there
is a single writer per buffer (§5), and a mutex would not be shared-memory-safe
across processes anyway. The header + control block together are `HEADER_SIZE =
0x80` bytes; the reader table starts immediately after.

### Reader-cursor table (starts at `reader_table_offset = 0x80`)

`max_readers` slots, each a 64-byte cache line (`READER_SLOT_SIZE = 64`):

| Slot offset | Size | Type        | Field    | Notes |
|-------------|------|-------------|----------|-------|
| `+0x00`     | 8    | `AtomicU64` | `cursor` | absolute read head, or `FREE_SLOT = u64::MAX` when unclaimed |
| `+0x08`     | 8    | `AtomicU64` | `epoch`  | generation bumped on claim; ABA-safe reuse hook |
| `+0x10`     | 48   | —           | pad      | to 64 B |

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

## 3. Cursor model & lap detection

All cursors are `u64` absolute, monotonically increasing byte counts; `phys = abs
& mask` where `mask = capacity - 1` (capacity is a power of two). The disruptor's
invariant carries over: a reader cursor is never ahead of `committed`, and the
in-flight (unread) bytes for a reader at `r` are `committed - r`. Absolute
counters make "caught up" (`r >= committed`) unambiguous and survive the physical
wrap. The single 16-EiB `u64` wrap at `committed` is not a concern in practice.

**Lap test.** A view at `r` is lapped iff

```
committed.wrapping_sub(r) > capacity
```

The oldest byte still intact is `committed - capacity`; while `r >= committed -
capacity` (equivalently `committed - r <= capacity`) the bytes at `r`'s physical
slot have not yet been overwritten. Wrap-gap bytes consume absolute cursor space
just like real bytes (the writer advances `committed` across them), so the test
is correct in the presence of gaps — with one refinement: a cursor parked
**exactly on a gap start** (`r == high_water_mark`) effectively sits at the next
lap boundary (`(r & !mask) + capacity`), because the gap bytes were never data.
`is_lapped()` and `locate()` both evaluate the lap at that effective cursor;
testing at `r` itself would spuriously hard-stop a reader whose next real record
is intact. `is_lapped()` additionally tests against `reserved_end` rather than
`committed`, so an *in-flight* overwrite of the reader's next bytes already
reads as lapped (one lap definition crate-wide with the read path's recheck,
§5), and a caught-up reader (`r_eff >= committed`) is never lapped — it has no
unread bytes to lose.

**Worked example** (`capacity = 64`, 16-byte payloads → 24-byte records: 8-byte
header + 16-byte payload, already a multiple of 8 so no tail pad):

- The writer has published a long stream; `committed = 200`. The oldest intact
  byte is `200 - 64 = 136`.
- View A at `r = 160`: `200 - 160 = 40 ≤ 64` → not lapped; A can still read the
  record at `160` (`phys = 160 & 63 = 32`).
- View B at `r = 120`: `200 - 120 = 80 > 64` → **lapped**. Physical slot `120 &
  63 = 56` was overwritten when the writer passed `184`. B's next read returns
  `Err(Lapped)`.
- **Wrap gap.** With the writer at `committed = 240` (`phys = 240 & 63 = 48`), a
  24-byte record will not fit in the 16 tail bytes (`48 + 24 > 64`), so the
  writer sets `high_water_mark = 240`, leaves bytes `48..64` as a 16-byte gap,
  and writes the record at absolute `256..280` (`phys 0..24`). A reader whose
  cursor reaches `240` exactly skips to `256`, the next lap boundary (`(r &
  !mask) + capacity`). The gap is a multiple of 8, so record-start alignment
  survives the wrap.

## 4. Write path

There is exactly one writer per buffer (each system owns its output buffer), and
the ring **enforces** it: `RingBuffer::writer()` CAS-claims the in-region
`writer_claim` word (`0 → 1`, Acquire success / Relaxed failure) and returns
`Err(WriterClaimed)` if a live writer already exists — the claim lives in the
region, so enforcement is cross-handle and cross-process. `Writer::drop` frees
the claim with a Release store, handing the whole region state to the next
claimer's Acquire CAS. A crashed process leaks its claim; the supervising host
reclaims it with the `unsafe fn force_release_writer()` escape hatch (it must
assert the claiming writer is truly gone). The writer is the sole mutator of
`committed`, `high_water_mark`, and `reserved_end`, and reads `committed`
Relaxed. `Writer::try_write(bytes)`:

1. `rec = frame_len(bytes.len())`. If `rec > capacity` →
   `Err(WriteError::InsufficientCapacity)`.
2. `c = committed` (Relaxed; sole writer). `reserve(c, rec)` computes the wrap:
   `phys = c & mask`; if `phys + rec > capacity` then `gap = capacity - phys` (a
   multiple of 8) and `start_abs = c + gap`, else `gap = 0` and `start_abs = c`.
3. **Mode branch.** In `Lossless` mode, check `fits(c, gap + rec)`: with `slowest
   = slowest_active_cursor()` (the min cursor over non-free slots, or `c` if no
   readers; the scan opens with a `SeqCst` fence — see §7), the write fits iff
   `c.wrapping_sub(slowest) + gap + rec <= capacity`. If it does not →
   `Err(WriteError::WouldBlock)`. In `Overwrite` mode there is **no** in-use
   check — the writer proceeds regardless of reader cursors (the key divergence
   from the disruptor's `try_grant`).
4. **Commit** (`commit(c, start_abs, gap, bytes)`):
   - **Seqlock begin:** `reserved_end.store(start_abs + rec, Relaxed)`, then
     `fence(Release)` — the reservation is published *before* any data byte is
     touched, in both modes. (A Release *store* would not be enough: it orders
     prior accesses, not the subsequent relaxed data stores; the fence-to-fence
     pairing with the reader's Acquire fence is what closes the torn-read
     window, §5.)
   - Write the record header + payload at `phys' = start_abs & mask`
     (`write_record`, mode-specific — see §6).
   - If `gap > 0`, store `high_water_mark = c` (Release) so readers skip the gap.
   - `committed.store(start_abs + rec, Release)`, then `data.notify()`.

The Release store of `committed` hands the freshly written bytes (and the
`high_water_mark` store) to readers, which Acquire-load `committed` before
touching any data byte — the same happens-before edge the disruptor relies on.

`Writer::write(bytes)` is the async lossless form: it loops, and when the write
does not fit it awaits the **space-available** sink (`space.wait_until(...)`)
until a reader frees enough room, then re-evaluates. In overwrite mode `write`
resolves immediately without ever suspending. In overwrite mode the writer is
wait-free with respect to readers; the only failure is an over-capacity message.

## 5. Read path & views

A `View` owns one reader-table slot (its index lives in the process-local
handle) and reads whole records. `locate()` finds the next readable record:

1. `r = slot.cursor` (Acquire), `c = committed` (Acquire), then `hwm =
   high_water_mark` (Acquire). The load order is load-bearing: seeing a
   post-wrap `committed` implies seeing that wrap's `hwm` store (it is
   sequenced before the `committed` Release), so a reader whose next bytes are
   a gap always sees the marker before it could misread stale gap bytes as a
   record header.
2. **Skip gap:** if `r == hwm`, store the cursor forward to the next lap
   boundary `(r & !mask) + capacity` (Release), notify the space sink, and
   retry from step 1. This runs *before* the lap test — a cursor parked on a
   gap start is effectively at the lap boundary (§3), and testing the lap at
   `r` would spuriously hard-stop it. The skip never accepts data; the next
   iteration re-runs the lap test at the boundary, so a real lap is still
   caught.
3. If `r >= c` → `Ok(None)` (caught up). Checked before the lap test because
   the gap skip can transiently park the cursor ahead of `committed`.
4. **(Overwrite only) pre-check lap:** if `c.wrapping_sub(r) > capacity` →
   `Err(Lapped)` (fast-path filter; the post-copy recheck is the authority).
5. Read `len` from the record header at `phys = r & mask` (`read_len` returns
   the raw `u32` widened to `u64` — under a lap race it can be arbitrary
   payload bytes).
6. **Straddle/length guard, both modes, u64 math:** if `len > capacity - 8 -
   phys` (the `phys + rec > capacity` predicate rewritten overflow-free), the
   length field is not a real record's. On an overwrite buffer a lapping
   writer was rewriting the header → `Err(Lapped)`; on a lossless buffer no
   lap can explain it → `Err(Corrupt)` (external corruption of the shared
   region — degrade to an error, never an out-of-bounds borrow). Validating in
   `u64` *before* any `usize` conversion matters on 32-bit targets, where
   `frame_len(garbage)` would wrap `usize` and defeat the check itself.
7. Return the located record (`r`, `phys`, `len`, `rec`) — every offset now
   proven in-bounds, so the downstream copy/borrow is bounded even under a
   concurrently scribbled length.

The read-soundness contract from here depends on the mode, and this is the crux
of the writer-policy design.

**Lossless buffers: tear-free, borrow is sound.** The writer's in-use check
provably keeps it from touching the bytes of a record a reader still owns — the
same guarantee that lets the disruptor hand out a borrowed `&[u8]`. The Acquire
load of `committed` establishes happens-before against the writer's Release, and
the in-use check is the happens-after edge that keeps the bytes stable until the
reader advances. `View::try_read()` therefore returns a `ReadGrant` borrowing the
payload in place (zero copy); dropping the grant advances the cursor and notifies
the writer. `is_lapped` can never be true here.

**Overwrite buffers: the writer races the reader, so copy + seqlock recheck.**
The writer may begin overwriting a record while the reader reads it, so a borrow
is unsound (`try_read` returns `Err(BorrowNotSupported)`). `View::try_read_into
(buf)` instead **copies the payload out** with race-tolerant relaxed atomic byte
loads — the bytes are a genuine concurrent read/write, not a plain `&[u8]` load —
and then **post-validates** against the write reservation: `fence(Acquire)`, then
load `reserved_end` (Relaxed; the fence carries the ordering). If
`reserved_end.wrapping_sub(r) > capacity`, a write that could have touched this
record's bytes was in flight during the copy → `Err(Lapped)`, do not advance.
The fence pairs with the writer's Release fence between its reservation store
and its data stores: if any relaxed payload load read a byte from an overlapping
write, that write's reservation (which exceeds `r + capacity`) is visible to the
recheck — so a passing recheck proves no copied byte came from an overlapping
write. Rechecking `committed` would **not** suffice: the writer scribbles data
bytes *before* its `committed` Release store, so a writer exactly one lap ahead
could tear the copy while `committed` still passes. (Tear-*old* is excluded by
`locate`'s Acquire on `committed`.) At steady state `reserved_end == committed`,
so an up-to-date reader never sees a spurious `Lapped`; a reservation-triggered
`Lapped` on an intact record means "about to be overwritten" — indistinguishable
from a lap one instruction later. Otherwise the copy is a consistent snapshot
and the cursor advances (Release) past `rec`. A single forward-only writer never
rewrites the *same* absolute slot within a lap, so this whole-buffer check is
sufficient — no per-record version word is needed.

`try_read_into` works on both kinds of buffer and is always safe: on a lossless
buffer the copy is a plain `copy_nonoverlapping` and the post-validate is skipped
entirely (it never fires). `View::read_into(buf)` is the async form — it loops
`try_read_into`, awaiting the **data-available** sink between attempts.

`is_lapped()` is the standalone lap check (`reserved_end - r_eff > capacity`
with the gap-start effective cursor and caught-up short-circuit of §3; always
`false` on a lossless buffer) and is what the coordinator calls before invoking
a cyclic system. `cursor()` / `committed()` accessors let the coordinator or a
monitor inspect lag without performing a read. `resync()` stores the cursor
forward to the current `committed` (Release), abandoning unread (possibly
lapped) data — the recovery after `Err(Lapped)`.

## 6. The byte-by-byte atomic copy

In overwrite mode `write_record` and `copy_payload` deliberately touch the data
bytes through **relaxed atomics**, not plain memory accesses:

- `write_record` stores the 8-byte header as one `AtomicU64` (Relaxed) and each
  payload byte as an `AtomicU8` (Relaxed).
- `copy_payload` / `read_len` load them back the same way (`AtomicU8` /
  `AtomicU64`, Relaxed).

This is a load-bearing invariant, not a quirk. In overwrite mode the writer and a
reader can legitimately access the *same* bytes at the same time; under the C/Rust
memory model a concurrent non-atomic read/write is undefined behavior regardless
of how the result is later validated. Performing both sides as relaxed atomics
makes the overlap a **defined data race with an indeterminate (but not UB)
result**, which the whole-buffer lap recheck then discards if the writer
overwrote the snapshot mid-copy. The later `committed` Release/Acquire still
orders the relaxed stores before publication, so a reader that is *not* lapped
observes exactly the bytes the writer published.

In lossless mode there is no such overlap, so `write_record`/`copy_payload` use
plain pointer writes and `copy_nonoverlapping` — race-free purely by the in-use
check and the `committed` handshake, exactly like the disruptor.

## 7. Reader registration

Readers register in a **fixed-size reader table** (§2): `max_readers` slots at a
known offset, addressed by index. A heap Treiber list of `Arc<ReadNode>` like the
disruptor's cannot live in shared memory — it stores process-local heap pointers
and its `Arc` strong counts are meaningless cross-process — so the table mirrors
only the disruptor's *slot-reuse-by-CAS* idea over a flat array:

- **`RingBuffer::view(data, space)`:** Acquire-load `committed` as the start
  cursor (so a fresh view only sees data committed from now on, never older
  possibly-lapped data — same rule as `Disruptor::reader`). Scan slots
  `0..max_readers`, CAS each `cursor` from `FREE_SLOT` to `start` (AcqRel
  success — Acquire pairs with `View::drop`'s Release for the slot-state
  handoff, Release publishes the claim; Relaxed failure). On success, bump
  `epoch` (`fetch_add(1, Release)`) and return a `View` holding that slot
  index. If no slot is free → `Err(FullReaderTable)`.
- **Lossless registration handshake.** On a lossless buffer, the fresh claim
  must be *provably visible* to the writer's `fits()` scan before the view is
  returned, or the writer could lap the new cursor and invalidate a later
  borrow. Release/Acquire alone cannot prove it — reader
  (`store cursor; load committed`) vs. writer (`store committed; load cursor`)
  is Dekker's pattern, where both sides may read the older values. So `view()`
  loops: `fence(SeqCst)`, re-load `committed`; if it moved, advance the claim
  to the new edge (a semantic no-op for a fresh view) and repeat, until
  `committed` is *stable* across the fence. The pairing `fence(SeqCst)` sits at
  the top of `slowest_active_cursor()`: in the fence total order, either the
  writer's scan is later and must observe the claim, or the reader's recheck is
  later and must observe that writer's `committed` — a stable `committed`
  therefore proves every unseen write was bounded by some other cursor at or
  below ours. The loop converges in 1–2 iterations (each extra one needs a
  commit inside a ~3-instruction window) and is deliberately unbounded —
  registration is a cold path. Overwrite mode skips the handshake: its reads
  self-validate via the seqlock recheck (§5).
- **`View::drop`:** `cursor.store(FREE_SLOT, Release)` — a single wait-free store
  that frees the slot for reuse.
- **`RingBuffer::reader_count()`** scans the table for non-free cursors.

Trade-offs versus the linked list: the maximum number of concurrent readers is
fixed at creation and registration is an `O(max_readers)` scan, but there is no
allocation, no `Arc`, no pointers, and the whole structure is addressable by
offset — exactly what shared memory requires. The `epoch` word guards against ABA
on slot reuse.

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
  a `WakeSink` (`data`) its `read_into` awaits.
- **Space-available** (readers → writer, when a reader advances): each `View`
  holds a `WakeSource` (`space`) it `notify()`s on advance / gap-skip; the
  lossless `Writer` holds a `WakeSink` (`space`) its async `write` awaits.

The predicate form (`wait_until(ready)`) lets the sink re-check the condition
after arming, closing the lost-wakeup window.

Implementations:

- **`NoWake`** — a no-op source/sink. The `try_*` paths never touch the wake
  hooks, so this is the right choice for synchronous consumers (the coordinator
  polling cyclic inputs once per cycle, and the Miri tests). Under `NoWake` the
  async paths degenerate to caller-driven polling.
- **`Notifier`** (behind the `async` feature) — backed by a `stellarator`
  `WaitQueue`, shared by clone between the writer and views for one direction.

## 9. Public API surface

```rust
pub const fn round_up8(n: usize) -> usize;
pub const fn frame_len(payload_len: usize) -> usize; // 8 + round_up8(payload_len)

pub enum Overrun { Overwrite, Lossless }

pub struct Config {
    pub capacity: usize,    // data-region bytes; power of two
    pub max_readers: usize, // reader-table slots (over-provision; see §11)
    pub overrun: Overrun,
}

pub unsafe trait Backing: Send + Sync {
    fn base(&self) -> *mut u8;     // re-derived on every access
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool { self.len() == 0 }
}

pub struct RingBuffer<B: Backing> { /* Arc<Inner<B>>: backing + cached geometry */ }

impl RingBuffer<BoxBacking> {
    pub fn create_in_memory(cfg: Config) -> Self;            // zeroes + writes header
}
#[cfg(feature = "mmap")]
impl RingBuffer<MmapBacking> {
    pub fn create_mmap(path: &Path, cfg: Config) -> std::io::Result<Self>;
    /// # Safety: caller asserts `path` is a compatible region, not being torn down.
    pub unsafe fn attach_mmap(path: &Path) -> std::io::Result<Self>;
}
impl RingBuffer<RawBacking> {
    /// # Safety: `base..base+len` is a live, header-valid region outliving all
    /// writers/views produced from it and not torn down concurrently.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError>;
}

impl<B: Backing> RingBuffer<B> {
    pub fn region(&self) -> (*mut u8, usize);   // hand the bytes to a second handle
    pub fn overrun(&self) -> Overrun;
    pub fn committed(&self) -> u64;
    pub fn reader_count(&self) -> usize;
    pub fn writer<WD: WakeSource, WS: WakeSink>(&self, data: WD, space: WS)
        -> Result<Writer<B, WD, WS>, WriterClaimed>;   // claims the in-region writer word
    /// # Safety: the claiming writer no longer exists (crashed/leaked).
    pub unsafe fn force_release_writer(&self);
    pub fn view<RD: WakeSink, RS: WakeSource>(&self, data: RD, space: RS)
        -> Result<View<B, RD, RS>, FullReaderTable>;
}

impl<B, WD: WakeSource, WS: WakeSink> Writer<B, WD, WS> {
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
}

impl<B, RD: WakeSink, RS: WakeSource> View<B, RD, RS> {
    pub fn is_lapped(&self) -> bool;
    pub fn cursor(&self) -> u64;
    pub fn committed(&self) -> u64;
    pub fn resync(&self);
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError>;
    pub async fn read_into(&mut self, buf: &mut Vec<u8>) -> Result<(), ReadError>;
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_, B, RS>>, ReadError>;
}

pub enum WriteError  { InsufficientCapacity, WouldBlock }
pub enum ReadError   { Lapped, BorrowNotSupported, Corrupt }
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

`try_read_into` returns `Ok(true)` (a record was read), `Ok(false)` (caught up),
`Err(Lapped)` (overwrite buffers only), or `Err(Corrupt)` (lossless buffers whose
region violated a structural invariant — unreachable from the crate's own
behavior, reachable only via external corruption of a shared mapping). `try_read`
returns `ReadGrant` only on a lossless buffer (`Err(BorrowNotSupported)`
otherwise); the grant derefs to the payload bytes and advances the cursor on
drop. `create_in_memory` / `create_mmap` panic if `capacity` is not a non-zero
power of two or `max_readers` is zero.

## 10. Backing storage

`Backing` is an `unsafe trait`: an implementor guarantees `base()` returns a
pointer to a single allocation of at least `len()` bytes that is interior-mutable,
8-byte aligned, and stable for the lifetime of `self`. The ring forms
`&AtomicU64`/`&AtomicU8` references and byte slices over this region, re-deriving
the pointer from `base()` on every access to keep provenance whole-allocation
wide.

- **`BoxBacking`** — in-process, heap-backed `Box<[UnsafeCell<u64>]>`:
  interior-mutable (sound to write through, Miri-clean) and `u64`-aligned for the
  control/cursor atomics. Default for in-process use and tests.
- **`MmapBacking`** (behind the `mmap` feature) — a `memmap2::MmapMut`,
  page-aligned and cross-process capable.
- **`RawBacking`** — a non-owning `(base, len)` over a region someone else owns
  (the host's backing, or another process's mmap). Its `Drop` frees nothing; the
  region's own atomics carry all synchronization. This is the same-process
  dlopen path: the host calls `region()` to read out `(base, len)` and a dlopen'd
  system reconstructs a ring over the very same bytes via `attach_raw`.

`create_*` lays out the region (`init_region`) and writes the header
single-threaded before any handle is published. `attach_mmap` / `attach_raw`
validate the header (`read_header`) and read the geometry back out of the region
rather than from arguments (`from_validated`), returning `AttachError` on
mismatch. Validation is exhaustive, in checked `u64` arithmetic so a hostile
header cannot overflow its way past a bound:

1. `region_len >= HEADER_SIZE` → `TooSmall` (shared path, so a sub-header mmap
   of a truncated file is rejected before any header read).
2. `attach_raw` base pointer 8-byte aligned → `Misaligned` (mmap/Box backings
   are aligned by construction).
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
comments on `atomic_u64`/`data_ptr` assume.

## 11. Memory ordering & Miri

The synchronization discipline, summarized:

- **Publication.** The writer publishes with a Release store to `committed`;
  readers Acquire-load `committed` before touching any data byte. The
  `high_water_mark` Release store is ordered before that same `committed` Release,
  so a reader that observes the new `committed` also observes the gap boundary
  (readers therefore load `hwm` *after* `committed` — §5).
- **Seqlock reservation (overwrite reads).** The writer stores `reserved_end`
  (Relaxed) and issues a `fence(Release)` before any data store; the reader
  issues a `fence(Acquire)` after its payload loads and rechecks `reserved_end`
  (Relaxed). Per the C++ fence rules, if any payload load read an overlapping
  write's byte, that write's reservation is visible to the recheck — a passing
  recheck proves a tear-free snapshot (§5).
- **Lossless mode is race-free** exactly like the disruptor: the in-use check
  keeps the writer off in-flight bytes, so the plain (non-atomic) record stores
  and reads are ordered solely by the `committed` handshake. Borrows are sound —
  *given* the registration handshake below.
- **Overwrite mode has a defined data race** on the data bytes: both writer and
  reader use relaxed atomics (§6), and the post-copy seqlock recheck discards
  any snapshot an overlapping write could have touched. No undefined behavior,
  no torn record ever escapes to the caller.
- **Reader registration** claims a slot with a CAS (AcqRel on success) and frees
  it with a Release store; `slowest_active_cursor` Acquire-loads each slot. On a
  lossless buffer, registration additionally runs the `SeqCst` fence handshake
  against the writer's cursor scan (§7) — Dekker's pattern needs the fences'
  total order; Release/Acquire alone admits the both-sides-stale outcome.
- **Writer claim.** `writer()` CAS (Acquire success / Relaxed failure) pairs
  with `Writer::drop`'s Release store, handing the region state from one writer
  to its successor.

The unsafe core follows the `libs/db` Miri strategy (`libs/db/MIRI.md`;
mirrored in this crate's `MIRI.md`): the synchronous tests use only the `try_*`
APIs over a `BoxBacking` (`UnsafeCell`) with `std::thread`, and pass under Miri
including Tree Borrows, many-seeds interleavings, and a 32-bit
(`i686-unknown-linux-gnu`) target run that executes the R3 length-overflow
path. `concurrent_overwrite_no_ub` is the overwrite race coverage (writer/reader
race the same bytes via relaxed atomics + the seqlock recheck; the payload is
the record index encoded twice, so a torn old/new mix is detected, not just a
garbage value); `concurrent_lossless_full_stream` exercises the lossless
zero-loss path; `concurrent_lossless_view_churn` hammers the registration
handshake under a live writer; `concurrent_reader_churn` /
`concurrent_writer_claim_churn` stress the slot and writer-claim CAS. The
deterministic seqlock/gap/corruption windows are hand-driven single-threaded
tests (`torn_read_rejected_by_reservation`, `reader_on_gap_start_*`,
`garbage_length_bounded`, `lossless_garbage_length_is_corrupt`) — see `MIRI.md`
for what runs under Miri and why.

## 12. Differences from the db disruptor

| Aspect | `metor-db` disruptor | `metor-fsw-ring` |
|---|---|---|
| Slow-reader handling | Backpressure only (`try_grant` → `WouldBlock`) | Writer-chosen: overwrite (default), or lossless error/wait |
| Writer blocked by readers | Always (`slowest_cursor` in-use check) | Never in overwrite; lossless reuses the in-use check |
| Read soundness | Borrow always safe | Borrow safe in lossless; copy + lap-recheck in overwrite |
| Reader registry | Heap Treiber list of `Arc<ReadNode>` | Fixed slot array, claimed by CAS, addressed by index |
| Internal references | `Arc`/`Box`/`AtomicPtr` (process-local) | Relative offsets only; no pointers in the region |
| Backing | `Box<[UnsafeCell<u8>]>` only | `Backing` trait: `Box`, `mmap`, or borrowed `Raw` |
| Cross-process | No | Yes (single mapped region, same layout both sides) |
| Record framing | caller's responsibility | `[len u32][pad u32][payload][tail pad]`, 8-byte aligned |
| Writer serialization | In-region `Mutex` (`write_lock`) | Single writer, enforced by the in-region `writer_claim` CAS (no lock) |
| Wake mechanism | `WaitQueue` baked into the core | `WakeSource`/`WakeSink` traits, out of the shared region |
| Carried over | — | Absolute cursors, `committed` Release/Acquire, `high_water_mark` wrap gap, slot-reuse-by-CAS |

## 13. Crate placement

- **Location / name:** `libs/metor-fsw-2/ring/`, package `metor-fsw-ring`
  (consistent with the `metor-fsw-*` family; the `-2` is a workspace-path
  detail). `edition = "2024"`, `version.workspace`/`repository.workspace`,
  matching `libs/db`.
- **Features:** `mmap` (memmap2, already in tree via `libs/db`) gates
  `MmapBacking`; `async` gates the `stellarator`-backed `Notifier`. The in-memory
  `BoxBacking` path and the `try_*`/`is_lapped` APIs need neither.
- The ring is byte-oriented: framing of `metor-proto` tables is a layer above it,
  so the crate has no `metor-proto` dependency. Keeping it standalone keeps the
  unsafe shared-memory core small and independently Miri-testable, and reusable
  by both the coordinator and out-of-process systems.

## 14. Reserved for future work

These are not implemented; layout room is reserved so they drop in without a
layout change:

- **Cross-process wake.** In-process notification uses `Notifier`/`NoWake`. The
  `wake_word` control slot and the `FLAG_WAKE_SHARED` flag bit reserve room for a
  shared-memory futex/eventfd, and the `WakeSource`/`WakeSink` traits keep the
  seam, but no cross-process notification mechanism exists. An out-of-process
  reader polls `try_read_into` until one is built.
- **Crash-slot reclamation.** A reader that crashes leaks its claimed slot.
  Callers over-provision `max_readers`. The `epoch` word and a future
  owner-pid/liveness sweep remain available as the reclamation hook. (If ever
  implemented, the reclaimer must bump `epoch` *before* freeing the cursor, and
  cursor stores through a `View` must become epoch-checked — a plain Release
  store on a reclaimed slot would corrupt the new owner's cursor.) A crashed
  **writer** likewise leaks the `writer_claim` word; `force_release_writer` is
  the supervised escape hatch until real cross-process liveness exists.
- **Writer-death recovery.** `committed` gates readers off uncommitted bytes, so
  a dead writer simply stalls the stream. `reserved_end` (now live as the
  seqlock reservation, §4) doubles as the recovery hook: a replacement writer
  (after `force_release_writer`) can see how far the dead one had reserved and
  resume past it.
