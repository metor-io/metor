# Running Miri on the ring buffer

`src/lib.rs` contains hand-written `unsafe`: atomics formed at fixed offsets over
the shared region (`&AtomicU64`/`&AtomicU8` from a re-derived base pointer),
raw-pointer record writes/reads, and — in **overwrite** mode — a genuine
concurrent read/write on the data bytes resolved by a whole-buffer lap recheck.
[Miri](https://github.com/rust-lang/miri) checks this for data races,
use-after-free, leaks, and provenance violations that `cargo test` cannot see.

## What is covered

Miri runs the **synchronous** tests (everything except `wait_writer_backpressures`,
which needs the async runtime Miri can't drive — it is `#[cfg(not(miri))]`, and
the `mmap_roundtrip` test which is behind a non-default feature). The sync tests
use only the `try_*` APIs + `std::thread`:

- `roundtrip`, `wraparound_aligned`, `multi_reader`, `reader_table_claim_free`,
  `overwrite_slow_reader_lapped`, `lossless_backpressure`, `lossless_borrow_read`,
  `overwrite_borrow_unsupported`, `oversize_message_rejected`.
- The race coverage: `concurrent_overwrite_no_ub` (writer/reader race the same
  bytes via relaxed atomics + lap recheck), `concurrent_lossless_full_stream`
  (backpressured plain read/write, zero loss), `concurrent_reader_churn`.

Loop bounds shrink under `cfg!(miri)`. Leak checking is on by default.

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
MIRIFLAGS="-Zmiri-many-seeds=0..12 -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-fsw-ring --lib --target x86_64-apple-darwin concurrent
```

All three pass clean as of this writing.

## Why it is Miri-clean

- **Interior mutability.** `BoxBacking` is `Box<[UnsafeCell<u64>]>` (interior
  mutable + 8-byte aligned for the control/cursor atomics). Atomics and byte
  slices are formed from a base pointer re-derived on every access, keeping
  provenance whole-allocation wide.
- **No data races.** Lossless mode never overwrites in-flight bytes (the in-use
  backpressure check), so plain reads/writes are ordered by the `committed`
  Release/Acquire handshake — exactly like the db disruptor. Overwrite mode does
  let the writer and reader touch the same bytes, so **both sides use relaxed
  atomics** (no UB race); the whole-buffer lap recheck after the copy then
  discards any snapshot the writer overwrote mid-read.
