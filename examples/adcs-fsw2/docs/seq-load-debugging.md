# DEBUGGING NOTES: panel can't load/swap sequences (mode slot)

Working state for an in-flight investigation, 2026-07-11. Delete when resolved.

## STATUS 2026-07-11: fixes applied, pending live panel re-test

The full debug/fix plan below has been executed; only step 5 (live panel
re-test, both startup orders) remains:

1. DONE — `Refused { reason }` added to `SequenceEventKind` (wkt); every
   command the slot state machine rejects now emits one (`do_load`/`do_start`/
   `do_stop`/`do_abort`/`do_reset`); the panel shows it as the status line
   without touching run state (`refused_reports_without_changing_run_state`).
2. DONE — `swap_repro.rs` PASSED unmodified: the backend Done→Load→Start path
   worked all along over the real uplink; the bug was purely the state gate.
3. DONE — semantic fix: `Load` now legal from any state except
   Running/Loading; loading over `Loaded` swaps occupants in place. Tests:
   `load_from_loaded_swaps_occupant`, `reset_then_load_swaps_occupant` (the
   exact reported wedge), `load_while_running_is_refused` (slot_integration).
4. DONE — the coordinator #0 bundle consumes `ReloadSequences` and re-emits
   the `SequenceRegistry` (test `reload_request_reemits_registry`);
   mission.kdl wires `uplink -> coordinator msg="ReloadSequences"`, so the
   panel's Reload button now works and a late-started panel recovers its
   Load picker.
5. TODO — live re-test with the real panel, both startup orders. If green,
   delete this file (and `tests/swap_repro.rs` if no longer wanted).

## Symptom

Loading a sequence from the panel leaves the channel on the existing sequence.
Reported in BOTH startup orders:
- Panel started AFTER the mission: panel shows `mode` Completed, "no sequence
  loaded", last message "commissioned"; Load and Reset both appear dead.
- Panel started FIRST (user confirmed): still can't load anything, even after
  Reset.

## Established facts (all verified against the tree/tests this session)

1. **Slot Load gate** (`libs/metor-fsw-2/src/coordinator/slot.rs` `do_load`):
   Load is accepted ONLY from `Empty | Done | Stopped`. Refusals from
   `Running` and `Loaded` are **silent** — bare `return`, no `Failed` event
   (the unknown-name arm right below DOES emit one). This gate predates the
   packs arc (identical at commit 49068ccd).
2. **Reset → Loaded → Load is a wedge**: after Reset (or Stop), state is
   `Loaded`, and `Loaded` refuses Load. So the user flow
   "Completed → Reset → Load(other)" is silently refused — this matches
   "can't load anything, even after reset" EXACTLY. There is no `Unload` in
   `SequenceCommandKind` (the design doc has it; the wire enum never grew it),
   so from `Loaded` the only exits are Start or Reset (same occupant).
3. **Old cube-sat sequencer allowed Load from any non-Running state**
   (`examples/cube-sat/src/sequencer.rs:264` — only gate is `running`). The
   panel was built against those semantics. fsw-2's SlotRunner tightened this.
4. **Panel Load button sends nothing directly** — it opens a picker over the
   channel's `available` list (`libs/metor-panel/src/views/sequence_panel.rs:376`).
   `available` comes only from the boot `SequenceRegistry` msg.
5. **The boot registry is one-shot** (`Coordinator` `seq_registry_emitted`
   flag, emitted before cycle 1) and the panel's **Reload button publishes
   `ReloadSequences`, which NOTHING in metor-fsw-2 consumes** (only registered
   in the msg table + used in a telemetry subscription test). Panel started
   late ⇒ empty `available` forever ⇒ Load picker empty. Also misses the boot
   `Loaded` event ⇒ "no sequence loaded".
6. **Panel does not gate Reset**: `is_resettable(Completed)` is true, click
   handler publishes unconditionally (`sequences/mod.rs:294-327`).
7. **The uplink command plane works when the broker is up first**: the user's
   live-leg repro `examples/adcs-fsw2/tests/abort_repro.rs` (embedded metor-db
   broker + real TcpUplink + panel-exact push_msg of Abort) PASSES (~41s).
   Uplink transport redials with 100ms→5s capped backoff and re-subscribes on
   each connect (`telemetry/mod.rs` TcpRecvTransport::ensure/recv) — looks
   sound for late-broker recovery but that exact order is untested.
8. Panel state is synced ONLY by events; silent slot refusals mean panel UI
   and slot state can diverge and stay diverged (the user may be clicking
   from a stale picture: panel says Completed while the slot is really
   Loaded post-Reset, etc.).

## Leading theory

Panel-first case: user reaches Completed, hits Reset (slot → Loaded), then
Load(safe_mode) → silently refused by the `Loaded` gate (fact 1/2). Without
Reset, Load directly from Done SHOULD work (backend-tested for the process
path in slot_integration's swap test) — untested end-to-end for the in-proc
dl path with the panel's exact flow; and any earlier stray click can move the
slot out of Done and into a refusing state without any feedback (fact 8).
Panel-after-boot case additionally hits the empty-`available` picker (facts
4/5).

## Repro assets

- `examples/adcs-fsw2/tests/abort_repro.rs` (user-written, passes): live-leg
  Abort while parked in warm-up (`est_delta_rad=0.0`,
  `warmup_timeout_s=1000000.0`, broker on 127.0.0.1:23240, `process=false`d).
- `examples/adcs-fsw2/tests/swap_repro.rs` (written this session, NOT yet
  run): same live leg, drives Abort→Done, then Load{safe_mode}+Start via
  push_msg, samples `mode.slot_status` (phase, occupant) transitions.
  Asserts (3,"commissioning") then (2,"safe_mode"). Broker 127.0.0.1:23241.

## Debug/fix plan (agreed direction, not yet applied)

1. **Observability first**: `do_load`/`do_start`/`do_reset` emit a `Failed`
   event on every refusal (state + reason), so the panel shows why a click
   did nothing. This alone would have made the whole bug legible.
2. **Backend truth**: run `swap_repro.rs` — (a) Done→Load(safe_mode)→Start
   should pass today; (b) add Reset→Load(safe_mode) which should FAIL today,
   confirming the wedge.
3. **Semantic fix (cube-sat parity)**: accept Load from `Loaded` (drop the
   current occupant — post-Stop it has no live future; pre-Start it was never
   polled — and build the named one). Keep `Running` → Load rejected but now
   with the Failed event. Tests: slot_integration Load-from-Loaded swap
   (in-proc + process), Running-Load emits Failed.
4. **Registry re-emission**: consume `ReloadSequences` — natural shape: the
   coordinator (#0) gains a `reload: MsgIn<ReloadSequences>` input in its
   bundle; drain each cycle; re-emit the `SequenceRegistry` on request.
   mission.kdl: uplink `msgs` list gains "ReloadSequences" +
   `connect "uplink" -> "coordinator" msg="ReloadSequences"`. Panel's Reload
   button then actually works; late-started panels recover `available`.
   (Alternative considered: periodic re-emit; explicit request is cleaner.)
5. Live re-test with the real panel, both startup orders.

## Context from the completed packs arc (background)

The whole authoring/pack arc (WP0-WP6) landed this session as commits
4c8da7f2..2896ba32; all suites green including slot swap tests. The Load gate
issue is pre-existing, not an arc regression. `SeqClock`→`CycleClock`,
occupant tail is mount-appended, sequences are `Pack::task` entries in
`examples/adcs-fsw2/systems/adcs-sequences`.
