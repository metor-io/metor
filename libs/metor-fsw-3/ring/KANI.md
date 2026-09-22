# Kani

Run all sequential harnesses, or select one:

```sh
cargo kani -p metor-fsw-3-ring --no-default-features
cargo kani -p metor-fsw-3-ring --no-default-features --harness validate_header_hostile
```

Harnesses in `src/verify.rs` check:

- Frame alignment, contiguous reservation, backpressure, and padding progress.
- Header validation, slot bounds, data pointers, and creation/attachment agreement.
- Read/write roundtrips, wrap skipping, and latest-record pinning.
- Bounded corrupt data and control words passed to borrowing reads.

Arithmetic harnesses bound symbolic capacity at `2^20`. Geometry validation
uses symbolic header fields and region length. Operational and corruption
harnesses use a 64-byte, single-reader stack region and an unwind bound of 12.
Their assertions apply only within their stated assumptions and bounds.
The stack region avoids modeling heap growth; the production ring operations
still run, including attachment and header validation.

`fits_checked_cursor_arithmetic` checks that invalid cursor order
produces overflow or a failed fit under checked arithmetic. It does **not**
prove that a release build rejects such inputs: wrapping addition can report a
successful fit. The protocol must establish the cursor-order precondition.

Kani does not verify thread ordering, OS mappings, notifications, or allocation
provenance here. An unwinding assertion failure means a loop exceeded the chosen
bound; it must be resolved before treating that harness as passed. Do not infer
full code coverage from the number of successful harnesses.

See [DESIGN.md](DESIGN.md), [MIRI.md](MIRI.md), and [LOOM.md](LOOM.md).
