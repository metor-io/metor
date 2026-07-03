# Review fixes — execution plan (2026-07-01)

Source: `docs/review-findings.md`. Decisions confirmed with Sascha:

- **Lossless mode stays** (future features) — R6/R7 get real fixes, not deletion.
- **mmap stays**; **`rate_hint`/`of_at` deleted** (ABI version bump).
- **KDL parsing → in-house serde `Deserializer`** over kdl-rs nodes. Static
  params become `#[derive(Deserialize)]`; the dl path deserializes to
  `serde_json::Value` through the same deserializer (postcard-schema used for
  validation only). Deletes `FromKdlNode`/`FromKdlScalar`/`encode_kdl_params`;
  fixes B1 structurally.
- **E1 fixed as `WireError`** on non-delayed edges pointing backward in
  registration order (not a topological sort).
- Full scope this round: §1 bugs, §2 ring safety, KDL/serde refactor,
  A1 (frame/message unification), A2+A3 (command plane + slots), E2
  (`#[system]` macro), cleanliness + style batch.

Commit at each wave boundary (verified work only, targeted `git add`).
Pre-existing dirty files (`examples/cube-sat/src/main.rs`, `DESIGN.md`, etc.)
are not ours to commit.

## Waves

### Wave 0 — designs (parallel, docs only)
- [x] D1 `docs/design-ring-safety.md` — R1 (reserved_end seqlock), R6
      (registration race + fences), R7 (writer claim), R3 (32-bit math),
      B6 (gap-skip before lap test), R8 (ordering/comment), R4 (attach
      geometry validation). Miri plan.
      (done 2026-07-02: fence-to-fence seqlock; stable-committed registration
      loop for R6; OFF_WRITER claim word; ring VERSION→2; 32-bit Miri run.)
- [x] D2 `docs/design-kdl-serde.md` — serde Deserializer over KDL, span/miette
      error mapping, dl dynamic path, migration, E5 surface fixes
      (unknown-node rejection, `artifact=` rename, `type=` validation,
      named WireErrors).
- [x] D3 `docs/design-port-unification.md` — A1: schema × delivery ×
      cardinality axes, one Registry/HandOff/Tap, PortDesc, user-facing
      facades kept thin; subsumes A5-A7, half of S5 panicking accessors.
      (done 2026-07-02: 4th axis OnLap; 7 compile-green commits; single ABI
      bump folds into v3 if unshipped; open questions pending.)
- [x] D4 `docs/design-command-slots.md` — A2+A3: explicit command edges,
      slot aux ports in descriptors, name-based slot addressing (wire
      protocol change — call out ground/panel impact), lifecycle enum merge.
      (done 2026-07-02: PortConn axis; name addressing w/ fresh PacketIds;
      per-slot explicit edges, no broadcast sugar; ground inventory complete;
      open questions pending.)
- [x] D5 `docs/design-system-macro.md` — E2: `#[system]` attribute deriving
      bundles/impls/export; insulates users from A1 trait churn. Also E7
      (`Seq::now()`), E8a (`Out<>` removal direction).
      (done 2026-07-02: attr on impl block; recommends macro BEFORE
      unification; new metor-fsw-2-macros crate; open questions pending.)

### Wave 1 — independent small bugs (parallel with wave 0; disjoint files)
- [x] W1a coordinator/mod.rs: B2 (stopped-set membership), B3 (simulated
      clock u64), B7 (dup msg edge), B8 (cycle_rate validation), B10
      (self-loop needs delayed), B11 (run_for guard → documented panic), E1
      (backward-edge WireError, cyclic↔cyclic only; async exempt)
      (done 2026-07-02: E1 caught a real backward edge in adcs-fsw2
      mission.kdl — now delayed=#true; WireError dropped Eq for f64 payload).
- [x] W1b wiring + slot: B4 (resolve_static rejects Postcard params), B5
      (occupant name validation at resolve/build + runtime Load event)
      (done 2026-07-02: 3 fail-before/pass-after tests; 112 passed total.
      DEFERRED → wave 4 A3: add_slot builder path still lacks the build-time
      initial-occupant check — fold into the slot descriptor rework).
- [x] W1c dl.rs: R2 (FswStatus via u32 match), R5 (null-create → Stopped)
      (done 2026-07-02: FswStatus::from_raw helper in abi; end-to-end
      null-create test drives the real fixture .so; tests green).
- [x] W1d descriptor/abi: delete `rate_hint` + `of_at`, bump FSW_ABI_VERSION
      (done 2026-07-02: ABI 2→3; Hz kept — cycle_rate uses it; fixtures build
      from source, nothing to regenerate; docs updated; tests green).
- [x] B9: document init-time emit gap (defer real fix).
      (done 2026-07-02, docs pass: `docs/telemetry.md` §3 — a fresh registry `View` starts at
      the buffer's current commit point, so a frame/message emitted only during another
      system's `init` — which always completes before telemetry's own `init` claims its taps,
      since telemetry is registered last — is invisible to the downlink for the rest of the
      run; same hazard for a live panel connecting mid-mission. Real fix (a backlog
      replay/re-publish) stays deferred.)
→ **Commit** per group when tests pass.

### Wave 2 — ring safety implementation (after D1)
- [x] R1, R6, R7, R3, B6, R8, R4 per design; ring tests + Miri
      (`ring/MIRI.md` recipe). → **Commit.**
      (done 2026-07-02, commit 07406cfc: fence-to-fence seqlock closing the torn-read window
      (R1, is_lapped moved to reserved_end); OFF_WRITER CAS claim word (R7); lossless
      registration loops until `committed` is stable across the cursor CAS + a SeqCst fence in
      `slowest_active_cursor` (R6), plus new `ReadError::Corrupt` for lossless length/straddle
      validation; 32-bit-safe straddle math (R3); gap-skip-before-lap-test (B6); ordering
      comment (R8); attach geometry validation (R4). `docs/ring-buffer.md` already reflects
      this shape — verified accurate against the code in this docs pass, not rewritten.)

### Wave 3 — KDL serde refactor (after D2; rebases on wave 1)
- [x] Deserializer, migration, delete bespoke traits, B1 regression test,
      E5 fixes. → **Commit.**
      (done 2026-07-02: wiring/de.rs + 18 unit tests; seq-param-fixture crate;
      lib=→artifact= hard rename; unknown top-level nodes rejected; Dl* error
      variants folded; FromKdlNode derive deleted from old macros crate.
      Deviations: telemetry/slot child nodes stay hand-parsed (shape);
      BadSlotState kept. Known pre-existing: metor-fsw standalone build broken
      at HEAD via metor-proto-wkt nox-gating — unrelated. 138 lib tests green.)

### Wave 4 — A1 unification (after D3), then A2+A3 (after D4)
- [x] W4a: command/slots PHASE 1 (A1-independent): name-addressed
      SequenceCommand/Event (fresh PacketIds 224,58-60 — design's 45-47
      collide with node protocol), SlotState merge, ChannelId deleted,
      telemetry-last WireError, full ground migration (panel 137 tests green,
      cube-sat, db). Also fixed pre-existing metor-fsw wkt gating build break
      and a silent dl_integration skip-pass. Docs still say channel_id →
      wave-6 docs pass. (2026-07-02)
- [x] W4b: A1 unification landed as 7 staged commits (0ba6c284, 5e5f9ad0,
      c79fa21a, 16b1b20c, c171bd28, 2e52afa4, 8e628dfd): four-axis PortDesc
      (schema × delivery × fan-in × on-lap) + telemetered flag + capabilities;
      PortId::Component/Packet; NamedMsg; one registry/tap/handoff/drain;
      connect_msg + CommandOut-type deleted (alias + token lowering);
      schema-tagged ABI folded into unshipped v3. (2026-07-02)
- [x] W4c: command/slots phases 2-5 landed as 4 commits (A2 90715ed9, A3
      739b0d52, A9 c5090d17, A8): explicit command edges (n_slots/
      command_producers residue deleted; slot declares a MsgIn<SequenceCommand>
      fan-in; KDL uplink{} node + reserved "coordinator" name + joint msg-edge
      resolution); PortConn axis {Edge, Host, SelfTap} — SlotAux/pop/re-append
      die, W1b builder-path initial-occupant check folded in; coordinator #0
      full bundle (keys golden); uplink RouteMsg dispatch + ReloadSequences
      second output + uplink.unroutable. Deviations: HostPort rejects
      consumer-side host inputs only (a Host OUTPUT must accept edges —
      coordinator.commands); zero-command-edge slots allowed silently
      (documented, no warn channel in resolve). metor-panel unchanged
      (wire format stable since W4a). (2026-07-02)

### Wave 5 — E2 `#[system]` macro (landed BEFORE wave 4, per the design's recommended order)
- [x] Macro + migrate adcs-fsw2 example + static-linking example (E8d).
      → **Commit.**
      (done 2026-07-02, commit 9ea79274: `metor-fsw-2-macros` crate; `#[system]` attribute on
      the inherent impl block, reading ports off `execute`/`run`'s signature; E3 `latest() ->
      Option<FrameRef>` (no `Result`), E6 infallible `publish`/`publish_with` +
      `SystemOutput::take_dropped`, `Out::split`, E7 sequence `now()`/`Seq::now()` + injected
      `<B: Backing>`. nav/ctrl/plant/commissioning/safe-mode examples converted — see
      `examples/adcs-fsw2/systems/nav/src/lib.rs`. `docs/system.md` §7 and
      `docs/sequences-slots.md` §4.1 updated to the shipped authoring surface in this docs
      pass.)

### Wave 6 — cleanliness + style batch (cheap models OK)
- [x] C2 build() split (post-A1 residue), C3 dedup, C4 remnants not subsumed
      by A1, C5 dead state, C6 structure, S1-S5, E4/E8b/E8e, B9 doc comments.
      → landed as 5 code commits (f05c74c5, 2ddd80a2, e623daf2, ef2099d6,
      860ace53) + 2 docs commits (a54d9633, a7cb29a8). Zero clippy warnings;
      workspace lints pinned for the fsw-2 crates. Deliberate keeps:
      WiringBuilder::connect_msg (model has no descriptors to infer from),
      dual wait()/Seq API (E7 design outcome), Box::leak'd slot names
      (descriptor name type change out of scope). Deferred with plan:
      TODO(E6, seq path) drop-counter fold. (2026-07-02)

## CAMPAIGN COMPLETE (2026-07-03)
24 commits, 69b59405..860ace53. All review findings addressed except the
deliberate keeps/deferrals noted per wave. Final gate: 245 tests across
metor-fsw-2 (169 lib + 26 integration + 1 doc), ring (37), macros (9),
adcs-fsw2 (5) — zero failures, zero clippy warnings.

## Decisions (2026-07-02, confirmed with Sascha)
- Ring header layout change lands WITHOUT a version bump (early dev, breaking
  change fine; persisted v1 dev regions just get recreated).
- Old SequenceCommand/Event PacketIds retired with NO legacy panel decoder.
- KDL: hard rename `lib=` → `artifact=` on system nodes (guidance error, no
  alias); repeated property/key is an error (stricter than KDL last-wins).
- PortId::Frame/Msg rename to Component/Packet in the unification.
- Executive calls (from design open questions): NamedMsg is an fsw-2-local
  trait; Many×Snapshot fan-in rejected; untelemetered ports stay in the
  registry behind a flag; unification ABI change folds into the unshipped v3;
  E3 latest() folds into the unification's policy commit; #[system] macro
  lands BEFORE the unification (both designs agree user code is insulated);
  control_handle becomes take-once Option (fixes the R7 multi-writer mint,
  converges with D4); SystemOutput::take_dropped approved; #[fsw(...)] helper
  namespace on the new macros crate; exports opt-in via export="feature" arg;
  fan-out connect sugar out of scope v1; uplink gets real multi-output
  dispatch; telemetry-last ENFORCED as WireError; Dl* LoadError variants fold
  into shared ones; SystemSpec.ty becomes Option with serde(default); R6
  registration loop unbounded (documented); ReadError::Corrupt added.

## Status log
- 2026-07-02: Wave 0 designs complete (D1-D5). Wave 1 complete (W1a-W1d),
  B9 doc deferral pending, committing now.
- 2026-07-02: Docs sweep (parallel with the wave-6 code pass): DESIGN.md, `docs/*.md` prose
  reconciled against the code through wave 5 + A1/A2/A3/A8/A9 (waves 2/3/5 checkboxes above
  corrected; B9 documented and checked off). `docs/design-*.md` marked LANDED (status headers
  only). See individual doc diffs for per-file detail; `docs/ring-buffer.md`,
  `docs/frames.md`, `docs/vtable-dynamic.md` were verified already-accurate and left
  unchanged. `db-dynamic-streaming.md` is unrelated to this refactor (a metor-db design doc)
  and was skipped.
