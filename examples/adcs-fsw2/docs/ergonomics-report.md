# metor-fsw-2 ergonomics report

Written while bringing `adcs-fsw2` to feature parity with the older `examples/cube-sat`
mission. The parity port — full plant physics (reaction wheels, orbit/gravity, sensor
suite), pointing modes wired through the `mode` slot into the controller, nav reference
modeling, live uplink — landed cleanly and entirely example-side. This report records the
friction that surfaced and what would make the framework more ergonomic for the next mission.

The headline: **`metor-fsw-2`'s architecture is a real step up from the cube-sat monolith**
(dlopen cdylib systems, declarative `mission.kdl`, a shared `contracts` crate, framework-
native slots/sequences, framework telemetry/uplink, a static-vs-dlopen parity test). Every
item below is a refinement, not a redesign. They are ordered roughly by impact.

---

## 1. There is no home for app-specific *host* logic — alarms forced this

The most consequential finding, and the reason **alarms were descoped** from this pass.

cube-sat declares an `AlarmDef` and raises/clears `AlarmRaised`/`AlarmCleared` with a
two-sample debounce (`examples/cube-sat/src/main.rs:573-747`). Reaching parity means emitting
those wkt Msgs so the panel's alarm view populates. In `adcs-fsw2` there is **nowhere to put
that code**:

- The **host** (`src/main.rs`) is `metor_fsw_2::cli::main()` and links *only* metor-fsw-2 —
  schema-agnostic by design. There is no example-owned `main` loop to bolt alarm logic onto
  (unlike cube-sat's hand-written `#[stellarator::main]`).
- A **dlopen occupant** (plant/nav/ctrl) *could* compute a limit breach, but **occupant-side
  message emit is deferred** (`docs/messages.md` §6–§7) — a `.so` cannot emit `AlarmRaised`.
- A **static host system** can hold a `MsgOut` (the bind-time capability), but adding one to
  this example would mean abandoning the `cli::main()` delegate and hand-building a
  coordinator — i.e. regressing to the cube-sat shape.

So anything host-side that emits subscribable messages **must be a metor-fsw-2 library
system**. That is the correct home for a *generic* monitor, but it means the framework, not
the mission, owns the feature.

**Recommendation — add a generic `AlarmSystem` to metor-fsw-2** (modeled on
`TelemetrySystem`, registered before it). The design is mostly free:

- **Reading any scalar by `ComponentId` needs no new primitive.** The output registry already
  carries each frame's `VTable`; `VTable::for_each_field` (`libs/metor-proto/src/vtable.rs`)
  + `ComponentView::as_f64` (`libs/metor-proto/src/types.rs`) resolve offset+type+value by id
  — the same metadata the panel plots with. The monitor scans the registry at `init` to bind
  each configured target's `View`, then reads it each cycle.
- **The only real new surface is a telemetered host `MsgOut`.** Today every `MessageEntry` is
  coordinator-minted and the `MessageRegistry` is frozen *before* the bind loop, while the
  command-bus `command_out()` capability deliberately stays *out* of that registry. An alarm
  channel must be in the registry (so the downlink taps it) **and** minted *before* the freeze
  — mirror the conditional mint of the `"sequences"` channel, exposed as a bind-time
  `RingSource::alarm_out()` capability.
- **KDL config** mirrors `telemetry`: `Wiring.alarms: Vec<AlarmSpec>`, a `parse_alarm` next to
  `parse_telemetry`, a resolve loop before the telemetry block. The wkt types
  (`AlarmDef`/`AlarmRaised`/`AlarmCleared`, `AlarmLimit`, `AlarmTarget`, `Severity`,
  `LimitKind`) already exist in `metor-proto-wkt`; the debounce state machine ports verbatim
  from cube-sat.

This generalizes beyond alarms: see §2.

## 2. Host systems can emit messages, but only through bespoke capabilities

`MsgOut` is described as a general capability ("ANY host system can hold a `MsgOut`",
`docs/messages.md` §1.2), yet in practice the only ways to get one are the special-cased
`command_out()` (commands, kept out of the registry) and the coordinator's own `"sequences"`
mint. There is no general **`message_out(channel_name)`** a host system can pull in
`BindPorts::bind` to get a *telemetered* channel.

**Recommendation** — add a general `message_out(channel)` bind capability that mints a
registry-registered message channel (resolving the freeze-ordering once, centrally). Alarms
(§1), future event emitters, and autonomy notifications all become one-liners instead of new
coordinator coupling. This is the natural generalization of the `alarm_out()` stopgap.

## 3. No live operator command path (component writes)

cube-sat lets an operator write the FSW surface live via `UpdateComponent` — switch the
pointing mode and **arm/disarm individual reaction wheels** from the panel
(`examples/cube-sat/src/main.rs:651-688`). metor-fsw-2's command plane is `SequenceCommand`
→ slots only, so:

- **Mode switching reached parity for free** — it is exactly what the `mode` slot + live
  `--uplink` already do (Load/Start `commissioning`/`safe_mode` from the panel).
- **Live per-wheel arm/disarm did not.** We covered the common case with a boot-time
  `disarmed` plant param (the `--disarmed` parity), but there is no way to write a running
  system's input/parameter from the ground mid-flight.

**Recommendation** — design a first-class operator-command path: a typed "parameter write"
(or component write) addressed by `ComponentId`, ingested by the uplink alongside
`SequenceCommand` and delivered to the target system. This is the single biggest *functional*
gap vs cube-sat and the most likely thing a real operator wants.

## 4. Sequences have no per-cycle clock value — `ModeCmd.timestamp` is always 0

A `#[sequence]` occupant gets the ambient `wait`/`progress`/`aborted` API but **no `now`**, so
every frame it writes is stamped `Timestamp(0)` (`contracts/src/lib.rs` `ModeCmd::at`). The
framework's own rule is "stamp output frames with `now`, never `Timestamp::now()`" — sequences
can't follow it. Today it is masked because the slot's `SequenceStatus` carries the
authoritative ordering, but any frame a sequence emits for *consumption* (here `mode_cmd` now
drives `ctrl`) has a meaningless timestamp.

**Recommendation** — thread the coordinator's per-cycle `now` into the sequence ambient (a
`now()` free function beside `wait`), so occupant-written frames are properly stamped.

## 5. The `Params` struct carries two parallel config decoders

Each system's `Params` must derive **both** `Schema` (the dlopen path, postcard across
`fsw_create`) **and** `FromKdlNode` (the static path, when a `Registry` resolves the same
`mission.kdl`). The two are independent re-encodings of the same fields, and a mission author
has to know to add both — the second only surfaces when you try to link a system statically
(which the parity test does). The contracts crate also repeats the long frame-derive litany
(`Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone` + `#[repr(C)]` + manual `_pad`
to keep zerocopy happy) on every frame.

**Recommendations** — (a) let one derive (or a `#[frame]`/`#[params]` attribute macro) expand
to the full derive set + the `_pad` insertion, so a frame/params struct reads as its fields;
(b) consider deriving `FromKdlNode` from the `Schema` (or vice-versa) so params declare their
config surface once.

## 6. Slots/sequences are dlopen-only — the parity test had to route around it

`CoordinatorBuilder::add_slot` only accepts `DlSystem` occupants; there is no way to add a
slot with a statically-linked sequence. That collided with the static-vs-dlopen parity test:
once `ctrl` consumes `ModeCmd`, the control loop depends on the slot's mode writes, so a
pure-static reference can no longer reproduce the dlopen run.

The resolution turned out clean and is worth noting as a **pattern**, not just a workaround:
the test resolves the **same `mission.kdl`** for both paths, nulling `plant`/`nav`/`ctrl`'s
`artifact` so `resolve` links them statically via a `Registry`, while the `mode` slot's
sequences stay dlopen in both (`tests/closed_loop.rs`). Result: the two runs differ in exactly
one variable (system linkage) and stay **bit-identical**, which is a *stronger* parity claim
than the old "no slot in the loop" version. But it leans on `SystemSpec.artifact` being
publicly mutable and on every `Params` deriving `FromKdlNode` (§5).

**Recommendation** — either a supported "swap these systems to static" knob on the resolve
path, or a static-sequence/slot API, so this is a first-class testing affordance rather than a
field-poking trick.

## 7. Nested component structs are awkward in a metor-fsw-2-only crate

Sharing one `ReactionWheel` struct as both the plant's internal state and its telemetry (a
`[ReactionWheel; 3]` field on the `wheels` frame) hit two framework gaps:

- **The component sub-derives target `::metor_fsw`, not `::metor_fsw-2`.** A nested component
  type (one that is *not* a top-level `Frame`) must derive `AsVTable`/`Metadatatize`/
  `Componentize`/`Decomponentize` standalone, and those derives expand to `::metor_fsw` paths
  (`metor_fsw_crate_name()` hard-`expect`s `metor-fsw` in `Cargo.toml`). `#[derive(Frame)]`
  bundles them with `::metor_fsw-2` paths, but there is no fsw2-flavored *standalone* derive —
  so a schema crate that otherwise depends only on `metor-fsw-2` must add a `metor-fsw`
  dependency purely to derive a nested component. **Recommendation:** a fsw2 `#[derive(Component)]`
  (or re-exported fsw2-pathed sub-derives) so a nested group needs no fsw1 dependency.
- **`Componentize`/`Decomponentize` had no array impl.** `AsVTable`/`Metadatatize` already
  implement `[T; N]` (indexing `field.i.*`), but the columnar `com_de` traits did not, so a
  `[Struct; N]` frame field would not compile even though the VTable side supports it. Added the
  two array impls (mirroring the existing tuple impls) in `metor-proto/src/com_de.rs`. Like a
  same-typed tuple they are index-lossy on the columnar path (unused here — the ring path is raw
  bytes + VTable), but they complete the set so arrays-of-structs are usable as frame fields.

## 8. Smaller notes

- **Frame fan-out 0 is fine but undocumented as a pattern.** `body` and `wheels` are emitted
  for the registry/telemetry tap (and `body` is also consumed); a frame with no edge consumer
  works but a reader has to infer it. A short note (or a `#[telemetry_only]` marker) would help.
- **Pointing-law singularity is the app's problem, correctly.** The shortest-arc target
  degenerates when the pointing direction is anti-parallel to the body axis (the velocity-
  vector law at orbit injection); the `target_for` NaN-guard handles it. Nothing for the
  framework here — just a reminder that app math owns its singularities.
- **`disarmed` is a KDL edit, not a CLI flag.** The runner exposes `--build/--wall/
  --telemetry/--uplink` but no mission-specific flags, so the `--disarmed` parity is a
  `mission.kdl` property. Fine, but worth a generic "override any system param from the CLI"
  story (`--set plant.disarmed=#true`) for operability.

---

## What parity required, by disposition

| Feature | Outcome | Where |
|---|---|---|
| Plant physics (wheels, orbit/gravity, IMU/mag/sun sensors, GPS) | **done** | example (`adcs-plant`, `adcs-contracts`) |
| Pointing modes (Nadir/HIL) wired slot → ctrl | **done** | example (`adcs-ctrl`, `mode_cmd` edge) |
| Nav reference modeling from orbit state | **done** | example (`adcs-nav`) |
| Live uplink (panel drives the `mode` slot) | **done** | `--uplink` (already in the framework) |
| Boot-time wheel disarm (`--disarmed` parity) | **done** | `disarmed` plant param |
| Static-vs-dlopen + convergence parity test | **done** (stronger) | `tests/closed_loop.rs` |
| Alarms / fault management | **descoped** → §1 | needs library `AlarmSystem` |
| Live operator component commanding (per-wheel arm/disarm) | **descoped** → §3 | needs operator-command path |
| Panel schematic / 3D viewport | **dropped** | vestige of the prior GUI (per owner) |
