# Loom

Run the concurrency models in a separate build directory:

```sh
RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-3-ring --lib --no-default-features
```

For a scheduling bound of three preemptions:

```sh
LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg ring_loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p metor-fsw-3-ring --lib --no-default-features
```

Report the scheduling bounds alongside results. A bounded run does not cover
schedules outside those bounds. The crate uses `ring_loom` rather than `loom`
to avoid activating unrelated dependencies' Loom configurations.

Models in `src/loom_tests.rs` cover reader registration, wrap-marker ordering,
writer claim handoff, held grants, the slowest of two readers, padding
publication, latest pins, and owner reclamation during slot reuse. They use
small rings and few operations to keep exploration manageable. The cursor-order
model exercises the assertion required by `fits`; it does not prove that
violating the precondition is harmless.

Loom atomics carry tracking state, so these models use heap construction and a
separate in-memory layout. They do not exercise raw attachment or mmap. Ordinary
unit tests are excluded because Loom atomics must run inside a model.

Payload accesses use raw pointers and are not tracked as Loom memory accesses.
Byte comparisons can expose overwritten data, but cannot establish race freedom.
Loom's modeling of SeqCst fences also has limitations; the registration models
provide supporting evidence for the argument in [DESIGN.md](DESIGN.md).
Use [Miri](MIRI.md) for executed pointer accesses and [Kani](KANI.md) for bounded
sequential arithmetic and geometry checks.
