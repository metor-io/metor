# Session DB cleanup and saving

Implemented: 2026-09-06. The original review and plan are retained below.

## Original findings

- `src/main.rs` creates the DB at the fixed `temp_dir()/metor_panel` path,
  with no cleanup. `DB::create` creates directories but does not clear old data.
- `PanelApp::new(Arc<DB>)` accepts a consumer-owned DB. Cleanup must be opt-in
  for those callers and the default for the standalone app's session DB.
- Closing the last window deliberately keeps the app running. The existing
  `on_app_quit` hook in `src/connections/mod.rs` only saves layouts.
- Connections, LoD, dynamic producers, and backfill can retain DB handles;
  several threads are detached. DB and log objects retain filesystem paths,
  so moving a live directory would leave subsequent writes using old paths.

## Original plan

1. **Own each temporary session.** Add a small session-storage owner and a
   managed-session constructor/builder option for `PanelApp`; use it from
   `main.rs`. Create a unique temporary directory per launch (promote
   `tempfile` to a runtime dependency), track its cleanup/save disposition,
   and leave `PanelApp::new(db)` caller-owned. Do not automatically delete
   the existing shared directory, which may contain wanted recordings.

2. **Add “Save DB on exit…”.** Expose a command in the palette and chord
   menu, using GPUI's existing `prompt_for_new_path` pattern. Pick a name
   ending in `.metor`, record the destination, and show that saving is
   scheduled for exit. Allow changing or clearing it; cancelling the picker
   leaves the previous choice intact. For this first version, the bundle is
   the DB directory itself, without compression or extra layout files.

3. **Finalize once on application quit.** Coordinate shutdown in `app.rs`:
   save layouts, prevent new work, cancel and await DB writers (connections,
   optional server, LoD, dynamic producers, backfill/hydration), then flush
   DB metadata and mapped logs and release handles. Add explicit shutdown
   tracking and a DB flush helper where needed. Use the GPUI quit lifecycle,
   not window-close cleanup or assumptions about destructors running after
   the event loop. Delete an unsaved session; otherwise move it to the chosen
   `.metor` path. Moving at exit avoids rebinding every live DB consumer.

4. **Handle failures and verify.** Refuse existing destinations. For a move
   across filesystems, copy to a staging directory beside the destination,
   finalize it, and only then remove the source. On save failure, preserve
   the source and report its recovery path. Test ordinary quit cleanup,
   reopenable bundles including recent samples/messages, picker cancellation,
   destination collisions, failed/cross-filesystem saves, concurrent sessions,
   and caller-owned DB preservation. Manually check quitting during ingestion
   and closing/reopening the last window.

Normal application exit is the initial scope; crash/SIGKILL recovery and
opening bundles in the panel can follow separately.


## Validation

- Managed sessions use `PanelApp::temporary()`; `PanelApp::new(db)` retains
  caller ownership. “Save DB on exit…” is available directly in the command
  palette and through the session database page (leader, then `d`).
- Quit finalization runs synchronously because GPUI limits asynchronous quit
  observers to 100 ms. It joins tracked producers and backfill before storage
  cleanup. Saves drain retained WAL readers through `DB::flush()` so accepted
  samples/messages survive executor teardown.
- All 12 final session tests and the background-task teardown test pass. They
  cover picker selection/cancellation, normal quit versus window
  close, executor teardown, queued samples/messages, concurrent sessions,
  destination collisions, staged copy success/failure, and caller ownership.
- All 84 DB library tests passed. The panel library suite passed 456 tests,
  including the initial 12 new storage/shutdown tests, with three font-setup
  failures. Those same three failures reproduce in an isolated HEAD checkout:
  `timeline_accessory_draws_below_anchored_menu_and_applies_time_edits`,
  `event_chip_body_hits_the_rendered_burst_and_span`, and
  `right_click_opens_shared_commands_and_can_execute_them`.
- All panel targets compile. Clippy completes with existing warnings in
  unrelated code. Native platform dialogs and an actual cross-device move
  still need manual verification; the picker and copy fallback have automated
  coverage.
- Existing data at the old shared temporary path is retained. Save failures
  preserve the source and log its recovery path; forced termination/crash
  cleanup remains outside this change.
