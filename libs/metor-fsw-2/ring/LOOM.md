# Running loom on the ring buffer

The ring coordinates through six atomic words and two `SeqCst` fences. The
argument that this is enough is written out in the crate docs (Dekker's
pattern for reader registration, `hwm` published before `committed` for the
wrap gap), and it is the kind of argument that is easy to write and hard to
check. `cargo test` exercises whichever interleaving the hardware produced;
Miri with `-Zmiri-many-seeds` samples more.
[Loom](https://github.com/tokio-rs/loom) enumerates them, running each model
under every interleaving and every reordering the C11 memory model permits.

Loom says nothing about the pointer arithmetic or the geometry validation;
those are Kani's, in `KANI.md`.

## What is covered

Models live in `src/loom_tests.rs` behind `#[cfg(all(test, ring_loom))]`.

- `registration_races_backpressure`: the one racy edge the crate docs name. A
  view registers while the writer is mid-stream, so the writer's in-use scan
  can run an instant before the new claim lands. The model asserts the view
  starts on a record boundary, reads whole records in order with the bytes that
  were written, and is never lapped. This is what `concurrent_view_churn` can
  only sample under Miri seeds.
- `hwm_visible_before_committed`: a reader parked exactly on a wrap gap must
  see the marker before it could misread the stale bytes behind it as a record
  header. Exercises the writer's `hwm`-then-`committed` store order against the
  reader's `committed`-then-`hwm` load order.
- `writer_claim_handoff`: two threads race the claim CAS; they cannot both
  hold it at once.
- `grant_pins_bytes`: a held `ReadGrant` pins its bytes. The writer must
  refuse a write that would reuse them, and the borrow reads back correctly
  throughout.
- `two_readers_slowest`: with one view draining and one parked, the writer is
  bounded by the slower of them whichever order the scan observes their claims
  in.
- `cursor_never_exceeds_committed_at_fits`: races a writer that wraps against
  a reader doing gap skips, to decide whether a writer can ever observe a cursor
  ahead of `committed`. See the note in `KANI.md`. The check is the
  `debug_assert!` in `Writer::fits`, which is `#[cfg(any(test, ring_loom))]`:
  a violation costs a spurious `WouldBlock` rather than a bad write, so it is
  not worth a panic in every downstream debug build.

The models are deliberately tiny. The writer's in-use scan is one atomic load
per reader slot inside a fenced region, so the state space grows fast in both
`max_readers` and message count. All six use a 32-byte capacity, at most two
reader slots, two threads, and at most three records.

The geometry is not arbitrary. Payloads are 12 bytes, so a record's frame is 24,
which does *not* divide the 32-byte capacity. With a frame that divides it,
records land flush against the lap boundary forever and the wrap-gap path never
runs at all. Two `const` asserts at the top of the module pin that relationship
so a later edit cannot quietly turn the interesting models into trivial ones.

## Running

```sh
RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-ring --lib
```

The separate `CARGO_TARGET_DIR` is worth keeping: the cfg changes every
fingerprint in the graph, so sharing `target/` with ordinary builds means
rebuilding the world in both directions.

`--lib` rather than `--all-targets`, because loom is a dev-dependency and only
the test build of the library links it.

The models run exhaustively and finish in about three seconds. If a future
model gets too large for that, bound it rather than letting it run unbounded:

```sh
LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-ring --lib
```

## Why `ring_loom` and not `loom`

The convention is `--cfg loom`, and it does not work here. `RUSTFLAGS` reaches
every crate in the graph, and a dependency elsewhere in the graph gates on the
`loom` cfg while declaring loom as an *optional* dependency this workspace
does not enable. A bare `--cfg loom` puts that crate into its loom
configuration without the crate it needs, and the build fails before it
reaches the ring. A crate-private cfg name sidesteps it entirely. Both names
are registered in the workspace `check-cfg` list.

## What loom does not cover

- **The std test suite is excluded.** A loom atomic touched outside
  `loom::model` panics, so `src/tests.rs` and `src/loom_tests.rs` are mutually
  exclusive; a loom run is not a substitute for `cargo test`.
- **mmap and `attach_raw` are out.** Loom atomics carry tracking state and are
  not byte-transmutable, so the models only use `create_in_memory`. Region
  layout constants (`HEADER_SIZE`, `ReaderSlot`'s padding) are `cfg`-dependent
  for the same reason, with a `#[cfg(not(ring_loom))] const _: ()` assert
  pinning the shipped format so the loom fork cannot move it.
- **`SeqCst` fences are loom's weakest area.** Its modelling of them is known
  to be incomplete, which is precisely the primitive
  `registration_races_backpressure` and `hwm_visible_before_committed` turn on.
  Treat those two as strong evidence, not proof.
- **The payload bytes are untracked.** They are reached through raw pointers
  rather than loom's `UnsafeCell` API, so loom does not see the accesses
  themselves. What the models check is the `committed` handshake that orders
  them, and the byte-for-byte payload assertions catch an overwrite even
  though loom would not flag the race directly. Miri is what watches the
  accesses.
