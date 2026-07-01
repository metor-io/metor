# Implementation plan — message wiring parity (`message-wiring`)

> **STATUS: IMPLEMENTED (all 9 WPs landed, each a green commit).** Whole workspace builds; all
> `metor-fsw-2` tests + the `adcs-fsw2` example (sequences/closed-loop/bundle) pass;
> `--no-default-features` clean; `tests/slot_integration.rs` unchanged. **Deviations from the plan,
> all confirmed with the reviewer:**
> - **WP5 (KDL resolution):** message endpoints resolve by matching the port's display name
>   (`msg_name::<M>()`), **not** by hashing the KDL name to a `PacketId` — the wkt sequence `Msg`s
>   hand-assign their ids and don't derive `Schema`, so no name hashes to them.
> - **WP7 (command reframe):** realized as **coordinator-collected fan-out** (each slot's host-side
>   `MsgIn<SequenceCommand>` views every command producer — the reserved `control_handle` ring +
>   every `SequenceCommand` output found by type), not the edge-based "slots as wired consumers"
>   form. Per-slot targeting is the command's `channel_id`, so no slot descriptor port, no
>   synthesized edges, no synthetic coordinator system. `CommandOut<M>` makes the uplink a fully
>   normal producer; `drain_command_bus`/`command_sources`/`CyclicSlot::command`/`command_out` are
>   gone.
> - **WP8 (uplink subscription):** derived from the uplink's **declared message-output ports**
>   (no out-edges exist under WP7's approach), via `RecvTransport::subscribe` — simpler than the
>   planned edge-derived `out_msg_ids` + Binder threading.



Design: `docs/message-wiring.md` (approved; §9 decisions 1-7 locked, §10 open questions Q1-Q10 all
resolved by the reviewer 2026-06-30 — **Q7 = implicit fan-out default + explicit `connect … msg=`
override; Q8 = keep command channels OFF the downlink via a `telemetered` bool**). Read that doc
first; this plan only sequences its implementation. Context: `design.md`, `docs/messages.md`
(the shipped message channel this builds on).

**What we build.** Messages get full wiring parity with component frames: a kind-tagged `PortDesc`,
typed `MsgOut<M>`/`MsgIn<M>` ports, message **edges** (`msg=` KDL / `connect_msg` builder), a
reusable `AllOutputs` receive-all tap, an edge-derived uplink subscription, and — the payoff — the
deletion of the coordinator's entire hardcoded command bus in favour of ordinary message edges.
**Zero ABI change** (`docs/message-wiring.md` §7): every change is host-side; the occupant `fsw_*`
surface, `FSW_ABI_VERSION`, and `RawBinder` are untouched. **The whole surface is ungated** (no
`kdl` feature) except the KDL front-end in WP5 — so `cargo build -p metor-fsw-2
--no-default-features` must stay green at every boundary.

> **Verified against the current tree (line numbers below are current, not the design's).** Eight
> subsystems were re-read; the design's file:line citations are mostly stale. Corrections that
> change the plan are called out inline as **[VERIFY]**. The most consequential:
> - `PortDesc`'s name field is **`frame_name`** (`descriptor.rs:42`), not `name`.
> - `MsgOut::bind` **already exists** (`message.rs:115`) as a seam; `MsgOut` is still type-erased
>   (`emit<M>` at `:95`). `MsgIn<M>` is already typed but has **no** `bind`/`descriptor`.
> - The host `Binder` has **no system-`id` field** (`binder.rs:78-88` is just two `slice::Iter`
>   cursors + registries + the `command_rings` collector) — so `out_msg_ids()` (WP8) must thread the
>   current system's out-edge id list into `Binder::new`, a new field. The design assumed the id was
>   already there (`design §5.2`); it is not.
> - `SlotRunner`'s command handler is the `CyclicSlot::command` trait method (`slot.rs:515-527`),
>   **not** an `apply_command`; the reframe deletes the trait method and inlines its body.
> - The derive macro lives in a **sibling crate** `../metor-fsw/macros/src/system.rs` (not
>   `metor-fsw-2/macros`), and is confirmed **type-blind** (`descs.push(<#ty>::descriptor())` at
>   `:35`, `<#ty>::bind(src)` at `:52`) — **untouched by this refactor**, as the design predicts.

## Dependency graph (strict edges →)

```
WP1 (kind-tagged PortDesc + PortRef)
  └▶ WP2 (typed MsgOut<M>)
       └▶ WP3 (typed multi-view MsgIn<M> + binder fan-in)
            └▶ WP4 (message edges in build() + low-level connect_msg)
                 ├▶ WP5 (wiring front-ends: KDL msg= / WiringBuilder::connect_msg)
                 └▶ WP6 (AllOutputs + telemetered flag + n_reg self-derive)
                      └▶ WP7 (command-plane reframe — the big one)
                           └▶ WP8 (uplink out_msg_ids + subscribe)
                                └▶ WP9 (test migration + final gate)
```

**Critical path: WP1 → WP2 → WP3 → WP4 → WP6 → WP7 → WP8.** WP5 (KDL/builder front-ends) is
independent of WP6/WP7 after WP4 and can land in parallel or be deferred to just before WP9. Build +
test after every WP; commit at each green boundary (task-boundary commit convention).

---

## WP1 — Kind-tagged `PortDesc` + `PortId` + `PortRef` (frame-only, no behaviour change)

**Goal.** Widen `PortDesc` to the kind-tagged shape (`PortId` + `PortKind`) and re-key `PortRef` on
`PortId`, with **only the `Frame` variant populated** — a pure representational change that keeps
every existing test green and every behaviour identical. This is the wide-but-mechanical blast-radius
step; landing it alone makes every later WP a small local edit.

**Files / functions (verified current lines).**
- `src/descriptor.rs`:
  - `PortDesc` struct `:36-62` (fields `frame_id:38`, `frame_name:42`, `vtable:46`, `max_size:48`,
    `rate_hint:50`, `announce:61`); hand-written `Debug` `:64-76`.
  - `PortDesc::of<F>` `:90-101`, `of_at<F>` `:104-109`; `announce_of<F>` `:82-86`; `AnnounceFn` type
    `:27`.
  - `compatible` `:149-159`; `realize_set` `:134-143`.
  - `SystemDescriptor` `:125-131` (`inputs:129`, `outputs:130`).
- `src/coordinator/mod.rs`: `PortRef` `:117-120`, `PortRef::new<F>` `:124`.
- Readers of `port.frame_id` / `port.vtable` / `port.announce` that must migrate to `match p.id` /
  `p.kind`:
  - `src/wiring/mod.rs:1657` (`p.frame_id == frame_id` in `resolve_endpoint`).
  - `src/coordinator/mod.rs`: the build port lookups + ring loop `:880-904`, the `RegistryEntry`
    construction (reads `port.vtable`/`announce` when freezing the `OutputRegistry`, near `:1054`),
    and the slot-aux frame-id comparisons.
  - `src/port.rs:74,169` (`Output/Input::descriptor` call `PortDesc::of` — **no change**, they route
    through the constructor).
  - `src/system/mod.rs:111-112` (`Out::descriptors` pushes `PortDesc::of::<SystemHealth/Log>()` —
    **no change**).

**Concrete changes.**
```rust
// src/descriptor.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortId { Frame(ComponentId), Msg(PacketId) }

pub enum PortKind {
    Frame { vtable: VTable, announce: AnnounceFn },
    Message { telemetered: bool },   // WP6 populates telemetered; WP1 may stub it as always-true
    ReceiveAll,
}

pub struct PortDesc {
    pub id: PortId,           // was frame_id
    pub name: &'static str,   // rename frame_name → name (F::NAME / M::SCHEMA.name / "")
    pub max_size: usize,
    pub rate_hint: Option<Hz>,
    pub kind: PortKind,       // was the loose vtable/announce fields
}
impl PortDesc {
    pub fn of<F: Frame>() -> Self { /* id: Frame(F::FRAME_ID), kind: Frame{vtable, announce} */ }
    pub fn of_at<F: Frame>(rate: Hz) -> Self { Self { rate_hint: Some(rate), ..Self::of::<F>() } }
    // msg::<M>() and receive_all() are ADDED but UNUSED until WP2/WP6 — add now to keep the enum total.
}
```
- `compatible` gets its kind-match skeleton now (frame arm = today's `frame_id`-equality +
  `realize_set` subset; `Message` and mismatched arms return the honest result but are dead until
  edges exist):
  ```rust
  pub fn compatible(p: &PortDesc, c: &PortDesc) -> bool {
      match (&p.kind, &c.kind) {
          (Frame{vtable: pv,..}, Frame{vtable: cv,..}) => p.id == c.id && subset(cv, pv),
          (Message{..}, Message{..}) => p.id == c.id,   // WP4 exercises this
          _ => false,
      }
  }
  ```
- `PortRef` re-keys: `pub struct PortRef { pub system: SystemHandle, pub port: PortId }`;
  `new<F>` → `port: PortId::Frame(F::FRAME_ID)`; add `msg<M>` → `PortId::Msg(M::ID)` (unused until
  WP4).
- The hand-written `PortDesc` `Debug` impl (`:64-76`) updates to match the kind.

**How it stays green.** No new port types, no edges, no behaviour: `PortDesc::of` still produces the
same data, just nested under `PortKind::Frame`. The derive macro is type-blind and untouched
(`../metor-fsw/macros/src/system.rs:35,52`). All migrations are `p.frame_id` → `match p.id { Frame(f)
=> f, _ => unreachable!() }` (safe: only frame ports exist) and `p.vtable`/`p.announce` → destructure
`PortKind::Frame`. `Msg`/`ReceiveAll` arms are added but never constructed yet.

**Tests.** No new tests. **All existing tests must pass unchanged** — this is the invariant that
proves the representational change is behaviour-neutral. If any test reads `PortDesc.frame_id`
directly, migrate it mechanically.

**Dependencies.** none.

**Risk / rollback.** Blast radius is wide but shallow (mechanical field access). Rollback is a clean
revert of one commit; nothing downstream depends on it yet. The only trap is missing a `p.frame_id`
reader — `cargo build` catches every one (the field is gone), so there is no silent-miss hazard.

---

## WP2 — Typed `MsgOut<M>` + `descriptor()`

**Goal.** Make `MsgOut` typed on one `M` (parity with `Output<F>`), give it a `descriptor()`
returning `PortDesc::msg::<M>()`, and migrate all call sites + the two type-erasure tests. This is
what lets a `MsgOut<M>` drop into a bundle in WP4.

**Files / functions (verified).**
- `src/message.rs`: `MsgOut` struct `:68-79` (type-erased today), `MsgOut::new` `:83`,
  `emit<M>` `:95-101`, `MsgOut::bind` `:104-119` (**already exists**). Constants `MAX_MSG_BYTES:32`,
  `MSG_DEPTH:37`, `msg_capacity:43`, `split_record:51`. Unit tests `:216-350`, the two-type erasure
  test `msg_out_emits_and_round_trips` `:230-291` (emits `SequenceRegistry` then `SequenceCommand`
  through one port, `:258,265`).
- Call sites emitting through a `MsgOut`:
  - `src/coordinator/mod.rs`: `control_handle` `:1554-1555` (`MsgOut<BoxBacking>` →
    `MsgOut<SequenceCommand>`); boot `seq_registry_out` `:1025,1035-1037` (→ `MsgOut<SequenceRegistry>`);
    `SlotAux.events` `:422-428` (→ `MsgOut<SequenceChannelEvent>`).
  - `src/coordinator/slot.rs`: `SlotRunner.events` `:201`, `SlotRunner::new` param `:238`, `emit_event`
    `:296-301` (all emit `SequenceChannelEvent`).
  - `src/binder.rs`: `command_out` `:158,216-224` returns `MsgOut<BoxBacking>` → `MsgOut<SequenceCommand>`.
  - `src/telemetry/mod.rs`: `UplinkPorts.commands` `:410` (`MsgOut<BoxBacking>` → `MsgOut<SequenceCommand>`),
    bound via `command_out` `:419-427`; `UplinkSystem::run` emit `:485`.
- `src/telemetry/tests.rs:424-444` — bare `MsgOut` emitting two Msg types (task-cited) — split.

**Concrete changes.**
```rust
pub struct MsgOut<M, B = BoxBacking, WD = NoWake, WS = NoWake> { writer, scratch, _m: PhantomData<fn()->M> }
impl<M: Msg, ..> MsgOut<M, ..> {
    pub fn emit(&mut self, msg: &M) -> Result<(), WriteError> { /* body as today, M fixed */ }
    pub fn descriptor() -> PortDesc { PortDesc::msg::<M>() }   // NEW — the twin of Output::<F>::descriptor
}
```
- `PortDesc::msg::<M>()` (added-but-unused in WP1) is now real: `id: PortId::Msg(M::ID)`,
  `name: msg_name::<M>()` (`M::SCHEMA.name`, `../metor-proto/src/types.rs:594-595`),
  `max_size: MAX_MSG_BYTES`, `kind: PortKind::Message { telemetered: true }`.
- `MsgOut::bind` (`:115`) stays; its signature loses the per-call `M` (now on the type).
- Migrate every call site above to the typed spelling. Each is single-type already (`docs/message-wiring.md`
  §2.1 confirmed: the `"sequences"` channel is already two separate single-type rings), so this is a
  type annotation, not a restructure.

**How it stays green.** After the migration compiles, behaviour is identical (one `M` per port was
already the de-facto usage). The type-erasure ergonomic is deliberately dropped (locked decision 1).

**Tests.**
- **Migrate** `src/message.rs` `msg_out_emits_and_round_trips` `:230-291`: split the one type-erased
  `MsgOut` into a `MsgOut<SequenceRegistry>` and a `MsgOut<SequenceCommand>` on two rings; keep the
  `assert_ne!(SequenceCommand::ID, SequenceRegistry::ID)` (`:283`) and each round-trip.
- **Migrate** `src/telemetry/tests.rs:424-444` similarly.
- **Add** a trivial `MsgOut::<E>::descriptor()` assertion (`id == PortId::Msg(E::ID)`, `telemetered`).

**Dependencies.** WP1 (`PortId::Msg`, `PortKind::Message`, `PortDesc::msg`).

**Risk / rollback.** Low. Contained to `message.rs` + a handful of typed annotations. Revert is clean.

---

## WP3 — Typed multi-view `MsgIn<M>` + binder fan-in (`BoundInput`, `next_input_fanin`)

**Goal.** Give `MsgIn<M>` a `descriptor()`/`bind()` and make it hold **K views** (one per producer
edge), and teach the binder to hand a message input its fan-in list positionally — without yet wiring
any message edges. Migrate the existing single-view `MsgIn` consumer (`command_sources`) to the
multi-view shape at K=1 so the build stays green.

**Files / functions (verified).**
- `src/message.rs`: `MsgIn<M>` struct `:137-148` (single `view` field), `MsgIn::new` `:159`,
  `drain` `:172-179` (id-filter `id == M::ID` at `:177`). No `bind`/`descriptor` today.
- `src/binder.rs`: `RingSource` trait `:115-161` (`next_output:121`, `next_input:127`,
  host-only `output_registry:138`/`message_registry:147`/`command_out:158`); host `Binder` `:78-88`
  (cursors `outputs:79`/`inputs:80`); `next_input` host impl `:184-198`; `BoundPort` `:39-43`;
  `BindPorts` trait `:232-235`.
- `src/coordinator/mod.rs`: `command_sources: Vec<MsgIn<SequenceCommand>>` `:1465`, built `:1258-1276`
  (each `MsgIn::new(view)` over one ring); the per-system `BoundPort` layout `:1214-1231` +
  `Binder::new` call `:1233-1239`.

**Concrete changes.**
```rust
// src/message.rs — MsgIn holds K views
pub struct MsgIn<M, B = BoxBacking, RD = NoWake, RS = NoWake> { views: Vec<View<B,RD,RS>>, scratch, _marker }
impl<M: Msg + DeserializeOwned, ..> MsgIn<M, ..> {
    pub fn drain(&mut self, mut f: impl FnMut(M)) -> Result<(), LapError> {
        for v in &mut self.views { /* today's per-view drain, id-filter kept (Q10) */ }
    }
    pub fn descriptor() -> PortDesc { PortDesc::msg::<M>() }       // same key as MsgOut<M>
    pub fn bind<S: RingSource<B=B>>(src: &mut S) -> Self {          // NEW — calls next_input_fanin
        let rings = src.next_input_fanin::<RD, RS>();
        Self { views: rings.into_iter().map(|(r,rd,rs)| r.view(rd, rs)).collect(), .. }
    }
    pub fn new(view) -> Self { Self { views: vec![view], .. } }     // keep for the K=1 migration
}
```
```rust
// src/binder.rs
pub enum BoundInput { One(BoundPort), Many(Vec<BoundPort>) }
trait RingSource {
    // next_output / next_input unchanged (frame ports)
    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer<Self::B>, RD, RS)> { Vec::new() }
}
```
- Host `Binder.inputs` becomes `slice::Iter<'a, BoundInput>` (was `BoundPort`). `next_input` pops a
  `BoundInput::One`; `next_input_fanin` pops a `BoundInput::Many` (or `One` treated as a 1-element
  list). Frame ports call `next_input`; `MsgIn::bind` calls `next_input_fanin`. Positional alignment
  holds: the coordinator lays out one `BoundInput` per input **port** in `descriptors()` order.
- The coordinator's per-system input layout (`:1214-1231`) builds `Vec<BoundInput>`: `One` for frame
  inputs (unchanged), `Many` for message inputs (empty until WP4 resolves fan-in).
- Migrate `command_sources` (`:1258-1276`) to `MsgIn::new(view)` (still valid — K=1) so it compiles;
  it is deleted wholesale in WP7 anyway.

**How it stays green.** No message edges yet, so every `MsgIn` in the tree is the coordinator-owned
`command_sources` at K=1 via `MsgIn::new`. The binder's `next_input_fanin` has a `Vec::new` default
(non-host sources) and a host impl that pops `BoundInput::Many`. Frame binding is byte-identical.

**Tests.** No new integration tests (nothing wired). **Add** a `message.rs` unit test that
`drain`s a `MsgIn<M>` hand-built with **two** views over two rings and asserts every record from both
is delivered (proves the multi-view loop before WP4 relies on it). Existing `MsgIn` drain/id-filter
test `:297-340` stays green.

**Dependencies.** WP2 (`PortDesc::msg`, typed ports).

**Risk / rollback.** Medium — it changes the binder's input cursor type (`BoundPort` →
`BoundInput`), touched by every system's bind. Mitigation: the `One` variant makes frame binding a
pass-through; keep the change purely additive (frame path unchanged) and lean on `cargo build` to
find every layout site. Rollback: revert; `command_sources` returns to single-view `MsgIn`.

---

## WP4 — Message edges in `build()` + low-level `connect_msg`

**Goal.** Make a message connection an ordinary **edge**: `EdgeKind` on the edge model,
`CoordinatorBuilder::connect_msg`/`connect_msg_delayed`, a kind-split `build()` edge pass (many-to-many
message inputs, optional inputs, id-equality compatibility, cycle exclusion, fan-out sizing), message
output ring allocation, and message input fan-in binding. After this WP a system can declare
`MsgOut<M>`/`MsgIn<M>` bundle ports and wire them via the typed builder.

**Files / functions (verified).**
- `src/wiring/model.rs`: `EdgeSpec` `:202-215` (fields `from/out/to/in_/delayed`; **no** `kind`).
- `src/wiring/mod.rs`: internal `Edge` `:645-652`.
- `src/coordinator/mod.rs`: `connect`/`connect_delayed`/`push_edge` `:743-786`; `build()` edge pass —
  `cons_edge` map `:796,827-828`, `DoubleConnect` `:827-835`, `compatible` call `:817-820`,
  `forward_adj` `:799,836-837`, `FeedbackCycle` `:842-846`, `UnconnectedInput` `:849-858`, `fan_out`
  map `:861-864`; ring-alloc loop `:880-904` (budget `:883-884`, `capacity_for` `:886`); per-system
  `BoundInput`/`Binder::new` `:1214-1249`.

**Concrete changes.**
- Add `EdgeKind { Frame, Msg }` and carry it on the internal `Edge` and on the `push_edge` path (the
  low-level `CoordinatorBuilder` edge already has `delayed`; add `kind`). `EdgeSpec` gains it in WP5
  when the front-ends grow (keep WP4 to the low-level builder + `Edge`).
- `CoordinatorBuilder::connect_msg(PortRef, PortRef)` / `connect_msg_delayed(...)` beside
  `connect`/`connect_delayed` (`:743,754`) — push an edge with `kind: Msg`. Typed path:
  `PortRef::msg::<M>(handle)` (added in WP1).
- **Kind-split `build()` edge pass** (`docs/message-wiring.md` §3.2). Split by `edge.kind`:

  | check (frame-only today) | Frame edges | Msg edges |
  |---|---|---|
  | `cons_edge` scalar insert / `DoubleConnect` `:827-835` | keep | **use `msg_cons_edges: HashMap<(cons,in_idx), Vec<(prod,out_idx)>>`**, no double-connect |
  | `compatible` `:817-820` | subset (WP1 frame arm) | **id-equality** (WP1 message arm) |
  | `UnconnectedInput` `:849-858` | keep | **drop** (zero producers legal) |
  | `forward_adj` push `:836-837` | keep | **exclude** (add `&& edge.kind == Frame`, §3.6) |
  | `fan_out` map `:861-864` | keep | **keep, generalized** to message outputs |

- **Message output ring allocation.** Edge-wired message output ports go through the ring-alloc loop
  (`:880-904`) with message sizing (`msg_capacity(MAX_MSG_BYTES, MSG_DEPTH)`) and the **fan-out
  reader budget** `fan_out + n_reg + READER_SLACK` (§3.3). **[VERIFY]** the coordinator-minted event/
  registry rings (`:985,1024`) are *not* edge ports — they keep today's `n_reg + READER_SLACK`
  sizing; only ports that appear in the descriptor + participate in edges get the fan-out term.
- **Message input fan-in binding.** For each message input port, resolve its `Vec<(prod,out_idx)>`
  from `msg_cons_edges`, collect those producer rings, and lay them out as `BoundInput::Many` (WP3)
  so `MsgIn::bind` claims all K. Async message consumers get a `MsgCopyIn { upstreams: Vec<View>,
  writer }` (the message twin of `CopyIn` `:343-347`) into a private `Overwrite` merge ring, run at
  cycle tail like frame copy-in (`docs/message-wiring.md` §3.3); cyclic consumers get direct
  multi-view.

**How it stays green.** Frame edges keep the scalar `cons_edge` + exactly-once/no-double/unconnected
guarantees byte-for-byte. Message edges are a parallel path only reachable once a system declares
message ports and calls `connect_msg` — which nothing in the shipped tree does yet, so all existing
tests are untouched. The new path is exercised only by this WP's new tests.

**Tests (new — `docs/message-wiring.md` §7.3).**
- `msg_edge_two_cyclic_systems`: a producer system with `MsgOut<E>`, a consumer with `MsgIn<E>`,
  wired by `connect_msg`; assert every emitted `E` drains at the consumer the same cycle.
- `msg_fanin_two_emitters_one_consumer`: two producers `MsgOut<E>` → one `MsgIn<E>` (cyclic
  multi-view); assert both producers' records arrive, no `DoubleConnect`.
- Negative: `MsgOut<A>` → `MsgIn<B>` (`A::ID != B::ID`) is an `Incompatible` build error; an
  unconnected `MsgIn<E>` builds fine and drains nothing.

**Dependencies.** WP3 (multi-view `MsgIn`, `BoundInput`, `next_input_fanin`).

**Risk / rollback.** Medium-high — the `build()` edge pass is the graph's heart. Mitigation: guard
every new branch behind `edge.kind == Msg` so the frame path is provably unchanged; add the
message-only tests in the same WP so the new path is exercised the moment it exists. Rollback: revert;
frame edges are unaffected.

---

## WP5 — Wiring front-ends: KDL `msg=` + `WiringBuilder::connect_msg` + `UnknownMsg`

**Goal.** Expose message edges in both front-ends, byte-equivalently. Independent of WP6/WP7; can
land any time after WP4.

**Files / functions (verified).**
- `src/wiring/model.rs`: `EdgeSpec` `:202-215` — add `kind: EdgeKind` (default `Frame` for serde
  back-compat).
- `src/wiring/mod.rs`: `parse_edge` `:1590-1634` (`frame=` read `:1626`, copied into `out`+`in_`
  `:1630-1631`); `resolve_endpoint` `:1639-1669` (`ComponentId::new(frame)` `:1652`, `p.frame_id`
  match `:1657`, `UnknownFrame` `:1658-1664`); edges pass `:804-816`; `UnknownFrame` LoadError
  `:191-200`; KDL `connect` collect `:717-731`.
- `src/wiring/builder.rs`: `connect` `:128-143`, `connect_delayed` `:147-162`.

**Concrete changes.**
- KDL: `parse_edge` accepts `msg="SequenceCommand"` beside `frame=` (exactly one required); store the
  Msg name into `out`+`in_` (like `frame=`) and set `EdgeSpec.kind = Msg` (**[Q3 resolved:
  explicit kind discriminant]**). A precise diagnostic falls out.
- `resolve_endpoint` branches on `EdgeKind`: for `Msg`, **match the name against the instance's
  `PortId::Msg` ports by `port.name`** — i.e. `ports.iter().find(|p| matches!(p.id, PortId::Msg(_))
  && p.name == msg)` → use that port's `p.id`. **[VERIFY — corrected in WP2]** the design/plan
  originally proposed hashing the name to a `PacketId` (`fnv1a_hash_str_16_xor`), on the assumption
  `M::ID == fnv1a16(M::SCHEMA.name)`. That is **false** for the wkt sequence types: `SequenceCommand`
  / `SequenceRegistry` / `SequenceChannelEvent` hand-assign `Msg::ID` (`[224,41/42/43]`,
  `../metor-proto/wkt/src/msgs.rs:685,730,758`) and do **not** derive `Schema`, so no name hashes to
  their id. Matching by `port.name` (the `msg_name::<M>()` = type-name string set in WP2's
  `PortDesc::msg`) is the robust resolution and needs no hash. Emit a new
  `LoadError::UnknownMsg { instance, msg, .. }` (parallel to `UnknownFrame` `:191-200`; **[VERIFY]**
  `UnknownMsg` does **not** exist yet — add it). The edges pass (`:804-816`) routes `Msg` edges to
  `builder.connect_msg` / `connect_msg_delayed`.
- `WiringBuilder::connect_msg` / `connect_msg_delayed` (`builder.rs`) push `EdgeSpec { kind: Msg }`.

**How it stays green.** Frame edges default `kind: Frame`; the serde default keeps existing `Wiring`
docs deserializing unchanged. No message edges exist in shipped missions.

**Tests (new).** `msg_kdl_round_trip`: a two-system KDL doc with `connect "a" -> "b"
msg="SequenceChannelEvent"`, parse → resolve → build; assert the edge resolves to the right
`PacketId` and a typo yields `UnknownMsg`. `--no-default-features` note: KDL parsing is `kdl`-gated,
so gate this test with `#[cfg(feature = "kdl")]`; the builder path (`connect_msg`) is ungated and
tested unconditionally.

**Dependencies.** WP4.

**Risk / rollback.** Low. Additive front-end surface. Revert is clean.

---

## WP6 — `AllOutputs` receive-all port + telemetry re-express + `telemetered` flag + self-derived `n_reg`

**Goal.** Introduce the reusable `AllOutputs` tap (`PortKind::ReceiveAll`), re-express the telemetry
downlink on it, make `n_registry_consumers` self-derived by counting `ReceiveAll` ports (deleting the
manual bump), and land the `telemetered` bool that keeps command channels off the downlink. Also
introduce the `CommandOut<M>` opt-out newtype (used in WP7/WP8).

**Files / functions (verified).**
- `src/telemetry/mod.rs`: `TelemetryPorts` `:562-584` (`bind` pulls `output_registry`+`message_registry`
  `:578-583`); `TelemetrySystem` `:628-645`, `init` reads `output.registry`/`output.messages`
  `:675-677`; message tap resolution `:713-728`, drain `:806-832`.
- `src/coordinator/mod.rs`: `n_registry_consumers` field `:522` (init `:534`), the manual `+= 1`
  `:600` in `add_telemetry` `:596-601`, read `let n_reg = ..` `:876` (used `:884,910,961,985,1024`).
- `src/registry.rs`: `MessageEntry` `:108-122` (fields `key/instance/channel/ring`; **no
  `telemetered`**); `MessageRegistry` `:135-179`.
- `src/descriptor.rs`: `PortKind::ReceiveAll` / `PortKind::Message { telemetered }` (WP1).

**Concrete changes.**
- `AllOutputs` port (new, `src/registry.rs` or a new `src/tap.rs`):
  ```rust
  pub struct AllOutputs { pub outputs: Arc<OutputRegistry>, pub messages: Arc<MessageRegistry> }
  impl AllOutputs {
      pub fn descriptor() -> PortDesc { PortDesc::receive_all() }   // kind = ReceiveAll
      pub fn bind<S: RingSource>(src: &mut S) -> Self {
          Self { outputs: src.output_registry(), messages: src.message_registry() }
      }
  }
  ```
  It rides the derive with **no macro change** (satisfies the `descriptor()`/`bind()` contract).
- `TelemetryPorts` becomes `{ all: AllOutputs }`; `bind` → `Self { all: AllOutputs::bind(src) }`;
  `init` reads `output.all.outputs` / `output.all.messages`. The bespoke double-pull (`:578-583`) is
  deleted; the `output_registry()`/`message_registry()` capabilities stay (now called by
  `AllOutputs::bind`).
- **`build()` treatment of `ReceiveAll`** (§4): skip it in the ring-alloc loop (`:880-904`) and the
  `BoundInput` layout (allocate no ring → the positional cursor never hands one out →
  `AllOutputs::bind` pops nothing, keeping alignment); it is never a valid edge endpoint.
- **Self-derived `n_reg`.** Delete the manual `n_registry_consumers += 1` (`:600`). Compute
  `n_reg` in `build()` by **counting `ReceiveAll` `PortDesc`s across all systems' descriptors**. All
  existing `n_reg` uses (`:884,910,961,985,1024`) read the derived value. `add_telemetry` becomes a
  plain `add_cyclic_named("telemetry", …)` (`:601`) with no bump.
- **`telemetered` flag.** `MessageEntry` gains `telemetered: bool` (`registry.rs:108-122`), populated
  from the message port's `PortKind::Message { telemetered }`. The telemetry message tap
  (`:713-728` resolution + `:806-832` drain) **skips** entries with `telemetered == false`. Frame
  outputs are always telemetered.
- **`CommandOut<M>` opt-out newtype (recommended spelling).** Introduce
  ```rust
  pub struct CommandOut<M>(MsgOut<M>);
  impl<M: Msg> CommandOut<M> {
      pub fn descriptor() -> PortDesc { PortDesc::msg_untelemetered::<M>() } // telemetered=false
      pub fn bind<S: RingSource>(src: &mut S) -> Self { Self(MsgOut::bind(src)) }
  }
  impl<M> Deref/DerefMut for CommandOut<M> -> MsgOut<M>   // emit through it unchanged
  ```
  **Recommendation: the newtype over a const or a builder flag.** Rationale: (a) it is a type-level
  marker the type-blind derive picks up for free via `descriptor()`/`bind()` — zero macro change;
  (b) it is self-documenting at the port declaration site (`commands: CommandOut<SequenceCommand>`);
  (c) it cannot be toggled at runtime, so "is this channel telemetered" is a compile-time property of
  the port, not a mutable flag. Unused until WP7/WP8.

**How it stays green.** Telemetry behaviour is identical: `AllOutputs` binds the same two registries;
the derived `n_reg` equals `1` for any mission with one telemetry system (one `ReceiveAll` port), so
every ring's reader budget is unchanged. `telemetered` defaults `true` for all current message
channels, so nothing is newly skipped.

**Tests (new — §7.3).** `all_outputs_on_non_telemetry_system`: a plain cyclic system declaring
`AllOutputs` in its bundle receives a view on every output + telemetered message ring, and the
derived `n_reg` grows to reserve it a slot on each. **Add** an assertion that a `telemetered == false`
message channel is present as a wired port but absent from the tap. Existing telemetry tests
(latest-wins snapshot + `MsgHandOff` FIFO) stay green.

**Dependencies.** WP4 (message ports/edges exist to tap and size).

**Risk / rollback.** Medium — `n_reg` becoming self-derived is a subtle sizing change. Mitigation:
assert the derived count equals the old manual count (1 per telemetry system) in a build test before
deleting the `+= 1`. Rollback: restore the manual bump; `AllOutputs` can coexist with it during a
bridge if needed.

---

## WP7 — Command-plane reframe (the big one)

**Goal.** Delete the coordinator's hardcoded command bus and express command delivery as ordinary
message edges: slots become host-side `MsgIn<SequenceCommand>` consumers; the coordinator becomes a
reserved hand-registered `coordinator.commands` producer keeping `control_handle()`; the uplink emits
via a normal `CommandOut<SequenceCommand>` output; an implicit fan-out wires every command emitter to
every slot. **`tests/slot_integration.rs` stays source-unchanged** (the acceptance test for Q7).

**Files / functions to DELETE (verified current lines).**

| Deleted | file:line | Replaced by |
|---|---|---|
| `drain_command_bus` fn | `coordinator/mod.rs:1693-1703` | per-slot `MsgIn` drain in `SlotRunner::step` |
| `self.drain_command_bus()` call | `coordinator/mod.rs:1614` | — (slots self-drain) |
| `CyclicSlot::command` trait method + no-op | `system/mod.rs` default + `slot.rs:515-527` impl | inlined into `SlotRunner::step` |
| `command_sources` field + build | `coordinator/mod.rs:1465,1258-1276,1306` | the slots' own inputs |
| ad-hoc `command_ring` alloc + field | `coordinator/mod.rs:1045-1051,1460,1305` | reserved `coordinator.commands` producer |
| `RingSource::command_out` + host impl | `binder.rs:158-160,216-224` | normal `CommandOut<M>` / `MsgOut<M>` output |
| `command_rings` collector + `Binder` field + `Binder::new` param | `binder.rs:87,96,103,222` + call `coordinator/mod.rs:1233-1239` | edge fan-in from wired producers |
| `UplinkPorts::bind` `command_out()` pull | `telemetry/mod.rs:419-427` | `CommandOut<SequenceCommand>::bind` |

**Concrete changes.**
- **Slots as `MsgIn<SequenceCommand>` consumers (host-side).** The slot's registered descriptor
  (`slot.rs`, the `CyclicSlot::descriptor`) gains one message **input port** `MsgIn<SequenceCommand>`
  named `"commands"`, so an edge can target it. `SlotAux` (`coordinator/mod.rs:422-428`) gains
  `commands: MsgIn<SequenceCommand>` — a multi-view `MsgIn` (WP3) over every producer ring wired to
  this slot. `SlotRunner` (`slot.rs:181-223`) holds it; `SlotRunner::new` (`:229-241`) takes it.
  `SlotRunner::step` (`:452-493`) drains it at the **head of the step**, before `execute_raw`:
  ```rust
  fn step(&mut self, now) {
      self.last_now = now;
      let mut cmds = Vec::new();
      self.commands.drain(|c| cmds.push(c));       // multi-view, §3.3
      for cmd in cmds { self.apply_command(&cmd); } // body of today's CyclicSlot::command (:515-527)
      self.publish_status(now);
      // … unchanged occupant poll …
  }
  ```
  `apply_command` is verbatim today's `command` body: the `cmd.channel_id == self.channel_id` filter
  (`:516-518`) + the `SequenceCommandKind` → `do_load/do_start/do_stop/do_abort/do_reset` match
  (`:519-524`) are unchanged. The `CyclicSlot::command` trait method is deleted; command handling is
  now private to `SlotRunner`. `channel_id` assignment and the boot `SequenceRegistry` are untouched.
- **Coordinator as a reserved producer (§6.3, Q6 = hand-registered).** Model the coordinator as a
  reserved pseudo-instance `"coordinator"` (`COORDINATOR_INSTANCE` `:1410`) owning one message output
  port `commands` (`CommandOut<SequenceCommand>`, telemetered=false). Its ring is hand-allocated at
  `build()` (as `command_ring` is today, `:1045`) and seeded into the producer/fan-out tables +
  a synthetic descriptor entry **before** slots are wired, so `PortRef::msg::<SequenceCommand>(
  COORDINATOR_HANDLE)` is a valid edge source and `"coordinator"` is edge-addressable in KDL.
  `control_handle()` (`:1554-1555`) mints a `MsgOut<SequenceCommand>` over that ring, unchanged.
  The ring's `max_readers` = slot fan-out + `READER_SLACK` (no `n_reg` — untelemetered, AllOutputs
  skips it).
- **Uplink normal output.** `UplinkPorts { commands: CommandOut<SequenceCommand> }`; `bind` →
  `CommandOut::bind(src)` (drops `command_out()`). Its ring is allocated/sized/bound as a normal
  message output; consumers (slots) wire edges to it.
- **Implicit fan-out (Q7 = implicit default + explicit override).** At `build()`, after descriptors
  are collected, for each message output port carrying `PortId::Msg(SequenceCommand::ID)` **that has
  no explicit command out-edge**, synthesize an ordinary `Msg` edge to **every** slot's `"commands"`
  input; a port with ≥1 explicit `connect … msg=` edge is left explicit-only. **[Sequencing decision
  — see open questions]** this "opt-out-by-wiring" rule cleanly covers the coordinator + uplink
  (implicit → all slots) while letting an autonomy emitter command one slot explicitly, resolving the
  latent tension in §6.3 (which otherwise would fan an autonomy emitter to all slots *and* its one
  explicit target).

**How it stays green.** Behaviour is identical to today's broadcast-then-filter: every command
emitter fans to every slot (implicit edges), each slot filters by `channel_id`. `control_handle()`
still returns `MsgOut<SequenceCommand>`; `channel_id(name)` (`:1531-1536`) still resolves the
build-order index. The one head-of-cycle `drain_command_bus` stage is replaced by each slot draining
at the head of its own step — same-cycle latency preserved (control_handle emits happen between
cycles; the uplink fills its ring out-of-band).

**Tests.**
- **`tests/slot_integration.rs` must pass with ZERO source change** (drives slots via
  `control_handle()` + `channel_id("adcs")` + `control.emit(&load(ch,…))`). This is the Q7 acceptance
  criterion; if it needs edits, the implicit fan-out is wrong.
- **New** `command_fanin_two_emitters_one_slot` (§7.3): the coordinator's `control_handle` **and** a
  mock uplink both command one slot; assert both land and are filtered by `channel_id`.
- The event/registry drain assertions + mock-uplink test in `slot_integration.rs` exercise the
  unchanged downlink/uplink record path — keep green.

**Dependencies.** WP6 (`telemetered` flag + `CommandOut`), WP4 (message edges + fan-out sizing).

**Risk / rollback (HIGHEST).** This deletes a load-bearing subsystem across `coordinator/mod.rs`,
`coordinator/slot.rs`, `binder.rs`, `telemetry/mod.rs` in one WP — it cannot be bridged (old
`drain_command_bus` + new per-slot drain would double-apply every command). Mitigation / internal
order within the WP: (1) add the slot `"commands"` input port + `SlotRunner`-owned `MsgIn` and the
reserved coordinator producer + implicit fan-out **first**, verifying the new path delivers commands;
(2) only then delete `drain_command_bus`/`command_sources`/`command_ring`/`command_out`/`command_rings`
and switch the uplink to `CommandOut`, in one commit, gated by `tests/slot_integration.rs` staying
green. Rollback is a single-commit revert of the WP. **Reviewer attention:** confirm the
implicit-fan-out opt-out rule (below) before coding.

---

## WP8 — Uplink subscription derived from out-edges (`out_msg_ids` + `subscribe`)

**Goal.** Derive the uplink's ground subscription from its wired out-edges (Q5 = edge-derived, prunes
unwired outputs) instead of the hardcoded `SequenceCommand::ID`.

**Files / functions (verified).**
- `src/binder.rs`: `RingSource` `:115-161`; host `Binder` `:78-88` — **no `id` field today**
  (**[VERIFY]** the design assumed one; it must be added).
- `src/coordinator/mod.rs`: per-system `Binder::new` call `:1233-1239` (inside the bind loop indexed
  by `id` `:1112-1252`); the resolved edges are in scope there.
- `src/telemetry/mod.rs`: `UplinkPorts` `:409-411`; `UplinkSystem::run` `:470-493`; `RecvTransport`
  trait `:96-100` (only `recv`); `TcpRecvTransport::ensure` `:205-225` (hardcoded `MsgStream {
  msg_id: SequenceCommand::ID }` `:215-220`).

**Concrete changes.**
- `RingSource::out_msg_ids(&self) -> Vec<PacketId> { Vec::new() }` (default). Host impl returns the
  `PacketId`s of the currently-binding system's message **output** ports that have ≥1 out-edge.
  **[VERIFY]** because `Binder` has no `id`, add a field `out_msg_ids: Vec<PacketId>` to `Binder`
  (computed for system `id` from the resolved edges at the `Binder::new` call site `:1233-1239`) and
  return it. This is the smallest change consistent with the current struct.
- `UplinkPorts` gains `subscribe: Vec<PacketId>`; `bind` → `{ commands: CommandOut::bind(src),
  subscribe: src.out_msg_ids() }`. `UplinkSystem::run` passes `output.subscribe` to the transport
  once before the first `recv`.
- `RecvTransport::subscribe(&mut self, ids: &[PacketId])` (default no-op for the mock).
  `TcpRecvTransport::ensure` (`:215-220`) sends one `MsgStream { msg_id }` per id instead of the
  hardcoded `SequenceCommand::ID`.

**How it stays green.** With the WP7 implicit fan-out, the uplink's `commands` output has out-edges to
every slot, so `out_msg_ids()` yields `[SequenceCommand::ID]` — identical to today's hardcoded
subscription. The cube-sat example's `MsgStream { SequenceCommand::ID }` subscribe still matches.

**Tests.** Extend the mock-uplink path: assert `subscribe` is called with exactly the wired ids;
assert an uplink whose output is unwired subscribes to nothing (the pruning Q5 buys). Existing uplink
loopback tests stay green.

**Dependencies.** WP7 (uplink is a normal edge output with out-edges to derive from).

**Risk / rollback.** Low-medium. The only subtlety is threading `out_msg_ids` into `Binder` (new
field). Rollback: revert; the uplink returns to a hardcoded subscription.

---

## WP9 — Test migration completion + final gate

**Goal.** Close the test-migration checklist, confirm the ground-side example is unaffected, and run
the full gate before the boundary commit.

**Checklist (verified locations).**
- [ ] `src/message.rs` unit tests `:230-291` — typed `MsgOut` split (**done in WP2**; confirm green).
- [ ] `src/telemetry/tests.rs:424-444` — typed `MsgOut` split (**done in WP2**; confirm).
- [ ] `tests/slot_integration.rs` — **must be source-unchanged** (Q7 acceptance; confirm in WP7).
- [ ] `tests/slot_wiring.rs`, `tests/wiring_resolve.rs`, `tests/dl_integration.rs` — verify green;
      `wiring_resolve.rs` may need a `msg=` case added (WP5), the others should be additive-safe.
- [ ] New tests present: two-system user message edge with cyclic multi-view fan-in (WP4);
      two-emitter fan-in into one slot (WP7) + generic two-emitter (WP4); `AllOutputs` on a
      non-telemetry system (WP6); `msg=` KDL round-trip (WP5).
- [ ] **cube-sat example** (`examples/cube-sat/src/main.rs`) is ground/panel-side (it
      `TcpStream::connect`s to the db and subscribes to streams; uses none of the coordinator/builder
      `add_*` API) — **essentially unaffected**. Confirm it still builds; its `MsgStream {
      SequenceCommand::ID }` subscribe still matches the id the FSW uplink derives (WP8).
- [ ] `examples/adcs-fsw2` (if present) sequence/closed-loop tests stay green (additive change).

**Dependencies.** WP8.

---

## Sequencing rationale

The order is chosen to make each WP an independently reviewable, behaviour-neutral-or-tested step,
minimizing churn:

- **WP1 first, alone.** The kind-tagged `PortDesc` is the widest blast radius but zero behaviour —
  landing it in isolation means every later WP is a small local edit against the new shape, and its
  review is "is this the same data, re-nested?" `cargo build` proves completeness (the old fields are
  gone).
- **WP2 → WP3 → WP4 is the typed-port spine.** Types before the binder before the graph: `MsgOut<M>`
  (WP2) and multi-view `MsgIn<M>` (WP3) satisfy the port contract so message ports drop into bundles
  with **no macro change** (the crux of the design, §2.3); only then does WP4 give them edges. Each
  step keeps frame code byte-identical (guard on `edge.kind`, `BoundInput::One` pass-through).
- **WP5 (front-ends) is deliberately decoupled** from WP6/WP7 — it only needs WP4's edge machinery, so
  it can land in parallel or be deferred; it is the only `kdl`-gated piece.
- **WP6 before WP7** because the command reframe *needs* `telemetered=false` (to keep commands off the
  downlink) and `CommandOut<M>` (the opt-out spelling). `AllOutputs` + self-derived `n_reg` also
  generalize the reader budget the command producer relies on.
- **WP7 is the payoff and the risk sink**, placed as late as possible so every primitive it needs
  (edges, fan-in, telemetered, `CommandOut`, reserved-producer addressing) already exists and is
  tested. It is atomic because the old and new command paths cannot coexist.
- **WP8 after WP7** because `out_msg_ids()` reads the uplink's out-edges, which only exist once WP7
  makes the uplink a normal edge output wired to slots.

Deviation from the design's suggested spine (§ task item 2): the design listed uplink
`out_msg_ids/subscribe` (f) *before* the command reframe (g). I **reverse them** — the uplink has no
out-edges to derive a subscription from until the command reframe wires it to slots, so subscription
derivation must follow the reframe.

---

## Open questions for the reviewer

**ALL RESOLVED (reviewer, 2026-06-30):** (1) **opt-out-by-wiring** confirmed; (2) **edge-derived
`out_msg_ids` threaded as a new `Binder` field** confirmed; (3) **`CommandOut<M>` newtype**
confirmed. Original text retained below.

1. **Implicit-fan-out opt-out rule (WP7).** The design §6.3 recommends "implicit default + explicit
   override" but leaves an ambiguity: a user autonomy emitter declaring `MsgOut<SequenceCommand>`
   would be *both* implicitly fanned to all slots and explicitly wired to its one target. I recommend
   the **opt-out-by-wiring** rule: a command output with **no** explicit command out-edge is
   implicitly fanned to all slots; a command output with ≥1 explicit `connect … msg=` edge is
   explicit-only. This keeps the coordinator + uplink zero-wiring (and `slot_integration.rs`
   unchanged) while giving autonomy emitters constrained topologies. Confirm this interpretation.

2. **`out_msg_ids` threading (WP8).** The design assumed the host `Binder` "already knows the system
   index `id`" (§5.2). It does not — `Binder` is two `slice::Iter` cursors with no `id`. I recommend
   computing the current system's out-edge `PacketId` set at the `Binder::new` call site and passing
   it in as a new `Binder` field. Confirm, or prefer the simpler declared-port-derived set (Q5's
   alternative, no per-system capability, no pruning of unwired outputs).

3. **`CommandOut<M>` opt-out spelling (WP6).** Recommended: a `CommandOut<M>` newtype (type-level
   `telemetered=false`, `Deref` to `MsgOut<M>`). Alternatives are a runtime builder flag or a const
   on the port. The newtype is the only one the type-blind derive picks up for free. Confirm.

---

## Verification per WP

At **every** WP boundary (the crate surface is ungated except WP5's KDL front-end):

```
cargo build -p metor-fsw-2
cargo build -p metor-fsw-2 --no-default-features
cargo test  -p metor-fsw-2 <targeted module>
```

Targeted module per WP: WP1 — full `cargo test -p metor-fsw-2` (representational, all must pass);
WP2 — `message::`, `telemetry::`; WP3 — `message::`, `binder::`; WP4 — `coordinator::`, the new
`msg_edge*` tests; WP5 — `wiring::` (with and without `--features kdl`); WP6 — `telemetry::`,
`registry::`; WP7 — `cargo test -p metor-fsw-2` **including `tests/slot_integration.rs` unchanged**;
WP8 — `telemetry::` uplink tests.

Final gate (WP9), then commit at the task boundary:
```
cargo build -p metor-fsw-2 && cargo build -p metor-fsw-2 --no-default-features \
  && cargo test -p metor-fsw-2 \
  && cargo build -p cube-sat            # ground-side example still builds
  # && cargo test -p adcs-fsw2          # if the example crate is present
```
