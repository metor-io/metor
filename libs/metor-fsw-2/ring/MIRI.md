# Running Miri on the ring buffer

`src/lib.rs` contains hand-written `unsafe`: atomics formed at fixed offsets over
the shared region (`&AtomicU64` from a re-derived base pointer) and raw-pointer
record writes/reads whose race-freedom rests on the in-use backpressure check
plus the `committed` Release/Acquire publication handshake.
[Miri](https://github.com/rust-lang/miri) checks this for data races,
use-after-free, leaks, and provenance violations that `cargo test` cannot see.

## What is covered

Miri runs the **synchronous** tests (everything except `wait_writer_backpressures`
and `wait_reader_borrows`, which need the async runtime Miri can't drive — they
are `#[cfg(not(miri))]` — and the `mmap` tests, which are behind a non-default
feature). The sync tests use only the `try_*` APIs + `std::thread`:

- Basic paths: `roundtrip`, `wraparound_aligned`, `multi_reader`,
  `reader_table_claim_free`, `backpressure`, `borrow_read`,
  `oversize_message_rejected`, `reader_on_gap_start_reads_through`.
- Latest-wins pinning: `latest_pins_newest` (consume-older / re-serve-newest),
  `latest_pin_backpressures_writer` (the pinned record's bytes are protected by
  the in-use check; moving the pin frees the writer).
- Bounded garbage lengths (R3/R6): `garbage_length_is_corrupt` — under Miri this
  proves a scribbled length field never produces an out-of-bounds access.
- Writer claim (R7): `second_writer_rejected`, `writer_claim_freed_on_drop`,
  `writer_claim_shared_across_attach`, `force_release_writer_reclaims`,
  `concurrent_writer_claim_churn` (the claim CAS under thread contention).
- Registration (R6): `view_starts_stable` (the handshake converges),
  `concurrent_view_churn` (register/borrow/drop churn against a live writer —
  the test that hits the pre-fix registration race as UB).
- Attach geometry (R4): `attach_rejects_truncated`, `attach_rejects_bad_capacity`,
  `attach_rejects_oob_offsets`, `attach_rejects_misaligned`,
  `raw_attach_bad_region_rejected` (`TooSmall`).
- The race coverage: `concurrent_full_stream` (backpressured plain read/write,
  zero loss — the writer spins on `WouldBlock`, the reader drains; delivery is
  exact and ordered), `concurrent_reader_churn` (the writer tolerates
  `WouldBlock` from churning views — the mode's contract, not a failure).
- The slot-swap reclaim path: `swap_writer_and_reader_reacquire`,
  `raw_attach_swap_reacquire` (drop a writer+view over a region, then re-acquire a
  fresh pair — the Load→Stop→Load cycle a coordinator slot runs; checks reader-slot
  free/reuse, writer-claim free/reuse, and the raw re-attach for provenance/leaks).

Loop bounds shrink under `cfg!(miri)`. Leak checking is on by default.

> History: v1 of the ring also had an **overwrite** mode whose reads raced the
> writer through relaxed per-byte atomics guarded by a seqlock reservation
> (`reserved_end` + fence pair); its tests (`concurrent_overwrite_no_ub`,
> `torn_read_rejected_by_reservation`, the mixed-size gap-lap tests Miri's store
> buffer ICE'd on) went with the mode. The lossless-only ring has no
> reader/writer byte race left for Miri's weak-memory emulation to explore —
> the checked properties are provenance, the claim/slot handoffs, and the
> publication handshake.

## Running

Like `libs/db`, build for `x86_64-apple-darwin` to sidestep a transitive NEON
build issue on recent nightlies (Miri interprets MIR regardless of host arch):

```sh
cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin
```

Stricter aliasing model:

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
- **No data races.** The writer never overwrites in-flight bytes (the in-use
  backpressure check **plus** the SeqCst registration handshake that makes a
  fresh claim visible to the writer's scan), so plain reads/writes are ordered by
  the `committed` Release/Acquire handshake — exactly like the db disruptor. A
  `ReadGrant` (including the `try_latest` pin) holds the view's cursor at or
  before the record start, so the same check keeps the borrowed bytes stable for
  the borrow's whole lifetime.
