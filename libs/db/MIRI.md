# Running Miri on the lock-free structures

`src/disruptor.rs` (the byte ring buffer, the intrusive `Readers` list, and the
`ArcAtomic<T>` manual-refcount pointer) and `src/arc_ring.rs` (`AtomicStack<T>`)
contain hand-written `unsafe` code: manual `Arc` strong-count
increment/decrement, raw-pointer slice construction, and lock-free linked-list
edits. [Miri](https://github.com/rust-lang/miri) interprets these under an
Undefined-Behavior–checking model and catches data races, use-after-free,
double-free, memory leaks, and pointer-provenance violations that ordinary
`cargo test` cannot see.

## What is covered

Miri runs the **synchronous** unit tests in those two modules. It cannot run the
`#[stellarator::test]` async tests, because stellarator's runtime drives
`kqueue`/`io_uring` syscalls that Miri does not support — those tests are
`#[cfg(not(miri))]` so Miri skips them. The Miri-facing tests use only the sync
`Disruptor::try_grant` / `Reader::try_next` APIs plus `std::thread`:

- `disruptor.rs`: `write_grant_roundtrip_sync`, `wraparound_sync`,
  `arc_atomic_refcount_balance`, `reader_create_drop_unlinks`,
  `concurrent_writer_reader_wraps`, `concurrent_multi_reader_wrap`,
  `concurrent_reader_churn`.
- `arc_ring.rs`: `deep_chain_teardown_sync`,
  `concurrent_readers_writer_and_splices`, plus the existing single-threaded
  `push`/`unlink`/`insert_older`/snapshot tests, which run under Miri unchanged.

The concurrency tests scale their loop bounds down under `cfg!(miri)` (Miri is
~50–100× slower), so they exercise the same code paths at a smaller size.

Leak checking is on by default — no flag required. An imbalanced `ArcAtomic`
strong count fails the run automatically at program exit.

## One-time setup

Miri only ships on nightly. We also pass `--target x86_64-apple-darwin` (see the
note below), so add that target too:

```sh
rustup toolchain install nightly --component miri
rustup +nightly target add x86_64-apple-darwin
```

## Running

```sh
cargo +nightly miri test -p metor-db --lib --target x86_64-apple-darwin disruptor
cargo +nightly miri test -p metor-db --lib --target x86_64-apple-darwin arc_ring
```

**Why `--target x86_64-apple-darwin`?** A transitive dependency (`pulp`, via
`nox`) has aarch64 NEON intrinsics that fail to compile on recent nightlies (an
`E0308` after a std SIMD signature change). Miri interprets MIR regardless of
host arch, so building for x86 sidesteps the broken NEON path while still running
on this macOS host (the `x86_64-apple-darwin` target uses the host toolchain to
build C deps like `mlua-sys`; the `-linux-gnu` target would need a cross `gcc`).
Drop the flag once `pulp` is fixed for current nightlies.

## Recommended extra passes

Run again under the stricter Tree Borrows aliasing model (the future default),
which exercises the `WriteGrant::deref_mut` raw write most aggressively:

```sh
MIRIFLAGS="-Zmiri-tree-borrows" \
  cargo +nightly miri test -p metor-db --lib --target x86_64-apple-darwin disruptor
```

Explore more thread interleavings for the concurrency tests:

```sh
MIRIFLAGS="-Zmiri-many-seeds -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-db --lib --target x86_64-apple-darwin
```

## Implementation notes for Miri-cleanliness

Two things make these structures pass Miri; keep them when editing the ring:

- **Interior mutability.** `DistruptorCore.ringbuf` is `Box<[UnsafeCell<u8>]>`,
  and `WriteGrant`/`ReadGrant` derive their slice pointers from
  `UnsafeCell::get()`. Writing through a `*mut` cast from a shared `&Vec` (the
  old `ringbuf.as_ptr() as *mut u8`) is UB under Stacked/Tree Borrows; `get()`
  yields valid mutable provenance instead. `DistruptorCore` carries an explicit
  `unsafe impl Sync` documenting the synchronization invariant.
- **No data races.** All cursors are absolute, monotonic byte counts; the writer
  publishes bytes via a `committed` Release store and readers gate on an Acquire
  load, and `slowest_cursor` backpressure stops the writer from overwriting a
  region a reader is still reading. That happens-before edge is what Miri's race
  detector checks — so the concurrent tests *must* wrap to cover it.

If a transitive dependency ever fails to *build* under nightly Miri, narrow the
build (e.g. feature-gate the offending module); the sync tests here touch no
dependency code that Miri rejects.
