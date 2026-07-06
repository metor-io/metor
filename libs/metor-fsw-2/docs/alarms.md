# Alarms (`alarms`)

> **Status: IMPLEMENTED (2026-07-05).** Landed as `docs/alarms-plan.md` waves W1–W6:
> `src/alarm/` (params + eval + system), the uplink `AlarmAck` route, the built-in registry
> + deferred-ReceiveAll resolve pass (§7 F1), and the adcs-fsw2 `ADCS_RATE_HIGH` example.

The framework has no alarm/FDIR story (`system.md` §4 closes with "fault management beyond
this telemetry is out of scope"). The panel already has a complete one: an `AlarmStore` state
machine, an alarm panel, a title-bar severity chip, and per-plot limit lines/tinting — all fed
by four wkt Msgs that flow through the db msg-log catch-all pub/sub. The only producer today is
the old-stack cube-sat example, which hand-rolls one debounced alarm straight onto the db
socket (`examples/cube-sat/src/main.rs:571-747`). The adcs-fsw2 ergonomics report descoped
alarms because there was nowhere to put producer logic; the blocker it named — no general
telemetered `MsgOut` — has since been dissolved by the wiring-parity work
(`docs/message-wiring.md`): messages are first-class typed ports.

This design adds alarms as a **normal system**. The thesis, stated once and honored
throughout:

> **An alarm is just a message, and the alarm engine is just a system.**

There is no coordinator special case, no new transport, no new port kind, no macro change, no
ABI change, and no new KDL node type. Every moving part is existing machinery:

| Concern | Mechanism (all existing) |
|---|---|
| Alarm events out | ordinary telemetered `MsgOut<AlarmDef / AlarmRaised / AlarmCleared>` ports |
| Downlink | the `AllOutputs`-tapping `TelemetrySystem`, automatically (telemetered by default) |
| Ground pub/sub | the db msg-log catch-all (`db/src/lib.rs:1639` — "recording it in the msg log is the pub/sub mechanism") |
| GUI | the panel's shipped `AlarmStore` + alarm panel + plot integration, unchanged |
| Reading monitored values | the `AllOutputs` receive-all capability + prefixed-`VTable` realization |
| Operator acks in | the uplink's declared-output routing + one ordinary `connect … msg="AlarmAck"` edge |
| Configuration | ordinary serde-KDL `system` params |
| Trouble reporting | `output.health()`, like every system |

The one genuinely new thing is the **evaluation state machine** (debounce, hysteresis,
severity escalation, latching, occurrence identity) — which is exactly the part that *should*
be new, and it is pure code with no framework surface.

---

## 1. The wire vocabulary (already shipped, unchanged)

`metor-proto-wkt` defines the alarm Msgs (`wkt/src/msgs.rs:559-662`); this design changes
none of them:

- **`AlarmDef` [224,37]** `{ id: AlarmId, name, description, target: Option<AlarmTarget>,
  limits: Vec<AlarmLimit>, default_severity }` — declaration/description. Latest-wins by `id`
  (re-publishing updates). `AlarmTarget { component_id, element_index }` lets plots
  auto-associate limit lines with traces. `AlarmLimit` entries are *display* lines — but see
  §4.2: in this design they are also the real firing thresholds, one source of truth.
- **`AlarmRaised` [224,38]** `{ def_id, occurrence: OccurrenceId, severity, value: Option<f64>,
  message }` — the source of truth that an alarm is firing.
- **`AlarmCleared` [224,39]** `{ def_id, occurrence }` — resolves one occurrence.
- **`AlarmAck` [224,40]** `{ def_id, occurrence, operator, note }` — published by the **panel**.

Panel semantics that constrain the design (verified in `metor-panel/src/alarms/`):

- The active map is keyed by **`OccurrenceId` alone** — occurrence ids must be globally unique
  across every alarm def and every producer, and (because the panel backfills the persisted
  msg log) across **runs**. This is the strongest argument for a single emitting authority
  (§4) and for the seeded occurrence counter (§5.4).
- Re-raising the **same** occurrence overwrites the active entry in place — that is how
  severity escalation is expressed (§5.3).
- An ack only marks an occurrence that is still active; ack state does not survive a clear.
- Event time is the **msg-log timestamp**; the messages carry no time fields.
- Severity is `Info < Warning < Critical`, exactly three levels, `Ord` is load-bearing.

---

## 2. The `AlarmSystem` — an ordinary cyclic system

One shipped system (new `src/alarm/mod.rs`, modeled on `TelemetrySystem`), an ordinary
`CyclicSystem` with ordinary bundles:

```rust
#[derive(SystemInput)]
pub struct AlarmIn {
    /// Panel-published operator acks, fan-in over ordinary message edges
    /// (`connect "uplink" -> "alarms" msg="AlarmAck"`). Zero producers is legal —
    /// a mission without an uplink simply never latches-clear via ack.
    acks: MsgIn<AlarmAck>,
}

#[derive(SystemOutput)]
pub struct AlarmOut {
    defs:    MsgOut<AlarmDef>,      // telemetered (default) → downlinked → db msg log → panel
    raised:  MsgOut<AlarmRaised>,
    cleared: MsgOut<AlarmCleared>,
    /// The receive-all tap (Capability::ReceiveAll): every telemetered entry in the
    /// graph, read through slot-accounted registry views. Host-only, and output-side
    /// exactly like the downlink's (§5.1 — `init` reaches it through the outputs).
    all: AllOutputs,
}

pub struct AlarmSystem { /* specs, watches, eval state, occurrence counter */ }

impl System for AlarmSystem {
    type Input = AlarmIn;
    type Output = Out<AlarmOut>;
    const NAME: &'static str = "alarms";
}
impl BuildSystem for AlarmSystem { type Params = AlarmsParams; /* … */ }
impl CyclicSystem for AlarmSystem { /* §5 */ }
```

Because the derives already handle capability fields and message ports, and
`AlarmsParams: serde::de::DeserializeOwned` satisfies `Registry::register`'s bound, the system
drops into **every existing registration path** with no framework change:

- **KDL** (the normal path): `system "alarms" type="Alarms" { … }` — resolved against the
  static `wiring::Registry`. The CLI runner registers the framework's built-in systems
  (`Registry::with_builtins()`, registering `AlarmSystem` under `"Alarms"`) instead of
  resolving against an empty registry, so every `cli::main()` mission gets alarms with zero
  Rust. A mission with its own registry adds one `register_system!` line.
- **Programmatic**: `builder.add_cyclic_named("alarms", AlarmSystem::new(params))`, after the
  other cyclic systems (§7 F1).

`NamedMsg` impls for the four alarm Msgs live in `src/message.rs` beside `SequenceCommand`'s —
they are what give `MsgOut`/`MsgIn` descriptors their KDL/registry token (`msg="AlarmAck"`,
registry keys `alarms.AlarmDef` etc.). No wkt change.

### 2.1 Why the panel sees it all with zero new plumbing

The three outputs are telemetered message ports, so the downlink taps them like any Log entry
(non-coalescing FIFO — an event stream never drops a record silently), sends them as
self-describing `OwnedPacket::Msg`, the db's catch-all records them into the per-id msg logs,
and the panel's four ingest loops (backfill + WAL tail per alarm packet id) light up. The
entire ground half of this feature is **already deployed**.

---

## 3. Configuration — serde-KDL params

Alarm definitions are ordinary system params (the in-house serde-KDL deserializer,
`docs/design-kdl-serde.md`). Grammar (F2 in §7 explains the property spellings):

```kdl
system "alarms" type="Alarms" {
    alarm id="ADCS_RATE_HIGH" name="Body Rate High" severity="warning" {
        description "Body rate exceeds the safe envelope"
        target component="plant.sensors.gyro_b" element=1
        warning above=0.5 below=-0.5
        critical above=1.0 below=-1.0
        debounce 2
        hysteresis 0.05
        latching #true
    }
    // repeated `alarm` children → Vec<AlarmSpec>
}

connect "uplink" -> "alarms" msg="AlarmAck"   // only needed for latching acks (§6)
```

```rust
#[derive(Deserialize)]
pub struct AlarmsParams {
    #[serde(default)]
    pub alarm: Vec<AlarmSpec>,
}

#[derive(Deserialize)]
#[serde(try_from = "RawAlarmSpec")]        // semantic validation → spanned LoadError
pub struct AlarmSpec {
    pub id: String,                        // AlarmId, e.g. "ADCS_RATE_HIGH"
    pub name: String,                      // human title
    pub description: String,               // default ""
    pub target: TargetSpec,                // { component: String, element: Option<usize> }
    pub warning: Option<BandSpec>,         // { above: Option<f64>, below: Option<f64> }
    pub critical: Option<BandSpec>,
    pub debounce: u32,                     // consecutive cycles to raise AND to clear; default 1
    pub hysteresis: f64,                   // absolute margin a clear must recover past; default 0
    pub latching: bool,                    // default false
    pub severity: Severity,                // AlarmDef::default_severity; default = lowest configured band
}
```

`TryFrom<RawAlarmSpec>` rejects at load time, with the enclosing `alarm` node's span: no band
configured at all, a band with neither `above` nor `below`, a critical threshold inside the
warning band, `debounce == 0`, `hysteresis < 0`. Config errors are **load errors**; runtime
errors (a target that resolves to nothing) are **health errors** (§5.1) — matching the
framework split between "the mission file is wrong" and "the running graph disagrees".

**Addressing an element** depends on how the field realizes (a property of its `AsVTable`
impl, not of the alarm system):

- a **shaped** component — a nox tensor/quaternion field (`gyro_b: V3` → one component,
  shape `[3]`) — is targeted by its path plus `element=` (`target
  component="plant.sensors.gyro_b" element=1`);
- a **primitive array** (`rates: [f64; 3]`) flattens into per-element scalar components —
  the dotted path *is* the element address (`target component="plant.gyro.rates.1"`, no
  `element=`);
- `bool` cannot be a zerocopy frame field; flags ride as `u8` and alarm with `above=0.5`.

### 3.1 One source of truth for thresholds

`AlarmSpec::to_def()` derives the wkt `AlarmDef` mechanically: each configured `above`/`below`
becomes one `AlarmLimit { kind: Upper/Lower, value, severity, label }`, and
`target` becomes `AlarmTarget { component_id: ComponentId::new(&component), element_index }`.
The wkt doc says display limits "are not the boundary that decides firing" — true in general,
but *here* the firing thresholds and the displayed lines are the same numbers by construction.
The panel draws exactly the boundary the FSW enforces (hysteresis/debounce refine *when*, not
*where*).

---

## 4. Where evaluation lives (and why it is centralized)

**Targets are params, not edges.** The alarm system reads monitored values through
`AllOutputs`, resolving each target component against the registry's prefixed vtables — the
same identity model the panel plots with (`<instance>.<frame>.<field>`, element index into the
component's shape). No `connect` lines per target.

- An alarm target is addressed by **component**, which is precisely what `AlarmTarget` on the
  wire is. A typed frame edge cannot express "watch `plant.sensors.gyro_b[1]`" — and the
  monitor must be schema-agnostic (on a dlopen mission there is no host-side Rust type to name
  in an `Input<F>`). `AllOutputs` + vtable realization is the sanctioned broad-reader path
  ("any broad/dynamic reader … reaches outputs the same way", `src/registry.rs`), with
  `TelemetrySystem` as precedent.
- It watches **anything telemetered** — including every system's implicit `health`/`log`
  frames. `alarm … target component="nav.health.errors" critical above=0.5` needs no new
  machinery.

**Lifecycle state is centralized in this one system.** Occurrence uniqueness is global (§1),
and the raise/clear/debounce/latch bookkeeping is exactly the kind of stateful, easy-to-fumble
code that should exist once.

**Domain alarms — conditions no threshold can express ("sensor timed out", "estimator
diverged") — are descoped from v1.** v1 alarms are limit alarms on telemetered components,
period. The likely future shape is a **custom message** a producing system emits over an
ordinary `MsgOut` that the alarm system (or the panel) ingests — deferred until a concrete
consumer forces the design (§9).

---

## 5. Behavior

### 5.1 Boot: resolve targets at `init`, emit defs at the first execute

Target **resolution** (and watch-view claiming) happens at `init` — the registry is frozen by
build, so `init` sees the whole graph, and claiming there rather than at the first execute
means cycle-1 records already evaluate. This puts `AllOutputs` in the **output bundle**, as
the downlink's is (`init` receives only the outputs; `message-wiring.md` Q9 allows either
side).

The boot `AlarmDef` burst happens at the **first `execute`, not `init`** — the documented
`AllOutputs` init-gap (B9, `src/registry.rs`): the downlink claims its tap views in its *own*
`init`, which runs after this system's, so records emitted from `init` would never be
downlinked. (Precedent: the coordinator's boot `SequenceRegistry` emits at the head of
`run_for` for the same reason.)

Per configured alarm, at `init`:

1. Scan `input.all.entries()` for `EntrySchema::Table` entries; run
   `vtable.for_each_field(None, …)` (registration mode) and match the realized
   `component_id` against `ComponentId::new(&spec.target.component)`.
2. On match: validate `element < shape.iter().product()`, then join (or create) that entry's
   **`Watch`** — one shared `entry.view()` per distinct registry entry, however many alarms
   target components in it. `ReceiveAll` budgets exactly **+1 reader slot on every ring**, so
   the system must never claim two views on one ring; the watch table enforces that
   structurally.
3. No match / bad element / exhausted reader table → `health().error("alarms.unresolved_target")`
   (/ `alarms.bad_element` / `alarms.reader_slot`) + a `Warn` log naming the alarm id and
   component; the alarm is disabled for the run. A duplicate alarm id likewise disables the
   later spec (`alarms.duplicate_id`) — the panel's def store and the ack path are keyed by
   id. The system never panics on config-vs-graph disagreement.

At the first execute, every `AlarmDef` is emitted once, disabled ones included (the panel may
still show the def; it just never fires). Defs are latest-wins on the wire, so re-emission is
always safe.

### 5.2 Each cycle: extract and evaluate

```text
cycle N:
  drain acks (MsgIn<AlarmAck>) → AlarmEval::ack per matching active occurrence   (§6)
  for each Watch:
      view.try_latest()                         // re-serves the pinned newest record
      vtable.for_each_field(Some(record), …)    // ingest mode
        → ComponentView::get(element) → ElementValue::as_f64()
      step each member alarm's AlarmEval with the value → emit Raised/Cleared
```

`try_latest` re-serves the newest record when nothing new arrived: a **silent producer keeps
being evaluated at its last value**. That is the correct default for a limit monitor (a
frozen-but-breaching value keeps the alarm up). Staleness detection is not an implicit
alarm-system behavior — a first-class stale-producer trigger is future work (§9). Before a
target's first record ever arrives, its alarms simply do not evaluate.

`as_f64` covers every numeric prim plus bool — ints, counters, and flags are all alarmable.

### 5.3 The evaluation state machine

Pure code (`AlarmEval`, no ports — unit-testable in isolation), one per alarm:

- **Breach**: `breach(v)` = worst configured band violated (`critical` beats `warning`; a band
  is violated by `v > above` or `v < below`). `debounce` consecutive breaching cycles raise:
  allocate a fresh occurrence, emit `AlarmRaised { severity, value: Some(v), message }` with
  the auto-generated message `"<component>[element] = <value>"` (cube-sat parity).
- **Escalation**: while active, a worse breach re-emits `AlarmRaised` with the **same
  occurrence** and the higher severity — the panel overwrites the active entry in place.
  Severity only ratchets up; de-escalation is not re-emitted — an occurrence that drops from
  critical back to a warning-band breach stays active at its recorded severity until it clears
  entirely (a still-breaching lower band is still a breach).
- **Clear**: `in_band(v)` requires every configured threshold respected by ≥ `hysteresis`
  margin. `debounce` consecutive in-band cycles mark the occurrence *recovered*:
  non-latching alarms emit `AlarmCleared` immediately; **latching** alarms emit it only once
  *recovered ∧ acked* (either order — ack-then-recover and recover-then-ack both clear).
- **Dead zone**: a value between a raw threshold and its hysteresis margin advances *neither*
  counter — both reset. Chatter around the boundary neither raises nor clears.

### 5.4 Occurrence identity

One global counter in the system, **seeded from wall-clock micros at construction**
(`Timestamp::now()` in `BuildSystem::new`). This is a deliberate, documented exception to the
"never call `Timestamp::now()`, stamp with the cycle `now`" rule: the seed is a one-time boot
nonce, not a data timestamp. The cycle clock cannot serve — a `Simulated` clock restarts
identically every run, and the panel backfills the *persisted* msg log, so two runs reusing
occurrence ids would corrupt its occurrence-keyed active map. Known residual (pre-existing,
not introduced here): a run that dies with an occurrence active leaves it active in the
panel's backfill forever — noted as future work (§9).

---

## 6. The ack path (latching, in v1)

The panel publishes `AlarmAck` into the db msg log. Getting it into the FSW is the uplink
doing exactly what it was reframed to do:

- `UplinkPorts` gains `acks: CommandOut<AlarmAck>` and one `RouteMsg` arm. Its ground
  `MsgStream` subscription set derives from its declared outputs, so it starts subscribing to
  `AlarmAck::ID` automatically. (This is the doc's own predicted example — `AlarmAck` is the
  future command type `design-command-slots.md` names.)
- One ordinary explicit edge delivers it: `connect "uplink" -> "alarms" msg="AlarmAck"` —
  same shape as the `SequenceCommand` edges, no broadcast sugar.
- The alarm system drains its `acks` fan-in at the head of each cycle and applies each ack to
  the matching *active* occurrence (`def_id` + `occurrence`); stale or unknown acks are
  dropped silently, mirroring the panel's own semantics.

Acks only *do* something for `latching #true` alarms (they gate the clear). For everything
else the FSW ignores them — ack display state remains a ground-side concern, as today. A
mission with no uplink (or no ack edge) simply has `acks` at zero producers, which a message
input explicitly permits.

---

## 7. Integration fixes required (verified against the code)

- **F1 — registration order vs `validate_receive_all_last`.** `build()` requires every cyclic
  system registered after a `ReceiveAll` holder to also hold `ReceiveAll`
  (`src/coordinator/mod.rs:1210`). `wiring::resolve` runs systems → slots → uplink →
  telemetry, so a `system "alarms"` node in a mission with a slot would register a ReceiveAll
  cyclic *before* a non-ReceiveAll cyclic (the slot) and fail the build — for exactly the
  flagship adcs-fsw2 mission. **Fix**: the static-registry factory entry gains a
  `descriptor: fn() -> SystemDescriptor`; `resolve` defers any static system whose descriptor
  carries `ReceiveAll` to a second pass after slots/uplink (before edges — edge resolution is
  order-insensitive over the instance map). Resulting order: systems → slots → uplink(async) →
  **alarms** → telemetry. The alarm system thus steps after every producer and before the
  downlink: it evaluates this cycle's values and its emits downlink the *same* cycle. dl
  systems cannot carry capabilities (rejected at load), so only the static branch needs this.
  Programmatic builders are documented to add the alarm system after their other cyclic
  systems.
- **F2 — nested KDL nodes take no positional args** (`src/wiring/de.rs:331`; only the
  top-level `system` node skips its leading name). Hence `alarm id="ADCS_RATE_HIGH"` and
  `target component="…"` are properties — the `alarm "ADCS_RATE_HIGH"` spelling is not
  expressible without deserializer surgery this feature does not justify.

**Bounds** (documented limits, not errors): the boot def burst rides a Log message ring of
depth 64 — ≤64 alarm defs per `alarms` instance (beyond that, `publish` counts drops into
`publish_dropped` health). Multiple `alarms` instances are legal if ever needed (the system is
normal; nothing is a singleton).

**ABI impact: none.** Host-side system, host-side capability, existing wkt wire types.

---

## 8. Resolved decisions

1. **Read surface — `AllOutputs` tap; targets are params, not edges.** (Reviewer-selected.)
   Schema-agnostic component addressing, zero per-target wiring, health frames alarmable for
   free; precedented by telemetry. *Trade-off:* the monitored dataflow is declared in params
   rather than visible as graph edges — accepted; the alternative required a new untyped
   frame-input primitive plus per-frame edges in every mission.
2. **`AlarmAck` ingest + latching ship in v1.** (Reviewer-selected.) One `CommandOut` field +
   route arm on the uplink, one explicit edge, a `latching` knob per alarm; recovered ∧ acked
   clears in either order. *Trade-off:* grows v1 scope; but the uplink machinery generalizes
   by design and the eval state machine has to model recovery anyway.
3. **Domain alarms are descoped from v1; no decentralized raise API.** (Reviewer-selected.)
   v1 ships limit alarms on telemetered components only; the alarm system stays the single
   emitting authority, so occurrence uniqueness stays trivial. The expected future direction
   is custom messages from producing systems (§9), designed when a concrete consumer exists.
   *Trade-off:* a condition a system detects internally has no v1 alarm expression beyond
   what its telemetered components already expose.
4. **Resolution at `init` (AllOutputs output-side, the downlink's Q9 shape); the def
   broadcast at the first execute** — claiming watch views at `init` lets cycle-1 records
   evaluate, while boot records emitted from `init` would miss the downlink's later-claimed
   taps (B9).
5. **Firing thresholds are the display limits** — `AlarmDef.limits` derived from the same
   `BandSpec`s that drive evaluation; the panel draws what the FSW enforces.
6. **Occurrence ids: global counter seeded from wall-clock micros at construction** — unique
   across alarms and across runs against the panel's persisted, occurrence-keyed backfill; a
   documented one-time exception to the no-`Timestamp::now()` rule.
7. **Registered as a framework built-in static system** (`Registry::with_builtins()`, type
   `"Alarms"`), instantiated by a normal `system` node — not a bespoke top-level KDL node like
   `uplink` (which exists only because it carries a transport resource; alarms are pure
   params).

---

## 9. Future work

- **Domain alarms via custom messages** — a producing system emits a purpose-built Msg over an
  ordinary `MsgOut` that the alarm system ingests (occurrence allocation staying centralized)
  or the panel consumes directly; the v1 descope (§8, decision 3).
- **Stale-producer alarms** — a first-class "no new record for N cycles" trigger kind (§5.2).
- **Rate / RoC alarms** and richer predicates (in-band-required, equality on enums).
- **Def re-emit knob** (periodic re-publish) for db-restart robustness; defs are latest-wins
  so this is purely additive.
- **Crash-orphaned occurrences** — a run that dies mid-occurrence leaves it active in the
  panel backfill (§5.4); a boot-time "clear all my prior occurrences" convention would need a
  wire-visible run identity and panel cooperation.
- **Occupant-declared alarm specs** — letting a dlopen system ship alarm defs beside its
  frames (today: mission-file only).
