# Verification

Three tools check the ring, each within its own bounds. None establishes every
possible execution. See [DESIGN.md](DESIGN.md) for the memory and
synchronization argument they support.

| Tool | Checks | Source |
|---|---|---|
| Miri | executed pointer accesses and data races | `src/tests.rs`, `src/owner.rs` |
| Loom | modeled atomic schedules | `src/loom_tests.rs` |
| Kani | bounded sequential arithmetic and geometry | `src/verify.rs` |

`src/models.rs` holds the concurrency bodies that both Loom and Miri run.

## Miri

```sh
cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features
# 32-bit pointer and length arithmetic
cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features \
  --target i686-unknown-linux-gnu
# Stricter aliasing, or more thread schedules
MIRIFLAGS="-Zmiri-tree-borrows" \
  cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features
MIRIFLAGS="-Zmiri-many-seeds=0..16 -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features concurrent
```

The tests exercise heap and caller-owned storage, borrowed and copied reads,
wrap gaps, latest pins, drains, claims, reclamation, and concurrent
registration, including empty payloads at the region boundary. Heap storage is
16-byte aligned, including on 32-bit targets.

The ownership callback tests cover exports, independently attached handles,
validation failure, and final release from another thread within one
executable. They do not exercise dynamic loading across an ABI. Runs with more
seeds sample more schedules; they do not prove the absence of races.

Mmap, OS wake syscalls, and `stellarator` executor tests are excluded under
`cfg(miri)`, and the commands above disable `notify`. A passing run does not
cover cross-process mapping or wake behavior.

## Loom

Run the models in a separate build directory, in a debug build so the `fits`
precondition assertion stays on:

```sh
RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-3-ring --lib --no-default-features
# With a scheduling bound of three preemptions
LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-3-ring --lib --no-default-features
```

Report scheduling bounds alongside results; a bounded run does not cover
schedules outside them. The crate uses `ring_loom` rather than `loom` to avoid
activating other dependencies' Loom configurations.

The models cover reader registration, wrap-marker ordering, writer claim
handoff, held grants, the slowest of two readers, padding publication, latest
pins, and owner reclamation during slot reuse, on small rings with few
operations. The cursor-order model exercises the assertion `fits` requires; it
does not prove that violating the precondition is harmless.

Loom atomics carry tracking state, so the models use heap construction and a
separate in-memory layout. They do not exercise raw attachment or mmap.
Ordinary unit tests are excluded because Loom atomics must run inside a model.
Payload accesses use raw pointers that Loom does not track: byte comparisons
can expose overwritten data, but cannot establish race freedom. Loom's SeqCst
fence modeling is limited; the registration models support the argument in
DESIGN.md rather than prove it.

## Kani

```sh
cargo kani -p metor-fsw-3-ring --no-default-features
cargo kani -p metor-fsw-3-ring --no-default-features --harness validate_header_hostile
```

The harnesses check:

- Frame alignment, contiguous reservation, backpressure, and padding progress.
- Header validation, slot bounds, data pointers, and creation/attachment agreement.
- Read/write roundtrips, wrap skipping, and latest-record pinning.
- Bounded corrupt data and control words passed to borrowing reads.

Arithmetic harnesses bound symbolic capacity at `2^20`. Geometry validation
uses symbolic header fields and region length. Operational and corruption
harnesses use a 64-byte, single-reader stack region and an unwind bound of 12;
the production ring operations still run on it, including attachment and header
validation. Assertions hold only within these assumptions and bounds.

`fits_checked_cursor_arithmetic` checks that invalid cursor order produces
overflow or a failed fit under checked arithmetic. It does **not** prove that a
release build rejects such inputs: wrapping addition can report a successful
fit. The protocol must establish the cursor-order precondition.

Kani does not verify thread ordering, OS mappings, notifications, or allocation
provenance. An unwinding assertion failure means a loop exceeded the bound and
must be resolved before the harness counts as passed.
