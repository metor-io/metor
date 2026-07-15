# Implementation plan — telemetry + uplink as ordinary systems

> **Status: LANDED (2026-07-05)** — W1/W2 as planned; W3+W4 landed as one commit (the CLI
> reads the model, so they were inseparable); the alarms feature had already shipped
> `Registry::with_builtins()`, the deferred-ReceiveAll pass, and the CLI's built-ins
> registry, so W2/W4 shrank to registering the two types and the flag/banner rewrite.
> `CoordinatorBuilder::add_telemetry`/`add_uplink` were deleted outright (not kept as
> sugar); `WiringBuilder::telemetry(addr)` dropped its mode param (subset taps are
> KDL/params-only).

Goal: delete the special-cased wiring surface for the telemetry downlink and the command
uplink so both are **normal registry systems** — declared as `system` nodes, instantiable
more than once, and replaceable by a user-written system (any static system may hold
`Capability::ReceiveAll` for the downlink role; any async system with `CommandOut` ports is
an uplink). The runtime is already normal; only the *construction* paths are special.

## 0. Survey — where the special cases live (verified against the code)

The systems themselves are ordinary: `TelemetrySystem` is a plain `CyclicSystem` whose
output bundle declares `AllOutputs` (`src/telemetry/mod.rs:630`), `UplinkSystem` a plain
`AsyncSystem` with two `CommandOut` ports (`src/telemetry/mod.rs:430`). The coordinator is
already generic: reader-slot sizing counts `ReceiveAll` capabilities
(`src/coordinator/mod.rs:1453`), `validate_receive_all_last` checks the capability, not the
type (`src/coordinator/mod.rs:1210`), and the dl loader rejects capabilities
(`src/dl.rs:121`), so ReceiveAll systems are static-host by construction. What remains
special, exhaustively:

1. **No `BuildSystem` impls** — neither type can enter the static `Registry`
   (`src/wiring/mod.rs:199`): `TelemetrySystem::new` takes a live transport, not data
   params; same for `UplinkSystem`.
2. **Dedicated wiring-model fields** — `Wiring.telemetry: Option<TelemetrySpec>` /
   `Wiring.uplink: Option<UplinkSpec>` (`src/wiring/model.rs:43-51`), each capped at one
   instance under a hardcoded name.
3. **Dedicated KDL nodes** — `telemetry { … }` / `uplink { … }` parse arms
   (`src/wiring/parse.rs:64-73`) with their own grammar (`parse_telemetry`,
   `parse_transport_addr`).
4. **Dedicated resolve blocks** — uplink inserted by hand before the edges pass
   (`src/wiring/mod.rs:311-325`), telemetry appended *after* the edges pass
   (`src/wiring/mod.rs:355-360`) to satisfy `validate_receive_all_last`.
5. **Dedicated builder methods** — `WiringBuilder::telemetry`/`::uplink`
   (`src/wiring/builder.rs:189-202`), and `CoordinatorBuilder::add_telemetry`/`add_uplink`
   (`src/coordinator/mod.rs:876-897`, thin sugar over `add_cyclic_named`/`add_async`).
6. **Dedicated CLI flags + banner** — `--telemetry`/`--no-telemetry`/`--telemetry-mode`/
   `--uplink` mutate the model fields (`src/cli/mod.rs:378-394`); the banner reads them
   (`src/cli/mod.rs:266-294`); the runner resolves against an **empty** registry
   (`src/cli/mod.rs:206`), so today no static system at all is reachable from `metor-fsw`.

**Ordering is the one real constraint.** `resolve` runs systems → slots → uplink → edges →
telemetry; a ReceiveAll cyclic registered before a slot (also cyclic) fails
`ReceiveAllNotLast`. This is alarms design §7 **F1** (`docs/alarms.md:327`), and alarms W5
already plans the fix: the registry entry gains a `descriptor: fn() -> SystemDescriptor`
peek, and `resolve` defers static ReceiveAll systems to a second pass after slots. This
plan **reuses that pass verbatim** — telemetry is just the second built-in riding it.

**Non-goals.** A params-configurable uplink command set (the `UplinkPorts` bundle is a
static type; per-instance port lists are a separate feature — E2 in
`src/telemetry/mod.rs:443`). Transport pluggability beyond TCP stays type-per-transport:
another transport = another registered type, which is the point of the registry. Shared
uplink/downlink sockets stay deferred (`docs/messages.md` §4.5).

**Coordination with alarms.** Alarms W5 (`docs/alarms-plan.md`) builds
`Registry::with_builtins()` + the F1 deferral. Whichever lands first builds it; the other
rebases onto it. Waves below assume this plan can land it (W2 is written self-contained).

Built in **5 waves**, `cargo test -p metor-fsw-2` green after each:

```
W1 (BuildSystem impls) ─▶ W2 (registry built-ins + F1 deferral) ─▶ W3 (model/parse/builder surgery) ─▶ W4 (CLI) ─▶ W5 (tests sweep + docs)
```

---

## Wave 1 — `BuildSystem` impls + data params (`src/telemetry/mod.rs`)

Give both systems a **data-only params surface** so the registry factory can construct
them; keep the transport generics (`TelemetrySystem<T>`, `UplinkSystem<R>`) and
`TelemetrySystem::new(TelemetryConfig)` untouched — tests and programmatic users still
inject mocks.

1. `DownlinkParams { addr: SocketAddr, #[serde(default)] instances: Option<Vec<String>>,
   #[serde(default)] frames: Option<Vec<String>> }`. Both lists absent ⇒
   `TelemetryMode::All`; either present ⇒ `Subset` (empty-vec defaults for the missing
   one). This deliberately avoids a KDL enum spelling: sequence params are child nodes
   (`docs/design-kdl-serde.md`), so the mission reads
   `system "telemetry" type="TcpDownlink" addr="127.0.0.1:2240" { instances "nav" "imu" }`.
   `TelemetryModeSpec` (`src/wiring/model.rs:264`) collapses into this — one mode type,
   `mode_from_spec` (`src/wiring/mod.rs:718`) deleted in W3.
2. `impl BuildSystem for TelemetrySystem<TcpTransport>`:
   `new(p)` = `Self::new(TelemetryConfig { transport: TcpTransport::new(p.addr), mode })`
   — `TcpTransport::new` is lazy (connects on first announce), so a sync constructor is
   safe.
3. `UplinkParams { addr: SocketAddr }`; `impl BuildSystem for
   UplinkSystem<TcpRecvTransport>` the same way (`TcpRecvTransport::new` is equally lazy).
4. Delete `CoordinatorBuilder::add_telemetry`/`add_uplink`
   (`src/coordinator/mod.rs:876-897`) — they are the coordinator-level special case, and
   pure sugar. In-crate callers move to
   `add_cyclic_named("telemetry", TelemetrySystem::new(cfg))` / `add_async(UplinkSystem::new(r))`.
   (Alternative: keep them as documented sugar. Deleting is preferred — two spellings of
   "add the downlink" is how the special case creeps back.)
5. Export the params types from `src/lib.rs`.

**Verify:** unit test each params→mode projection (absent/one/both lists); existing
telemetry + uplink tests compile against `add_cyclic_named`/`add_async`; SocketAddr
round-trips through the KDL deserializer (string property → serde `SocketAddr`).

## Wave 2 — registry built-ins + the deferred-ReceiveAll pass (`src/wiring/mod.rs`)

*(= alarms W5 items 1–2; skip whatever has already landed and extend it.)*

1. The factory table value becomes `struct Entry { factory: SystemFactory, descriptor:
   fn() -> SystemDescriptor }` — `Registry::register` already has
   `<S as AddToBuilder<K>>::descriptor` in scope (`src/wiring/mod.rs:199`).
2. `Registry::with_builtins()` = `new()` + `register::<TelemetrySystem<TcpTransport>, _>("TcpDownlink")`
   + `register::<UplinkSystem<TcpRecvTransport>, _>("TcpUplink")` (+ `"Alarms"` when W5 of
   the alarms plan lands). Type-name strings are the one bikeshed; `Tcp*` encodes that
   another transport is another type.
3. **F1 deferral** in `resolve` (`src/wiring/mod.rs:275-290`): the systems pass peeks each
   *static* spec's descriptor; specs whose capabilities contain `ReceiveAll` are deferred
   and instantiated in a second pass **after** the slots pass, preserving their relative
   KDL order (so `alarms` before `telemetry` in the document keeps same-cycle downlink of
   alarm emits). dl specs are never deferred (capabilities rejected at open,
   `src/dl.rs:121`). Edges resolve after, order-insensitively, off the completed
   `instances` map — so command edges naming `"uplink"` and any future frame edge naming a
   deferred instance both work unchanged.

**Verify:** a wiring test where `system "telemetry"` is declared *before* a `slot` builds
(fails `ReceiveAllNotLast` without the deferral); two downlink instances at different
addresses build and both tap (reader-slot sizing already counts per-holder,
`src/coordinator/mod.rs:1453`); a dl spec is not deferred.

## Wave 3 — delete the special wiring surface (`src/wiring/{model,parse,builder,mod}.rs`)

1. **Model:** drop `Wiring.telemetry`/`Wiring.uplink`, `TelemetrySpec`, `UplinkSpec`,
   `TelemetryModeSpec`; fix the `pub use` fan-out (`src/wiring/mod.rs:71-75`,
   `src/lib.rs:155`).
2. **Parse:** drop the `"telemetry"`/`"uplink"` arms and their grammar fns. Replace each
   arm with a **spelled-out migration error** (a `LoadError` variant whose help text shows
   the `system "telemetry" type="TcpDownlink" …` spelling) rather than falling through to
   `UnknownTopLevelNode` — the nodes appear in every existing mission file and doc, and
   the parse error is the only migration notice a bundle gives. Old bundles' `mission.kdl`
   stops loading with that error; no `FSW_ABI_VERSION` bump (the dl ABI is untouched —
   this is a document-schema change, and the error is self-explaining).
3. **Resolve:** delete the uplink block (`src/wiring/mod.rs:311-325`) and the telemetry
   block (`:355-360`) — both roles now enter through the systems pass + W2 deferral.
4. **Builder:** rewrite `WiringBuilder::telemetry`/`::uplink` as sugar that pushes an
   ordinary `SystemSpec { name: "telemetry", ty: Some("TcpDownlink"), artifact: None,
   params: ParamSource::Kdl(rendered node text) }` (the static path deserializes KDL, so
   builder-origin params must render to it — `resolve_static` already re-parses
   `ParamSource::Kdl` text, `src/wiring/mod.rs:399-411`). Keeping the two methods keeps
   ~10 existing tests one-line; they are now pure spelling, not model surface.

**Verify:** `wiring/tests.rs` grammar cases move to the `system` spelling; the migration
error fires on a legacy `telemetry { … }` node with the new spelling in its help; builder
round-trip (`WiringBuilder::telemetry(...)` → resolve → downlink instance present under
`"telemetry"`).

## Wave 4 — CLI (`src/cli/mod.rs`)

1. Resolve against `Registry::with_builtins()` (`src/cli/mod.rs:206`) — this also makes
   `metor-fsw run` capable of any future built-in without a binary change.
2. Flag rewrite over `wiring.systems`:
   - `--telemetry <addr>` — remove any existing spec with `ty == "TcpDownlink"`, push a
     synthesized one (name `"telemetry"`, `ParamSource::Kdl`). `--telemetry-mode` folds
     into the rendered params.
   - `--no-telemetry` — remove all `ty == "TcpDownlink"` specs.
   - `--uplink <addr>` — same pattern for `"TcpUplink"` under name `"uplink"`.
   Overrides key on **type**, not instance name, so they also override a renamed or
   user-typed downlink? No — keying on type would silently delete a user's custom
   downlink system; key on the built-in type strings only, which is exactly what the
   flags have always meant (the TCP built-ins).
3. Banner (`src/cli/mod.rs:266-294`): find specs by built-in type, extract `addr` by
   re-parsing the spec's params node (the same `KdlDocument` one-liner `resolve_static`
   uses); print one line per instance — the banner now naturally shows multiple
   downlinks.

**Verify:** all of `src/cli/tests.rs` (`telemetry_flags_mutually_exclusive`,
`no_telemetry_override_disables_kdl_telemetry`, `uplink_override_sets_wiring_uplink`, …)
rewritten against the systems-vec shape; a mission declaring the downlink as a `system`
node shows it in the banner without any flag.

## Wave 5 — test sweep, example missions, docs

1. **Integration tests:** `tests/wiring_resolve.rs:117` and `tests/dl_integration.rs`
   (builder `.telemetry(...)` — unchanged spelling, now sugar);
   `tests/slot_integration.rs` uses `add_uplink`/`add_telemetry` — move to
   `add_async(UplinkSystem::new(...))` / `add_cyclic_named(...)` per W1.4.
2. **Example missions:** any in-repo `mission.kdl` carrying `telemetry`/`uplink` nodes
   (adcs-fsw2 et al.) move to the `system` spelling; this is also the doc-example
   sweep's source of truth.
3. **Docs reconcile:** `telemetry.md` §8 (KDL surface), `wiring.md` §1 grammar,
   `messages.md` §4.4/§4.5 (uplink registration), `coordinator.md` (add_* surface),
   `docs/alarms-plan.md` W5 cross-reference, `DESIGN.md` document map. State plainly:
   *the downlink and uplink are registry systems; "telemetry" and "uplink" are
   conventional instance names, not reserved words* (only `"coordinator"` stays reserved,
   `src/wiring/mod.rs:264-274`).

**Verify:** full crate tests + `--no-default-features` build; `metor-fsw run` of an
updated example mission boots with banner, downlink, and uplink live end-to-end.

---

## Resulting invariants

- A downlink is *any static cyclic system holding `Capability::ReceiveAll`*; the framework
  ships `TcpDownlink` as one of them. Multiple instances are legal (per-holder reader
  slots; per-instance health prefixes — the `telemetry.dropped` keys already hang off the
  instance name).
- An uplink is *any system emitting `CommandOut` ports*; `TcpUplink` is the shipped TCP
  one. Command edges were already name-addressed (`resolve_msg_edge` has no hardcoded
  names) — nothing changes downstream of registration.
- Registration-order safety is automatic for KDL/builder users (F1 deferral) and
  documented for direct `CoordinatorBuilder` users (add ReceiveAll holders last —
  unchanged, still enforced by `validate_receive_all_last`).
