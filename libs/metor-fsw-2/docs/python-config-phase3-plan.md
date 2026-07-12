# Phase 3 plan — IR bundles + `WiringManifest` telemetry

> **Status: LANDED.** Implements Phase 3 of `docs/design-python-config.md`
> (§8 bundles, §9 visualization, §11 phasing). Phases 0-2 landed through
> `fb938709`: manifest sidecars, the `metor_config` recorder, generated typed
> packs with `sha256:` staleness enforcement, and ABI v6 doc strings. Phase 3
> shipped the four milestones below: the `WiringManifest` wkt message +
> coordinator emission (M1), the `wiring.json` + `meta.json` IR bundle with the
> target-triple and manifest-hash checks (M2), the single-file `.metor`
> uncompressed-tar form (M3), and the `package --check-ir` determinism gate +
> this docs sweep (M4). The bundle format is documented in `cli-runner.md` §4.

Goal: the bundle becomes the frozen, versioned IR (`wiring.json` + `meta.json`
+ `.so`s, with a target-triple check and a single-file form), so a `.py`
mission packages and runs with no Python and no KDL on target; and the running
FSW emits the full IR as a `WiringManifest` wkt message at startup and on
reload, giving the panel live/historical topology over the existing pub/sub
plane. The panel graph tile itself is out of scope — the contract is the
versioned IR plus the message.

Scope discipline: no KDL retirement (Phase 4 — the KDL front-end keeps
working and `parse` stays; only the *bundle's* verbatim-KDL carrier is
replaced), no uplink `msgs=` derivation, no scope-table consumers.

## Grounding (what the code says today)

- `wiring/bundle.rs` writes `mission.kdl` (verbatim source) + `meta.kdl`
  (abi_version, ir_version, profile, built_at_unix) + copied `.so`s and their
  `<cdylib>.manifest` sidecars ("unread today" — Phase 3 reads them).
  `load_bundle` re-parses the KDL; `cmd_package` refuses `.py` missions
  outright with a "Phase 3" message.
- `Wiring` is fully serde (`ir.rs`), including `manifest_hash` (Phase 2) and
  `src`/`scopes`. `serde_json::to_string(&wiring)` is already the emitter
  contract (`tests/ir_contract.rs` pins it).
- The `SequenceRegistry` pattern to copy (`coordinator/mod.rs`): a
  `PortDesc::msg_named::<SequenceRegistry>("sequences")` Host output on the
  coordinator #0 bundle, an owned `MsgOut` writer bound at build,
  `emit_sequence_registry()` called once at the head of `run_for` and re-fired
  on a `ReloadSequences` drain. Unmatched wkt messages recorded as telemetry
  are the pub/sub plane, so no new downlink surface is needed.
- **Size constraint (measured):** the adcs mission IR is ~5.4 KB compact JSON,
  over `MAX_MSG_BYTES` (4096). The `wiring` output ring must be sized from the
  actual serialized payload at build time (the coordinator has the `Wiring` in
  hand), not the default message cap.
- `build_driver.rs` already has `host_triple()` (from `cargo -vV`) and
  `requested_target()` (from `--target` args) — the target-triple recorder
  reuses them.
- `metor-proto-wkt` has no `WiringManifest` yet; `AlarmDef`/`SequenceRegistry`
  live in `wkt/src/msgs.rs` with pinned `Msg::ID`s.

## Milestones

Four milestones, one commit each, in order.

### M1 — `WiringManifest` wkt message + coordinator emission

1. **`WiringManifest` in `metor-proto-wkt`**: `{ ir_version: u32, ir_json:
   String }` — the payload is the full IR as JSON (the §6 wire format;
   self-describing, diffable, and exactly what `load_bundle`/panel consumers
   already speak). Pin its `Msg::ID` in the wkt id test. postcard carries the
   JSON string across the ring; consumers `serde_json` the payload.
2. **Coordinator emission**: `resolve` serializes the (built, path-stripped —
   see M2.3) `Wiring` once and hands the string to the builder; the
   coordinator #0 bundle gains a `PortDesc::msg_named::<WiringManifest>
   ("wiring")` Host output whose `max_size` is computed from the actual
   payload (rounded up with headroom), telemetered. `emit_wiring_manifest()`
   fires at the head of `run_for` next to `emit_sequence_registry()`, and
   re-fires on the same `ReloadSequences` drain (the panel's re-emit request
   channel; topology doesn't change on reload today, but slot occupancy
   consumers resync off one message).
3. KDL front-end parity for free: both front-ends produce `Wiring`, so the
   manifest flows regardless of the mission's source language.
4. Tests: wkt id pin; a TestBench/coordinator test that the boot message
   decodes back to the same `Wiring` (round-trip through the ring); a
   ReloadSequences-triggered re-emit test; an oversized-mission sizing test
   (IR > 4096 bytes emits intact).

### M2 — the IR bundle (`wiring.json` + `meta.json`)

1. **Layout** (replaces `mission.kdl`/`meta.kdl` in `write_bundle`):

   ```
   mission.bundle/
     wiring.json         frozen versioned IR (src anchors, scopes, hashes)
     meta.json           abi_version, ir_version, target, profile,
                         built_at_unix, ir_sha256, metor_config_version
     lib<pack>.so        + <so>.manifest sidecars (already copied)
     mission.py|.kdl     optional provenance copy, never consumed
   ```

   `meta.json` is plain serde JSON (a `BundleMeta` struct) — the KDL meta
   parser in `bundle.rs` goes away with it. `target` records the built triple:
   `requested_target(extra_args)` when cross, else `host_triple()`.
   `ir_sha256` hashes the `wiring.json` bytes (the determinism backstop: CI
   re-evaluates and diffs).
2. **`write_bundle`** takes `&Wiring` + `BundleMeta` inputs instead of the
   KDL text; the wiring is serialized with artifact `path`s stripped (paths
   are re-derived on load; a bundle must stay relocatable and reproducible).
   `src` anchors and scopes are kept — they are the panel's deep-link data.
   The optional provenance copy of the source file rides along verbatim.
3. **`load_bundle`**: read `meta.json`; check `abi_version`, `ir_version`,
   and **target triple** against the host (a mismatch is a clean
   `BundleError::TargetMismatch`, today's dlopen mystery); deserialize
   `wiring.json`; fill artifact paths from the copied `.so`s; verify each
   artifact's recorded `manifest_hash` (when `Some`) against its copied
   sidecar bytes *at load* — same hash function as stubgen — so a
   tampered/mismatched bundle fails before resolve.
4. **Compatibility**: a directory containing `mission.kdl` + `meta.kdl` (an
   old-layout bundle) is rejected with a "rebuild the bundle" error naming
   the layout change — bundles are rebuildable by design; no migration
   shims.
5. **`cmd_package` accepts `.py`**: `load_source` → `build_artifacts` →
   `write_bundle`, deleting the refusal. KDL missions package through the
   same IR path (their `ParamSource::Kdl` text serializes fine).
6. Tests: rewrite `wiring/bundle.rs` tests + the adcs `--test bundle` target
   for the new layout; target-mismatch negative; tampered-sidecar negative;
   `.py` package→load→resolve round-trip over the dl fixture; old-layout
   rejection.

### M3 — single-file bundle (`.metor`)

1. **Format: uncompressed `tar`** (design open item 5 — leaning recorded
   here: tar over zip because it is streamable, order-stable for reproducible
   bytes, and a plain `tar` crate with no compression keeps the load path
   `mmap`-friendly after unpack; `.so`s don't compress usefully anyway).
   Extension `.metor`.
2. `metor-fsw package -o mission.metor` (extension-dispatched) writes the
   directory layout into one tar with a **stable entry order** (meta,
   wiring, then artifacts sorted by id, then provenance) and zeroed
   tar timestamps — byte-reproducible given identical inputs.
3. `load_bundle` on a `.metor` file unpacks to a temp dir (or reads entries
   directly for meta/wiring and unpacks only `.so`s — dlopen needs real
   files) and proceeds identically. `is_bundle` learns the extension.
4. Tests: pack→load round-trip; reproducibility (two packs of the same
   inputs are byte-identical); the adcs bundle target gains a `.metor` leg.

### M4 — CI determinism gate + docs sweep

1. `metor-fsw package --check-ir <bundle>`: re-evaluate the mission source
   named by the bundle's provenance copy, diff the produced IR against
   `wiring.json` (normalized: paths stripped), non-zero on drift — the
   §2 "determinism enforced operationally" hook, runnable in CI.
2. Docs: `docs/wiring.md` bundle section rewritten for the new layout;
   `design-python-config.md` decisions log updated (single-file format
   decided); the Phase 3 plan marked landed.
3. Full-net verification: `cargo test -p metor-fsw-2`, all tracked adcs
   targets, clippy no-new-warnings, Python suite.

## Known constraints and traps

- Never run bare `cargo test -p adcs-fsw2`; use the tracked `--test` targets
  (bundle, closed_loop, sequences, alarms, eclipse, momentum, equivalence,
  stubgen).
- Pre-existing rustfmt drift and the two clippy warnings — no blanket
  formatting, add no warnings.
- `MAX_MSG_BYTES` is 4096 and the adcs IR is ~5.4 KB compact: the
  `WiringManifest` ring must be sized from the concrete payload. Do not raise
  the global cap.
- Bundle `wiring.json` must strip artifact `path`s and stay byte-reproducible:
  no timestamps inside `wiring.json` (meta.json carries `built_at_unix`,
  which is provenance, not identity — it is excluded from `ir_sha256`).
- `meta.kdl`→`meta.json` changes the bundle contract; the panel does not read
  bundles today, so the blast radius is the adcs bundle test + CLI.
- The provenance `mission.py` is never consumed on load — resolve reads only
  `wiring.json`. Keep it that way; the run path must need no Python.

## Acceptance for the phase

- `metor-fsw package mission.py -o adcs.bundle` (and `.metor`) works; the
  bundle runs cargo-free via `metor-fsw run`; wrong-target and
  tampered-sidecar bundles fail with clean errors before any dlopen.
- A running mission's recorded telemetry contains a `WiringManifest` whose
  payload round-trips to the resolved `Wiring`; re-emit on `ReloadSequences`.
- Bundle bytes are reproducible for identical inputs; `--check-ir` catches an
  IR drift.
- Full net green: metor-fsw-2 suite, all tracked adcs targets, clippy clean
  of new warnings, Python suite.
