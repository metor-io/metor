# Running Miri on the ring buffer

`src/lib.rs` contains hand-written `unsafe`: `#[repr(C)]` control structures
(`Control`, `ReaderSlot`) pointer-cast over the shared region from the backing's
owning base pointer (their `AtomicU64` fields carry the synchronization), a
transient shared byte view of the write-once `RegionHeader`, and raw-pointer
record writes/reads whose race-freedom rests on the in-use backpressure check
plus the `committed` Release/Acquire publication handshake.
[Miri](https://github.com/rust-lang/miri) checks this for data races,
use-after-free, leaks, and provenance violations that `cargo test` cannot see.

Miri is one of three verification passes over this crate. `KANI.md` covers the
position arithmetic and geometry validation; `LOOM.md` covers the atomic
orderings. Miri is what watches real allocation and provenance.

## What is covered

Miri runs the **synchronous** tests: they use only the `try_*` APIs plus
`std::thread`, so none needs the async runtime Miri cannot drive.

Three things are excluded, all `#[cfg(not(miri))]`. The `wake` module is
syscall-based, and its cross-process behavior needs real separate processes to
exercise. The two mmap tests (`mmap_roundtrip`, `attach_mmap_rejects_truncated_file`)
need a real temp directory and a real `mmap`, neither of which Miri provides.
And the async tests need the executor.

That last exclusion is worth stating plainly, because it is a genuine hole
rather than a technicality: `View::read` and `Notifier` are reached by no
Miri-checkable test, no Kani harness (bounded model checking is sequential)
and no loom model. They are covered only by the `stellarator`-driven tests at
the end of `tests.rs`.

- Basic paths: `roundtrip`, `wraparound_aligned`, `multi_reader`,
  `reader_table_claim_free`, `backpressure`, `borrow_read`,
  `oversize_message_rejected`, `reader_on_gap_start_reads_through`.
- Latest-wins pinning: `latest_pins_newest` (consume-older / re-serve-newest),
  `latest_pin_backpressures_writer` (the pinned record's bytes are protected by
  the in-use check; moving the pin frees the writer).
- Bounded garbage lengths: `garbage_length_is_corrupt`, which under Miri shows
  a scribbled length field produces no out-of-bounds access for the one value
  it scribbles. `straddle_bound_is_sufficient` in `KANI.md` covers every value.
- Writer claim: `second_writer_rejected`, `writer_claim_freed_on_drop`,
  `writer_claim_shared_across_attach`, `concurrent_writer_claim_churn` (the
  claim CAS under thread contention).
- Owner reclamation: `reclaim_frees_dead_reader`,
  `reclaim_frees_dead_writer_claim`, `reclaim_skips_other_owners`.
- Registration: `view_starts_stable` (the handshake converges),
  `concurrent_view_churn` (register/borrow/drop churn against a live writer;
  the test the registration handshake protects).
- Attach geometry: `attach_rejects_truncated`, `attach_rejects_bad_capacity`,
  `attach_rejects_oob_offsets`, `attach_rejects_misaligned`,
  `attach_mmap_rejects_truncated_file`, `raw_attach_bad_region_rejected`
  (`TooSmall`).
- mmap backing: `mmap_roundtrip`.
- The race coverage: `concurrent_full_stream` (the writer spins on
  `WouldBlock` while the reader drains; delivery is exact and ordered),
  `concurrent_reader_churn` (the writer tolerates `WouldBlock` from churning
  views).
- The slot-swap reclaim path: `swap_writer_and_reader_reacquire`,
  `raw_attach_swap_reacquire` (drop a writer and view over a region, then
  re-acquire a fresh pair; checks reader-slot and writer-claim free/reuse, and
  the raw re-attach for provenance and leaks).

Loop bounds shrink under `cfg!(miri)`. Leak checking is on by default. The
module is `#[cfg(all(test, not(ring_loom)))]`: a loom atomic touched outside
`loom::model` panics, so these tests and the loom models never build together.

The current ring has no reader/writer byte race left for Miri's weak-memory
emulation to explore; the checked properties are provenance, the claim/slot
handoffs, and the publication handshake.

## Running

Build for `x86_64-apple-darwin` to sidestep a transitive NEON build issue on
recent nightlies (Miri interprets MIR regardless of host arch):

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

All three pass clean as of this writing.

A 32-bit run against `i686-unknown-linux-gnu` used to be the only practical
way to execute the length-overflow path where `frame_len(garbage)` wraps a
32-bit `usize`; it no longer runs, since `stellarator` is an unconditional
dependency and does not build for that target. Kani covers the same property
symbolically, on any host, in `straddle_bound_blocks_32bit_overflow`.

The i686 run caught a real portability bug: `Box<[UnsafeCell<u64>]>` is only
4-byte aligned there, which is why `Backing::heap` allocates
`repr(align(8))` words. With leak checking on by default, these runs also
prove `Backing`'s `Drop` reconstructs and frees its allocation correctly.

## Why it is Miri-clean

- **Interior mutability.** `Backing::heap` allocates a `Box<[Word]>` where
  `Word` is a `repr(C, align(8))` `UnsafeCell<u64>` (interior mutable, and
  8-byte aligned on every target for the control/cursor atomics).
  `Box::into_raw` hands over the whole-allocation pointer, and no live `Box`
  is retained, so every atomic and byte slice derives from that one owning
  base pointer, and provenance stays whole-allocation wide. `Backing`'s `Drop`
  reconstructs the box to free it exactly once.

  The emphasis is load-bearing: holding the `Box` in `BackingOwner::Heap`
  instead would retag it as `Unique` at the point it moves into the enum,
  invalidating the `base` pointer derived a line earlier, so every subsequent
  access would run on a dead tag. Miri catches this on the first write in
  `init_region`; nothing else does.
- **No data races.** The writer never overwrites in-flight bytes (the in-use
  backpressure check **plus** the SeqCst registration handshake that makes a
  fresh claim visible to the writer's scan), so plain reads/writes are ordered
  by the `committed` Release/Acquire handshake. A `ReadGrant` (including the
  `try_latest` pin) holds the view's cursor at or before the record start, so
  the same check keeps the borrowed bytes stable for the borrow's whole
  lifetime.
- **Struct views over the region.** The control block and reader slots are
  reached as `&Control` / `&ReaderSlot` (`#[repr(C)]`, offsets pinned by const
  asserts), sound because every non-pad field is an atomic, and `ReaderSlot`'s
  pad bytes are written exactly once in `init_region` before publication (a
  shared borrow freezes them). `read_header` forms a transient `&[u8]` over
  exactly the 48 `RegionHeader` bytes, never wider: those bytes are write-once
  pre-publication, while the control words past them may be concurrently
  mutated by a live writer during attach.
