# Sequences and slots: review

Range `a4b2c7de..5ba883ab` (12 commits, slice 5). Validated by reading every
file in the diff plus `ring/src/lib.rs` (`view`, `slowest_active_cursor`),
`log.rs` (`RestoreSlot`), `panic.rs`, `port/mod.rs` (`write`), and
`deploy/index.rs`; `cargo test -p metor-fsw-3` passes, the 73 Python tests
pass, and `cargo kani -p metor-fsw-3 --harness test_live_channel_transitions`
verifies (23 checks).

## Findings

1. **Medium: a `Load` whose `create` fails empties the slot and reports it as
   a refusal.** `src/dl/slot.rs:243-257`: `load` sets `self.current = None`
   and only then calls `create`. The doc comment on it ("Drops the occupant
   only once the new one's library checks out, so a refused load keeps it")
   and the design's "a refused load keeps its occupant" hold for `Unknown`,
   `Hash`, `Open`, and `Mismatch`, but not for `Create`: a candidate whose
   constructor refuses its baked params (`UnknownKey`, a bad value), or an
   async candidate (`ParamError::Channel` from the guest's `place`), fails
   after the old occupant was destroyed. Scenario: `safe_mode` is stopped
   after completing; the operator sends `Load commissioning`, whose params
   were baked wrong; the event is `Refused { creating the system: .. }`, but
   `channel_status` flips to `Unloaded`, and `Start` is now refused with
   `Ended`. Worse, `src/channel/adapter/mod.rs:333-344`: `report_end` tracks
   only whether *an* end was reported, so when the previous occupant had
   already ended (`reported == true`) the `Completed -> Unloaded` change
   emits no `Unloaded` event at all; the operator sees a refusal and a
   channel that silently lost its system. Fix: keep the slot's own state
   honest, either by re-creating the previous candidate on a `Create`
   failure (its library is still resident and its rings are free) and
   reporting `Failed`, or by documenting that `Create` empties the slot and
   emitting `Unloaded`; and make `report_end` compare the end it last
   reported (`Option<End>`), not a bool.

2. **Medium: a slot fed by another slot's output cannot be built.**
   `src/deploy/slot.rs:41-63` computes every slot's context against a
   `DefIndex` that borrows the envelope (`src/deploy/index.rs:28-34`) and
   writes the resolved params back only after the loop. `SlotCx::edges` ->
   `DefIndex::output` -> `DefIndex::def` -> `dl::slot::def`, which reads
   `allow[0].def` from the *envelope's* params. Scenario: `mode` (a slot) and
   `seq` (a slot with `cmd=mode.cmd`); the first `metor build` fails with
   "slot `seq`: `commissioning` is not resolved; run `metor build`", and no
   params are written, so it fails the same way every time. A non-slot
   consumer of a slot is fine only because `deploy::resolve` runs after the
   write-back. Fix: resolve slots in step order and write each one's params
   into the envelope before computing the next (`DefIndex` needs `&mut` or a
   rebuild per slot), or run the slot pass to a fixpoint. Add the chained
   case to `tests/slot.rs`.

3. **Medium: `Reset` and `Load` always re-run `initial`, so one bad candidate
   wedges the channel.** `src/dl/slot.rs:125-128`: `make` loads `initial`
   and fails the make if that load fails. `src/channel/adapter/mod.rs:261-300`:
   `Core::load` calls `remake` when the step is gone, and `remake` re-makes
   (which loads `initial`) then re-applies `loaded`, so a `Load b` after a
   failed `Reset` first has to succeed at loading `initial` and then the old
   `loaded`, and reports *their* error as `Failed` without ever trying `b`.
   Scenario: `initial = commissioning`; its store file is corrupted (hash
   mismatch, the case the design says the re-hash is for). `Reset` ->
   `Failed`, step gone. `Load safe_mode` -> `remake` -> `make` fails on
   `commissioning` -> `Failed` again; the step stays gone, so every later
   `Load` repeats it. The channel is unusable for the rest of the run
   although `safe_mode` is fine. Also, a `Reset` on a healthy slot with
   `initial != loaded` opens, creates, and destroys `initial` before creating
   `loaded`: two dlopens and a guest constructor per reset, for nothing. Fix:
   make the slot empty in `make` and treat `initial` as the first `Load`
   the adapter applies (`loaded.or(initial)`), and in `Core::load` re-make
   without re-applying `loaded`, since the new name replaces it.

4. **Low: a refused `Load` on a channel with no step re-makes it.**
   `src/channel/adapter/mod.rs:261-266`: when `inner` is `None` (an unwound
   step, or a failed re-make) `load` calls `remake` before asking the step
   whether it can load. For a sequence or a plain system the answer is
   `NotLoadable`, so the event is `Refused`, but the channel has meanwhile
   been rebuilt: `end` cleared, `run` still whatever it was, status
   `Panicked -> Stopped`, and no `Loaded` event. The design says a refused
   command "changes nothing". Scenario: a sequence panics; a ground script
   sends `Load` to the wrong channel; the panic latch is gone and `Start`
   now runs it again. Fix: in `load`, refuse when `inner` is `None` unless
   the recipe is known to be loadable, or only re-make after the step
   reports it can load (which needs `Step::loadable()` or a flag captured
   at make).

5. **Low: the store containment check compares two fields of one params
   object.** `src/dl/slot.rs:178` refuses a candidate whose `path` is
   outside `store`, but `store` comes from the same `SlotParams`
   (`src/dl/slot.rs:32,100-102`). Whoever writes one writes the other, and
   a `Load` command can only pick a name from `allow`, so the check bounds
   nothing a command can influence. It does cost a `canonicalize` of both
   paths per load, and it refuses every load when the store directory is
   missing. Either derive the store at run time from the target directory
   `prepare` already knows (`src/cli/run.rs:178-180`) and pass it through
   the coordinator, or drop the check and the `store` param.

6. **Low: the hash check is best effort, not a gate.** `src/dl/slot.rs:184-191`
   hashes the file at `path`, then `Pack::open(path)` opens it by path
   again; a rename between the two loads other bytes than were hashed. And
   `resident_library` (`src/dl/mod.rs:74-90`) is keyed by canonical path, so
   a second `Load` of the same candidate returns the library mapped by the
   first, whatever the file holds now; the re-hash then checks bytes nobody
   maps. The design says the hash runs "before anything is mapped" and
   catches "a store file that was changed or corrupted after build"; it
   catches that only for the first open of a path. Fix: hash and open the
   same `File` (`Library::new` takes a path, so at least compare
   `dev`/`ino` of the hashed file with `metadata(path)` after the open), and
   skip the hash when the path is already resident, since it proves
   nothing then.

7. **Low: an accepted `Abort` reports progress whether or not the record was
   written.** `src/channel/adapter/mod.rs:238-245` ignores
   `abort.write(..)`'s result. The private ring holds `ABORT_DEPTH = 4`
   records and a cyclic system with an `Abort` parameter that does not call
   `requested()` every cycle never drains it. Scenario: a mode system polls
   `abort` only in one state; four aborts fill the ring; the fifth is
   dropped, the event still says `abort requested`, and the operator waits.
   Fix: emit `Refused`/`Failed` on a write error.

8. **Low: events that do not fit are dropped silently.**
   `src/channel/adapter/mod.rs:358-364` ignores `events.write` errors, and
   `Refused { reason }` carries `LoadError` text that embeds `DefError` and
   serde messages (`src/dl/slot.rs:199`), which can pass
   `SequenceChannelEvent::MAX_LEN = 1024` (`src/channel/records.rs:31`). A
   refusal then produces no event at all. Fix: truncate `reason` to a fixed
   budget before building the event.

9. **Low: a target with channels and no `fsw.commands(..)` emits without a
   commands edge.** `python/metor-config/metor_config/_target.py:158-159`
   adds the `commands` input only when `_commands` is non-empty, so every
   channel in such a target is silently uncommandable; the Rust side sees an
   input with no edges and reads nothing. Fix: raise `ConfigError` in
   `to_config` when a channel exists and `commands` was never called, or
   require `commands` before the first `channel=` add.

10. **Low: two event mappings hide what happened.** The wire's `Stopped`
    means "hard-stopped (dropped), may have left the system in an unsafe
    state" (`libs/metor-proto/wkt/src/msgs.rs:761-762`).
    `src/channel/adapter/mod.rs:234-237` emits it for an operator `Stop`,
    which is a resumable pause, and `src/channel/adapter/mod.rs:374` emits it
    for a panic too; a consumer of the event stream cannot tell a pause from
    a crash, and `channel_status` is the only place the difference lives.
    `src/channel/adapter/mod.rs:251-259` emits `Loaded { name: <instance
    id> }` for a `Reset` of a system that loads nothing, and on an empty slot
    `Loaded { name: "mode" }` followed by `Unloaded`. Both are recorded
    deviations, but the first is a bad call: the wire's `Progress` already
    carries free text, so a pause can be `Progress { "stopped" }` and a
    panic `Failed { "panicked" }`, both of which the panel shows.

11. **Low: test gaps.** None of the following runs: a `Reset` whose re-make
    fails (`Failed` event, step gone, a second `Reset` works); a `Load` on a
    channel whose step unwound; a `Load` failing at `create` (finding 1); a
    slot reading another slot (finding 2); a `Reset` on the real `DlSlot`
    with `initial` set and a different `loaded`; an abort ring that is full;
    the Python target with channels but no `commands()`; `DlSlot` with
    `initial=None` emitting `null`. `tests/slot.rs:307-317`
    `test_stale_ports_refused` loads a candidate of a different type, not a
    candidate whose baked `def` went stale, which is the case the design
    names; both take the `Mismatch` path, so rename it or bake a stale def.

12. **Low: the Kani harness proves less than the design says.**
    `src/channel/apply.rs:165-176` proves: an accepted command is `Stop` or
    `Abort` iff the state is live (`Running && !ended`). That is the whole
    content of a six-line function, and it holds. The design's "a running
    channel leaves `Running` only by `Stop`" is not what is proven and is
    false as written: `Reset` and `Load` from `Running && ended` set `run =
    Stopped` (`src/channel/adapter/mod.rs:252,272`). Say what the harness
    checks in the design, and note that `apply` never touches `run`, so the
    transition side lives in `Core::transition` and is covered by tests
    only.

## Nits

- `src/channel/adapter/mod.rs:67-125` `make` is 58 lines doing four things
  (split the outer rings, build the abort ring, build the recipe, bind the
  adapter); `Core::load` (`:261-286`) nests a re-make, a catch, and three
  outcomes. Both read harder than the style guide wants.
- `src/channel/registry.rs:62-68` encodes the registry once at make only to
  size-check it, then `channel_registry` encodes again every write. Keep the
  bytes and write them, as the design says, or drop the make-time encode
  and let the first write fail loudly.
- `src/channel/records.rs:31` `SequenceChannelEvent::MAX_LEN` is 1024; the
  plan said 256 and the deviation is not recorded.
- `src/fn_system/param.rs:469-474` `Cycle::seq()` panics on a `#[system]`
  fn; the macro prevents it, but `Ports` and `Param` are public, so a
  hand-written `SystemFn` with `Params = (Seq,)` reaches it at run time.
- `src/coordinator/config.rs:87-99` `adapter_keys::serialize` clones the
  adapter to build `Keys`; matching on `adapter.as_ref()` with borrowed
  fields avoids it.
- `src/dl/slot.rs:243-244` the `load` doc comment is wrong (finding 1).
- `docs/plans/12-sequences.md:184-208` still shows one `Channel` struct with
  `inner`, `recipe`, and `loaded`; the code splits it into `Channel {
  commands, core: Core }` for borrow splitting. Note it or fold `commands`
  into `Core` behind a `mem::take` of the drained commands.
- `src/channel/adapter/mod.rs:275-276` clones `name` twice on a successful
  load (`loaded` and the event).

## Not findings

- `apply` matches the design table for every command from every state, and
  `Command::of` covers every `SequenceCommandKind`.
- `adapter_keys`: `thread` and `channel` round-trip, neither key emits
  nothing, both keys are refused, a bad `start` is refused.
- Reset reclaims reader slots and the writer: `Core::remake` drops the old
  step before making the new one; a `View` drop leaves the slot inactive,
  `slowest_active_cursor` ignores it, and `view` registers at the published
  position, so a stale `AbortRequest` from before the reset is never read.
  `test_reset_makes_fresh_step` runs on a ring with one reader slot and
  would report `Failed` if the claim failed.
- Abort ring: allocated once per channel at make (`ABORT_DEPTH = 4`, one
  reader), bound at the inner `abort` position by name, kept in the recipe,
  and handed to a `DlSlot` through its rings so a guest occupant reads it;
  config edges into `abort` under a channel are refused
  (`ReservedChannelPort`), as are inner ports named `commands`, `events`,
  `channel_status`.
- `catch_step`/`Stepped`/`Entry::run`: an ended step is kept and skipped
  with zero time, an unwound step is faulted once and dropped, and
  `Channel::ended()` is `None` so a channel keeps reading commands after its
  system ends. `Coordinator::latched()` is the `Panicked` subset, so `metor
  run` does not fail on a completed sequence.
- `SequenceStep`: `now` and `aborted` are set before the poll; the poll runs
  under `with_log_port`, whose `RestoreSlot` guard resets the thread-local
  on unwind, so a panicking sequence cannot leave a dangling `LogPort`
  pointer; `Waker::noop()` is safe because the coordinator re-polls every
  cycle; deadlines arm on the first poll and count paused time; abort wins
  over a deadline on the same poll; `next_cycle` counts polls and ignores
  abort; a failure writes a `kind=failed` fault line before the end is set.
- `#[sequence]`: refuses non-async, a trailing `stop`, a non-`Result` return,
  `Timestamp` and `Abort` parameters; `#[system]` refuses `Seq`; `ports()`
  excludes nothing for a sequence since `stop` is false; the ui tests cover
  each.
- Pack ABI 9: words 0-4 map both ways, an unknown word is `Panicked`,
  `Unloaded` has no word and goes out as `Failed`, `DlStep` latches the end
  and returns early; `Instance.adapter` crosses as an externally tagged
  enum; `DefCxOwned.params` defaults to `Null` for a context an older host
  wrote.
- `dl::create` is unchanged in shape; `DlSlot::create` borrows the slot's
  rings for the call and the guest exports keep them alive.
- `store::put` never rewrites an existing path and goes through
  `copy_atomic`; `store::contains` canonicalizes both sides, so symlinks and
  `..` cannot escape (`test_contains_only_inside`).
- `FrameStr`: `CAPACITY = N - 1`, `new` refuses over-length and NUL,
  `to_str` is fallible on decoded bytes, one `U8` component of shape `[N]`
  with `is_string`; `FrameBytes` is one component; `ChannelStatus` is 112
  bytes with one `hash` component; `deploy::slot::check_name` and
  `Resolved::of` agree on the 63-byte bound.
- Steady-state allocation: `Channel::execute` decodes commands only when
  there are commands, `write_status` builds a `Copy` frame, `Output::write`
  encodes into its scratch buffer, `Abort::requested` counts a drain,
  `channel_registry` writes from a held value, `DlSlot::execute` forwards.
  Everything that allocates (events, refusals, re-make, load, `Pack::open`'s
  1 MiB descriptor buffers) runs at command rate, as decided.
- Python JSON matches what Rust parses: `{"channel": {"name", "start"}}` or
  `{"thread"}` beside `id`; `allow[]` entries with `name`, `ty`, `params`
  (`null` for none), `pack`; `initial` `null`; registry `channels[]` with
  `name` and `available`, `period = round(cycle_rate)`. `thread` with
  `channel`, `start` without `channel`, a bad `start`, an empty `allow`, a
  duplicate candidate, an `initial` not in `allow`, an unknown keyword
  input, and a candidate with its own inputs all raise.
- `check_channels` runs before `resolve`, so a channel on an async system
  reports `ChannelOnAsync` from `place`, and names are checked against
  `SEQUENCE_CHANNEL_NAME_CAP` in bytes, as the wire caps them.
