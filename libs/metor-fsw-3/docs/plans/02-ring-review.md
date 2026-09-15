# Ring review fixes

Implemented in ring format version 6. Existing version 5 regions must be
recreated.

## Reader reclamation

`View::drop` clears the owner before releasing the cursor. Reclamation
compare-exchanges the dead owner to zero; only its successful caller frees
the cursor. This prevents stale tags and concurrent reclaimers from freeing
a reused slot.

The caller still guarantees the PID is dead and has not been reused.
A crash between cursor claim and owner publication can leave an
unattributed slot; this change does not address that recovery limitation.

## Payload alignment

`PAYLOAD_ALIGNMENT = 16` applies to heap allocation, raw/mmap attachment,
data offsets, record headers, and record spacing. Each record occupies
`16 + round_up16(payload_len)` bytes. The length field stays at header offset
zero.

Port definitions carry frame alignment. Build rejects frames aligned above
16, including unconnected ports. Direct `Input::new` and `Output::new`
return `Result` and reject unsupported alignment before execution.

## Wrap progress

When a record plus its wrap gap cannot fit, the writer checks and publishes
the gap separately, notifies readers, then rescans before writing the
record. With no readers, the write completes in the same call. A caught-up
reader can skip padding and allow the next attempt to succeed.

A `WouldBlock` result may advance the published position through padding;
it never publishes a partial payload. `try_latest` excludes trailing
padding when finding the last data edge, keeping the newest record pinned.

## Other changes

`create_mmap` is unsafe and requires exclusive creation access and stable
backing storage. `NoWake` documents that empty async reads busy-spin.
Wall-clock rates must be finite and at least 0.001 Hz.

## Verification

Unit tests remain in `tests.rs` and the typed modules; concurrency models
remain in `loom_tests.rs`. Coverage includes aligned writes and wraps,
incompatible regions, oversized frames, padding-only publication, pinned
grants, reader registration, and concurrent reclamation. Geometry and
padding arithmetic are covered by Kani harnesses in `verify.rs`.
