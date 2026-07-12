# Slots & sequences (`sequences-slots`)

> **Status: v1 IMPLEMENTED.** This proposed a runtime-loadable-system layer (`slots`) and an
> ergonomic author surface for the most common occupant, a futures-driven state machine
> (`sequences`, via a `#[sequence]` decorator). It is now built (WP10, plan
> `sequences-slots-plan.md`); the body and the **resolved decisions** in §9 describe the
> shipped design. Two decisions landed differently than the prose above and are worth calling
> out:
>
> 1. **Ring reclamation (§2.3 / §9 Q2) needed no change.** Grounding the premise in the tree
>    showed the ring already frees a reader slot on `View::drop` and a `Writer` holds no claim,
>    so the swap re-acquires with zero ring surgery — Wave 1 is verification-only (the §2.3/Q2
>    text is corrected).
> 2. **v1 slots hold *sequence* occupants only.** A slot's implicit `SlotControlIn` cancel input
>    and `SequenceStatus`/health/log output tail make the occupant contract sequence-shaped, so
>    v1 restricts the allowed set to `#[sequence]` occupants (their descriptors carry those
>    implicit ports). Loading an arbitrary plain-cyclic `.so` into a slot is future work.
>
> One operational rough edge surfaced: `Coordinator::run_for` re-runs `start()` (and thus each
> dl/slot occupant's `fsw_bind_init`) on every call, which is not idempotent for dl occupants —
> so a mission with slots is driven by a single `run_for`, with runtime commands injected via
> `control_handle()` during that run. Making `run_for` re-entrant is a separate follow-up.
>
> Slots have since grown a **process mode**: `process=#true` on the `slot` node runs every
> occupant in its own worker process, spawned per `Load`, with the same isolation rule as
> process systems (the host never dlopens an occupant artifact). Design and rationale:
> `docs/process-slots.md`.
>
> **Unification update (2026-07-11, the packs arc — `docs/packs.md`,
> `docs/design-packs-authoring.md`):** the sequence *stack* this document specifies is
> retired, and the "sequence occupants only" restriction (callout 2 above) is **lifted**:
>
> - The occupant tail (`SlotControlIn` cancel input; `SequenceStatus` output) is a
>   **mount property**, not descriptor content: `add_slot` and the occupant-mount bind
>   append it around any pack entry's own ports, so **slots accept any entry** — an async
>   occupant keeps the cooperative `aborted()` cancel; a sync (cyclic) occupant gets
>   stop-on-cancel with a terminal `Aborted` (`docs/packs.md` §9). An occupant that
>   declares the tail itself is a pre-pack artifact and is rejected.
> - `#[sequence]` (§4) is a deprecated passthrough slated for deletion. A sequence is a
>   plain `async fn` registered with `Pack::task("name", f)`; typed params ride a
>   `Params<P>` wrapper argument; the crate exports via `export_pack!`. The generated
>   `SeqSystem`/`SeqBound`/`SeqStatusOut` machinery and the whole `run_seq_*` ABI family
>   are **deleted** — occupants run behind the unified `Driver` seam
>   (`FutureDriver`/`OccupantFuture`, `src/handler/driver.rs`).
> - `SeqClock` survives as a type alias of `CycleClock`, and async entries gained
>   `cycle().await` (suspend until next cycle) beside `wait`/`now`/`progress`/`aborted`.
> - The wiring surface in §5's examples predates packs: `artifact` nodes carry no `type=`
>   anymore, and `allow occupant="…"` takes an optional `artifact=`
>   (`docs/packs.md` §8).
>
> The slot layer itself — the state machine (§2), the command plane (§3), ring reuse
> (§2.3), telemetry (§7) — shipped as described and is still current.

The cube-sat example (`examples/cube-sat/src/sequencer.rs`) has a feature `metor-fsw-2`
does not: **sequences** — small re-entrant programs that command the spacecraft through
timed transitions, loaded into named **channels** and started/stopped by an operator. The
shape is excellent and worth keeping: an `async fn` body, suspended at `wait(dur)` points,
polled **once per control cycle** with a no-op waker so it is a deterministic state machine
driven by the loop rather than a free-running task (`Sequencer::step`,
`sequencer.rs:344-377`).

This document generalizes that shape onto the fsw-2 framework under one unifying idea:

> **A slot is a system loaded at runtime; a sequence is one ergonomic kind of
> slot-loadable system.**

The cube-sat sequencer hard-codes its slots (`"ADCS"`/`"Recovery"`, `sequencer.rs:233`),
its registry (`build`, `sequencer.rs:139`), and its command surface (in-process
`SequenceCommand`). fsw-2 already has the machinery to do better: a slot is a **position in
the coordinator's cyclic call chain** with a **fixed port contract**, and its occupant is a
**dl-opened `CyclicSystem`** loaded/unloaded/started/stopped at runtime. Nothing about the
data path, the rings, the descriptors, validation, telemetry, or the ABI needs reinventing
— this is a runtime-control front-end onto `add_dl_cyclic` / `DlSlot` (dl-open.md,
`src/dl.rs:232-369`).

The code this builds on:

- `CyclicSlot` — the per-system trait the coordinator drives each cycle
  (`src/coordinator/mod.rs:247-253`); `CyclicRunner` (`src/system/mod.rs:250-349`) and
  `DlSlot` (`src/dl.rs:273-369`) are its two impls.
- The dlopen ABI — `fsw_create`/`fsw_bind_init`/`fsw_execute`/`fsw_shutdown`/`fsw_destroy`
  + `FswRing`/`FswStatus` (`src/abi/mod.rs`, dl-open.md §"The C-ABI surface"). Loading
  binds an occupant's ports over host rings via `RawBinder`/`attach_raw`.
- `DlSystem::open`/`into_slot` (`src/dl.rs:142-260`) — opens, describes, validates, and
  (at build today) `fsw_create`s an occupant.
- The wiring model (`src/wiring/model.rs`) — `Artifact`/`SystemSpec`/`EdgeSpec`; the KDL
  `artifact`/`system`/`connect` surface and `resolve`.

---

## 1. Vocabulary & the unifying model

| Term | Meaning |
|---|---|
| **Slot** | A named, fixed position in the cyclic call chain with a **declared port contract** (input frames + output frames) and **pre-allocated rings**. The occupant is dynamic; an empty slot is a no-op in the cycle. |
| **Occupant** | A dl-opened `CyclicSystem` currently bound into a slot. Any cyclic `.so` can occupy a slot whose contract it satisfies. |
| **Sequence** | An occupant authored with `#[sequence]` from an `async fn`: a re-entrant state machine the slot polls once per cycle. The ergonomic, common case — but structurally just an occupant. |
| **Allowed set** | The slots' candidate occupants, declared in wiring and **pre-opened/validated at build**. `Load` selects one by name. v1 does not `dlopen` an arbitrary runtime path — **live/runtime path loading is planned future work** (§9, Resolved Q7). |

Where this sits next to the existing system kinds:

- A **static cyclic** system (`add_cyclic`) — fixed occupant, fixed for the run, owns a
  `CyclicRunner`.
- A **dl cyclic** system (`add_dl_cyclic`) — fixed occupant chosen at build, owns a
  `DlSlot`. Loaded/created at `build()`, never swapped.
- A **slot** (new) — the same `DlSlot` machinery, but `fsw_create`/`fsw_bind_init` happen at
  **runtime** on operator command and the occupant is swappable. A slot is a third
  `CyclicSlot` impl (`SlotRunner`) holding `Option<DlSlot>` plus a state machine.

The slot **is** a cyclic system from the coordinator's view: it sits in
`Coordinator::cyclic: Vec<Box<dyn CyclicSlot>>` (coordinator.md §1.2) and is `step(now)`d
every cycle like any other. When empty/stopped/done, its `step` is a cheap no-op; when
running, it forwards `fsw_execute` to the occupant.

---

## 2. Slot lifecycle & state machine

```text
            Load(occ)              Start
  ┌───────┐ ────────▶ ┌────────┐ ────────▶ ┌─────────┐
  │ Empty │           │ Loaded │           │ Running │
  └───────┘ ◀──────── └────────┘ ◀──────── └─────────┘
      ▲       Unload    ▲    ▲    Stop (hard-drop:    │
      │                 │    │    drops the future)   │ future → Ready(Outcome)
      │  Unload         │    │                        ▼
      ├─────────────────┤    │                  ┌──────────┐
      │           Reset │    └──── Reset ────────┤   Done   │ (Completed/Aborted/Failed)
      │ (rebuild future)│                        └──────────┘
      │                 └──────── Reset ──────── ┌──────────┐
      └──────────────── Unload ──────────────────┤ Stopped  │ (Panicked, terminal)
                                                 └──────────┘
```

Edges, in words: `Load` Empty→Loaded; `Start` Loaded→Running; `Stop` Running→Loaded (hard-drop,
§2.1); the future returning `Ready` takes Running→`Done`; a panicked occupant takes
Running→`Stopped`; `Reset` rebuilds from any of Loaded(post-Stop)/`Done`/`Stopped`; `Unload`
returns any state to `Empty`.

```rust
pub enum SlotState {
    Empty,                              // no occupant; step() is a no-op
    Loaded   { occupant: OccupantId },  // created+bound+init'd; future built but NOT yet polling
    Running  { occupant: OccupantId },  // polled every cycle
    Done     { occupant: OccupantId, outcome: Outcome },  // future returned Ready; terminal
    Stopped  { occupant: OccupantId, reason: StopReason }, // panicked; terminal
}
```

`Stop` returns Running→Loaded but **drops the future** (§2.1), so the Loaded state after a
Stop has no live future and is not directly re-runnable — `Start` from it is rejected;
`Reset` rebuilds. (We keep one `Loaded` variant for simplicity; the slot tracks "has a live
future" alongside it.)

This is a richer machine than the existing `SlotState { Running, Stopped }`
(`src/coordinator/mod.rs:225-235`). **Decided (§9 Q6):** the slot layer introduces its **own**
`SlotState` (above) rather than overloading the existing two-variant enum, which stays for
static/dl slots. It maps cube-sat's `SequenceRunState`
(`Idle`/`Running`/`Stopped`/`Completed`/`Aborted`) nearly one-to-one, with `Empty` added
(cube-sat channels always have an `available` list and never an empty occupant).

### 2.1 What each command does (in terms of existing ABI)

The headline: **no new lifecycle ABI symbols are required.** Every transition is an existing
`fsw_*` call or a pure host-side scheduling change.

| Command | Action | ABI |
|---|---|---|
| **Load(occ, params)** | Pick `occ` from the allowed set, `fsw_create` it, hand it the slot's pre-built `FswRing` arrays, `fsw_bind_init` (binds ports over the slot rings, runs `init`, **builds the future**). Empty→Loaded. | `fsw_create` + `fsw_bind_init` (existing) |
| **Start** | Begin calling `fsw_execute` each cycle. Loaded(with live future)→Running. | none (host scheduling) |
| **Stop** | **Hard drop:** stop calling `fsw_execute` *and* **drop the occupant's future** (cube-sat parity, `sequencer.rs:293-302`). Running→Loaded with the future **gone**; re-running requires `Reset`. | `fsw_destroy` (drops the future + its owned ports) |
| **Abort** | Cooperative cancel: raise the slot's cancel signal; the running sequence observes it at its next `wait` point and runs its safing branch, completing `Done{Aborted}`. | a control input frame (§4.4), no new ABI |
| **Reset** | `fsw_destroy` + `fsw_create` + `fsw_bind_init` — rebuild the occupant (and its future) from the beginning. Allowed only from `Done`/`Stopped`/post-`Stop` Loaded. | existing symbols |
| **Unload** | `fsw_destroy`, drop the occupant. →Empty. | `fsw_destroy` (existing) |

A sequence whose future returns `Ready(Outcome)` transitions Running→`Done` on its own (the
occupant reports it through the terminal `FswStatus::Done` — §7/§8). `Done`/`Stopped` are
terminal until `Reset` or `Unload`, exactly as cube-sat gates `Reset` to terminal
`run_state`s (`sequencer.rs:303-315`).

**Decided (§9 Q3): `Stop` = hard-drop, matching cube-sat.** cube-sat's `Stop` is a hard drop
of the future (`sequencer.rs:293-302`); its cooperative path is `Abort`. We keep that operator
muscle-memory: `Stop` drops the future immediately (no async cleanup), so re-running a stopped
slot is an explicit `Reset` (rebuild). This composes cleanly with the ring-reclaim decision
(§2.3, Q2): the dropped future owns its ports (§4.2, Q5), so dropping it drops those
non-owning `Writer`/`View`s, which **releases the slot's ring roles** — `Stop` naturally
frees ring capacity with no special handling. A future explicit **`Pause`** (stop polling but
keep the future resumable) is possible later work; v1 is hard-drop only.

### 2.2 Where the slot sits in the cycle

`SlotRunner::step(now)` (the new `CyclicSlot` impl):

```rust
fn step(&mut self, now: Timestamp) {
    self.publish_slot_status(now);            // host-side state machine telemetry (§7)
    match &mut self.state {
        SlotState::Running { .. } => {
            // forward to the occupant exactly as DlSlot::step does:
            let status = self.occupant.as_mut().unwrap().execute(now);  // fsw_execute → polls the future once
            self.fold_status(status, now);     // Running | Done | Stopped(Panicked)
        }
        _ => { /* Empty/Loaded/Done/Stopped: no fsw_execute this cycle */ }
    }
}
```

Empty/Loaded/Done/Stopped slots cost one status publish and a branch — they do not touch the
occupant. The occupant, when polled, runs the verbatim `CyclicRunner::step`
timing / health logic inside the `.so` (`src/abi/mod.rs` `run_execute`), so a slot
occupant gets the standard health counters for free. An input can never be lapped (the rings
are lossless — a slow occupant backpressures its producer instead); the only occupant
hard-stop is a caught panic, which the slot folds into `SlotState::Stopped`.

### 2.3 Rings are host-owned for the whole run; the occupant borrows transiently

This is the load-bearing reuse. The coordinator already owns every ring in `RingTable` for
the whole mission and hands occupants `Writer`/`View` handles over them (coordinator.md
§1.2/§1.3). For a slot:

- The slot's rings are **allocated up front** from its declared contract (sized to the max
  over the allowed occupants), owned by `RingTable` like any output/input.
- On **Load**, `fsw_bind_init` hands the occupant the slot's `FswRing` arrays; the `.so`
  `attach_raw`s each region and claims its `Writer` (outputs) / `View` (inputs) — exactly
  `DlSlot::init` (`src/dl.rs:299-314`).
- On **Stop/Unload/Reset**, `fsw_destroy` drops the occupant's non-owning ports, **releasing**
  the writer role and reader slots back to the host-owned ring. A later Load re-claims them.

The single subtlety this introduces over the build-time dl path is **re-acquisition**: a slot
reloads N times, so a dropped holder's reader slot / writer role must be reclaimable.

**Decided (§9 Q2): the ring already does this — Wave 1 is verification-only, no ring change.**
Grounding the original premise in the tree showed it was inaccurate: a `View` **already**
frees its reader-table slot on drop (`ring/src/lib.rs:1102-1107`,
`slot_cursor(slot).store(FREE_SLOT, Release)`; reuse is tested by `reader_table_claim_free`,
`ring/src/tests.rs:90`), and a `Writer` holds **no** runtime claim at all — `writer()` is just
an `inner.clone()` with no header flag and no `Drop` (`ring/src/lib.rs:692-703`); "at most one
live writer" is a documented *discipline* (`:844`), not an enforced lock. Because slot swaps
are **strictly ordered** — `fsw_destroy` drops the occupant's non-owning ports before any
re-`Load` constructs new ones (§6 teardown) — a fresh `view()` re-CASes the freed slot
(`:710-737`) and a fresh `Output` re-acquires the writer for free. The only genuinely-missing
capability would be reclaiming a slot whose owner died *without* running `Drop` (crash
reclamation, for which the slot-epoch at `:727` is already reserved); that is a cross-process
concern, **out of scope** here. So this is a real reuse with **zero ring surgery** — Wave 1
adds swap re-acquire tests (incl. a raw attach/detach round-trip under Miri) to prove
the Load→Stop→Load cycle, and nothing more. `Stop`'s ring-freeing (§2.1) then falls out of the
existing `View`/`Writer` drops.

---

## 3. Runtime control plane

cube-sat commands its sequencer with an in-process `SequenceCommand` enum routed by
`channel_id` (`sequencer.rs:260-317`). fsw-2's coordinator runs its cycle loop on a single
stellarator task holding `&mut self` (`Coordinator::run_for`, `src/coordinator/mod.rs:1101`),
so an operator command must reach a *running* loop without a second mutable borrow.

**Decided (§9 Q1, since superseded in shape but not in spirit — see below): commands are
`Msg`-carried, drained once per cycle.** This matches the framework's "everything flows as ordinary
ports; live updates arrive as ordinary input records" stance (system.md §1.5) and mirrors telemetry
being the downlink: slot control is the **uplink**.

> **Superseded (2026-07-02): the mechanism below shipped, then was redesigned twice more** —
> once to move commands off a bespoke `SlotCommand` zerocopy frame onto a `SequenceCommand`
> **message** channel (`docs/messages.md` §4), and again to replace the coordinator-drained
> broadcast with **per-slot explicit message edges** and **name** addressing
> (`docs/message-wiring.md` §6, `docs/design-command-slots.md`). What is actually wired today:
>
> - Each slot declares an ordinary `commands: MsgIn<SequenceCommand>` **fan-in** port (a wired
>   port with `FanIn::Many`, not a coordinator capability) — every producer that should reach a
>   given slot is connected to it by an **explicit** KDL/builder edge; there is no implicit
>   broadcast-to-every-slot sugar. A mission wires each producer × slot pair it wants:
>   ```kdl
>   system "uplink" type="TcpUplink" addr="127.0.0.1:2241"
>   connect "uplink"      -> "mode" msg="SequenceCommand"   // ground commands
>   connect "coordinator" -> "mode" msg="SequenceCommand"   // in-proc control_handle()
>   ```
> - At the head of its own `step`, **before** polling its occupant, a slot drains its `commands`
>   fan-in (`MsgIn::drain`, every-record — commands must not be dropped) and applies each one
>   whose `cmd.channel` (a `String`) equals the slot's own **instance name**
>   (`SlotRunner::apply_command`, `src/coordinator/slot.rs`) — filtering by name, not by a numeric
>   `channel_id`/`ChannelId` (that type is gone; `SequenceCommand`/`SequenceChannelEvent` are
>   name-addressed end-to-end, matching how the panel already addresses a slot).
> - The coordinator is registered as an ordinary system **#0** under the reserved instance name
>   `"coordinator"` (`docs/design-command-slots.md` §2.6): `Coordinator::control_handle()` returns
>   a take-once `Option<MsgOut<SequenceCommand>>` over that bundle's own `commands` output, so the
>   in-proc convenience is wired exactly like the uplink's — `connect "coordinator" -> "<slot>"
>   msg="SequenceCommand"` is an ordinary edge, not a coordinator special case. There is **no**
>   coordinator-side command-drain stage anymore; the coordinator does not know slots exist.
> - The **uplink** is an ordinary `AsyncSystem` (`UplinkSystem`) with one `CommandOut<M>` output
>   per forwarded command type (`CommandOut<M>` is `MsgOut<M>` sugar, untelemetered by default);
>   it routes each received wire `Msg` to the declared output whose `PacketId` matches
>   (`RouteMsg::route`, A8) and subscribes on the ground to exactly its declared outputs' ids.
>
> See `docs/messages.md` and `docs/message-wiring.md` for the full history and rationale; this
> section is kept for the original "why frames/messages, not a `&mut` API" argument, which still
> holds.

Why a message channel rather than a `&mut` API: it needs no lock, no second executor handle, no
change to the single-threaded cycle invariant (coordinator.md §3.7); commands are serialized
through the same ring discipline as data. Unlike the original all-frame design, `commands` is
**untelemetered by construction** at the producer (the uplink's/coordinator's `CommandOut`
outputs) — inbound control is not automatically echoed onto the downlink, though a slot's own
lifecycle *transitions* (Loaded/Started/Stopped/…) are still telemetered on its `SequenceChannelEvent`
channel (§7), so the operator still sees the effect of a command even though the raw command
Msg itself is not relayed. The cooperative **Abort** signal then crosses to the occupant as
ordinary ring data too — see §4.4 — so cancellation needs no new ABI either.

`Load` carries an **occupant id** (a name in the allowed set), not a path: the coordinator
resolves it against the slot's pre-opened `DlSystem`s and `fsw_create`s with the carried
params. Arbitrary runtime `dlopen` of an unknown path is out of scope for v1 but is **planned
future work** — a `Load` variant carrying a path is the natural extension once live re-sizing
and live validation land (§9, Resolved Q7).

---

## 4. The `#[sequence]` decorator

The ergonomic author surface. It turns an `async fn` into a complete, dl-loadable occupant:
a future-driven state machine **plus** the C-ABI exports — driven by the host exactly like
any cyclic `.so`.

**Decided (§9 Q5): the author writes the ports as direct parameters; the future owns them.**
There is no `ctx` bundle and no per-cycle re-borrow. Every `Input<T>`/`Output<T>`
parameter is a port the macro binds once (at `fsw_bind_init`) and **moves into the future**,
which owns it for its whole life. Because the ports are owned by the `'static` future, there
is no per-cycle port threading at all — the old "stored future touching per-cycle `&mut`
borrows" problem (and its refresh-cell machinery) evaporates.

### 4.1 Author shape

> **Updated (E7, then the backing erasure — both landed):** the ring-`Backing` generic is gone
> entirely. Rings are backing-erased, so a sequence fn is written — and emitted — over plain
> `Input<T>`/`Output<T>` with **no** generic parameters. (E7 briefly had the macro inject a
> hidden `__B: Backing` and rewrite each port type to `Input<T, __B>`; the erasure removed that
> injection, and `#[sequence]` now *rejects* any generic parameters on the fn: "#[sequence] fns
> take no generic parameters (rings are backing-erased)".) A free `now()` (and `Seq::now()` on
> an explicit handle) reads the ambient
> `SeqClock`, so a sequence can stamp the frames it emits without threading `Timestamp` through by
> hand, and output ports gained the infallible `publish()`/`publish_with()` (E6, system.md §2.1) —
> `#[sequence]` adopts it the same way `#[system]` does. The example below is the current shape;
> `att.latest().ok().flatten()`/`cmd.write(...).ok()` from an earlier revision of this doc are gone.

```rust
// examples/adcs-fsw2/systems/commissioning/src/lib.rs, as shipped:
use core::time::Duration;
use adcs_contracts::{AttitudeEstimate, ModeCmd};
use metor_fsw_2::sequence::{now, progress, wait};
use metor_fsw_2::{Input, Outcome, Output};

// `name` defaults to the fn name; override with #[sequence(name = "...")].
#[metor_fsw_2::sequence]
async fn commissioning(mut att: Input<AttitudeEstimate>, mut mode: Output<ModeCmd>) -> Outcome {
    progress("warming up");                                        // free fn — ambient (§4.3)
    if wait(Duration::from_millis(100)).await.aborted() {
        mode.publish(&ModeCmd::safe().stamped(now()));              // E6 publish + E7 now()
        return Outcome::Aborted;
    }
    let _ = att.latest();                                          // E3: Option, no Result
    mode.publish(&ModeCmd::settling().stamped(now()));
    progress("reaction wheels enabled");
    if wait(Duration::from_millis(150)).await.aborted() {
        mode.publish(&ModeCmd::safe().stamped(now()));
        return Outcome::Aborted;
    }
    mode.publish(&ModeCmd::pointing().stamped(now()));
    progress("pointing");
    Outcome::Completed
}
```

This is cube-sat's `commissioning` body (`sequencer.rs:149-162`) almost verbatim, with the
shared `FSW` handle replaced by typed owned ports (`Input<AttitudeEstimate>`/`Output<ModeCmd>` —
plain types; rings are backing-erased) and the timer replaced by the free `wait`/`progress`
API. `#[sequence(name = "safe_mode")]` overrides the name (`examples/adcs-fsw2/systems/safe-mode`).

The **port set is read straight off the signature** — no separate `SystemInput`/
`SystemOutput` bundle structs to declare. (Alternative considered: an attribute list
`#[sequence(inputs(...), outputs(...))]` — §9 Q5; direct params win because the ports are
both *declared* and *used* in one place, fully typed.)

### 4.2 What the macro generates

The macro scans the signature, partitions the params into `Input<_>` and `Output<_>` ports,
and emits (a) a generated **descriptor** built from those param types, and (b) the `fsw_*`
C-ABI exports — the same symbols, `FswRing` arrays, and `FswStatus` contract every dl system
uses (dl-open.md §"The C-ABI surface"), so the host `DlSlot`/`SlotRunner` drives a sequence
`.so` **indistinguishably from any cyclic occupant**. It does **not** route through
`CyclicRunner`: the ports live in the future, not in a coordinator-owned bundle, so the macro
emits its own thin lifecycle delegating to new `abi::run_seq_*` helpers (the sequence twin of
the `run_*` helpers `export_system!` uses, `src/abi/mod.rs`).

```rust
// 1. The opaque state the ABI threads (the sequence twin of AbiState); the port types
//    are plain — rings are backing-erased, so the sequence stack has no type parameter.
struct SeqState<S: SeqSystem> {
    params: Option<S::Params>,            // decoded in fsw_create; () if none
    bound:  Option<SeqBound>,             // built in fsw_bind_init (below)
    clock:  Rc<SeqClock>,                 // the per-cycle ambient (§4.3)
    poisoned: bool,
}
// SeqBound (src/sequence/mod.rs) is the bound occupant: the 'static future (the user
// ports moved inside it), the wrapper-owned status/health/log tail, the cancel input:
pub struct SeqBound {
    pub future:  Pin<Box<dyn Future<Output = Outcome>>>,
    pub status:  Out<SeqStatusOut>,       // wrapper-owned: SequenceStatus + health/log
    pub control: Input<SlotControlIn>,    // the cancel input (§4.4)
}

// 2. The descriptor: enumerated from the SIGNATURE, in a fixed order the bind walk mirrors.
//    inputs  = [att, gyro, SlotControlIn]  (the Input<T> params, in signature order, + control)
//    outputs = [cmd, SequenceStatus, health, log]   (Output<T> params, then the implicit tail)
fn descriptor() -> SystemDescriptor { /* PortDesc::of::<AttitudeEstimate>(), ::<GyroBias>(), … */ }

// 3. fsw_bind_init (via abi::run_seq_bind_init): build a RawBinder over the host's FswRing
//    arrays and bind the ports in descriptor() order — the SAME plain port types and the
//    same monomorphic bind path the host uses, just over non-owning attaches. The user
//    ports MOVE INTO the future; the implicit control/SequenceStatus/health/log stay in
//    SeqBound for per-cycle telemetry. (This is SeqSystem::build — descriptor() and
//    build() walk one order over one set of types; there is no separate descriptor-side
//    vs bind-side backing.)
fn build(params, binder: &mut RawBinder, clock: &Rc<SeqClock>) -> SeqBound {
    let att  = Input::<AttitudeEstimate>::bind(binder);    // user inputs →
    let gyro = Input::<GyroBias>::bind(binder);            //   future
    let cmd  = Output::<AttitudeCmd>::bind(binder);        // user outputs →
    let status = Out::<SeqStatusOut>::bind(binder);        //   wrapper (tail)
    let control = Input::<SlotControlIn>::bind(binder);
    let future = SEQ_CLOCK.set(clock, || {                 // task-local (§4.3)
        Box::pin(commissioning(att, gyro, cmd))
    });
    SeqBound { future, status, control }
}

// 4. fsw_execute (via abi::run_seq_execute): refresh the clock, fold cancel, poll ONCE.
fn execute(state, now: Timestamp) -> FswStatus {
    state.clock.now.set(now);
    state.clock.cancel.set(read_cancel_input(state));        // Abort folds in here (§4.4)
    let mut cx = Context::from_waker(Waker::noop());
    let bound = state.bound.as_mut().unwrap();
    let poll = SEQ_CLOCK.set(&state.clock, || bound.future.as_mut().poll(&mut cx));
    match poll {
        Poll::Ready(outcome) => { bound.status.seq_status().complete(now, outcome); FswStatus::Done }   // §4.5
        Poll::Pending        => { bound.status.seq_status().running(now, state.clock.drain_progress());
                                  bound.status.health().end_cycle(now, micros); FswStatus::Running }
    }
}
```

The `execute` body is structurally cube-sat's `step` (`sequencer.rs:344-377`): poll once with
`Waker::noop()`, drain progress, and on `Poll::Ready` report the terminal outcome — except the
ports are already inside the future, so nothing is threaded in. The future is built at
`fsw_bind_init` (so **Load builds it from the beginning**, matching `load_slot`,
`sequencer.rs:321-340`) and polled from Start onward.

**Reconciling `type Input`/`type Output`, `descriptor()`, and owned ports.** A sequence
occupant is *not* a `CyclicRunner` and does not use the stock `run_bind_init` (which binds
`S::Input`/`S::Output` into a runner-owned bundle and passes them per cycle). Instead:

- **`descriptor()` is the single source of truth** for ring sizing, `compatible()`
  validation, and the prefixed `announce` — and the macro generates it directly from the
  signature's port types. `fsw_describe` lowers exactly this descriptor (dl-open.md
  §"The serialized descriptor"), so the **host sizes and allocates the right rings with the
  existing machinery, unchanged**.
- The host hands the bound `.so` its `FswRing` arrays **in `descriptor()` order** (as it does
  for any dl system). The macro's `bind_init` walks that same order, so the positional bind
  contract holds — the only difference from a `CyclicRunner` is *where each bound port goes*
  (into the future, vs into a runner field). There is no synthetic `S::Input`/`S::Output`
  bundle the host must own: a sequence has no host-side bundle, which is precisely why it
  bypasses `CyclicRunner`. "Sequences are just systems loaded at runtime" holds **at the
  host/ABI boundary** — the seam that matters — because the coordinator drives the same `fsw_*`
  symbols and the same `CyclicSlot`; the future-vs-runner choice is invisible behind the `.so`.

### 4.3 The only per-cycle ambient: the clock cell + free functions

The future owns its ports, so the **only** state the cycle still refreshes is the coordinator
clock and the cancel flag (for `wait`) plus a progress sink. A tiny shared cell carries them —
far smaller than a port-threading ctx (no port pointers, no `unsafe` deref):

```rust
#[derive(Default)]
struct SeqClock {
    now:      Cell<Timestamp>,        // refreshed by execute() before each poll
    cancel:   Cell<bool>,             // folded from the Abort control frame (§4.4)
    progress: RefCell<Vec<String>>,   // drained into SequenceStatus each cycle
}
```

`wait(dur)` / `progress(msg)` / the `Step::aborted()` helper are **free functions** in the
`sequence` module — so the body needs no `ctx`/`seq` parameter at all. They reach the current
`SeqClock` through a **task-local** the generated `execute` sets around the synchronous poll
(`SEQ_CLOCK.set(&clock, || future.poll(cx))`) and clears after. This is sound because the poll
is synchronous and single-threaded (coordinator.md §3.7): the task-local is live *only* during
a poll, exactly when `wait`/`progress` run, and never escapes.

`wait(dur)` resolves by **comparing a stored deadline against `clock.now`** — not a maitake
timer wheel — so it is driven entirely by coordinator time and is deterministic under a
`Simulated` clock (coordinator.md §6), exactly as cube-sat advances sim ticks per step
(`sequencer.rs:345-346`):

```rust
impl Future for Wait {
    fn poll(self, _cx) -> Poll<Step> {
        SEQ_CLOCK.with(|c| {
            if c.cancel.get()              { Poll::Ready(Step::Aborted) }   // .aborted() == true
            else if c.now.get() >= self.deadline { Poll::Ready(Step::Elapsed) }
            else                           { Poll::Pending }                // re-checked next cycle
        })
    }
}
```

**Optional explicit form.** For authors who prefer explicit over ambient, the macro also
recognizes a trailing `seq: Seq` parameter alongside the ports — a small handle exposing
`seq.wait(dur)` / `seq.progress(msg)` over the same `SeqClock`. The free-function form is the
headline; the handle is opt-in (§9 Q5).

### 4.4 Cancellation across the ABI

`Abort` (§3) must reach `clock.cancel` *inside the `.so`*. It crosses as **ordinary ring
data**, needing no new ABI: the slot contract carries an implicit `SlotControlIn { cancel:
bool }` input port (one of the `FswRing`s `bind_init` reserves) that `execute` reads at the
top of each cycle (`read_cancel_input`) and folds into `clock.cancel`. The coordinator writes
that frame when an `Abort` command targets the slot. This is the same "live update is a frame"
mechanism (system.md §1.5) and keeps the cancel observable "within one cycle of the request"
(cube-sat's guarantee, `sequencer.rs:106-107`).

### 4.5 Outcome / terminal completion

`Outcome` mirrors cube-sat's (`sequencer.rs:130-133`) plus the protocol's `Failed`:

```rust
pub enum Outcome { Completed, Aborted, Failed }
```

When the future is `Ready(outcome)`, `execute` (a) writes a terminal `SequenceStatus` frame
(§7) and (b) returns the **terminal `FswStatus::Done`** (§8) so the host `SlotRunner` flips to
`Done{outcome}`. `Completed`/`Aborted`/`Failed` is *telemetry* detail carried in the frame;
the host only needs the single bit "this occupant is terminal, stop polling." That single new
status code is the one genuine ABI addition — see §8 / §9 Resolved Q6.

---

## 5. Wiring / KDL

A slot is a first-class wiring node alongside `system`, declaring its contract, its allowed
occupants, and an optional initial occupant/state.

```kdl
// Each sequence cdylib is an artifact, like any system (one export per .so).
artifact "commissioning" crate="adcs-seqs" lib="adcs_commissioning" type="commissioning"
artifact "safe_mode"     crate="adcs-seqs" lib="adcs_safe_mode"     type="safe_mode"

slot "adcs" {
    input  frame="sensors"        // the port contract (a VTable contract)
    output frame="mode"
    allow occupant="commissioning"   // pre-opened & validated against the contract at build
    allow occupant="safe_mode"
    initial occupant="commissioning" state="loaded"   // optional; default Empty
}

// A slot is addressable in the graph exactly like a system instance:
connect "plant" -> "adcs"  frame="sensors"
connect "adcs"  -> "plant" frame="mode"   delayed=#true
```

- **Contract.** *Decided (§9 Q4): the contract is declared explicitly.* The `input`/`output`
  lines declare the slot's port contract (frame names → `PortDesc`s, resolved like any port).
  Every `allow`ed occupant's `DlSystem::descriptor()` is checked `compatible()` against it at
  build (`src/descriptor.rs:149`), reusing the exact validation the static/dl path uses — so
  the slot's graph wiring exists before any occupant and occupants may differ
  (forward-compatible subset).
- **Rings** are sized to the **max** `max_size`/fan-out over the allowed occupants, so any
  occupant fits the pre-allocated buffers.
- **Edges** attach to the slot's contract ports by `(slot_name, frame)`. Validation runs
  against the **contract**, not the current occupant (which may be empty/swapped). An empty
  slot simply produces no records, and downstream consumers already tolerate "nothing yet"
  (`latest()` → `None`).
- **Portability** is unchanged: occupants are ordinary `cdylib` artifacts with the `lib=`
  stem model (cli-runner.md §4.6), pre-opened at `resolve` like any dl system, packaged into
  a bundle alongside the others.

### 5.1 `Wiring` model additions

A new `SlotSpec` joins `SystemSpec` in the model (`src/wiring/model.rs:28-40`):

```rust
pub struct Wiring {
    // ...existing...
    pub slots: Vec<SlotSpec>,
}

pub struct SlotSpec {
    pub name:    String,                 // graph + telemetry instance name
    pub inputs:  Vec<String>,            // contract input frame names
    pub outputs: Vec<String>,            // contract output frame names
    pub allow:   Vec<AllowedOccupant>,   // { occupant_name, artifact_id, params: ParamSource }
    pub initial: Option<InitialOccupant>,// { occupant_name, state: Empty|Loaded|Running }
}
```

`resolve` opens each allowed occupant's `DlSystem` (once), validates it against the contract,
and registers a `SlotRunner` (a new builder method `add_slot`, the runtime-swap twin of
`add_dl_cyclic`, `src/coordinator/mod.rs:592`) carrying the pre-opened occupants and their
params blobs. `EdgeSpec` is unchanged — a slot name resolves to a `SystemHandle` like any
instance.

---

## 6. Coordinator integration

`SlotRunner` is the third `CyclicSlot` impl. The builder gains:

```rust
impl CoordinatorBuilder {
    /// Register a runtime-swappable slot: its contract descriptor (for sizing/validation),
    /// its pre-opened allowed occupants, and an optional initial occupant/state.
    pub fn add_slot(&mut self, name: impl Into<String>, contract: SlotContract,
                    allowed: Vec<(String, DlSystem, Vec<u8>)>,
                    initial: Option<InitialOccupant>) -> SystemHandle;
}
```

At `build()` the slot pushes a synthetic `SystemDescriptor` from its **contract** so the
existing `compatible()` / `WireError` validation and ring sizing/allocation run over it
unchanged (coordinator.md §2). The slot's rings are allocated by the host like any output/
private buffer; the `SlotRunner` holds them as `FswRing` arrays ready to hand any occupant.
`build()` applies the `initial` occupant (a `Load`/`Start` at startup). The control-ring
drain is added to the cycle loop ahead of the slot steps (§3).

Everything downstream — the `OutputRegistry` tap, telemetry `All`, status folding — sees a
slot's outputs like any system's, because they are registered from the contract descriptor
(dl-open.md §"Telemetry and health"). The teardown ordering that makes `DlSlot` sound
(`fsw_destroy` before the library unloads and before the ring regions free, `src/dl.rs:18-26`)
applies per occupant and per swap: **Stop/Unload/Reset destroy the occupant (and its owned
ports, releasing the ring roles, §2.3) before reusing its rings**, and slot drop destroys the
live occupant before `RingTable` frees. The `Arc<Library>` stays loaded across swaps (the
occupant is pre-opened in the allowed set); only the per-occupant state cycles.

---

## 7. Telemetry / health

Two layers, both ordinary frames on the telemetry `All` tap (no special channel):

- **Host-side `SlotStatus`** (the `SlotRunner` writes it): current `SlotState`, occupant
  name, and the allowed set — the operator's view of "what is loaded and is it running." This
  is the fsw-2 analogue of cube-sat's `SequenceChannelEvent` / `SequenceRegistry`
  (`sequencer.rs:246-258`). The `SlotRunner` knows load/run state; the occupant does not.
- **Occupant-side `SequenceStatus`** (an implicit output the `#[sequence]` macro appends after
  the user ports, then the standard health/log — the `Out` tail, `src/system/mod.rs:101-115`):
  `run_state`, the drained `progress` details, and the terminal `Outcome`
  (`Completed`/`Aborted`/`Failed`). It is bound into the wrapper state (not the future) so
  `execute` writes it each cycle (§4.2). This carries cube-sat's
  `Progress`/`Completed`/`Aborted` events (`sequencer.rs:357-374`).

Both reuse the `HealthPort` machinery for counters and the registry/instance-prefix path for
naming (`coordinator.md §5`). A sequence's per-cycle health (cycles, execute micros) comes
from the wrapper-owned `Out` tail (`end_cycle`, §4.2), exactly as a normal cyclic occupant's
does.

---

## 8. ABI impact

Deliberately minimal — the load/unload/start/stop story is built almost entirely from
existing symbols (§2.1):

- **No new lifecycle symbols.** Load = `fsw_create`+`fsw_bind_init`; Reset = `fsw_destroy`+
  `fsw_create`+`fsw_bind_init`; Stop (hard-drop, §2.1) and Unload = `fsw_destroy`; Start is
  pure host scheduling; Abort is a control frame. **Reset is therefore "destroy + create,"
  not a new `fsw_reset` entry** — the decided answer to "does reset need a new symbol."
- **New `abi::run_seq_*` helpers, same C symbols.** A sequence `.so` bypasses
  `CyclicRunner`/`run_*` (its ports live in the future, §4.2), so the macro emits the `fsw_*`
  exports over new `run_seq_create`/`run_seq_bind_init`/`run_seq_execute`/… helpers. These
  export the **identical** `SYM_*` names, `FswRing` arrays, and `FswStatus` contract, so the
  host loader/`DlSlot` is unchanged — the sequence path is new code *inside* `src/abi`, not a
  new ABI surface.
- **One new `FswStatus` code** for terminal success: a sequence whose future is `Ready` is
  not an error stop. *Decided (§9 Q6):* add `Done`; the host maps it to the
  slot-layer `SlotState::Done`. (The status word is now `Running = 0`/`Panicked = 1`/`Done = 2`
  — the lap retirement deleted `StoppedLapped` and renumbered, `src/abi/mod.rs`, dl-open.md.)
  The `Completed`/`Aborted`/`Failed` detail rides the
  `SequenceStatus` frame, **not** the status word.
- **`FSW_ABI_VERSION` bumped to 2** for the new status code (the version word guards exactly
  this, dl-open.md §"Version word"; it has since bumped again — `4` today).
- The cancel signal and all commands are **ring frames**, so they ride the existing data
  path with no ABI surface.

Re-binding an occupant over rings the host owns is already the `RawBinder`/`attach_raw`
contract (dl-open.md §"RawBinder"); it places **no new requirement on the ring** either —
reader-slot release-on-drop and writer re-acquisition already exist, so a later `Load`
re-acquires over the same region with no ring change (*decided*, §2.3, §9 Q2).

---

## 9. Scope cut for v1 & resolved decisions

**v1 scope.** Slots fixed at build (no runtime slot creation); single occupant per slot;
cyclic occupants only (sequences are cyclic by construction — the user's hard constraint);
allowed occupants pre-declared and pre-opened in wiring; control plane via frames + in-proc
handle; `Stop` = hard-drop. **Deferred / planned future work:**

- **Live/runtime path loading (prominent).** v1 loads only the pre-declared, pre-opened
  allowed set; **loading a freshly-uploaded `.so` from a path at runtime is explicitly planned
  for a later date** (Resolved Q7). "Let's start here, but we may want live reloading later" —
  the `Load`-by-name surface is designed to extend to `Load`-by-path without reshaping the
  slot model.
- Nested / dynamic slots; multi-occupant slots; an explicit resumable `Pause` distinct from
  `Stop` (Resolved Q3); an uplink command *system* with auth (v1 ships the control ring + a
  host handle; the full uplink system is future work); offline validation of a slot config
  without the `.so`s.

Every fork is resolved. Each entry states the **decision** and keeps the trade-off prose so
the rationale survives.

1. **Control plane mechanism — DECIDED: command frames drained per cycle + an in-proc
   `Coordinator::control_handle()`.** *Trade-off:* frames need a tiny `SlotCommand` frame + a
   coordinator-owned control ring and a per-cycle drain, vs a `&mut` API that would break the
   single-borrow / single-threaded cycle model and need a lock or second executor handle.
   Frames also self-telemeter the command stream. (See §3.)

2. **Ring writer/reader-slot reclamation on occupant swap — DECIDED: already works; Wave 1 is
   verification-only (no ring change).** The original premise — "the writer flag and the
   reader-table CAS slot are claimed once and never cleared" — was **inaccurate against the
   tree**: a `View` already frees its reader slot on drop (`ring/src/lib.rs:1102-1107`, tested
   `ring/src/tests.rs:90`) and a `Writer` holds no runtime claim (no `Drop`, no header flag,
   `:692-703`). Because slot swaps are strictly ordered (`fsw_destroy` runs the occupant's port
   `Drop`s before any re-`Load`, §6), re-acquisition over the same host-owned region works
   today. *Trade-off:* the cheaper-but-leaky alternatives once considered (sizing `max_readers`
   for a bounded reload count; a host-side persistent View + copy-in) are moot — there is no
   ring change to make. The release-on-drop the swap needs is the existing `View`/`Writer` drop
   behavior; Wave 1 just proves it with swap re-acquire tests (incl. a raw attach round-trip
   under Miri). Crash-slot reclamation (owner dies without `Drop`) remains future cross-process
   work (the slot-epoch at `ring/src/lib.rs:727` is reserved for it). (See §2.3.)

3. **`Stop` semantics — DECIDED: hard-drop (cube-sat parity), NOT pause.** `Stop` drops the
   occupant's future (Running→Loaded with the future gone); re-running requires `Reset`.
   *Trade-off:* matches the operator muscle-memory of `sequencer.rs:293-302` and composes with
   Q2 (dropping the future drops its owned ports, releasing the ring roles, §2.1/§4.2) — at the
   cost of not being directly resumable. A separate explicit **`Pause`** (stop polling, keep
   the future) is possible later work; v1 is hard-drop only. (See §2.1.)

4. **Slot contract — DECIDED: explicit KDL `input`/`output`.** Declared explicitly and each
   allowed occupant validated `compatible()` against it. *Trade-off:* more verbose than
   inferring from the allowed set, but lets the slot's graph wiring exist before any occupant
   and lets occupants differ (forward-compatible subset); inferring would force all occupants
   to share one descriptor and couple wiring to the `.so`s being present at parse. (See §5.)

5. **Decorator author API — DECIDED: direct port params, owned by the future; ambient free
   functions.** Ports are written as plain `Input<T>`/`Output<T>` parameters the macro binds
   once and moves into the `'static` future; `wait`/`progress`/`aborted()` are free functions
   backed by a task-local `SeqClock` (with an opt-in explicit `seq: Seq` handle).
   `#[sequence]` defaults `NAME` to the fn name; `#[sequence(name="…")]` overrides. *Trade-off:*
   this **replaces** the earlier typed-`SeqCtx`-bundle / refresh-cell design — it is strictly
   simpler and safer (the future owns its ports, so there is no per-cycle re-borrow and no
   `unsafe` port deref), at the cost of the macro scanning the signature to synthesize the
   descriptor and emitting its own `run_seq_*` lifecycle instead of reusing
   `CyclicRunner`/`export_system!`. (See §4.)

6. **Terminal completion — DECIDED: new `FswStatus::Done` (ABI v2; renumbered to `2` at v4,
   §8), outcome in the frame,
   a slot-layer `SlotState`.** The host needs only the terminal bit; `Completed`/`Aborted`/
   `Failed` rides the `SequenceStatus` frame. A **new** slot-layer `SlotState` is introduced
   rather than overloading the existing 2-variant `SlotState`/`StopReason`. *Trade-off:* one
   ABI version bump and a parallel state enum, vs stuffing outcome into status codes
   (`Done`+`Aborted`+`Failed` = three new codes) and widening the existing enum (which would
   touch the static/dl status-frame path). (See §2 / §8.)

7. **Allowed occupants — DECIDED: pre-declared & pre-opened in wiring for v1; runtime path
   loading is planned future work.** `Load` selects by name from the build-time validated/sized
   set. *Trade-off:* v1 loses "upload a brand-new `.so` and load it live" — but that is the
   **explicitly planned next step** ("let's start here, but we may want live reloading at a
   later date"), which needs live ring re-sizing and live validation; pre-declaring keeps every
   validation/sizing guarantee and avoids unvalidated runtime `dlopen` in the interim. (See §3,
   the scope note above, and §5.)

8. **Future/ports soundness — RESOLVED by the Q5 owned-ports model.** Because the ports are
   owned by the future (Q5), there is no per-cycle port re-borrow and **no raw port pointers**:
   the only state the cycle refreshes is the `SeqClock` cell (the coordinator `now` + cancel +
   progress), reached through a task-local that is live only during the synchronous,
   single-threaded poll. The soundness surface shrank from "raw `*mut` to coordinator-owned
   bundles, refreshed each poll" to "a task-local clock cell" — no `unsafe` port deref remains.
   (See §4.3.)
