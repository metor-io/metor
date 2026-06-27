# Work-Package 1 — Ring Buffer

> Design document only. No implementation exists yet. This describes the data
> path primitive for `metor-fsw-2`; it is meant to be reviewed before any crate
> or Rust is written. Where it can, it reuses the proven techniques from the
> existing `metor-db` disruptor (`libs/db/src/disruptor.rs`); where it must
> diverge — overwrite semantics and shared memory — that is called out
> explicitly.

## 1. Purpose & role in metor-fsw-2

The ring buffer is the single transport over which `metor-fsw-2` systems
exchange data. Every system writes its output frames into one or more ring
buffers and reads its inputs from *views* into other systems' buffers (cyclic
systems view upstream outputs directly; async systems read private input buffers
the coordinator fills for them — see `DESIGN.md`). Because a system may be a
dl-opened library in the coordinator's process *or* a separate process, the ring
buffer must work identically in-process and across a process boundary, which
means it must live in shared memory and contain no process-local pointers. The
defining behavioral requirement is a **writer-chosen overrun policy** with a
**non-blocking, overwrite-on-lap default**: by default a producing system always
makes progress and a reader that falls more than a buffer behind is *detected*
rather than waited on, but a writer may opt into the disruptor's
guarantee-delivery behavior (error or wait) where its channel needs it. This
generalizes the db disruptor (which only ever backpressures) and matches
`DESIGN.md`: "writers do not block by default but may opt to error or wait."

## 2. Requirements (made precise)

1. **Writer-chosen overrun policy.** When a write would reuse data-region bytes
   an active reader has not consumed, the *writer* decides the outcome. Three
   behaviors are supported, selected per buffer (recorded in the header) with a
   further per-call choice inside the lossless mode:
   - **Overwrite** (default; never blocks) — the writer proceeds and overwrites;
     a lapped reader detects it via `is_lapped()` / `Err(Lapped)`. This is the
     framework default for cyclic-system outputs (matches `DESIGN.md`).
   - **Error / skip** — `try_write` returns `WouldBlock` if it would overwrite
     the slowest *active* reader (the disruptor's in-use check), so the writer
     can drop the write instead of clobbering data.
   - **Wait** — `write().await` suspends until a reader frees enough space.

   Overwrite is a distinct *buffer* mode (it changes the read-soundness
   contract — see below and §6); error and wait are two per-call flavors of the
   single **lossless** mode, which never overwrites in-flight bytes. The mode
   the buffer was created with is the read contract every view relies on, so a
   buffer cannot mix overwrite and lossless writes (that would silently void the
   borrow guarantee).
2. **Lap detection on the read side.** Each view holds its own absolute read
   head `r`. A view is *lapped* when `committed - r > capacity`: the writer has
   advanced more than one full data region past the view's cursor, so the bytes
   at `r`'s physical slot have been overwritten. The view exposes this directly
   (`View::is_lapped()`), and every read returns `Err(ReadError::Lapped)` rather
   than handing back overwritten or torn data. This is the API the coordinator
   calls before invoking a cyclic system; on `Lapped` it telemeters and stops
   invoking that module (a hard error, per `DESIGN.md`). In a lossless buffer a
   view can never become lapped, since the writer respects the in-use check.
3. **Shared-memory capable, offset-based.** The buffer is a single contiguous
   region. Everything stateful inside it — write cursor, high-water mark,
   per-reader cursors, framing — is plain integers/atomics at *fixed byte
   offsets*; all internal references are relative offsets, never absolute
   pointers. No `Box`, `Arc`, `Vec`, or any process-local pointer is ever stored
   inside the region. The backing store is abstracted: a `Box<[UnsafeCell<u8>]>`
   for in-process use and tests, an `mmap` for shared use.
4. **Absolute monotonic cursors.** All cursors are `u64` absolute byte counts;
   the physical offset of absolute position `a` is `a % capacity`. The
   `committed` Release/Acquire handshake and the high-water-mark wrap handling
   are taken directly from the disruptor.
5. **Multiple readers, independent cursors,** stored in a fixed-size reader
   table at a known offset (capacity-bounded number of readers, chosen at
   creation). The disruptor's heap linked list of `Arc<ReadNode>` cannot be used
   — see §7.
6. **Variable-length messages,** length-prefixed and never straddling the wrap
   (a wrap gap is left, exactly as the disruptor does).
7. **Async wake** is behind a trait so the in-process build uses a local
   notifier and a future cross-process build can use a process-shared primitive.
   The notifier is *not* part of the shared layout. Cross-process notification
   is explicitly out of scope for WP1.

## 3. Memory layout

One contiguous region (`Box<[u8]>` or `mmap`), three logical zones: a **header**
(immutable after creation), a **control block** (the live atomics), a
**reader-cursor table** (fixed array of slots), and the **data region**. All
multi-byte fields are little-endian; the region is assumed to be shared only
between processes of the same architecture (see open questions). Control words
and reader slots are padded to 64-byte cache lines to avoid false sharing.

### What is shared-memory-resident vs process-local

- **In the region (shared):** header, control block atomics, reader-cursor
  table, data bytes. Addressed only by fixed offset.
- **In the process-local handle (never shared):** the mapping base
  pointer/`Backing`, the cached `capacity`/`max_readers`, the owned reader-slot
  *index* a `View` holds, and the async `WakeSource`/`WakeSink`. These are
  reconstructed on `attach`, never read out of the region.

### Header (cache line 0, bytes `0x00..0x40`)

| Offset | Size | Type   | Field                 | Notes |
|--------|------|--------|-----------------------|-------|
| `0x00` | 4    | `u32`  | `magic`               | e.g. `b"MFR1"`; validated on attach |
| `0x04` | 2    | `u16`  | `version`             | layout version |
| `0x06` | 2    | `u16`  | `flags`               | bit 0: shared-futex wake word present (future) |
| `0x08` | 8    | `u64`  | `capacity`            | size of the data region in bytes |
| `0x10` | 8    | `u64`  | `data_offset`         | region-relative offset of byte 0 of data |
| `0x18` | 4    | `u32`  | `max_readers`         | number of slots in the reader table |
| `0x1C` | 4    | `u32`  | `reader_table_offset` | region-relative offset of slot 0 |
| `0x20` | 8    | `u64`  | `total_size`          | full region size in bytes |
| `0x28` | 24   | —      | reserved / pad        | to 64 B |

### Control block (cache line 1, bytes `0x40..0x80`)

| Offset | Size | Type        | Field              | Notes |
|--------|------|-------------|--------------------|-------|
| `0x40` | 8    | `AtomicU64` | `committed`        | absolute bytes published; Release by writer, Acquire by readers |
| `0x48` | 8    | `AtomicU64` | `high_water_mark`  | end of valid data before a pending wrap gap; `u64::MAX` = no gap |
| `0x50` | 8    | `AtomicU64` | `reserved_end`     | writer-private reservation end (single writer; lets an attacher recover) |
| `0x58` | 8    | `AtomicU64` | `wake_word`        | reserved for a future cross-process futex/eventfd; unused in WP1 |
| `0x60` | 32   | —           | reserved / pad     | to next cache line |

`committed`, `high_water_mark`, and the wrap gap mechanism are semantically
identical to the disruptor's `WriteHead`. The disruptor's `write_lock` mutex is
**not** in the region — see §5 (single writer, no in-region lock).

### Reader-cursor table (starts at `reader_table_offset`)

`max_readers` slots, each one 64-byte cache line:

| Slot offset | Size | Type        | Field    | Notes |
|-------------|------|-------------|----------|-------|
| `+0x00`     | 8    | `AtomicU64` | `cursor` | absolute read head, or `FREE_SLOT = u64::MAX` when unclaimed |
| `+0x08`     | 8    | `AtomicU64` | `epoch`  | generation bumped on claim; for ABA-safe reuse / liveness (see §7, §12) |
| `+0x10`     | 48   | —           | pad      | to 64 B |

### Data region (starts at `data_offset`, length `capacity`)

Raw bytes, framed into **8-byte-aligned records** so payloads can be mapped as
`repr(C)` frames in place (see below). Each record is:

```
[ len: u32 ][ _pad: u32 ][ payload: len bytes ][ tail_pad: (8 - len % 8) % 8 ]
└──── 8-byte record header ────┘
```

The 8-byte header (a `u32` length plus a `u32` reserved/pad word) puts the
payload at an 8-byte boundary, and the tail pad rounds the whole record up to a
multiple of 8. Record total = `8 + round_up(len, 8)`, always a multiple of 8.
Because `data_offset` is 64-byte aligned (header + control + table are all
cache-line multiples) and every record length is a multiple of 8, every record
*start* is 8-byte aligned — and stays so across the wrap as long as `capacity` is
a multiple of 8 (the wrap gap, `capacity - phys`, is then also a multiple of 8).
This is why payloads are mappable in place: a borrowed payload (lossless mode) or
a copy into an 8-aligned destination buffer (overwrite mode) is correctly aligned
for a `repr(C)` / zerocopy frame with no realignment step.

A record never straddles the wrap: if it would not fit contiguously in the tail,
the writer leaves a gap from the current physical offset to the end of the data
region (recording the boundary in `high_water_mark`) and writes the record at
offset 0 of the next lap. The `len` prefix makes records self-describing so a
reader recovers boundaries without external metadata.

```
region:
  0x00  ┌─────────────── header (64 B) ───────────────┐
  0x40  ├─────────────── control block (64 B) ────────┤
  0x80  ├─ reader slot 0 (64 B) ─┬─ slot 1 ─┬─ … ──────┤   max_readers slots
        ├──────────────── data region (capacity B) ────┤
        │ [hdr|payload|pad][hdr|payload|pad]…[gap][hdr… │   each record 8-aligned
        └──────────────────────────────────────────────┘
```

## 4. Cursor model & lap detection

All cursors are `u64` absolute monotonically increasing byte counts; `phys =
cursor % capacity`. The disruptor's invariant carries over: a reader cursor is
never ahead of `committed`, and in-flight (unread) bytes for a reader at `r` are
`committed - r`.

**Ordering.** The writer publishes data with a Release store to `committed`; a
reader does an Acquire load of `committed` before touching any data byte. This is
the same happens-before edge the disruptor relies on (and that its Miri tests
exercise). In **lossless** mode this edge is fully sufficient for data-race
freedom (the writer never overwrites in-flight bytes, exactly as the disruptor),
so reads can borrow. In **overwrite** mode the writer may race a reader on the
same bytes, so the edge is the publication mechanism but reads additionally need
the race-tolerant copy + lap-recheck — see §6 and §12.

**Lap test.** A view at `r` is lapped iff

```
committed - r > capacity
```

The oldest byte still intact is `committed - capacity`; while `r >= committed -
capacity` (equivalently `committed - r <= capacity`) the bytes at `r` have not
yet been overwritten. `gap` bytes from a wrap consume absolute cursor space just
like real bytes (the writer advances `committed` across them), so the test is
correct in the presence of wrap gaps.

**Worked wrap example** (`capacity = 104`, 8-byte payloads → 16-byte records:
8-byte header + 8-byte payload, already a multiple of 8 so no tail pad):

- Writer has published a long stream; `committed = 1040`.
- View A at `r = 960`: `1040 - 960 = 80 ≤ 104` → not lapped. Oldest intact byte
  is `936`; A can still read records at `960, 976, …`.
- View B at `r = 928`: `1040 - 928 = 112 > 104` → **lapped**. The physical slot
  `928 % 104 = 96` was overwritten when the writer passed `1032`. B's next read
  returns `Err(Lapped)`.
- A wrap gap example: with the writer at `committed = 96` (`phys = 96`), a
  16-byte record will not fit in the 8 tail bytes, so the writer sets
  `high_water_mark = 96`, leaves bytes `96..104` as an 8-byte gap, and writes the
  record at absolute `104..120` (`phys 0..16`). A reader whose cursor reaches `96`
  exactly skips to `104`, identical to the disruptor's `poll_grant` hwm path. The
  gap is a multiple of 8, so record-start alignment survives the wrap.

## 5. Write path

There is exactly one writer per buffer (each system owns its output buffer), so
the disruptor's `write_lock` mutex is unnecessary and is removed — its purpose
was to serialize multiple grant callers, and there is only one here. Removing it
also keeps the control block free of any in-region lock (a mutex is not
shared-memory-safe across processes). The writer keeps `committed`/`hwm`/
`reserved_end` purely as atomics.

The first steps are common to all modes. `write_message(bytes)`:

1. `len = bytes.len()`; `rec = 8 + round_up(len, 8)` (8-byte record header + tail
   pad). If `rec > capacity` → `Err(InsufficientCapacity)`.
2. `c = committed` (the writer owns it; Relaxed load suffices, no other writer).
   `phys = c % capacity`.
3. **Wrap check:** if `phys + rec > capacity`, set `gap = capacity - phys` (a
   multiple of 8), `start_abs = c + gap`; else `gap = 0`, `start_abs = c`.
   `need = gap + rec`.

What happens next depends on the buffer's overrun mode:

- **Overwrite mode (default):** skip straight to the publish. There is **no**
  `slowest_cursor` / in-use check; the writer proceeds regardless of reader
  cursors. This may write over bytes a slow reader has not consumed — intended,
  and what the lap test detects. (Key divergence from `try_grant`.)
- **Lossless mode — `try_write` (error/skip):** compute `slowest =
  slowest_active_cursor()` (the min cursor over non-free reader slots, like the
  disruptor's `slowest_cursor`) and `in_use = committed - slowest`. If `in_use +
  need > capacity` → `Err(WouldBlock)`; the writer drops the message. Otherwise
  proceed.
- **Lossless mode — `write().await` (wait):** the same in-use check, but instead
  of erroring the writer awaits a *space-available* notifier (the reverse-
  direction wake of §8, woken when a reader advances its cursor) and rechecks,
  exactly the pattern the disruptor's commented-out async `grant` wanted but
  without the mutex that made it deadlock-prone.

Then, common to all modes:

4. If `gap > 0`, store `high_water_mark = c` (Release) so readers skip the gap.
5. Write the 8-byte record header (`len`, reserved word) then the payload into
   `phys'..phys'+rec` where `phys' = start_abs % capacity`.
6. Publish: `committed.store(start_abs + rec, Release)`; then `wake.notify()`.

The grant form (`try_write(len) -> WriteGrant`) defers the `committed` publish to
grant drop, mirroring the disruptor's `WriteGrant::drop`; the record header is
written by the grant constructor and the caller fills the payload. In overwrite
mode a writer is wait-free with respect to readers (the only failure is an
over-capacity message); in lossless mode it can additionally return `WouldBlock`
or suspend.

## 6. Read path & views

A `View` owns one reader-table slot (index stored in the process-local handle)
and reads whole records. The common skeleton of `try_read_into(buf)`:

1. `r = slot.cursor` (Acquire). `c = committed` (Acquire).
2. **Pre-check lap:** if `c - r > capacity` → `Err(Lapped)`.
3. If `r >= c` → `Ok(false)` (nothing new; absolute counters make "caught up"
   unambiguous, as in the disruptor).
4. **Skip gap:** if `r == high_water_mark`, advance `r` to the next lap boundary
   `((r / capacity) + 1) * capacity` and continue (identical to the disruptor's
   `poll_grant` hwm branch).
5. Read `len` from the record header at `phys = r % capacity`; the payload is
   `bytes[phys+8 .. phys+8+len]`.
6. (overwrite mode only) **Post-validate**, then advance:
   `slot.cursor.store(r + 8 + round_up(len, 8), Release)`; `Ok(true)`.

The read-soundness contract for step 5 depends on the buffer's overrun mode, and
this is the crux of the writer-policy design:

**Lossless buffers (error/wait writers): tear-free, borrow is sound.** The writer
honors the in-use check, so it provably will not touch the bytes of a record a
reader still owns — the same guarantee that lets the disruptor hand out borrowed
`&[u8]`. Here `View` can offer a zero-copy `try_read()` returning `&[u8]` into the
ring: the Acquire load of `committed` establishes happens-before against the
writer's Release, and the writer's in-use check is the happens-after edge that
keeps the bytes stable until the reader advances. This is exactly the disruptor's
proven read path. `is_lapped` can never be true on such a buffer.

**Overwrite buffers (default): the writer races the reader, so copy + recheck.**
The writer may begin overwriting a record while a reader reads it, so a borrow is
unsound. The safe read **copies the payload out** using race-tolerant accesses
(relaxed atomic / `volatile` byte reads, not a plain `&[u8]` load — the bytes are
a genuine concurrent read/write), then **post-validates**: re-load `committed`
(Acquire); if `c2 - r > capacity` the writer lapped us *during* the copy and the
snapshot may be torn → `Err(Lapped)`, do not advance. Otherwise the copy is a
consistent snapshot and the cursor advances. This is the seqlock-style validate;
it is required *only* in overwrite mode. The exact mechanism (whole-buffer
recheck vs. a per-record version word) is the soundness decision in §12, and the
Miri story (below / §12) targets this path specifically.

`try_read_into` (copy) works on both kinds of buffer and is always safe — on a
lossless buffer the post-validate is a cheap no-op that never fires. The
borrowing `try_read() -> &[u8]` is offered only on lossless buffers (gated by the
header overrun mode); on an overwrite buffer a borrow remains an `unsafe`/advanced
escape hatch (§12). `View::read_into` is the async form: `wait().await` then
`try_read_into` in a loop.

`is_lapped(&self) -> bool` is step 2 standalone (`committed - cursor >
capacity`), and is what the coordinator calls before invoking a cyclic system on
an overwrite buffer. `committed()` / `cursor()` accessors let the coordinator or
a monitor process inspect lag without performing a read.

## 7. Reader registration in shared memory

The disruptor registers readers in a heap-allocated, lock-free Treiber list of
`Arc<ReadNode>` (`Readers` / `ArcAtomic`). **That structure cannot live in
shared memory:** it stores `Arc` pointers (process-local heap addresses) and
allocates nodes on the local heap; another process mapping the region would see
dangling pointers, and the `Arc` strong counts are meaningless cross-process.

Instead WP1 uses a **fixed-size reader table** (§3): `max_readers` slots at a
known offset, addressed by index. Registration mirrors the disruptor's *slot
reuse* idea (claim a free slot with a single CAS) but over a flat array instead
of a linked list:

- `view()`: scan slots `0..max_readers`; for each, CAS `cursor` from `FREE_SLOT`
  to the current `committed` (Acquire-load of `committed` first, so a new reader
  only sees data committed from now on — same rule as `Disruptor::reader()`). On
  success, bump `epoch`, return a `View` holding that slot index. If no slot is
  free → `Err(FullReaderTable)`.
- `View::drop`: `cursor.store(FREE_SLOT, Release)`, freeing the slot for reuse —
  a single wait-free store, like `Reader::drop`.

Trade-offs vs. the linked list: the maximum number of concurrent readers is
fixed at creation (must be sized for the worst case); registration is an
`O(max_readers)` scan; but there is **no allocation, no `Arc`, no pointers**, and
the whole thing is addressable by offset, which is exactly what shared memory
requires. The `epoch` word guards against ABA on reuse and is the hook for
crash-reclamation (§12).

## 8. Async wake abstraction

Async systems waiting on new data need to be woken when the writer commits. The
notifier is kept **out of the shared region** and behind a trait so the
mechanism can vary:

```rust
/// Writer side: signal that new data was committed.
pub trait WakeSource: Send + Sync {
    fn notify(&self);
}

/// Reader side: await the next notification.
pub trait WakeSink: Send + Sync {
    fn wait(&self) -> impl core::future::Future<Output = ()> + '_;
}
```

There are two notification directions: **data-available** (writer → readers, on
commit) and, for the lossless **wait** writer, **space-available** (readers →
writer, when a reader advances its cursor). Both use the same trait pair.

- **In-process (v1):** `WaitQueue`-backed notifiers (e.g. stellarator / maitake,
  the same primitive the disruptor's `new_data_queue` uses) implement both
  traits. The `Writer` holds a data-available `WakeSource` and (in wait mode) a
  space-available `WakeSink`; each async `View` holds a data-available `WakeSink`
  and (against a wait-mode writer) signals a space-available `WakeSource` when it
  advances. `View::read_into` is `wait().await` then `try_read_into` in a loop,
  mirroring `Reader::next`.
- **Cross-process: DEFERRED, room reserved.** A future impl backed by the
  reserved `wake_word` control slot plus a futex/eventfd/named-semaphore. The
  header `flags` bit and the `wake_word` slot reserve layout room for it without
  committing to a mechanism. **v1 does not implement cross-process wake** and is
  in-process only; a future out-of-process system can poll `try_read_into` until
  it is built. The `WakeSource`/`WakeSink` traits exist precisely so this drops
  in later without touching the shared layout.

Synchronous consumers (the coordinator polling cyclic inputs once per cycle) use
`try_read_into` / `is_lapped` and need no waker at all.

## 9. Proposed public API surface

Proposal, not implementation. Names and signatures are for review.

```rust
/// Pluggable backing storage. `Box` for in-proc/tests, mmap for shared.
pub trait Backing: Send + Sync {
    /// Base of the region. Interior-mutable byte storage (see §12 / Miri).
    fn base(&self) -> *mut u8;
    fn len(&self) -> usize;
}

/// Overrun policy, fixed at creation and recorded in the header. Determines the
/// read-soundness contract every view relies on, so it cannot be mixed.
pub enum Overrun {
    /// Default: writer never blocks, overwrites slow readers; reads copy+recheck.
    Overwrite,
    /// Writer honors the in-use check (error or wait per call); reads may borrow.
    Lossless,
}

pub struct Config {
    pub capacity: usize,    // data-region bytes (multiple of 8)
    pub max_readers: usize, // reader-table slots (over-provision; see §12)
    pub overrun: Overrun,
}

pub struct RingBuffer<B: Backing> { /* process-local handle: backing + cached header */ }

impl RingBuffer<BoxBacking> {
    pub fn create_in_memory(cfg: Config) -> Self;          // zeroes + writes header
}
impl RingBuffer<MmapBacking> {
    pub fn create_mmap(path: &Path, cfg: Config) -> std::io::Result<Self>;
    /// # Safety: caller asserts the file is a valid, same-arch region.
    pub unsafe fn attach_mmap(path: &Path) -> std::io::Result<Self>;
}

impl<B: Backing> RingBuffer<B> {
    pub fn writer<W: WakeSource>(&self, wake: W) -> Writer<'_, B, W>; // one per buffer
    pub fn view<S: WakeSink>(&self, wake: S) -> Result<View<'_, B, S>, FullReaderTable>;
    pub fn overrun(&self) -> Overrun;
    pub fn committed(&self) -> u64;
}

pub struct Writer<'r, B: Backing, W: WakeSource> { /* … */ }
impl<'r, B: Backing, W: WakeSource> Writer<'r, B, W> {
    /// Overwrite mode: always Ok unless the message exceeds capacity.
    /// Lossless mode: Err(WouldBlock) if it would overwrite the slowest reader.
    pub fn try_write_message(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
    pub fn try_write(&mut self, len: usize) -> Result<WriteGrant<'_>, WriteError>;

    /// Lossless mode: suspend until space frees, then write. Overwrite mode:
    /// resolves immediately (never actually suspends).
    pub async fn write_message(&mut self, bytes: &[u8]) -> Result<(), WriteError>;
    pub async fn write(&mut self, len: usize) -> Result<WriteGrant<'_>, WriteError>;
}

pub struct View<'r, B: Backing, S: WakeSink> { /* owns a reader slot index */ }
impl<'r, B: Backing, S: WakeSink> View<'r, B, S> {
    /// True iff `committed - cursor > capacity`. Always false on a lossless buffer.
    pub fn is_lapped(&self) -> bool;
    pub fn cursor(&self) -> u64;
    pub fn committed(&self) -> u64;

    /// Copy the next record into `buf` (safe on both kinds of buffer).
    /// `Ok(true)` = a record was read, `Ok(false)` = caught up,
    /// `Err(Lapped)` = overwritten (overwrite buffers only), stop reading.
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError>;

    /// Await + copy one record (async systems).
    pub async fn read_into(&mut self, buf: &mut Vec<u8>) -> Result<(), ReadError>;

    /// Zero-copy borrow of the next record. Available ONLY on a lossless buffer
    /// (where the writer cannot overwrite a borrowed record); returns
    /// `Err(BorrowNotSupported)` on an overwrite buffer. The borrow holds the
    /// cursor until dropped/consumed.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError>;
}

pub enum WriteError { InsufficientCapacity, WouldBlock }
pub enum ReadError  { Lapped, BorrowNotSupported }
pub struct FullReaderTable;
```

## 10. Differences from the db disruptor

| Aspect | `metor-db` disruptor | `metor-fsw-2` ring (proposed) |
|---|---|---|
| Slow-reader handling | **Backpressure only**: `try_grant` returns `WouldBlock` | **Writer-chosen**: overwrite (default), or lossless error/wait |
| Writer blocked by readers | Always (`slowest_cursor` in-use check) | **Never in overwrite**; lossless error/wait reuses the in-use check |
| Read soundness | Borrow always safe (writer never overwrites in-flight) | **Borrow safe in lossless**; **copy + lap-recheck in overwrite** |
| Reader registry | Heap **Treiber linked list** of `Arc<ReadNode>` | **Fixed array** of slots, claimed by CAS, addressed by index |
| Internal references | `Arc`/`Box`/`AtomicPtr` (process-local pointers) | **Relative offsets only**; no pointers in the region |
| Backing storage | `Box<[UnsafeCell<u8>]>` only | **`Backing` trait**: `Box` (in-proc) or `mmap` (shared) |
| Cross-process | No | **Yes** (single mapped region, same layout for both) |
| Record framing | caller's responsibility (raw bytes) | `[len u32][pad u32][payload][tail pad]`, **8-byte aligned** |
| Writer serialization | In-region `Mutex` (`write_lock`) | **None** (single writer per buffer; no in-region lock) |
| Wake mechanism | `WaitQueue` field baked into the core | **`WakeSource`/`WakeSink` traits**, kept out of the shared region |
| Reader count visibility | `reader_count()` walks the list | Scan the fixed table; cursors observable cross-process |
| Reused techniques | — | **Absolute cursors, `committed` Release/Acquire, `high_water_mark` wrap gap, slot-reuse-by-CAS** all carry over |

## 11. Crate placement proposal

- **Location:** `libs/metor-fsw-2/ring/` as its own crate.
- **Package name:** `metor-fsw-ring` (consistent with the `metor-fsw-*` family;
  the `-2` is a workspace-path detail, not a published name).
- **Edition / version:** `edition = "2024"`, `version.workspace = true`,
  `repository.workspace = true`, matching `libs/db`.
- **Dependencies (minimal):** `memmap2` (already in tree via `libs/db`) behind an
  `mmap` feature so the in-memory `Box` backing has no mmap dependency;
  optionally `stellarator`/`maitake` behind an `async` feature for the in-proc
  `WakeSource`/`WakeSink` impls. `thiserror` for the small error enums. No
  `metor-proto` dependency in WP1 — the ring is byte-oriented; framing of
  metor-proto tables is a layer above.
- **Workspace registration (proposal only):** add `"libs/metor-fsw-2/ring"` to
  the `members` list in the root `Cargo.toml`. (Not done in this doc — review
  first.)
- **Alternative considered:** a module inside a single `metor-fsw-2` core crate.
  Rejected for WP1 because a standalone crate keeps the unsafe shared-memory core
  small, independently Miri-testable (as `libs/db` does for the disruptor), and
  reusable by both the coordinator and out-of-process systems without pulling in
  the rest of the framework.

## 12. Open questions / risks for the reviewer

### Still open

1. **Overwrite-path data race & Miri (biggest item).** *Only the overwrite mode*
   has a genuine concurrent read/write on the same bytes (lossless mode is
   race-free exactly like the disruptor — no special handling needed). Two
   candidate fixes for overwrite reads: (a) a **whole-buffer recheck** seqlock
   (copy with relaxed/volatile byte reads, then re-validate the lap condition —
   §6), or (b) a **per-record version word** written before/after each record so
   a reader can detect a torn record locally. Which do we adopt? (b) is more
   local but costs bytes and a second store per record; (a) is cheaper and, since
   a single forward-only writer never rewrites the *same* absolute slot within a
   lap, (a) may suffice. Needs a decision plus a Miri test plan mirroring
   `libs/db/MIRI.md` (sync-only tests, `UnsafeCell` backing, Tree Borrows pass),
   focused on the overwrite path.
2. **Sizing `max_readers` and `capacity`.** Both are fixed at creation. How are
   they chosen/configured per buffer, and what happens when `view()` returns
   `FullReaderTable` — hard error like a lap?
3. **Power-of-two capacity.** Mandating a power-of-two `capacity` turns `% cap`
   into a mask and keeps `phys` aligned across the `u64` cursor wrap at 16 EiB.
   The disruptor allows arbitrary `cap` and ignores the 16 EiB wrap; do we
   mandate power-of-two here? (Independent of that, `capacity` must be a multiple
   of 8 for record alignment.)
4. **Overwrite-mode borrow.** The lossless zero-copy borrow (`try_read -> &[u8]`)
   is decided and in the API. Is a borrowing read on an *overwrite* buffer (where
   the caller must re-check `is_lapped` before trusting the slice) worth offering
   as an `unsafe`/advanced escape hatch, or do we keep overwrite reads copy-only?
5. **Cross-arch / endianness.** The layout assumes same-architecture shared
   memory (little-endian, same atomic widths). Is heterogeneous sharing ever in
   scope, or do we assert single-arch in the `magic`/`version` handshake?
6. **Writer process death mid-record (lower priority for v1).** `committed` gates
   readers off uncommitted bytes, so a dead writer just stalls the stream. Should
   an *attaching* replacement writer resume from `reserved_end`/`committed`, and
   how is a dead writer detected? **Lower priority** because v1 is in-process: a
   crashed writer takes the coordinator with it. Revisit when out-of-process
   writers land.

### Decided / deferred (recorded here so the reviewer sees them resolved)

- **Record alignment — DECIDED:** records are 8-byte aligned (8-byte header +
  tail pad, §3); payloads map as `repr(C)` frames in place. (Was an open
  question; now closed.)
- **Crash-slot reclamation — DEFERRED for v1.** A reader that crashes leaks its
  `CLAIMED` slot. v1 is in-process, so a crashed reader takes the process down;
  we simply **over-provision `max_readers`**. The `epoch` word and a future
  owner-pid/liveness sweep remain available; no v1 mechanism.
- **Cross-process wake — DEFERRED for v1.** v1 is in-process (`WaitQueue`
  notifiers). The reserved `wake_word` control slot and `flags` bit keep layout
  room, and the `WakeSource`/`WakeSink` traits keep the seam, but no shared-memory
  notification is implemented in v1.

## Implementation Plan

Design gate passed; this is the build plan for the v1 crate. Decisions applied:
overwrite reads use a **whole-buffer lap recheck** (not a per-record version
word); `Overrun::{Overwrite, Lossless}` is per-buffer in the header; records are
8-byte aligned; **capacity is mandated power-of-two** (`% cap` → `& mask`); attach
**asserts single-architecture** via a magic/version/`arch_tag` handshake.
Crash-slot reclamation and cross-process wake are deferred (room reserved).

Steps:

- **Crate scaffold.** `libs/metor-fsw-2/ring`, package `metor-fsw-ring`,
  `edition = "2024"`, `version.workspace`/`repository.workspace`. Features:
  `mmap` (memmap2), `async` (stellarator `WaitQueue` notifier), `async` on by
  default. Register in the root `Cargo.toml` members.
- **Backing.** `unsafe trait Backing { fn base(&self) -> *mut u8; fn len(); }`
  with `BoxBacking` (`Box<[UnsafeCell<u64>]>`, 8-byte aligned, default) and
  `MmapBacking` (behind `mmap`). Atomics are formed on demand as `&AtomicU64` /
  `&AtomicU8` at fixed offsets re-derived from `base()` (keeps provenance fresh,
  like the disruptor's `byte_ptr`).
- **Layout + header handshake.** Header / control block / reader table / data
  region at the §3 offsets; `create_*` writes magic/version/`arch_tag`/flags and
  inits cursors to `FREE_SLOT`; `attach_mmap` validates them (rejects mismatched
  arch/endianness).
- **Cursor core.** Absolute `u64` cursors, `phys = abs & mask`, `committed`
  Release/Acquire, `high_water_mark` wrap gap (multiple of 8). Ported from the
  disruptor.
- **Writer.** `try_write(&[u8])` (overwrite: always Ok bar oversize; lossless:
  `WouldBlock` on the `slowest_active_cursor` in-use check) and async
  `write(&[u8])` (lossless waits on a space-available notifier). Overwrite path
  stores the record via relaxed atomics (8-byte header as `AtomicU64`, payload as
  `AtomicU8`); lossless path stores plainly.
- **View + reader table.** CAS-claim a free slot (Acquire-load `committed` as the
  start), free on drop. `try_read_into`/`read_into` copy (overwrite: atomic copy +
  post-`committed` lap recheck → `Err(Lapped)`; lossless: plain copy). `try_read`
  returns a tear-free `&[u8]` `ReadGrant` (lossless only; `BorrowNotSupported`
  otherwise). `is_lapped`, `cursor`, `committed`, `resync` (skip to `committed`
  after a lap).
- **Wake.** `WakeSource`/`WakeSink` traits, `NoWake` (sync-only), and a
  feature-gated `Notifier` (`Arc<WaitQueue>`) for in-proc data- and
  space-available signalling.
- **Unsafe discipline.** SAFETY comments on every unsafe block; a `// SAFETY:`
  invariant block on the `Inner` Sync story, mirroring `disruptor.rs`.

Test list (sync tests Miri-runnable per `libs/db/MIRI.md`: `UnsafeCell` backing,
sync APIs + `std::thread`, Tree Borrows):

- `roundtrip` — write/read a few records, exact bytes back.
- `wraparound_aligned` — small power-of-two cap with a payload that forces an
  8-byte wrap gap; reader skips the gap and reconstructs the stream.
- `multi_reader` — independent cursors each see the full stream.
- `reader_table_claim_free` — claim/free slots, `FullReaderTable` when exhausted.
- `overwrite_slow_reader_lapped` — overwrite buffer, a reader left behind gets
  `Err(Lapped)` (never torn/garbage); earlier in-range reads returned correct
  bytes.
- `lossless_backpressure` — lossless `try_write` returns `WouldBlock` rather than
  lapping; after a read frees space the next write succeeds.
- `concurrent_overwrite_no_ub` (Miri race coverage) — writer thread overwrites
  fast, reader copies + rechecks, every non-lapped record parses as a valid
  value, `resync` on lap.
- `concurrent_lossless_lossless` (Miri) — lossless writer (spin on `WouldBlock`)
  + reader reconstruct the full ordered stream with zero loss (the disruptor's
  guarantee).
- `concurrent_reader_churn` (Miri) — register/drop views under a running writer;
  table returns to all-free.
- `wait_writer` (async, `cfg(not(miri))`) — lossless `write().await` suspends and
  resumes as a reader drains; no data lost.
- `mmap_roundtrip` (`cfg(feature = "mmap")`) — create + attach an mmap region,
  write/read across the handle.
