# Sequences and slots: review fixes

Fixes for `12-sequences-review.md`. Five tasks in order; each ends with
`cargo test -p metor-fsw-3 --no-fail-fast`, the Python tests, clippy
clean, and one commit. Finding numbers are the review's.

Findings 5 and 6 (the store containment check and the hash gate) are
deferred: T5 records them in `TODO.md`. Finding 10 is fixed for panics
only; see T1.

## T1. Channel re-make and load (findings 3, 4, 7, 8, 10, part of 1)

Files: `src/coordinator/table.rs`, `src/channel/adapter/mod.rs`,
`src/channel/adapter/tests.rs`, `src/sequence/mod.rs`, `src/dl/slot.rs`,
`src/coordinator/step.rs`.

The adapter re-makes through the recipe for both `Reset` and a `Load`
on a channel with no step. Today a re-make always runs the slot's
`initial` and then re-applies `loaded`. One bad `initial` wedges the
channel, and a healthy reset opens two packs. Give the make the name to
load instead:

1. `MakeCx` gains `load: Option<&str>`, the system the make should load
   in place of its configured initial. Build passes `None`.
   - `DlSlot`'s make loads `cx.load.or(initial)`.
   - Every other make refuses `Some` with `ParamError::NotLoadable`,
     through one helper beside `refuse_thread`, which the cyclic entry,
     `sequence::entry`, and `thread::place` all call.
2. `Core::remake(load: Option<&str>)` drops the step and makes it once
   with that name.
   - `Reset` re-makes with `loaded`.
   - `Load` with a step asks the step, as now. `Load` with no step
     re-makes with `Some(name)`.
   - A make that refuses with `NotLoadable` emits `Refused` and changes
     nothing: no end cleared, no run change. This is finding 4.
   - Any other make error emits `Failed`, as now.
3. `report_end` keeps the `Option<End>` it last reported, not a bool, so
   `Completed -> Unloaded` emits `Unloaded`. `End` derives `Clone` and
   `PartialEq` for this.
4. A panic reports `Failed { reason: "panicked" }`, not `Stopped`. The
   wire's `Stopped` stays for an operator `Stop`, which the panel's
   sequence view shows as stopped. `Loaded { name }` on `Reset` stays;
   the panel uses it to mark a channel ready.
5. An `Abort` whose ring write fails emits `Refused { "abort ring full" }`
   instead of `Progress` (finding 7).
6. Event reasons are cut to `REASON_CAP = 512` bytes on a char boundary
   before encoding, so a long `LoadError` still yields an event
   (finding 8).
7. Split `channel::make` into its four parts (outer rings, abort ring,
   recipe, adapter) and flatten `Core::load` into the three cases above.
   Clone `name` once on a successful load.

Tests (`adapter/tests.rs`, using the fake slot and the counters):
- `Reset` whose re-make fails emits `Failed` and leaves no step; a second
  `Reset` succeeds once the make is fixed;
- `Load` on a plain system whose step unwound is `Refused`, and status
  stays `Panicked`;
- `Load b` on a slot whose re-make of `a` fails still loads `b`;
- `Reset` with a loaded name makes once, with that name (count makes and
  the names the fake saw);
- `Completed -> Unloaded` emits `Unloaded`;
- a panic emits `Failed { "panicked" }`;
- a full abort ring emits `Refused` on the fifth abort;
- a 2 KiB refusal reason arrives cut to `REASON_CAP`.

## T2. A failed create keeps the occupant (finding 1)

Files: `src/dl/slot.rs`, `tests/slot.rs`,
`tests/fixtures/echo-pack/src/lib.rs` if a refusing candidate is needed.

`DlSlot::load` drops the current occupant before `create`, because the
new one needs its views and writer. On a `create` failure, re-create the
previous candidate: its library is resident and its rings are free. The
error is still returned, so the adapter emits `Refused`. If the
re-create fails too, the slot is empty, and T1's `report_end` emits
`Unloaded`. Fix the doc comment to say this.

Tests (`tests/slot.rs`):
- a candidate whose baked params its constructor refuses: the `Load` is
  `Refused` and the previous occupant is still loaded and startable;
- rename `test_stale_ports_refused` to what it tests (a candidate of
  another type), and add one that bakes a stale `def` for the right type.

## T3. Slots that read slots (finding 2)

Files: `src/deploy/slot.rs`, `src/deploy/index.rs` if needed,
`src/deploy/tests.rs` or `tests/slot.rs`.

Resolve slots in step order and write each slot's params into the
envelope before computing the next, rebuilding the `DefIndex` per slot
(it borrows the envelope). Slots are few, so the rebuild costs nothing
that matters.

Tests: a target where slot `seq` reads `mode.<port>` of slot `mode`
resolves in one `metor build` pass and builds; the reversed order (a
consumer before its producer) still fails with the existing error.

## T4. Python: channels need a command source (finding 9)

Files: `python/metor-config/metor_config/_target.py`,
`python/metor-config/tests/test_target.py`.

`to_config` raises `ConfigError` when the target has a channel and
`commands(..)` was never called, naming the first channel.

Tests: that raise; `DlSlot` with no `initial` emits `"initial": null`.

## T5. Docs and nits (finding 12, the nits, the deferred findings)

Files: `docs/plans/12-sequences.md`, `TODO.md`,
`src/coordinator/config.rs`, `src/channel/registry.rs`.

- Design: say what the Kani harness proves (an accepted command is `Stop`
  or `Abort` exactly when the channel is live). Say that `apply` never
  sets `run`, so the transitions are covered by tests. Replace the
  single `Channel` struct with the shipped `Channel { commands, core }`
  split. Add to the shipped deviations: `SequenceChannelEvent::MAX_LEN`
  is 1024, and a panic reports `Failed { "panicked" }`.
- `TODO.md` under item 5: findings 5 and 6 as one follow-up. It covers
  deriving the store root at run time rather than from params, hashing
  the file that is opened (or comparing `dev`/`ino`), and skipping the
  hash when the path is already resident.
- `adapter_keys::serialize` borrows instead of cloning the adapter.
- `ChannelRegistry` writes the bytes it encoded at make if `Output` has
  a raw write. If not, drop the make-time encode and check the size by
  encoding once into a scratch buffer. Either way, one encode path.

## Shipped deviations

- No `MakeCx.load`. `Reset` reloads the last load in place on the
  existing step and re-makes only when the step is gone or nothing was
  loaded. A bad `initial` no longer fails the make, so a re-make cannot
  wedge on it.
- Findings 5 and 6 are fixed rather than deferred: the `store` param and
  containment check are removed, a first open hashes through one handle
  and rechecks device and inode after `dlopen`, and a resident library
  skips the hash.
- The long-reason test is a unit test of the clip, since a command
  carrying a 2 KiB name does not fit its 256-byte record.
- `ChannelRegistry` keeps its make-time size check and writes the typed
  record: `Output<T>` has no raw write, and the write encodes into the
  output's scratch buffer without allocating.
- The stale-def test bakes another type's ports as the slot's, since a
  non-first candidate's baked `def` is only read at build.
