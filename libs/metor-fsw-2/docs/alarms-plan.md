# Implementation plan — alarms

> **Status: COMPLETE (2026-07-05).** All six waves landed in order, one commit each.
> Deviations from the plan as written: target resolution moved from the first execute to
> `init` (with `AllOutputs` in the **output** bundle, the downlink's Q9 shape) so cycle-1
> records already evaluate — only the def broadcast waits for the first execute (B9); and
> the W3 tests surfaced the two target-addressing realizations (shaped nox tensors take
> `element=`; primitive arrays flatten to dotted per-element components), recorded in
> `docs/alarms.md` §3.

Design: `docs/alarms.md` (approved 2026-07-05; decisions locked — AllOutputs targeting, ack +
latching in v1, domain alarms descoped). Summary of what we build: a shipped **`AlarmSystem`**
(an ordinary `CyclicSystem`, `src/alarm/`) that evaluates KDL-declared limit alarms against any
telemetered component via the `AllOutputs` tap and emits the wkt
`AlarmDef`/`AlarmRaised`/`AlarmCleared` Msgs over ordinary telemetered `MsgOut` ports; the
uplink gains an `AlarmAck` route so latching alarms can gate their clear on operator ack; the
static registry gains framework built-ins (`type="Alarms"`) plus the deferred-ReceiveAll
resolve pass (design §7 F1). **Zero ABI change** (`docs/alarms.md` §7).

Built in **6 waves**. Dependency graph (strict edges →):

```
W1 (NamedMsg impls) ─┬─▶ W2 (params + eval) ─▶ W3 (AlarmSystem) ─┐
                     └─▶ W4 (uplink ack route) ──────────────────┴─▶ W5 (wiring/CLI) ─▶ W6 (example + docs)
```

- **W2 and W4 are independent** after W1 — different files (`src/alarm/` vs
  `src/telemetry/mod.rs`).
- **W3 depends on W2** (the system embeds the params + eval machine) and on W1 (its ports'
  descriptors need `NamedMsg`).
- **W5 depends on W3** (it registers/instantiates the system) and touches `wiring/mod.rs` +
  `cli/mod.rs` only.
- **W6 depends on everything** (the adcs-fsw2 mission exercises the full path).

**Critical path: W1 → W2 → W3 → W5 → W6.** Build + test after each wave;
`cargo build -p metor-fsw-2 --no-default-features` must stay green at every wave — the alarm
module itself is **ungated** (it lives beside `telemetry`/`registry`; serde and
`metor_proto_wkt` are non-optional deps); only the built-in registration and the KDL
params/grammar tests ride the `kdl` feature.

---

## Wave 1 — `NamedMsg` for the alarm wkt types (`src/message.rs`)

**Independent.** `impl NamedMsg for AlarmDef/AlarmRaised/AlarmCleared/AlarmAck` beside
`SequenceCommand`'s (`src/message.rs:43-54`), `NAME` = the type name (`"AlarmDef"`, …) — the
KDL `msg=` token and the registry channel key (`alarms.AlarmDef`, …).

**Verify:** extend the frozen-token test (`wkt_named_msg_tokens_frozen`-style, in
`src/descriptor.rs`/`src/message.rs` tests) with the four new tokens; full crate tests +
no-default-features build.

## Wave 2 — params + the pure eval state machine (`src/alarm/mod.rs` new)

**After W1 (module placement only; no hard dep).** `mod alarm;` in `src/lib.rs`, re-exports
(`AlarmSystem` lands in W3; this wave ships `AlarmsParams`, `AlarmSpec`, `TargetSpec`,
`BandSpec`, and the crate-private `AlarmEval`).

1. `AlarmsParams { alarm: Vec<AlarmSpec> }` (+ `#[serde(default)]`);
   `AlarmSpec`/`RawAlarmSpec` with `#[serde(try_from = …)]` validation (design §3: no band,
   empty band, critical inside warning, `debounce == 0`, `hysteresis < 0`);
   `AlarmSpec::to_def()` → wkt `AlarmDef` (thresholds = display limits, design §3.1).
2. `AlarmEval` (design §5.3): breach/in-band with hysteresis dead-zone, debounce both ways,
   escalation on the same occurrence, latching clear on recovered ∧ acked (either order),
   stale-ack drop. Events out as a small `EvalEvent` enum; no ports, no I/O.

**Verify:** unit tests (`src/alarm/tests.rs`): debounce raise/clear counts; dead-zone resets
both counters; warning→critical escalation reuses the occurrence and only ratchets up;
non-latching clear; latching × ack orderings; stale ack ignored; occurrence monotonicity;
`try_from` rejections. KDL-gated params tests deserializing the exact design-§3 grammar
(properties + scalar children + repeated `alarm` children + `severity="warning"` enum +
integer→f64 coercion), plus the F2 regression (positional `alarm "X"` → spanned error).

## Wave 3 — the `AlarmSystem` (`src/alarm/mod.rs`, `src/lib.rs`)

**After W1 + W2.** The bundles (`AlarmIn { acks: MsgIn<AlarmAck>, all: AllOutputs }`,
`AlarmOut { defs, raised, cleared }`), `System`/`CyclicSystem`/`BuildSystem` impls
(`NAME = "alarms"`, `Params = AlarmsParams`, occurrence counter seeded
`Timestamp::now()` in `new` — design §5.4).

1. First-execute boot (design §5.1, B9): resolve each target against
   `input.all.entries()` Table vtables (`for_each_field(None, …)`), one shared `Watch`/view
   per distinct entry (ReceiveAll budgets +1 reader per ring), element-bounds check;
   unresolvable → `health().error("unresolved_target")` + `Warn` log, alarm disabled; then
   emit every `AlarmDef`.
2. Per cycle (design §5.2): drain acks → `try_latest` per watch → `for_each_field(Some(rec))`
   → `ComponentView::get(element)` → `as_f64` → `AlarmEval::step` → emit
   `AlarmRaised`/`AlarmCleared` (message `"<component>[i] = <value>"`).

**Verify:** integration test (hand-built `CoordinatorBuilder`, sim clock, modeled on
`src/telemetry/tests.rs`): producer ramps a value; registry views on `alarms.AlarmDef` /
`alarms.AlarmRaised` / `alarms.AlarmCleared` claimed before `run_for`; assert def on the first
cycle (not init), raise after `debounce` cycles with correct severity/value/message,
escalation reuses the occurrence, clear after recovery + hysteresis; unresolved target →
health error, no raise; two alarms on one frame share one view; a bool component alarms with
`above=0.5`.

## Wave 4 — the uplink ack route (`src/telemetry/mod.rs`)

**After W1; parallel to W2/W3.** `UplinkPorts` gains `acks: CommandOut<AlarmAck>` and
`RouteMsg::route` gains the `AlarmAck::ID` arm (the declared-outputs subscription set picks up
the new id automatically).

**Verify:** extend the mock-uplink tests: a fed `AlarmAck` Msg lands on the `acks` channel
(registry-invisible — `CommandOut` is untelemetered); the subscribe id set includes
`AlarmAck::ID`; malformed payload dropped without breaking the loop.

## Wave 5 — wiring + CLI integration (`src/wiring/mod.rs`, `src/cli/mod.rs`)

**After W3 (+W4 for the e2e edge).**

1. **Built-ins:** `Registry::with_builtins()` = `new()` + `register::<AlarmSystem, _>("Alarms")`;
   the CLI runner resolves against it (`src/cli/mod.rs:206`); doc comment updated.
2. **F1 (design §7):** the factory table entry gains `descriptor: fn() -> SystemDescriptor`;
   `resolve` defers static systems whose descriptor carries `Capability::ReceiveAll` to a
   second pass after slots + uplink (before edges). Order: systems → slots → uplink →
   alarms → telemetry.

**Verify:** wiring tests (`load(kdl, &Registry::with_builtins())`): the design-§3 KDL builds
and runs with a raise observed via a registry view; an `Alarms` node declared **before** a
`slot` still builds (F1 regression — fails `ReceiveAllNotLast` without the deferral);
misconfig KDL (no bands, unknown key, positional id) → spanned `LoadError`s; the
`connect "uplink" -> "alarms" msg="AlarmAck"` edge resolves.

## Wave 6 — example + docs (`examples/adcs-fsw2/`, `docs/`)

**After W5.**

1. `examples/adcs-fsw2/mission.kdl`: an `ADCS_RATE_HIGH` alarm on `plant.sensors.gyro_b`
   element 1 (thresholds tuned so the boot tumble raises it and detumble clears it) + the
   `AlarmAck` edge.
2. `examples/adcs-fsw2/docs/ergonomics-report.md` §1: resolution note (superseded by
   `docs/alarms.md`; the `alarm_out()` capability it proposed was obsoleted by wiring parity).
3. Reconcile `docs/system.md` §4's "fault management out of scope" line and `DESIGN.md`'s
   document map with a pointer to `docs/alarms.md`; flip this plan's + the design's status
   banners.

**Verify:** `cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build` boots
clean and the convergence test still passes; live check (`--wall --telemetry`) against
metor-panel: alarm chip, raise → ack → clear in the alarm panel, limit lines on the gyro plot.
