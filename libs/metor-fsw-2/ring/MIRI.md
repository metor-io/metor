# Running Miri on the ring buffer

`src/lib.rs` contains hand-written `unsafe`: atomics formed at fixed offsets over
the shared region (`&AtomicU64`/`&AtomicU8` from a re-derived base pointer),
raw-pointer record writes/reads, and — in **overwrite** mode — a genuine
concurrent read/write on the data bytes resolved by a seqlock-style reservation
recheck (`reserved_end` + fence pair).
[Miri](https://github.com/rust-lang/miri) checks this for data races,
use-after-free, leaks, and provenance violations that `cargo test` cannot see.

## What is covered

Miri runs the **synchronous** tests (everything except `wait_writer_backpressures`,
which needs the async runtime Miri can't drive — it is `#[cfg(not(miri))]`, the
`mmap` tests which are behind a non-default feature, and the two `reader_on_gap_start_*`
tests — see the mixed-size note below). The sync tests use only the `try_*` APIs +
`std::thread`:

- Basic paths: `roundtrip`, `wraparound_aligned`, `multi_reader`,
  `reader_table_claim_free`, `overwrite_slow_reader_lapped`,
  `lossless_backpressure`, `lossless_borrow_read`, `overwrite_borrow_unsupported`,
  `oversize_message_rejected`.
- Seqlock reservation (R1): `torn_read_rejected_by_reservation` hand-drives the
  writer's commit phases through `inner` (reserve + Release fence + scribble, no
  `committed` store) and asserts the reader rejects the torn window;
  `reservation_no_false_lap` pins `reserved_end == committed` at steady state.
- Bounded garbage lengths (R3/R6): `garbage_length_bounded` (overwrite →
  `Lapped`), `lossless_garbage_length_is_corrupt` (lossless → `Corrupt`) — under
  Miri these prove a scribbled length field never produces an out-of-bounds
  access.
- Writer claim (R7): `second_writer_rejected`, `writer_claim_freed_on_drop`,
  `writer_claim_shared_across_attach`, `force_release_writer_reclaims`,
  `concurrent_writer_claim_churn` (the claim CAS under thread contention).
- Registration (R6): `lossless_view_starts_stable` (the handshake converges),
  `concurrent_lossless_view_churn` (register/borrow/drop churn against a live
  lossless writer — the test that hits the pre-fix registration race as UB).
- Attach geometry (R4): `attach_rejects_truncated`, `attach_rejects_bad_capacity`,
  `attach_rejects_oob_offsets`, `attach_rejects_misaligned`,
  `raw_attach_bad_region_rejected` (`TooSmall`).
- The race coverage: `concurrent_overwrite_no_ub` (writer/reader race the same
  bytes via relaxed atomics + seqlock recheck; the payload encodes the record
  index **twice**, so an old/new tear across a lap is detected by the halves
  disagreeing, not just by a range check), `concurrent_lossless_full_stream`
  (backpressured plain read/write, zero loss), `concurrent_reader_churn`.
- The slot-swap reclaim path: `swap_writer_and_reader_reacquire`,
  `raw_attach_swap_reacquire` (drop a writer+view over a region, then re-acquire a
  fresh pair — the Load→Stop→Load cycle a coordinator slot runs; checks reader-slot
  free/reuse, writer-claim free/reuse, and the raw re-attach for provenance/leaks).

Loop bounds shrink under `cfg!(miri)`. Leak checking is on by default.

### Mixed-size limitation (`reader_on_gap_start_*`)

The two B6 gap-start tests are `#[cfg(not(miri))]`: the gap-start window
mathematically requires **mixed record sizes** (uniform records tile every lap
identically), and a later record's `AtomicU64` header store then lands partially
over a previous lap's `AtomicU8` payload bytes. Miri's weak-memory store buffer
ICEs on such mixed-size atomic accesses ("cannot have partially overlapping
store buffer when previous write was atomic") — a Miri limitation, not a
soundness verdict. The tests are deterministic single-threaded logic; their
unsafe surface is the same write/read path the uniform-size tests cover under
Miri.

## Running

Like `libs/db`, build for `x86_64-apple-darwin` to sidestep a transitive NEON
build issue on recent nightlies (Miri interprets MIR regardless of host arch):

```sh
cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin
```

Stricter aliasing model (exercises the overwrite atomic stores hardest):

```sh
MIRIFLAGS="-Zmiri-tree-borrows" \
  cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin
```

More interleavings for the concurrency tests:

```sh
MIRIFLAGS="-Zmiri-many-seeds=0..16 -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin concurrent
```

32-bit target (the only practical way to *execute* the R3 length-overflow path —
`frame_len(garbage)` wrapping a 32-bit `usize`; flight targets are plausibly
32-bit). `--no-default-features` because the default `async` feature pulls
`stellarator` → `io-uring`, which does not build for this target; the ring's
`try_*` core is what the run needs:

```sh
cargo +nightly miri test -p metor-fsw-ring --lib --no-default-features \
  --target i686-unknown-linux-gnu
```

All four pass clean as of this writing. The i686 run also caught a real
portability bug: `Box<[UnsafeCell<u64>]>` is only 4-byte aligned on i686, so
`BoxBacking` now allocates `repr(align(8))` words.

## Why it is Miri-clean

- **Interior mutability.** `BoxBacking` is `Box<[Word]>` where `Word` is a
  `repr(C, align(8))` `UnsafeCell<u64>` (interior mutable + 8-byte aligned on
  every target for the control/cursor atomics). Atomics and byte slices are
  formed from a base pointer re-derived on every access, keeping provenance
  whole-allocation wide.
- **No data races.** Lossless mode never overwrites in-flight bytes (the in-use
  backpressure check **plus** the SeqCst registration handshake that makes a
  fresh claim visible to the writer's scan), so plain reads/writes are ordered by
  the `committed` Release/Acquire handshake — exactly like the db disruptor.
  Overwrite mode does let the writer and reader touch the same bytes, so **both
  sides use relaxed atomics** (no UB race); the seqlock recheck after the copy
  then discards any snapshot an overlapping write could have touched.
- **The R1 fence pair.** The writer stores `reserved_end` then `fence(Release)`
  before its relaxed data stores; the reader runs `fence(Acquire)` after its
  relaxed payload loads and rechecks `reserved_end`. Under the C++ fence rules
  this makes any overlapping write's reservation visible to the recheck, so a
  copy that passes is provably tear-free — Miri's weak-memory emulation explores
  the relaxed reorderings around this pair (it is not exhaustive for SeqCst-fence
  omission itself; the deterministic hand-driven tests are the primary guard,
  `-Zmiri-many-seeds` the secondary).
