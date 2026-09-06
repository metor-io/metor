# fsw2 pre-merge simplification plan

Goal: delete and simplify. Inline single-use functions, merge near-duplicates,
delete dead code and in-body architecture prose. All findings below were
verified by reviewers with workspace-wide greps (examples, python, macros
included). Baseline: `cargo test -p metor-fsw-2 -p metor-fsw-ring -p metor-fsw-2-macros`
is green.

## Batch 1 — wiring + ir

1. `src/wiring/resolve.rs:1101` — `resolve_endpoint` is only called from the
   `EdgeKind::Frame` match arm (~207–226); its `kind` param is always `Frame`.
   Delete the `EdgeKind::Msg` arm, drop the `kind` param and call-site args,
   keep the Frame logic. (~18 lines, high confidence)
2. `src/wiring/bundle.rs:191` — `sha256_hex` is byte-identical to
   `stubgen::manifest_hash` (bundle.rs already calls that at :404). Delete
   `sha256_hex`, use `super::stubgen::manifest_hash` at :249. (~10 lines, high)
3. `src/ir.rs:280` — `ParamSource::is_none()` has zero callers. Delete method +
   doc. (~6 lines, high)
4. `src/wiring/builder.rs:143` — `connect` / `connect_delayed` are identical
   except the `delayed` flag; `connect_msg` shares the EdgeSpec push shape.
   Back the frame pair with one private helper (or have `connect_delayed`
   delegate). Don't force `connect_msg` in if it reads worse. (~14 lines, medium)
5. `src/wiring/build_driver.rs:281` — `current_host_triple` is a one-line
   pass-through to `host_triple` with a single caller (`bundle.rs:375`). Delete
   the wrapper, call `host_triple` directly. (~4 lines, medium)

Skipped (reviewed, deliberately not doing): `OccupantEntry` collapse in
resolve.rs (risky, shared body is real); cross-module test-fixture hoisting
(adds coupling for ~10 lines).

## Batch 2 — coordinator

6. `src/coordinator/slot.rs:169-184` — `impl InitialOccupant { loaded(),
   running() }` never called; construction sites use struct literals. Delete
   the impl block. (~16 lines, high)
7. `src/coordinator/mod.rs:538-553` — `Coordinator::emit_sequence_registry` and
   `emit_wiring_manifest` are pub forwarders with zero callers repo-wide (boot
   goes through `channels.emit_boot()`, reload through `service_reload()`).
   Delete both. The `CoordChannels` methods they forward to stay. (~16 lines, medium)
8. `src/coordinator/slot.rs:254` — `plan_slot`'s `_initial:
   Option<&InitialOccupant>` param is never read. Drop it and the args at its
   7 call sites (resolve.rs + 6 tests). (medium)
9. `src/coordinator/mod.rs` `run_for` body (~608-660) — trim multi-line design
   comments that restate module docs (per-slot command drain block at 637-641,
   boot-emission note at 608-610, Simulated-yield note at 657-660) down to the
   non-obvious constraint, or delete. (~10 lines, low)

Skipped: `BufferRole` payload removal (deliberate Debug aid, documented).

## Batch 3 — telemetry / message / health

10. `src/message.rs:381-401, 617-656` — `MsgIn::try_next` and `MsgIn::drain_iter`
    have no caller anywhere (all consumers use `drain`); `drain_iter` calls
    `try_next` so both go together, plus their test
    `msg_in_try_next_and_drain_iter`. Delete all three. (~55 lines, medium —
    public API, but zero users and pre-merge is the time)
11. `src/telemetry/tests.rs:862-881, 973-992, 1219-1238` — `MockRecv` defined
    verbatim twice; `ScriptedRecv` is the same shape. Hoist one module-level
    scripted `RecvTransport` mock, delete the copies. (~30 lines, medium-high)
12. `src/telemetry/tests.rs:884-907, 1240-1263` — `AckSink`+`AckSinkIn`+
    `AckSinkOut`+impls byte-identical in two tests. Hoist to one module-level
    helper. (~24 lines, high)
13. `src/telemetry/mod.rs:179-214` — `TcpTransport::try_announce` has exactly
    one caller (`announce`) which only wraps it to drop `conn` on error.
    Inline. (~6 lines, medium)

Skipped: inlining `publish_health`/`flush_logs` in health.rs (named steps read
well, saves ~5 lines).

## Batch 4 — ring / proc / dl

14. `ring/src/lib.rs:183, 668-669, 968, 1001-1008, 1116` — reader-slot `epoch`
    word is only ever `fetch_add`'d, never loaded; speculative forward-compat
    for a live reclaimer that doesn't exist. Delete the field (widen `_pad`
    40→48), the `slot_epoch` accessor, both `fetch_add` sites, the init line,
    and the ~10-line comment in `view()`. Bump the layout `VERSION` const.
    Run the full ring test suite after. (~18 lines, medium)
15. `src/dl.rs:295-300` — `read_manifest_sidecar` is
    `#[cfg_attr(not(test), allow(dead_code))]` with only `#[cfg(test)]`
    callers in build_driver.rs; kept for a python-config phase-2 TODO. Delete
    fn + doc + the two test call sites (re-add in phase 2 if needed; the live
    path is `manifest_sidecar_bytes`). (~9 lines, medium)
16. `src/proc/worker.rs:305-312` — 8-line in-body block re-explaining the
    panic/terminal-ack policy already covered by the module header. Trim to a
    one-line non-obvious note. (~7 lines, low)

## Batch 5 — core surface + integration tests

17. `tests/slot_integration.rs:71`, `tests/proc_integration.rs`,
    `tests/dl_integration.rs`, `tests/wiring_resolve.rs` — `locate_fixture`
    (~35 lines) and `fixture_lib_name`/`fixture_lib_stem` are copy-pasted
    verbatim across 3–4 integration test files. Extract one copy into
    `tests/common/mod.rs` and `mod common;` from each. (~80 lines, high)
18. `src/system/mod.rs:47-55, 67-73` — `SystemInput::port_descs` and
    `SystemOutput::port_descs` default methods are used only by the unit test
    `fsw_attrs_lower_onto_descriptors`. Delete both; inline
    `decls().into_iter().filter_map(PortDecl::into_port).collect()` in that
    test. (~12 lines, medium)
19. `src/dynamic.rs:139-168, 232-259` — in `FrameList` and `FrameMap`,
    `vtable_fields` and `element_fields` are byte-identical past the one-line
    prefix derivation. Factor each pair's shared body into a private helper
    taking the prefix. (~10 lines, medium)
20. `src/registry.rs:141-149` — `Registry::len`/`is_empty` have no caller
    anywhere. Delete both together (clippy len_without_is_empty). (~9 lines, medium)
21. `src/registry.rs:203-207` — `AllOutputs::get` unused; consumers use
    `.entries()`. Delete. (~6 lines, medium)
22. `src/registry.rs:75-80` — `RegistryEntry::frame_id()` called only from 3
    assertions in telemetry/tests.rs. Delete; have those tests match on
    `desc.schema` directly. (~6 lines, low)

## Execution rules

- Apply batch by batch; after each batch run
  `cargo test -p metor-fsw-2 -p metor-fsw-ring -p metor-fsw-2-macros`.
- Also `cargo check` the workspace consumers at the end:
  `cargo check -p adcs-fsw2 -p cube-sat` (verify exact package names first).
- If a finding turns out wrong when you open the file (symbol actually used,
  line numbers drifted), skip it and note why — do not force it.
- Do NOT commit; leave the working tree for review.
- House style: comments only for non-obvious constraints; no defensive
  re-validation; match surrounding idiom.
