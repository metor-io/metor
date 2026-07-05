# Delete `Overrun::Overwrite` — lossless-only ring, zero-copy reads

Goal (user request): the ring's two-mode design (`Overrun::Overwrite` vs
`Overrun::Lossless`) causes most of the crate's complexity — the seqlock write
reservation, per-byte atomic data paths, lap detection, and the whole
`OnLap` policy machinery layered on top. Delete the overwrite mode, keep only
lossless, and then remove the read-side scratch copies that existed only
because overwrite-mode reads could not borrow.

## Doctrine change (what replaces laps)

- A writer never overwrites unread data. A full ring means `try_write` →
  `WouldBlock`; the infallible `publish` counts it as a drop (same
  `publish_dropped` health counter that today counts `InsufficientCapacity`).
  The async `write` suspends until a reader frees space.
- Laps cannot happen, so **all lap machinery dies**: `OnLap` (descriptor
  axis 4), `#[fsw(on_lap = …)]`, `Input::lap_fault/resync`, `MsgIn` twins,
  `SystemInput::any_lapped`, the runner's pre/post-execute lap checks,
  `StopReason::LappedInput`, `FswStatus::StoppedLapped`.
- Latest-wins becomes a **reader-side** behavior: a snapshot consumer drains
  to the newest committed record each cycle. New ring API `View::try_latest()`
  pins the newest record (cursor parks at its start, so it stays re-readable
  and the writer cannot reclaim it) and hands out a zero-copy borrow.
  Steady-state cost: one pinned record per latest-consumer; `DEFAULT_DEPTH=8`
  absorbs it.
- **Stopped slots must release their reader views.** A view whose cursor never
  advances now backpressures the producer forever (previously it just got
  lapped), starving *every* consumer of that ring. `CyclicRunner` holds its
  input bundle in an `Option` and drops it on permanent stop; `DlSlot`
  likewise on its stop path.

## WP1 — ring crate (`ring/src/lib.rs`, `ring/src/tests.rs`)

Delete:
- `Overrun` enum, `Config::overrun`, `Inner::overrun`, `FLAG_LOSSLESS`,
  `RingBuffer::overrun()`.
- The seqlock write reservation: `OFF_RESERVED_END`, `Inner::reserved_end`,
  the reserve/fence pair in `Writer::commit`, the recheck in `try_read_into`.
- `View::is_lapped`, `View::resync`, `ReadError::Lapped`,
  `ReadError::BorrowNotSupported` (keep `Corrupt`).
- The overwrite branches of `write_record`/`read_len`/`copy_payload` (plain
  stores/copies only; `copy_payload` may fold into a grant copy).
- `locate`'s lap branch; straddle violation is always `Corrupt` now.

Keep / change:
- The lossless in-use check, wrap-gap (`hwm`), writer claim, reader table,
  and the SeqCst registration handshake in `view()` (now unconditional).
- Layout: drop the `reserved_end` word (shift `wake_word`/`writer` down),
  bump `VERSION` to 2. Regions are ephemeral IPC state; no migration.
- `try_read` (borrow grant) is now the primary read; keep `try_read_into` as
  a thin copy convenience over it (async `read_into` stays for `recv`-style
  consumers, reimplemented over the grant path or kept as-is minus mode
  branches).
- New: `View::try_latest(&mut self) -> Result<Option<ReadGrant>, ReadError>`
  — skip (and free) all but the newest committed record, park the cursor at
  its start, return a borrow. Re-callable with no new data: returns the same
  record. Grant drop for this path does *not* advance past the record.
- Tests: delete lap/tear/seqlock tests; keep and de-parameterize lossless
  tests; add `try_latest` coverage (pin semantics, writer WouldBlock against
  a pinned record, advance-on-new-data). Run Miri per `libs/db/MIRI.md`
  strategy if feasible.

## WP2 — port layer (`src/port.rs`)

- `drain_view` loses `scratch` + `OnLap`: iterate `try_read` grants, call
  `f(&grant)` per record. `Corrupt` propagates.
- `Input<F>`: delete `scratch`, `have`, `lapped`, `on_lap`, `with_on_lap`,
  `lap_fault`, `resync`.
  - `latest()` → `view.try_latest()`, returning a typed guard over the grant.
    Add `FrameGrant<'a, F>` (owns the `ReadGrant`, exposes `get`/`table`/
    `list`/`map`/`apply` like `FrameRef`, or derefs to a `FrameRef`).
  - `drain(f)` passes `FrameRef`s borrowed from per-record grants.
  - `recv()` returns a `FrameGrant` (consumes on drop).
- `Output`: unchanged mechanically; docs updated — `publish` failure now
  includes `WouldBlock` (slow reader), still counted, and `write_async`
  actually suspends.

## WP3 — messages (`src/message.rs`)

- `MsgIn`: delete `scratch`, `lapped`, `on_lap`, `with_on_lap`, `lap_fault`;
  `drain` decodes each grant in place (`split_record` + postcard borrow).
- `MsgOut`: keep the serialization scratch (it is a write-side serialize
  buffer, not an overrun artifact); doc `publish`/`emit` for `WouldBlock`.

## WP4 — system layer (`src/system/mod.rs`, health)

- Delete `SystemInput::any_lapped` (trait method + every impl + derive
  output), the runner's pre/post-execute lap stop, `record_lapped`/
  `lapped_inputs` health counter if now unused.
- `CyclicRunner`: input bundle becomes droppable on stop (Option + take), so
  a stopped slot frees its reader slots. `StopReason` = `{ Panicked }`.

## WP5 — coordinator + telemetry

- `alloc_ring`: lossless `Config` (no `overrun` field).
- `CopyIn`: drop `scratch`; a Snapshot copy-in mirrors only the **newest**
  upstream record per cycle (`try_latest` grant → `try_write` into the
  private ring; `WouldBlock` = skip, the consumer is behind).
- `slot.rs`: remove `seq_status.resync()` lap handling; command drains are
  plain grant drains. Stopped `DlSlot` releases its views.
- Telemetry `Tap`: delete `scratch`; Coalesce lane frames from a
  `try_latest` grant, Fifo lane frames per-record grants.

## WP6 — ABI, dl, macros, descriptor

- `abi`: remove `FswStatus::StoppedLapped` (renumber; pre-1.0 ABI, no compat
  shim), `from_raw`/`to state` mappings, `dl.rs` mirrors.
- `macros`: remove `#[fsw(on_lap = …)]` parsing (`system.rs`) and its docs
  (`lib.rs`); derives stop emitting `any_lapped`/`with_on_lap`.
- `descriptor.rs`: remove `OnLap`, `PortDesc::on_lap`, `with_on_lap`,
  `PortDecl::with_on_lap` and Debug/tests.

## WP7 — tests, examples, docs

- Rewrite/delete lap tests across `system/tests.rs`, `coordinator/tests.rs`,
  `abi/tests.rs`, `telemetry/tests.rs`, `wiring/tests.rs`,
  `tests/slot_integration.rs`, `tests/slot_wiring.rs`,
  `examples/adcs-fsw2` — replace with WouldBlock/backpressure/pinning
  coverage where the scenario still exists.
- Docs reconciliation: `docs/ring-buffer.md`, `DESIGN.md`, `docs/system.md`,
  `docs/messages.md`, `docs/coordinator.md` — lap doctrine → backpressure
  doctrine. `docs/memserve/memserve.rs` if it exercises the ring API.
- Do not touch `examples/cube-sat/src/main.rs` beyond what compilation
  requires (it has unrelated uncommitted changes).

## Order & checkpoints

1. WP1, `cargo test -p metor-fsw-ring` green → commit.
2. WP2+WP3, crate compiles with port/message tests green → commit.
3. WP4+WP5+WP6 together (they interlock), full `cargo test -p metor-fsw-2`
   → commit.
4. WP7 examples + docs, workspace build + clippy → commit.
