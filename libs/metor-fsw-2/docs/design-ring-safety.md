# Ring safety fixes (`metor-fsw-ring`) — design

> **Status: LANDED** — implemented in commit `07406cfc` ("fix(ring): wave-2 — seqlock
> torn-read fix, writer claim, lossless registration race, 32-bit soundness").
> `docs/ring-buffer.md` reflects the shipped shape.
>
> **Since superseded in part**: the overwrite mode (and with it the R1 seqlock
> reservation and the B6 lap semantics this doc designs) was later **removed** —
> the ring is lossless-only (see `docs/delete-overrun-plan.md`). The lossless-mode
> analysis here (R6/R7/R3/R4) still describes the shipped code.

Design for the ring-crate items from `docs/review-findings.md` §1–§2: **R1**
(overwrite-mode torn read), **R6** (lossless view-registration race), **R7**
(single-writer enforcement), **R3** (32-bit OOB on garbage length), **B6**
(spurious lap on a wrap-gap start), **R8** (reader-claim ordering note), and
**R4** (attach geometry validation). Decision input: **lossless mode and the
`mmap` feature stay** — every fix below preserves both modes' semantics.

All line references are `ring/src/lib.rs` at `69b59405`.

---

## 0. Cross-cutting: layout, version, and the invariant ledger

Two control words change meaning, one is added:

```text
// Control block (cache line 1).
const OFF_COMMITTED:    usize = 0x40; // AtomicU64 (unchanged)
const OFF_HWM:          usize = 0x48; // AtomicU64 (unchanged)
const OFF_RESERVED_END: usize = 0x50; // AtomicU64 — NOW LIVE: seqlock write reservation (R1)
const OFF_WAKE_WORD:    usize = 0x58; // AtomicU64 (still reserved for cross-proc wake)
const OFF_WRITER:       usize = 0x60; // AtomicU64 — NEW: writer claim, 0 = free (R7)
```

`HEADER_SIZE` (0x80) is unchanged; 0x60 was already inside the zero-initialized
control cache line. **Bump `VERSION` to 2.** This is not optional: a v1 writer
never stores `reserved_end`, so a v2 reader attached to a v1 region over mmap
would see `reserved_end == 0` and report every record `Lapped` forever
(`0u64.wrapping_sub(r)` is huge), and a v1 build ignores the writer claim.
Regions are ephemeral IPC state, not archives, so rejecting cross-version
attach (`AttachError::BadVersion`) is the correct failure mode.

Post-fix control-word invariants (writer-only stores, all monotonic):

1. `committed <= reserved_end` at every instant (writer stores `reserved_end`
   strictly before the record bytes, `committed` strictly after).
2. A byte at absolute position `p` is only ever stored by a write whose
   reservation satisfies `reserved_end > p` *and* whose reservation store is
   fence-ordered before that byte store (R1).
3. In lossless mode, no write's `end_abs` exceeds `slowest_cursor + capacity`
   where `slowest_cursor` is over every claim the write's scan observed; the
   R6 handshake guarantees a scan that misses a new claim is harmless.
4. `OFF_WRITER != 0` ⇔ a live `Writer` exists (or a crashed one leaked the
   claim — see R7 reclamation).

`init_region` explicitly writes `OFF_WRITER = 0` (it is already zeroed by both
backings, but the init list stays exhaustive).

---

## 1. R1 — overwrite-mode torn read: seqlock pre-write reservation

### The bug

`try_read_into`'s post-copy recheck (lib.rs:1001–1008) compares the reader's
record start `r` against **published** `committed`. `write_record` scribbles
data bytes *before* the `committed` Release store (lib.rs:917–928). With the
writer exactly one lap ahead (`committed == r + capacity`, legal), its next
write targets `phys(r)`; the reader copies concurrently, rechecks against the
still-unpublished `committed`, and returns `Ok(true)` with an old/new byte mix.

### Change

**Writer** — `Writer::commit` gains a reservation before `write_record`
(unconditionally, both modes, to keep invariant 0.1 uniform):

```rust
unsafe fn commit(&self, committed: u64, start_abs: u64, gap: u64, bytes: &[u8]) {
    let rec = frame_len(bytes.len()) as u64;
    // Seqlock begin: reserve [start_abs, start_abs + rec) before touching data.
    // The Release *fence* (not a Release store — see ordering note) orders this
    // store before the relaxed data stores as observed by any reader that sees
    // any of those data bytes and then runs its Acquire fence.
    self.inner.reserved_end().store(start_abs + rec, Relaxed);
    std::sync::atomic::fence(Release);
    let phys = (start_abs & self.inner.mask) as usize;
    unsafe { self.inner.write_record(phys, bytes) };
    if gap > 0 {
        self.inner.hwm().store(committed, Release);
    }
    self.inner.committed().store(start_abs + rec, Release);
    self.data.notify();
}
```

(`Inner::reserved_end()` is a new accessor like `committed()`/`hwm()`.)

**Reader** — `try_read_into`'s recheck compares against `reserved_end`, with
an Acquire fence between the payload loads and the recheck load:

```rust
if self.inner.overrun == Overrun::Overwrite {
    // Seqlock validate: any writer that touched a byte of this record must
    // have reserved past r + capacity first; the fence pair makes that
    // reservation visible here if any of our relaxed payload loads saw its data.
    std::sync::atomic::fence(Acquire);
    let re = self.inner.reserved_end().load(Relaxed);
    if re.wrapping_sub(loc.r) > self.inner.capacity {
        return Err(ReadError::Lapped);
    }
}
```

### Exact ordering requirements (and why a Release *store* is not enough)

A `Release` store on `reserved_end` orders *prior* accesses before itself; it
does **not** prevent the *subsequent* relaxed data stores from becoming visible
first. Symmetrically, an `Acquire` load of `reserved_end` does not stop the
*earlier* payload loads from being satisfied late. Both reorderings reopen the
window. The correct shape is the fence-to-fence seqlock (Boehm's pattern):

- Writer: `reserved_end.store(v, Relaxed)`; **`fence(Release)`**; relaxed data
  stores. Per C++ `[atomics.fences]`, if a reader's relaxed load reads a value
  written by a store sequenced after the release fence, the release fence
  synchronizes-with the reader's acquire fence.
- Reader: relaxed header/payload loads; **`fence(Acquire)`**; `reserved_end`
  load (`Relaxed` suffices; the fence carries the ordering).

**Tear-new direction.** Suppose the copy contains a byte from a write `W` that
overlaps the record at `r`. Any byte of `W` overlapping `[r, r+rec)` sits at
absolute `p ≥ r + capacity`, and `W`'s reservation is `re_W = start_W + rec_W >
p ≥ r + capacity`. The reader's payload load read from one of `W`'s data
stores, which are sequenced after `W`'s release fence, which is sequenced after
the `re_W` store — so the fence pair puts the `re_W` store happens-before the
reader's recheck load, and coherence (single monotonic location) forces the
recheck to observe `reserved_end ≥ re_W > r + capacity` → `Err(Lapped)`.
Contrapositive: recheck passes ⇒ no copied byte came from any overlapping
write.

**Tear-old direction** (a byte older than the record itself) is already
excluded by the existing `committed` Release/Acquire handshake: `locate` only
returns records below an Acquire-loaded `committed`, and the record's own
stores are sequenced before that Release store. Both directions together give
tear-freedom.

**No false negatives from `wrapping_sub`:** `reserved_end` is monotonic and
`reserved_end ≥ committed ≥ r` for any record `locate` returns, so the
subtraction never wraps in a passing case.

**Conservatism is bounded:** `reserved_end - r > capacity` can fire while the
record is still physically intact (writer reserved but has not yet reached
`phys(r)`). That is the same "about to be overwritten" semantic `Lapped`
already carries and is indistinguishable from a lap one instruction later. At
steady state (writer idle) `reserved_end == committed`, so a non-lapped reader
never sees a spurious `Lapped`.

### Interaction with the wrap gap / hwm store

`start_abs` already includes the gap skip, so the reservation
`[start_abs, start_abs + rec)` covers exactly the bytes `write_record` touches;
gap bytes are never stored and need no reservation. The `hwm` Release store
stays where it is (before the `committed` Release store, so any reader that
sees the new `committed` sees the gap marker). The reservation store commutes
with `hwm` ordering because readers never read data based on `hwm` alone.

### `is_lapped()`

Switch it from `committed` to `reserved_end` (one load, same cost):

```rust
let re = self.inner.reserved_end().load(Acquire);
re.wrapping_sub(r_eff) > self.inner.capacity   // r_eff: see B6
```

Not strictly required for soundness — `is_lapped` is advisory and a stale
`false` is always caught by the read-path recheck — but it makes the
coordinator's pre-step check catch an in-flight overwrite instead of stepping a
system that will immediately fail, and it keeps one lap definition crate-wide.

`locate`'s pre-copy lap check (lib.rs:1065) **stays on `committed`**: it is a
fast-path filter, the post-copy recheck is the authority, and keeping it on
`committed` avoids a second Acquire chain in the loop.

### Cost on the hot write path

One `Relaxed` store + one `fence(Release)` per write; one `fence(Acquire)` +
one `Relaxed` load per successful overwrite-mode read.

- x86-64: both fences are compiler-only (no instruction); net cost ≈ one store
  + one load to a hot, writer-owned cache line (`reserved_end` shares cache
  line 1 with `committed`, which the writer already owns exclusive).
- aarch64: one `dmb ish` on the write path and one `dmb ishld` on the read
  path, roughly doubling barrier count per record vs. today's single
  `stlr`/`ldar` pair. Acceptable: the alternative (making every data store
  Release) is strictly worse.

---

## 2. R6 — lossless view-registration race

### The bug

`view()` (lib.rs:710–737) loads `start = committed`, then CAS-claims a slot.
Until the writer's `fits()` scan observes the claim, the check is vacuous and
the writer may lap past `start + capacity`. The lossless `locate` path has no
lap or straddle check, so `read_len` over overwritten bytes yields an arbitrary
length → OOB `from_raw_parts`/`copy_nonoverlapping` from safe code. There is
also a plain store-buffer variant: with only Release/Acquire, the reader's
cursor store and the writer's `committed` store can both sit unobserved while
each side loads the other's stale value.

### Why release/acquire alone is insufficient (store-buffer case)

This is Dekker's pattern on two locations: reader does
`store(cursor); load(committed)`, writer does `store(committed); load(cursor)`.
Release stores and Acquire loads only build ordering *when the load reads the
store*; nothing forbids the outcome where **both** loads read the older values
(each store still in its core's store buffer — StoreLoad reordering, explicitly
allowed on x86 and by the C++ model). Then the writer's scan misses the reader
*and* the reader's recheck sees a stale `committed`: both sides conclude the
other is absent. Only `SeqCst` fences (totally ordered with each other) exclude
the both-miss outcome: whichever fence is later in the total order forces its
side's load to observe the other side's earlier store.

### Change

**Writer side** — one `SeqCst` fence at the top of `slowest_active_cursor()`
(lib.rs:423), which is the cursor scan used by `fits()` and the async wait
predicate, and is only called on lossless paths:

```rust
fn slowest_active_cursor(&self) -> Option<u64> {
    // SeqCst: pairs with the registration fence in `view()`. Sits between this
    // writer's previous `committed` store and this scan, so for every write W:
    // either W's scan observes a new reader's claim, or that reader's
    // registration recheck observes committed_{W-1} (see view()).
    std::sync::atomic::fence(SeqCst);
    ...
}
```

**Reader side** — `view()` gains a post-CAS stabilization loop, **lossless mode
only** (overwrite-mode reads self-validate via R1, and the loop would add
useless registration latency under a fast writer):

```rust
let mut start = self.inner.committed().load(Acquire);
for slot in 0..self.inner.max_readers {
    if self.inner.slot_cursor(slot)
        .compare_exchange(FREE_SLOT, start, AcqRel, Relaxed)   // AcqRel: see R8
        .is_ok()
    {
        self.inner.slot_epoch(slot).fetch_add(1, Release);
        if self.inner.overrun == Overrun::Lossless {
            // Registration handshake: loop until the claim is provably stable.
            loop {
                std::sync::atomic::fence(SeqCst);
                let c2 = self.inner.committed().load(Acquire);
                if c2 == start { break; }
                // The writer committed while our claim may not have been
                // visible to its scan; those writes were validated without us.
                // Advance the claim to the new edge and re-verify. "A fresh
                // view only sees data committed from now on" makes this a
                // semantic no-op.
                start = c2;
                self.inner.slot_cursor(slot).store(start, Release);
            }
        }
        return Ok(View { ... });
    }
}
```

**Why `c2 == start`, not the weaker `c2 - start > capacity` bail:** the weaker
check has a residual hole. Let the writer be exactly one lap ahead when the
reader claims (`committed_N - start == capacity`, passes `≤ capacity`), and let
write `N+1`'s scan have missed the claim (allowed when write `N+1`'s fence
precedes the reader's fence in the SeqCst total order — the reader is then only
guaranteed to observe `committed_N`, not `N+1`'s existence). Write `N+1` starts
at `committed_N = start + capacity ≡ phys(start)` and scribbles the exact bytes
the new view will borrow — UB again. Requiring a *stable* `committed` closes
it, per the argument below.

### Invariant argument

Claim: when `view()` returns with final cursor `s`, every write `W` (past,
in-flight, or future) satisfies **either** (a) `W`'s cursor scan observed a
claim `≥ s`'s slot value at scan time, so `fits()` bounds
`end_W ≤ slowest + capacity ≤ cursor + capacity`, **or** (b)
`end_W ≤ s + capacity` anyway. Take any `W` whose scan missed the claim. `W`'s
scan `L_w` is sequenced after `W`'s SeqCst fence `F_w`, which is sequenced
after the `committed_{W-1}` store. The reader's final iteration ran a SeqCst
fence `F_r` after its last cursor store (value `s`) and then loaded
`committed == s`. In the fence total order either `F_r < F_w` — then `L_w` must
observe the cursor store of `s` (contradiction, the scan did not miss it) — or
`F_w < F_r` — then the reader's load observes `committed_{W-1}` or later, so
`s ≥ committed_{W-1}`, and `fits()` against the remaining readers (or the
writer itself) bounds `end_W ≤ committed_{W-1} + capacity ≤ s + capacity`. ∎

An in-flight unseen write `W` with `end_W ≤ s + capacity` cannot corrupt a
readable record: the reader only reads records at `r ∈ [s, c)` with
`c ≤ committed_{W-1}` until `W` commits, `W`'s byte range
`[start_W, end_W) ⊆ [committed_{W-1}, s + capacity)` is disjoint from `[s, c)`
in absolute terms, and mod-`capacity` aliasing needs `p ≥ r + capacity ≥
s + capacity > end_W`. After registration, the steady-state invariant is
maintained by (a): every later scan observes the (monotonically advancing)
cursor.

Termination: each extra iteration requires the writer to have committed inside
a ~3-instruction window; in practice the loop runs once or twice. Registration
is a cold path; no iteration bound is imposed (documented).

### Lossless locate hardening (defense in depth)

Make the length/straddle validation in `locate` **unconditional** (today gated
on `Overwrite`, lib.rs:1083). Combined with R3's u64 math:

```rust
let len = unsafe { self.inner.read_len(phys) } as u64; // ≤ 0xFFFF_FFFF
if len > cap - 8 - phys as u64 {
    return Err(match self.inner.overrun {
        Overrun::Overwrite => ReadError::Lapped,       // existing meaning
        Overrun::Lossless => ReadError::Corrupt,       // new variant
    });
}
```

New variant `ReadError::Corrupt`: "the region violated a structural invariant
(record straddles the wrap or overruns the data region); possible external
corruption — stop reading." Post-R6 it is unreachable from crate behavior;
it exists so a corrupted shared mapping degrades to an error instead of an OOB
borrow. (`phys ≤ cap - 8` always: record starts are 8-aligned and `< cap`, so
the subtraction cannot underflow.)

### Cost

One `SeqCst` fence per lossless write attempt (`mfence`/`dmb ish`) — lossless
is not the framework's hot path (the framework only creates `Overwrite` rings
today). Zero cost on overwrite-mode paths. Registration gains one fence + one
load per stabilization iteration.

---

## 3. R7 — single-writer enforcement

### Change

New control word `OFF_WRITER` (0x60, cache line 1): `0` = free, `1` = claimed.

```rust
/// A writer already exists for this buffer (or a crashed process leaked its
/// claim — see [`RingBuffer::force_release_writer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterClaimed;

impl<B: Backing> RingBuffer<B> {
    pub fn writer<WD: WakeSource, WS: WakeSink>(
        &self, data: WD, space: WS,
    ) -> Result<Writer<B, WD, WS>, WriterClaimed> {
        self.inner
            .writer_claim()
            .compare_exchange(0, 1, Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer { inner: self.inner.clone(), data, space })
    }

    /// Forcibly release a leaked writer claim.
    ///
    /// # Safety
    /// The caller asserts the claiming writer no longer exists (its process
    /// crashed or its `Writer` was leaked) and no store of its is still in
    /// flight. Calling this while the writer is alive re-creates the very
    /// two-writer race this word exists to prevent.
    pub unsafe fn force_release_writer(&self) {
        self.inner.writer_claim().store(0, Release);
    }
}

impl<B: Backing, WD: WakeSource, WS: WakeSink> Drop for Writer<B, WD, WS> {
    fn drop(&mut self) {
        self.inner.writer_claim().store(0, Release);
    }
}
```

Orderings: claim CAS success = `Acquire` so the new writer's accesses are
ordered after it observes the release; the previous writer's `Drop` store is
`Release`, so drop→claim forms a synchronizes-with edge handing the whole
region state (committed/hwm/reserved_end and data bytes) to the successor.
Failure ordering `Relaxed` (no state is read on failure).

### Decisions

- **`Result`, not panic.** Matches `view()`'s `Result<_, FullReaderTable>`
  precedent, and in the dlopen/attach world a claimed writer is a *reachable
  runtime condition* (stale claim after a crash), not only a programming error.
  Framework call sites add `.expect("single ring writer")` — for them it *is*
  a programming error and the panic message says so.
- **New control word, not reused reserved space.** `OFF_RESERVED_END` is now
  live for R1 and `OFF_WAKE_WORD` stays reserved for the cross-process wake;
  0x60 is free in the control cache line, costs no layout growth, and keeps
  all mutable words on cache line 1 (the header line stays immutable).
- **Claim value is `1`, not a PID.** PID-based liveness (`kill(pid, 0)`) is
  platform-specific, racy under PID reuse, and meaningless for two writers in
  one process. Not worth carrying in the layout.
- **Crash reclamation: out of scope for v1, with an escape hatch.** This
  matches the crate's existing stance for reader slots ("v1 has no crash-slot
  reclamation", `Config::max_readers` doc). The `unsafe force_release_writer`
  covers the concrete near-term story — the coordinator/host *knows* when a
  dlopen occupant it supervised died and can reclaim before re-attaching. Full
  cross-process crash detection (robust-futex-style) is future work, noted next
  to `OFF_WAKE_WORD`.

### Interaction with `attach_mmap` / `attach_raw`

The claim lives in the region, so enforcement is naturally cross-handle and
cross-process: a second process attaching to a claimed region gets
`Err(WriterClaimed)`. A crashed process leaves the claim set — the attaching
supervisor uses `force_release_writer` (see above). Same-process
`attach_raw` swap (the `raw_attach_swap_reacquire` pattern) needs no change:
occupant teardown drops its `Writer`, freeing the claim before the next attach.

### Call-site churn (informational, for the implementation pass)

`ring/src/tests.rs` (~15 sites) and `metor-fsw-2` proper: `src/port.rs:91`,
`src/message.rs:125`, `src/coordinator/slot.rs:563`,
`src/coordinator/mod.rs:1191,1397–1400,1505` are one-writer-per-ring and take
`.expect(...)`. Two genuine conflicts:

1. **`Coordinator::control_handle()` (`src/coordinator/mod.rs:1670`)** mints a
   fresh writer over `command_ring` per call — under R7 the second mint fails.
   This is the A2 finding's "only writer-side invariant violation" surfacing.
   Needs a decision (open question below); recommended interim: create the
   command `MsgOut` once at `build()` and have `control_handle()` hand out
   access to that single writer (e.g. return it once via `Option::take`, or
   wrap in a `Mutex`), pending the A2 command-plane refactor.
2. **`src/message.rs` tests (`:363–364, :431–432`)** create two `MsgOut`
   writers over one ring; rewrite to sequential scopes or two rings.

---

## 4. R3 — 32-bit OOB: u64 length/record math

### The bug

`read_len` returns `usize`; under an overwrite lap race it can return payload
bytes as a length. On a 32-bit target, `frame_len(0xFFFF_FFFF)` wraps
(`round_up8` overflows) to `rec = 8` in release, the straddle check passes, and
`copy_payload` walks ~4 GiB out of bounds. 64-bit is sound (verified in the
review) because the u32-truncated length cannot overflow 64-bit math.

### Change

Do all record math in `locate` in `u64`, and validate the payload length
against the remaining data region *before* constructing `Located`:

```rust
/// Read a record's length field at `phys`. Returns the raw u32 as u64; the
/// caller must validate it before any usize conversion or pointer math.
unsafe fn read_len(&self, phys: usize) -> u64 { ... (hdr & 0xFFFF_FFFF) }

// in locate(), replacing lib.rs:1081-1090:
let len = unsafe { self.inner.read_len(phys) };            // u64
if len > cap - 8 - phys as u64 {                           // u64 compare, no overflow
    return Err(...);                                       // Lapped / Corrupt, see R6
}
let len = len as usize;                                    // now ≤ cap ≤ usize::MAX
let rec = frame_len(len);                                  // fits: rec ≤ cap
```

`len > cap - 8 - phys` is exactly the old straddle predicate
`phys + rec > cap` rewritten overflow-free: `cap - phys - 8` is a multiple of 8
(record starts are 8-aligned, `phys ≤ cap - 8`), and for a multiple of 8 `m`,
`round_up8(len) > m ⇔ len > m`. After the check, `len`, `rec`, and every
derived offset fit `usize` on all targets (`capacity` originated as a `usize`).
`Located`'s field types are unchanged.

The writer path needs no change: `payload.len()` is a real slice length
(`≤ isize::MAX`), so `frame_len` cannot overflow there on any supported target.

### Invariant

After `locate` returns `Some(Located)`, `phys + 8 + len ≤ capacity` holds as
checked u64 arithmetic — every downstream `unsafe` (`copy_payload`,
`from_raw_parts`) inherits an in-bounds range even when the length field was
concurrently scribbled garbage (acceptance is then still gated by R1's
recheck; this fix only bounds the *copy*).

---

## 5. B6 — wrap-gap start: skip before the lap test

### The bug

Both lap checks (`locate` lib.rs:1065, `is_lapped` lib.rs:978) run before the
`r == hwm` gap skip. A reader parked exactly on a gap start has effective
cursor `lap_end = (r & !mask) + cap`, so for
`committed ∈ (r + cap, lap_end + cap]` it is declared lapped while its next
real record is intact — a spurious, unrecoverable hard-stop for a cyclic
system.

### Change

**`locate`** — reorder the loop body: load `r`, load `hwm`, perform the gap
skip, *then* evaluate lap/caught-up against the (possibly advanced) cursor on
the next iteration:

```rust
loop {
    let r = self.inner.slot_cursor(self.slot).load(Acquire);
    let hwm = self.inner.hwm().load(Acquire);
    if r == hwm {
        let lap_end = (r & !self.inner.mask) + cap;
        self.inner.slot_cursor(self.slot).store(lap_end, Release);
        self.space.notify();
        continue;                                  // re-evaluate at lap_end
    }
    let c = self.inner.committed().load(Acquire);
    if self.inner.overrun == Overrun::Overwrite && c.wrapping_sub(r) > cap {
        return Err(ReadError::Lapped);
    }
    if r >= c { return Ok(None); }
    ... // header read + validation (R3/R6), unchanged order
}
```

**`is_lapped`** — apply the same effective-cursor rule (combined with R1's
`reserved_end`):

```rust
let r = self.inner.slot_cursor(self.slot).load(Acquire);
let hwm = self.inner.hwm().load(Acquire);
let r_eff = if r == hwm { (r & !self.inner.mask) + cap } else { r };
let re = self.inner.reserved_end().load(Acquire);
re.wrapping_sub(r_eff) > cap
```

### Adversarial check: can the reorder mask a REAL lap?

Scenario: reader sits on `r == hwm`; the writer meanwhile laps for real.

- **Writer wrapped again (new gap published):** `hwm` values are the absolute
  `committed` at each wrap and strictly increase, so `hwm' > r` — the reader
  sees `r != hwm'`, takes no skip, and the ordinary lap check fires. A given
  `r` can match at most one gap, ever.
- **Writer lapped without wrapping again:** impossible past `lap_end + cap` —
  writing `capacity` bytes from `lap_end` reaches the next wrap boundary, whose
  `hwm'` store is Release-ordered *before* the `committed` store that would
  make `c` exceed `lap_end + cap`.
- **Stale-`hwm` skip:** the skip itself never accepts data — it only advances
  the cursor to `lap_end`, and the *next iteration* re-runs the lap check
  against `lap_end` with a fresh `committed`. If the writer truly lapped past
  `lap_end + cap`, that check returns `Lapped`; the skip merely cost one
  cursor store. If the reader sees a *fresh* `hwm'` equal to its own `r` before
  seeing the corresponding `committed` advance, then `r` was the committed
  value at wrap time (a caught-up reader) and the skip lands it exactly where
  the next record will appear — it then reports caught-up until `committed`
  publishes. All safe.

The skip stores a *larger* cursor before the lap verdict, which can only make
the verdict more lenient in exactly the cases where the gap proves the bytes at
`[r, lap_end)` were never data — the definition of not-lapped.

---

## 6. R8 — reader-claim CAS ordering + epoch invariant

Cheap-now change plus the documentation the future feature needs:

- Bump the claim CAS success ordering to **`AcqRel`** (`view()`, lib.rs:722):
  the Release half publishes the claim store itself, so any writer-scan
  Acquire load that reads it is downstream-ordered without leaning on the R6
  fences. Cold path; zero practical cost.
- Add the invariant comment at the claim site:

```rust
// Claim ordering: AcqRel — Acquire pairs with `View::drop`'s Release store of
// FREE_SLOT (slot-state handoff between successive owners); Release publishes
// the claim. NOTE: visibility of this claim to the *lossless writer's* fits()
// scan is guaranteed by the SeqCst registration handshake below, not by this
// CAS. The epoch word is a generation counter reserved for crash reclamation;
// it is written (Release) but never yet read. If reclamation is ever
// implemented, the reclaimer must bump the epoch *before* freeing the cursor,
// and every cursor store made through a View handle must be preceded by an
// epoch check (or become a CAS on (epoch, cursor)) — a plain Release store on
// a reclaimed slot would corrupt the new owner's cursor.
```

No layout or behavior change beyond the ordering token.

---

## 7. R4 — attach geometry validation

### Change

`from_validated` passes `backing.len()` down; `read_header` becomes

```rust
struct Geometry {
    capacity: u64,
    data_offset: usize,
    reader_table_offset: usize,
    max_readers: u32,
    overrun: Overrun,
}

/// # Safety
/// `base` points at a readable region of at least `region_len` bytes.
unsafe fn read_header(base: *mut u8, region_len: usize) -> Result<Geometry, AttachError>
```

and validates, in order (all arithmetic in `u64` via `checked_add`/`checked_mul`
so a hostile header cannot overflow its way past a bound):

1. `region_len >= HEADER_SIZE` → `TooSmall`. (Moves the existing `attach_raw`
   guard into the shared path so **`attach_mmap` gets it too** — today a
   sub-header-sized file is mapped and read out of bounds.)
2. `base as usize % 8 == 0` → `Misaligned`. Checked in `attach_raw` before
   `from_validated` (mmap and `BoxBacking` are aligned by construction, but the
   raw path takes an arbitrary pointer).
3. Magic, version, arch tag → `BadMagic` / `BadVersion` / `ArchMismatch`
   (existing; version now 2 per §0).
4. `capacity` is a nonzero power of two **and** `capacity <= usize::MAX as u64`
   (32-bit target attaching a 64-bit-sized region) → `BadGeometry`. This also
   makes `mask = capacity - 1` well-defined.
5. `max_readers > 0` → `BadGeometry`.
6. Reader table in bounds: `reader_table_offset >= HEADER_SIZE`,
   `reader_table_offset % 8 == 0`, and
   `reader_table_offset + max_readers * READER_SLOT_SIZE <= data_offset`
   (checked mul/add) → `BadGeometry`.
7. Data region in bounds: `data_offset % 8 == 0` and
   `data_offset + capacity <= total_size` (checked add) → `BadGeometry`.
8. `total_size <= region_len as u64` → `RegionTruncated` (the truncated-on-disk
   file case gets its own variant because it is the diagnosable real-world
   failure; all other inconsistencies collapse into `BadGeometry`).

```rust
pub enum AttachError {
    BadMagic,
    BadVersion,
    ArchMismatch,
    /// Region shorter than the fixed header.
    TooSmall,
    /// `attach_raw` base pointer not 8-byte aligned.
    Misaligned,
    /// Header fields are internally inconsistent (capacity not a nonzero
    /// power of two / doesn't fit this target, offsets overlap or overflow).
    BadGeometry,
    /// Header is self-consistent but `total_size` exceeds the backing region
    /// (e.g. a truncated file).
    RegionTruncated,
}
```

Behavior change: `attach_raw` on a sub-header region now returns `TooSmall`
instead of the repurposed `BadMagic` (test `raw_attach_bad_region_rejected`
updates accordingly).

### Invariant

After a successful attach, every offset the ring ever dereferences —
control words, all `max_readers` slots, `data_offset + phys` for
`phys < capacity` — is inside `[0, backing.len())` and 8-aligned, by checks
4–8. `attach` therefore restores the same geometry invariant `layout()` +
`init_region` establish at creation, which is what all the `SAFETY` comments
on `atomic_u64`/`data_ptr` assume.

---

## 8. Verification plan

Deterministic tests first: the past `libs/db` disruptor bug lost data under
free-running concurrent wrap, and free-running stress both misses interleavings
and flakes. `tests.rs` is a child module of `lib.rs`, so tests can reach
`rb.inner` — the writer's commit phases (reserve / scribble / publish) can be
hand-driven as separate steps for exact interleaving control, and a `View` can
be constructed field-by-field to plant adversarial cursor states.

### New unit tests (single-threaded, controlled interleaving; all Miri-clean)

- **R1 `torn_read_rejected_by_reservation`** — the exact reported window:
  fill an overwrite ring so `committed == r + capacity` (reader not lapped);
  then hand-emulate the first half of the next commit via `inner`
  (`reserved_end.store(committed + rec)`, `fence(Release)`, scribble the
  header+payload atomics at `phys(r)` with garbage) **without** storing
  `committed`. `try_read_into` must return `Err(Lapped)` (old code returns
  `Ok(true)` with torn bytes — this test fails before the fix, passes after).
  Then complete the commit, `resync()`, and read normally to prove recovery.
- **R1 `reservation_no_false_lap`** — steady state: after every normal write,
  `reserved_end == committed` and an up-to-date reader never sees `Lapped`.
- **R6 `lossless_garbage_length_is_corrupt`** — plant a `View` (constructed
  directly) with a stale cursor over a lossless ring whose bytes at that
  cursor were scribbled `0xFF`: `try_read`/`try_read_into` must return
  `Err(Corrupt)`, never build an OOB slice. This pins the defense-in-depth
  check independently of the registration fix.
- **R6 `lossless_view_starts_stable`** — `view()` on a quiescent lossless ring
  returns with `cursor() == committed()`; after heavy prior write/drain
  traffic, ditto (exercises the stabilization loop's convergence).
- **R7 `second_writer_rejected`**, **`writer_claim_freed_on_drop`**,
  **`writer_claim_shared_across_attach`** (claim via the `BoxBacking` handle,
  `attach_raw` the same region, `writer()` there → `Err(WriterClaimed)`; drop
  the first, the raw-side claim succeeds), **`force_release_writer_reclaims`**
  (leak a writer with `mem::forget`, force-release, re-claim).
- **R3 `garbage_length_bounded`** — write a record, poke its header length to
  `0xFFFF_FFFF` via `inner`, `try_read_into` → `Err(Lapped)` on overwrite /
  `Err(Corrupt)` on lossless, and (under Miri) no OOB access. The 32-bit
  overflow itself is covered by the Miri 32-bit target run below.
- **B6 `reader_on_gap_start_not_lapped`** — cap 64: two 16-byte payloads
  (records 0..24, 24..48), drain reader to 48; a third 16-byte payload forces
  the gap (hwm = 48, record at 64..88); two 8-byte payloads advance
  `committed` to 120. Old code: `120 - 48 = 72 > 64` → spurious `Lapped`.
  New code: `is_lapped() == false` and all three post-gap records read back
  intact.
- **B6 `reader_on_gap_start_real_lap_detected`** (adversarial from the task):
  same setup to park the reader on `r == hwm == 48`, then keep writing until
  the writer wraps again and truly passes `lap_end + capacity`. Assert
  `is_lapped() == true` and `try_read_into == Err(Lapped)` — proves the
  reordered skip cannot mask a real lap (the second wrap's `hwm'` store breaks
  the `r == hwm` match, and the post-skip recheck catches the rest).
- **R4** — `attach_rejects_truncated` (poke `total_size` up / hand a shorter
  `len`), `attach_rejects_bad_capacity` (0, non-power-of-two),
  `attach_rejects_oob_offsets` (reader table past `data_offset`, `data_offset
  + capacity > total_size`), `attach_rejects_misaligned` (base+1),
  `attach_rejects_short_region` (`TooSmall`, replacing the current BadMagic
  expectation); `#[cfg(feature = "mmap")]` `attach_mmap_rejects_truncated_file`
  (create, `set_len` shorter, re-attach → `RegionTruncated`).

### Concurrency tests (Miri-driven interleavings, bounded, no free-running wrap stress)

- **Strengthen `concurrent_overwrite_no_ub` into a tear detector**: payload =
  the record index encoded twice (`[i.to_le_bytes(), i.to_le_bytes()]
  .concat()`); the consumer asserts both halves agree and are `< n`. A torn
  old/new mix across a lap disagrees between halves — the current
  "value `< n`" assertion cannot see it. Keep bounds Miri-small.
- **New `concurrent_lossless_view_churn`**: one writer thread doing bounded
  lossless writes (records = index pattern), one fast drainer view, plus a
  thread repeatedly `view()`-ing and immediately `try_read`-borrowing with
  full content validation, then dropping. Before the R6 fix, Miri reports the
  OOB/UB when the registration race hits; after, it must be clean. Bounded
  iterations (`cfg!(miri)`-scaled), no unbounded spinning.
- Existing `concurrent_reader_churn` / `concurrent_lossless_full_stream`
  continue as regression nets (writer-claim churn: adapt churners that also
  claim/drop writers to exercise the R7 CAS under contention).

Note on fence coverage: Miri's weak-memory emulation explores relaxed/acquire
reordering but is not exhaustive for the SeqCst-fence omission itself; the
deterministic R1/R6 tests are the primary guard, `-Zmiri-many-seeds` the
secondary. (A `loom` model of `view()`+`try_write` would be the exhaustive
option; noted as optional follow-up, not required by this design.)

### Miri (extends `ring/MIRI.md` recipe)

```sh
# Baseline + strict aliasing (existing recipe)
cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin
MIRIFLAGS="-Zmiri-tree-borrows" \
  cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin

# Interleaving exploration for the race tests (R1/R6 coverage)
MIRIFLAGS="-Zmiri-many-seeds=0..16 -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin concurrent

# NEW: 32-bit target run — Miri interprets any target on any host; this is the
# only practical way to execute the R3 overflow path (flight targets are
# plausibly 32-bit).
cargo +nightly miri test -p metor-fsw-ring --lib --target i686-unknown-linux-gnu
```

`MIRI.md` gains the 32-bit run and a line explaining what R1's fence pair adds
to the "why it is Miri-clean" section. `docs/ring-buffer.md` §1/§6 and the
`lib.rs` module doc get the reservation word, the writer claim, and the v2
version note in the same pass as the code.

---

## Open questions (need a human decision)

1. **`Coordinator::control_handle()` under R7** — it mints a writer per call
   over `command_ring` (`src/coordinator/mod.rs:1670`), which R7 correctly
   rejects on the second call. Interim recommendation: build the command
   `MsgOut` once at `build()` and hand out shared access; the real fix is the
   already-flagged A2 command-plane refactor. Which is in scope for this
   change?
2. **`VERSION` bump to 2** — required by the `reserved_end` semantics (see §0);
   confirms that any persisted v1 mmap regions in dev environments are
   disposable.
3. **`ReadError::Corrupt`** — new public variant for lossless structural-
   invariant violations (vs. overloading `Lapped`, which lossless docs promise
   never happens). Acceptable API addition?
4. **R6 stabilization-loop bound** — the design leaves the registration loop
   unbounded (converges in 1–2 iterations in practice). Add an iteration cap +
   error, or accept as documented?
