# metor-fsw-2 performance review — allocations & hot-path

Hot path = per-cycle coordinator loop (`Coordinator::run_for` → `slot.step` →
`execute` + `HealthPort::end_cycle`), the telemetry snapshot
(`TelemetrySystem::execute`), the copy-in step, and the frame writer/ring data
path. Cold = builder/`build()`, wiring/KDL, dl load (`DlSystem::open`,
`into_port_desc`, `prefix_announce_vtable`).

## High-confidence fixes (EXECUTED)

1. **`src/port.rs` `Output::write_with` — pool the `LenPacket` buffer.** HOT.
   Every cyclic system publishes a health record **every cycle** via
   `HealthPort::publish_health` → `Output::write_with`, which previously did
   `FrameWriter::new` → `LenPacket::table(..)` (a fresh heap alloc) and dropped
   it (free) each call. Same for `flush_logs`, coordinator `publish_status`, and
   any dynamic-member output. Fix: `Output` retains an `Option<LenPacket>`
   scratch; `write_with` takes it, `FrameWriter::from_packet` clears+reseeds it,
   and it is stored back after the ring write. Eliminates 1 malloc+free per
   system per cycle on the health path. Behavior-preserving: `table()` bytes are
   byte-identical; `LenPacket::clear()` resets to the same 8-byte table base a
   fresh `LenPacket::table([0,0],_)` has. Confidence: high.

2. **`src/writer.rs` `FrameWriter::map` — hoist the per-entry temp buffer.** HOT
   (dynamic-map frame writes, incl. health `error_counts` each cycle once a
   system has reported any error). Previously `let mut entry = vec![0u8; stride]`
   allocated **once per map entry**. Fix: allocate one `entry` buffer before the
   loop and `entry.fill(0)` each iteration. Confidence: high.

3. **`src/writer.rs` `MapWriter` — store `V`, not `Vec<u8>`.** HOT (same path).
   `insert` previously did `value.as_bytes().to_vec()` — a heap alloc per entry
   per insert (per cycle for re-published health counters). Fix: store
   `Vec<(String, V)>` and call `val.as_bytes()` at serialize time. The key still
   needs `String` (the `&str` has no stable lifetime through the `FnOnce`).
   Confidence: high.

## Flagged — not changed (risky / needs decision), by value

- **`src/telemetry/mod.rs:457` snapshot double-copy.** HOT. `execute` copies
  ring → `tap.scratch` (`try_read_into`) then `tap.scratch` → fresh `LenPacket`
  (`extend_from_slice`). The owned `LenPacket` is required (it moves into the
  coalescing hand-off, latest-wins), but the intermediate `scratch` copy could
  be removed if `View::try_read_into` could target a `LenPacket`'s buffer
  directly, or if drained-but-unsent `LenPacket`s were recycled back from the
  hand-off into a free-list. Both change the `HandOff`/`View` contract → flag.
- **`src/coordinator/mod.rs` `run_copy_ins`.** HOT-ish (async graphs only).
  ring → `scratch` → `try_write` (ring) double copy, inherent to the copy-in
  buffer design; zero-copy would need a ring-to-ring splice API. Flag.
- **`ring/src/lib.rs` `copy_payload` (Overwrite).** HOT. Byte-by-byte
  `AtomicU8` load loop is **intentional** (the relaxed-atomic lap-race contract);
  do NOT convert to `copy_nonoverlapping`. Left as-is.
- **`src/abi/mod.rs` `into_port_desc`/`prefix_announce_vtable`.** COLD (dl load
  time). Redundant `vtable.clone()`+`metadata.clone()` and a `to_vec`, but
  one-time per dlopen'd port. Not worth churn.
